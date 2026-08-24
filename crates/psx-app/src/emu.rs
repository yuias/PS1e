//! Emulator worker thread.
//!
//! Owns the [`PsxSystem`] and the audio output, paces emulation against the
//! audio buffer (wall clock when no device exists) and publishes read-only
//! snapshots for the UI. The UI never touches the system directly — it sends
//! [`Command`]s and reads [`Shared`] — so heavy scenes can no longer starve
//! the audio thread behind repaints, and the frontend stays thin enough to
//! port (a wasm build can drive the same snapshots single-threaded).

use crate::audio::Audio;
use psx_core::{CPU_CLOCK_HZ, PsxSystem};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Emulation slice: 5ms of machine time per pacer iteration.
const SLICE: u64 = CPU_CLOCK_HZ / 200;
/// Audio cushion the pacer keeps buffered (frames; ~80ms). Doubles as the
/// output latency, and absorbs host-side load spikes of the same length.
const AUDIO_TARGET: usize = 3_528;

pub enum Command {
    SetRunning(bool),
    Step,
    Reset,
    /// Open the drive lid. The drive stops and reports the shell as open
    /// until a [`Command::CloseShell`] follows.
    OpenShell,
    /// Close the lid, optionally over a new disc (`None` puts the current
    /// one back). A running game sees the swap through the drive status,
    /// so this needs no reset.
    CloseShell(Option<psx_core::cdrom::Disc>),
    SaveState,
    LoadState,
    SetGpuLog(bool),
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum DebuggerState {
    /// No --debug-port.
    #[default]
    None,
    Listening,
    /// --wait-debugger holds execution until the first attach.
    Waiting,
    Running,
    Halted,
}

/// Cheap per-slice snapshot for the UI panels.
#[derive(Clone, Default)]
pub struct Status {
    pub pc: u32,
    pub cycles: u64,
    pub regs: [u32; 32],
    pub hi: u32,
    pub lo: u32,
    pub running: bool,
    pub debugger: DebuggerState,
    /// Stereo frames queued at the audio device.
    pub audio_buffered: usize,
    /// Callbacks that ran out of samples (audible as crackle).
    pub audio_underruns: u64,
}

/// Copy of the GPU's vblank-latched frame (see [`psx_core::gpu::Frame`]).
#[derive(Default)]
pub struct FrameSnapshot {
    pub pixels: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub is_24bit: bool,
    pub enabled: bool,
    /// Vblank counter; lets the UI skip uploads of unchanged frames.
    pub count: u64,
}

/// State published by the worker and inputs fed back by the UI.
#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<FrameSnapshot>,
    pub status: Mutex<Status>,
    /// Full TTY text, appended incrementally.
    pub tty: Mutex<String>,
    /// VRAM copy, refreshed per frame while `vram_requested`.
    pub vram: Mutex<Vec<u16>>,
    pub vram_requested: AtomicBool,
    /// Digital pad bits (UI -> worker).
    pub buttons: AtomicU16,
    /// Master volume as f32 bits (UI -> worker).
    pub volume: AtomicU32,
}

/// Everything the worker owns besides the system itself.
pub struct WorkerConfig {
    pub bios: Vec<u8>,
    pub memcard_path: PathBuf,
    pub state_path: PathBuf,
    pub debugger: Option<psx_debug::DebugServer>,
    pub wait_debugger: bool,
    pub volume: f32,
    pub log_gpu: bool,
}

pub struct Emu {
    pub shared: Arc<Shared>,
    tx: mpsc::Sender<Command>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Emu {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for Emu {
    /// Stop the worker; it flushes the memory card before exiting.
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn spawn(sys: PsxSystem, cfg: WorkerConfig, ctx: eframe::egui::Context) -> Emu {
    let shared = Arc::new(Shared::default());
    shared.volume.store(cfg.volume.to_bits(), Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let sh = shared.clone();
    let join = std::thread::Builder::new()
        .name("emu".into())
        .spawn(move || Worker::new(sys, cfg, sh, rx, ctx).run())
        .expect("failed to spawn emulator thread");
    Emu {
        shared,
        tx,
        join: Some(join),
    }
}

struct Worker {
    sys: PsxSystem,
    cfg: WorkerConfig,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Command>,
    ctx: eframe::egui::Context,
    /// Created on this thread: cpal streams are not Send everywhere.
    audio: Option<Audio>,
    running: bool,
    debugger_seen: bool,
    scratch: Vec<i16>,
    published_frame: u64,
    tty_len: usize,
    /// Wall-clock pacer (only used when no audio device exists).
    clock: Instant,
    deficit: f64,
}

impl Worker {
    fn new(
        sys: PsxSystem,
        cfg: WorkerConfig,
        shared: Arc<Shared>,
        rx: mpsc::Receiver<Command>,
        ctx: eframe::egui::Context,
    ) -> Self {
        Self {
            sys,
            cfg,
            shared,
            rx,
            ctx,
            audio: None,
            running: false,
            debugger_seen: false,
            scratch: Vec::new(),
            published_frame: 0,
            tty_len: 0,
            clock: Instant::now(),
            deficit: 0.0,
        }
    }

    fn debugger_active(&self) -> bool {
        self.cfg.debugger.as_ref().is_some_and(|d| d.attached())
            || (self.cfg.wait_debugger && !self.debugger_seen)
    }

    fn run(mut self) {
        self.audio = Audio::new();
        self.sys.bus.gpu.log_commands = self.cfg.log_gpu;
        loop {
            if !self.handle_commands() {
                break;
            }
            self.sys
                .set_buttons(self.shared.buttons.load(Ordering::Relaxed));

            // While a debugger is attached (or awaited) it owns execution.
            let mut worked = false;
            if let Some(dbg) = &mut self.cfg.debugger {
                dbg.pump(&mut self.sys, SLICE);
                self.debugger_seen |= dbg.attached();
                worked = dbg.attached() && !dbg.halted();
            }

            if !self.debugger_active() && self.running {
                worked |= self.pace_slice();
            }

            self.push_audio();
            self.publish();
            self.flush_memcard();

            if !worked {
                // Paused / halted / buffer full: 2ms is well inside the
                // ~80ms audio cushion (44.1 frames drain per ms)
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.flush_memcard();
    }

    /// Returns false when Quit was received.
    fn handle_commands(&mut self) -> bool {
        while let Ok(cmd) = self.rx.try_recv() {
            let debugger_active = self.debugger_active();
            match cmd {
                Command::SetRunning(r) if !debugger_active => self.running = r,
                Command::Step if !debugger_active => {
                    self.running = false;
                    self.sys.step();
                }
                Command::Reset if !debugger_active => {
                    self.running = false;
                    // Ambient assets survive a reset: disc, memory card
                    // (mid-write contents included) and the log switch
                    let log = self.sys.bus.gpu.log_commands;
                    let disc = self.sys.bus.cdrom.take_disc();
                    let memcard = std::mem::take(&mut self.sys.bus.sio.memcard);
                    self.sys = PsxSystem::new(self.cfg.bios.clone()).expect("reset failed");
                    self.sys.bus.gpu.log_commands = log;
                    self.sys.bus.cdrom.set_disc(disc);
                    self.sys.bus.sio.memcard = memcard;
                }
                Command::OpenShell if !debugger_active => self.sys.open_shell(),
                Command::CloseShell(disc) if !debugger_active => self.sys.close_shell(disc),
                Command::SetRunning(_)
                | Command::Step
                | Command::Reset
                | Command::OpenShell
                | Command::CloseShell(_) => {}
                Command::SaveState => match self.sys.save_state() {
                    Ok(data) => match std::fs::write(&self.cfg.state_path, &data) {
                        Ok(()) => {
                            tracing::info!("state saved to {}", self.cfg.state_path.display())
                        }
                        Err(e) => tracing::error!("state save failed: {e}"),
                    },
                    Err(e) => tracing::error!("state save failed: {e}"),
                },
                // Loading mutates execution state, so it stays with the
                // debugger while one is attached (same rule as run control)
                Command::LoadState => {
                    if debugger_active {
                        continue;
                    }
                    match std::fs::read(&self.cfg.state_path) {
                        Ok(data) => match self.sys.load_state(&data) {
                            Ok(()) => tracing::info!(
                                "state loaded from {}",
                                self.cfg.state_path.display()
                            ),
                            Err(e) => tracing::error!("state load failed: {e}"),
                        },
                        Err(e) => tracing::error!(
                            "state load failed: {e} ({})",
                            self.cfg.state_path.display()
                        ),
                    }
                }
                Command::SetGpuLog(v) => self.sys.bus.gpu.log_commands = v,
                Command::Quit => return false,
            }
        }
        true
    }

    /// Run one slice if the pacer allows it. With an audio device the SPU's
    /// cycle-locked 44.1kHz output is the clock: run whenever the buffer is
    /// below target, which also gives full-host-speed catch-up after a load
    /// spike. Without one, pace against the wall clock.
    fn pace_slice(&mut self) -> bool {
        match &self.audio {
            Some(audio) => {
                if audio.buffered_frames() < AUDIO_TARGET {
                    self.sys.run_cycles(SLICE);
                    true
                } else {
                    false
                }
            }
            None => {
                let dt = std::mem::replace(&mut self.clock, Instant::now()).elapsed();
                self.deficit += dt.as_secs_f64() * CPU_CLOCK_HZ as f64;
                // Cap the backlog so a long stall doesn't fast-forward
                self.deficit = self.deficit.min(3.0 * SLICE as f64);
                if self.deficit >= SLICE as f64 {
                    self.deficit -= SLICE as f64;
                    self.sys.run_cycles(SLICE);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn push_audio(&mut self) {
        self.scratch.clear();
        self.sys.bus.spu.drain_output(&mut self.scratch);
        if let Some(audio) = &self.audio {
            let vol = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
            for s in &mut self.scratch {
                *s = (*s as f32 * vol) as i16;
            }
            audio.push_samples(&self.scratch);
        }
    }

    fn publish(&mut self) {
        let gpu = &self.sys.bus.gpu;
        if gpu.frame_count != self.published_frame {
            self.published_frame = gpu.frame_count;
            {
                let mut f = self.shared.frame.lock().unwrap();
                f.pixels.clear();
                f.pixels.extend_from_slice(&gpu.frame.pixels);
                f.width = gpu.frame.width;
                f.height = gpu.frame.height;
                f.stride = gpu.frame.stride;
                f.is_24bit = gpu.frame.is_24bit;
                f.enabled = gpu.frame.enabled;
                f.count = gpu.frame_count;
            }
            if self.shared.vram_requested.load(Ordering::Relaxed) {
                let mut v = self.shared.vram.lock().unwrap();
                v.clear();
                v.extend_from_slice(&gpu.vram);
            }
            self.ctx.request_repaint();
        }

        {
            let mut st = self.shared.status.lock().unwrap();
            st.pc = self.sys.cpu.pc;
            st.cycles = self.sys.cycles();
            st.regs = self.sys.cpu.regs;
            st.hi = self.sys.cpu.hi;
            st.lo = self.sys.cpu.lo;
            st.running = self.running;
            st.debugger = match &self.cfg.debugger {
                None => DebuggerState::None,
                Some(d) if d.attached() && d.halted() => DebuggerState::Halted,
                Some(d) if d.attached() => DebuggerState::Running,
                Some(_) if self.cfg.wait_debugger && !self.debugger_seen => DebuggerState::Waiting,
                Some(_) => DebuggerState::Listening,
            };
            if let Some(audio) = &self.audio {
                st.audio_buffered = audio.buffered_frames();
                st.audio_underruns = audio.underruns();
            }
        }

        let tty = self.sys.tty_output();
        if tty.len() > self.tty_len {
            self.shared
                .tty
                .lock()
                .unwrap()
                .push_str(&tty[self.tty_len..]);
            self.tty_len = tty.len();
        }
    }

    fn flush_memcard(&mut self) {
        if self.sys.bus.sio.memcard.take_dirty() {
            if let Err(e) = std::fs::write(&self.cfg.memcard_path, &self.sys.bus.sio.memcard.data) {
                tracing::error!("failed to save memory card: {e}");
            } else {
                tracing::info!("memory card saved");
            }
        }
    }
}
