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
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Emulation slice: 5ms of machine time per pacer iteration.
const SLICE: u64 = CPU_CLOCK_HZ / 200;
/// Audio cushion the pacer keeps buffered (frames; ~80ms). Doubles as the
/// output latency, and absorbs host-side load spikes of the same length.
const AUDIO_TARGET: usize = 3_528;

/// The register file is copied into [`Status`] only for this bit.
pub const PANEL_REGS: u8 = 1 << 0;
/// The 1 MiB VRAM copy runs only for this bit.
pub const PANEL_VRAM: u8 = 1 << 1;
/// The memory viewer's window is copied only for this bit.
pub const PANEL_MEMORY: u8 = 1 << 2;

/// Bytes the memory viewer shows at once.
pub const VIEW_BYTES: usize = 256;

/// One window of RAM for the viewer, refreshed per frame while the page
/// is open. `base` is a RAM offset, not a bus address.
#[derive(Clone, Default)]
pub struct MemoryView {
    pub base: u32,
    pub bytes: Vec<u8>,
}

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
    CloseShell(Option<crate::disc::LoadedDisc>),
    SaveState,
    LoadState,
    SetGpuLog(bool),
    /// Replace the cheat table, e.g. after an enable toggle or a reload.
    SetCheats(psx_core::cheats::CheatList),
    /// Master switch over the cheat table.
    SetCheatsEnabled(bool),
    /// Run one scanner pass and leave the result in [`Shared::scan`].
    Scan(crate::scan::Request),
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

impl DebuggerState {
    /// The debugger holds run control, so the UI must not offer it and the
    /// worker must drop the commands that would take it. Both sides ask
    /// this rather than each spelling the rule out.
    pub fn owns_execution(self) -> bool {
        matches!(self, Self::Waiting | Self::Running | Self::Halted)
    }
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoticeLevel {
    Info,
    Error,
}

/// The outcome of something the user asked for that has nowhere else to be
/// seen. Commands are fire-and-forget, so without this a failed save or a
/// failed screenshot reaches the log and nothing else.
#[derive(Clone)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

/// State published by the worker and inputs fed back by the UI.
#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<FrameSnapshot>,
    pub status: Mutex<Status>,
    /// Full TTY text, appended incrementally.
    pub tty: Mutex<String>,
    /// VRAM copy, refreshed per frame while [`PANEL_VRAM`] is set, and the
    /// vblank count it was taken at so the UI can tell a fresh copy from
    /// the one it already turned into a texture.
    pub vram: Mutex<Vec<u16>>,
    pub vram_count: AtomicU64,
    /// Memory viewer: the RAM offset the UI is looking at (UI -> worker)
    /// and the window read back from it.
    pub view_base: AtomicU32,
    pub memory: Mutex<MemoryView>,
    /// Last scanner pass. The candidate list itself stays on the worker,
    /// so it survives the page being switched away from -- the whole
    /// point of the loop is to go back to the game and come back.
    pub scan: Mutex<Option<crate::scan::Outcome>>,
    /// Which UI panels are on screen, as [`PANEL_REGS`] and friends. What a
    /// hidden panel would show costs nothing to leave unpublished, so the
    /// worker skips the copy rather than the UI skipping the draw.
    pub panels: AtomicU8,
    /// Latest notice for the status bar. A slot rather than a queue: the
    /// bar has room for one line, and the newest outcome is the one the
    /// user is waiting on. Written by both the worker and the UI.
    pub notice: Mutex<Option<Notice>>,
    /// Digital pad bits (UI -> worker).
    pub buttons: AtomicU16,
    /// Master volume as f32 bits (UI -> worker).
    pub volume: AtomicU32,
}

impl Shared {
    /// Post a notice for the status bar, replacing any earlier one. The
    /// worker must follow this with a repaint request; the UI is already
    /// inside a frame when it calls this.
    pub fn notify(&self, level: NoticeLevel, text: impl Into<String>) {
        *self.notice.lock().unwrap() = Some(Notice {
            level,
            text: text.into(),
        });
    }
}

/// Everything the worker owns besides the system itself.
pub struct WorkerConfig {
    pub memcard_path: PathBuf,
    pub state_path: PathBuf,
    pub debugger: Option<psx_debug::DebugServer>,
    pub wait_debugger: bool,
    pub volume: f32,
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

/// Wakes the UI so it draws what the worker has just published. Boxed
/// rather than an `egui::Context` so nothing in this module depends on the
/// windowing layer — the portability claim in the module doc is only true
/// if the worker cannot name it.
pub type Repaint = Box<dyn Fn() + Send>;

pub fn spawn(sys: PsxSystem, cfg: WorkerConfig, repaint: Repaint) -> Emu {
    let shared = Arc::new(Shared::default());
    shared.volume.store(cfg.volume.to_bits(), Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let sh = shared.clone();
    let join = std::thread::Builder::new()
        .name("emu".into())
        .spawn(move || Worker::new(sys, cfg, sh, rx, repaint).run())
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
    repaint: Repaint,
    /// Created on this thread: cpal streams are not Send everywhere.
    audio: Option<Audio>,
    running: bool,
    debugger_seen: bool,
    scratch: Vec<i16>,
    published_frame: u64,
    /// Monotonic TTY position already copied to `shared.tty`.
    tty_pos: u64,
    /// Wall-clock pacer (only used when no audio device exists).
    clock: Instant,
    deficit: f64,
    /// Scanner candidates, kept across passes and across the UI showing
    /// some other page.
    scan: Option<crate::scan::Scan>,
}

impl Worker {
    fn new(
        sys: PsxSystem,
        cfg: WorkerConfig,
        shared: Arc<Shared>,
        rx: mpsc::Receiver<Command>,
        repaint: Repaint,
    ) -> Self {
        let autostart = !cfg.wait_debugger;
        Self {
            sys,
            cfg,
            shared,
            rx,
            repaint,
            audio: None,
            // A configured BIOS is enough to boot, so start the machine
            // instead of opening every session on a paused black screen.
            // Only --wait-debugger deliberately holds at the reset vector.
            running: autostart,
            debugger_seen: false,
            scratch: Vec::new(),
            published_frame: 0,
            tty_pos: 0,
            clock: Instant::now(),
            deficit: 0.0,
            scan: None,
        }
    }

    /// What the UI is told about the debugger, and the same value the
    /// worker gates commands on — one rule, not two that can drift.
    fn debugger_state(&self) -> DebuggerState {
        match &self.cfg.debugger {
            None => DebuggerState::None,
            Some(d) if d.attached() && d.halted() => DebuggerState::Halted,
            Some(d) if d.attached() => DebuggerState::Running,
            Some(_) if self.cfg.wait_debugger && !self.debugger_seen => DebuggerState::Waiting,
            Some(_) => DebuggerState::Listening,
        }
    }

    fn debugger_active(&self) -> bool {
        self.debugger_state().owns_execution()
    }

    /// Post a notice and wake the UI to draw it. The worker is off the UI
    /// thread, so unlike the UI's own notices this one needs the repaint.
    fn notify(&self, level: NoticeLevel, text: impl Into<String>) {
        self.shared.notify(level, text);
        (self.repaint)();
    }

    fn run(mut self) {
        self.audio = Audio::new();
        loop {
            if !self.handle_commands() {
                break;
            }
            self.sys
                .set_buttons(self.shared.buttons.load(Ordering::Relaxed));

            // One pacer for both owners: a debugger's `continue` gets the
            // same budget the free-running case does, so attaching gdb does
            // not turn the machine into a fast-forward with unpaced audio
            // piling up behind it.
            let before = self.sys.cycles();
            let budget = self.slice_budget();

            // While a debugger is attached (or awaited) it owns execution.
            if let Some(dbg) = &mut self.cfg.debugger {
                dbg.pump(&mut self.sys, budget);
                self.debugger_seen |= dbg.attached();
            }

            if budget > 0 && !self.debugger_active() && self.running {
                self.sys.run_cycles(budget);
            }

            // Whether anything ran, not whether someone was entitled to run:
            // a paced-out iteration must still reach the sleep below.
            let worked = self.sys.cycles() != before;

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
                Command::Reset if !debugger_active => self.sys.reset(),
                Command::OpenShell if !debugger_active => self.sys.open_shell(),
                Command::CloseShell(disc) if !debugger_active => {
                    // A new disc brings its own cheats; putting the same
                    // one back (`None`) leaves the table alone.
                    if let Some(loaded) = disc {
                        self.sys.set_cheats(loaded.cheats);
                        self.sys.close_shell(Some(loaded.disc));
                    } else {
                        self.sys.close_shell(None);
                    }
                }
                Command::SetRunning(_)
                | Command::Step
                | Command::Reset
                | Command::OpenShell
                | Command::CloseShell(_) => {}
                Command::SaveState => {
                    let path = self.cfg.state_path.clone();
                    match self
                        .sys
                        .save_state()
                        .map_err(|e| e.to_string())
                        .and_then(|data| std::fs::write(&path, &data).map_err(|e| e.to_string()))
                    {
                        Ok(()) => {
                            tracing::info!("state saved to {}", path.display());
                            self.notify(
                                NoticeLevel::Info,
                                format!("state saved to {}", path.display()),
                            );
                        }
                        Err(e) => {
                            tracing::error!("state save failed: {e}");
                            self.notify(NoticeLevel::Error, format!("state save failed: {e}"));
                        }
                    }
                }
                // Loading mutates execution state, so it stays with the
                // debugger while one is attached (same rule as run control)
                Command::LoadState => {
                    if debugger_active {
                        continue;
                    }
                    let path = self.cfg.state_path.clone();
                    match std::fs::read(&path)
                        .map_err(|e| format!("{e} ({})", path.display()))
                        .and_then(|data| self.sys.load_state(&data))
                    {
                        Ok(()) => {
                            tracing::info!("state loaded from {}", path.display());
                            self.notify(
                                NoticeLevel::Info,
                                format!("state loaded from {}", path.display()),
                            );
                        }
                        Err(e) => {
                            tracing::error!("state load failed: {e}");
                            self.notify(NoticeLevel::Error, format!("state load failed: {e}"));
                        }
                    }
                }
                Command::SetGpuLog(v) => self.sys.set_gpu_log(v),
                Command::SetCheats(list) => self.sys.set_cheats(list),
                Command::SetCheatsEnabled(on) => self.sys.set_cheats_enabled(on),
                Command::Scan(req) => {
                    let (scan, outcome) =
                        crate::scan::Scan::pass(self.scan.take(), req, self.sys.ram());
                    self.scan = Some(scan);
                    *self.shared.scan.lock().unwrap() = Some(outcome);
                    (self.repaint)();
                }
                Command::Quit => return false,
            }
        }
        true
    }

    /// Cycles the owner of execution may advance this iteration: one slice,
    /// or zero while the pacer is still ahead of the wall clock.
    ///
    /// With an audio device the SPU's cycle-locked 44.1kHz output is the
    /// clock: run whenever the buffer is below target, which also gives
    /// full-host-speed catch-up after a load spike. Without one, pace
    /// against the wall clock; that branch consumes a deficit, so this must
    /// be called exactly once per iteration.
    fn slice_budget(&mut self) -> u64 {
        match &self.audio {
            Some(audio) => {
                if audio.buffered_frames() < AUDIO_TARGET {
                    SLICE
                } else {
                    0
                }
            }
            None => {
                let dt = std::mem::replace(&mut self.clock, Instant::now()).elapsed();
                self.deficit += dt.as_secs_f64() * CPU_CLOCK_HZ as f64;
                // Cap the backlog so a long stall doesn't fast-forward
                self.deficit = self.deficit.min(3.0 * SLICE as f64);
                if self.deficit >= SLICE as f64 {
                    self.deficit -= SLICE as f64;
                    SLICE
                } else {
                    0
                }
            }
        }
    }

    fn push_audio(&mut self) {
        self.scratch.clear();
        self.sys.drain_audio(&mut self.scratch);
        if let Some(audio) = &self.audio {
            let vol = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
            for s in &mut self.scratch {
                *s = (*s as f32 * vol) as i16;
            }
            audio.push_samples(&self.scratch);
        }
    }

    /// Copy the window the viewer is looking at. Clamped and 16-aligned
    /// here rather than in the UI so a typo cannot ask for an out-of-range
    /// slice, and so the page always has a full row to draw.
    fn publish_memory(&mut self) {
        let ram = self.sys.ram();
        let base = (self.shared.view_base.load(Ordering::Relaxed) as usize & !0xF)
            .min(ram.len() - VIEW_BYTES);
        let mut m = self.shared.memory.lock().unwrap();
        m.base = base as u32;
        m.bytes.clear();
        m.bytes.extend_from_slice(&ram[base..base + VIEW_BYTES]);
    }

    fn publish(&mut self) {
        let panels = self.shared.panels.load(Ordering::Relaxed);
        let gpu = self.sys.gpu();
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
            if panels & PANEL_VRAM != 0 {
                let mut v = self.shared.vram.lock().unwrap();
                v.clear();
                v.extend_from_slice(&gpu.vram);
                self.shared
                    .vram_count
                    .store(gpu.frame_count, Ordering::Relaxed);
            }
            (self.repaint)();
        }

        {
            let mut st = self.shared.status.lock().unwrap();
            st.pc = self.sys.cpu.pc;
            st.cycles = self.sys.cycles();
            if panels & PANEL_REGS != 0 {
                st.regs = self.sys.cpu.regs;
                st.hi = self.sys.cpu.hi;
                st.lo = self.sys.cpu.lo;
            }
            st.running = self.running;
            st.debugger = self.debugger_state();
            if let Some(audio) = &self.audio {
                st.audio_buffered = audio.buffered_frames();
                st.audio_underruns = audio.underruns();
            }
        }

        if panels & PANEL_MEMORY != 0 {
            self.publish_memory();
        }

        let (new, pos) = self.sys.tty_since(self.tty_pos);
        if !new.is_empty() {
            self.shared.tty.lock().unwrap().push_str(new);
        }
        self.tty_pos = pos;
    }

    fn flush_memcard(&mut self) {
        if self.sys.memcard_mut().take_dirty() {
            if let Err(e) = std::fs::write(&self.cfg.memcard_path, &self.sys.memcard().data) {
                tracing::error!("failed to save memory card: {e}");
                self.notify(NoticeLevel::Error, format!("memory card not saved: {e}"));
            } else {
                tracing::info!("memory card saved");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ps1e-emu-test-{}-{tag}", std::process::id()))
    }

    /// A worker with no audio device and no debugger, driven by calling its
    /// methods directly rather than by `run()`.
    fn worker(cfg: WorkerConfig) -> (Worker, Arc<Shared>, mpsc::Sender<Command>) {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let sys = PsxSystem::new(vec![0; 512 * 1024]).expect("system");
        let w = Worker::new(sys, cfg, shared.clone(), rx, Box::new(|| {}));
        (w, shared, tx)
    }

    fn config(tag: &str) -> WorkerConfig {
        WorkerConfig {
            memcard_path: tmp(&format!("{tag}.mcr")),
            state_path: tmp(&format!("{tag}.sst")),
            debugger: None,
            wait_debugger: false,
            volume: 1.0,
        }
    }

    fn notice(shared: &Shared) -> Notice {
        shared.notice.lock().unwrap().clone().expect("a notice")
    }

    /// The wall-clock pacer must not bank an unbounded backlog: a long stall
    /// is worth at most three slices of catch-up, or resuming a stalled
    /// window would fast-forward the machine by the whole stall.
    #[test]
    fn the_wall_clock_pacer_caps_its_backlog_at_three_slices() {
        let (mut w, _, _) = worker(config("pacer"));
        assert!(w.audio.is_none(), "the test worker has no audio device");
        w.clock = Instant::now() - Duration::from_secs(10);

        let granted: Vec<u64> = (0..4).map(|_| w.slice_budget()).collect();
        assert_eq!(granted, [SLICE, SLICE, SLICE, 0]);
    }

    #[test]
    fn quit_stops_the_worker() {
        let (mut w, _, tx) = worker(config("quit"));
        tx.send(Command::SetRunning(false)).unwrap();
        assert!(w.handle_commands());
        tx.send(Command::Quit).unwrap();
        assert!(!w.handle_commands());
    }

    /// Run control belongs to the debugger while it owns execution, so the
    /// commands that would take it back are dropped rather than queued.
    #[test]
    fn run_control_is_dropped_while_the_debugger_owns_execution() {
        let mut cfg = config("gated");
        cfg.debugger = Some(psx_debug::DebugServer::bind(0).expect("bind"));
        cfg.wait_debugger = true;
        let (mut w, _, tx) = worker(cfg);
        assert!(w.debugger_active());

        w.running = false;
        tx.send(Command::SetRunning(true)).unwrap();
        assert!(w.handle_commands());
        assert!(
            !w.running,
            "SetRunning must not reach a debugger-owned worker"
        );
    }

    #[test]
    fn a_state_round_trip_reports_through_the_notice_slot() {
        let cfg = config("roundtrip");
        let path = cfg.state_path.clone();
        let _ = std::fs::remove_file(&path);
        let (mut w, shared, tx) = worker(cfg);

        w.sys.run_cycles(10_000);
        let saved_at = w.sys.cycles();
        tx.send(Command::SaveState).unwrap();
        assert!(w.handle_commands());
        assert_eq!(notice(&shared).level, NoticeLevel::Info);

        w.sys.run_cycles(10_000);
        assert_ne!(w.sys.cycles(), saved_at);
        tx.send(Command::LoadState).unwrap();
        assert!(w.handle_commands());
        assert_eq!(w.sys.cycles(), saved_at);
        assert_eq!(notice(&shared).level, NoticeLevel::Info);

        let _ = std::fs::remove_file(&path);
    }

    /// The failure that used to reach `tracing` and nothing else.
    #[test]
    fn a_failed_state_save_reaches_the_notice_slot() {
        let mut cfg = config("unwritable");
        cfg.state_path = tmp("unwritable-dir").join("nested").join("state.sst");
        let (mut w, shared, tx) = worker(cfg);

        tx.send(Command::SaveState).unwrap();
        assert!(w.handle_commands());
        let n = notice(&shared);
        assert_eq!(n.level, NoticeLevel::Error);
        assert!(n.text.contains("state save failed"), "{}", n.text);
    }
}
