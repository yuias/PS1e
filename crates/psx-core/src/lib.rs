//! psx-core: platform-independent PlayStation 1 emulator core.
//!
//! No windowing, graphics-API or I/O dependencies live here; frontends
//! (native, wasm) drive [`PsxSystem`] and present its output.

pub mod bus;
pub mod cdrom;
pub mod cpu;
pub mod dma;
pub mod gpu;
pub mod mdec;
pub mod memcard;
pub mod scheduler;
pub mod sio;
pub mod spu;
pub mod timers;

use bus::Bus;
use cpu::Cpu;
use scheduler::{EventKind, Scheduler};
use tracing::info;

/// CPU clock: 33.8688 MHz.
pub const CPU_CLOCK_HZ: u64 = 33_868_800;
/// NTSC field, in CPU cycles. A nominal unit for callers that need a
/// stable "one frame" figure; the running machine follows the display
/// mode's [`gpu::VideoTiming`], which is longer in PAL.
pub const CYCLES_PER_FRAME: u64 = gpu::VideoTiming::NTSC.cycles_per_frame();

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PsxSystem {
    pub cpu: Cpu,
    pub bus: Bus,
    scheduler: Scheduler,
    cycles: u64,
    next_sample: u64,
    tty: String,
}

/// Save-state file magic + format version. Bump the version on any change
/// to a serialized struct.
const STATE_MAGIC: &[u8; 4] = b"PS1E";
const STATE_VERSION: u16 = 7;

/// Cheap content fingerprint (FNV-1a) to flag cross-BIOS state loads.
fn bios_fingerprint(bios: &[u8]) -> u32 {
    bios.iter().fold(0x811c_9dc5u32, |h, b| {
        (h ^ *b as u32).wrapping_mul(0x0100_0193)
    })
}

impl PsxSystem {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        let mut scheduler = Scheduler::new();
        scheduler.schedule(CYCLES_PER_FRAME, EventKind::VBlank);
        Ok(Self {
            cpu: Cpu::new(),
            bus: Bus::new(bios)?,
            scheduler,
            cycles: 0,
            next_sample: spu::CYCLES_PER_SAMPLE,
            tty: String::new(),
        })
    }

    /// Total elapsed CPU cycles since reset.
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Serialize the complete machine state. The BIOS image, disc image and
    /// memory card are *not* included: they are ambient assets the frontend
    /// owns (and rolling back the memory card would corrupt real saves).
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(8 * 1024 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&bios_fingerprint(&self.bus.bios).to_le_bytes());
        postcard::to_extend(self, out).map_err(|e| format!("serialize failed: {e}"))
    }

    /// Restore a state produced by [`PsxSystem::save_state`], carrying over
    /// the current BIOS, disc and memory card.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let (header, body) = data
            .split_at_checked(10)
            .ok_or("state file too short".to_string())?;
        if &header[..4] != STATE_MAGIC {
            return Err("not a PS1e save state".into());
        }
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        if version != STATE_VERSION {
            return Err(format!(
                "state version {version} not supported (expected {STATE_VERSION})"
            ));
        }
        let fingerprint = u32::from_le_bytes(header[6..10].try_into().unwrap());
        if fingerprint != bios_fingerprint(&self.bus.bios) {
            tracing::warn!("state was saved with a different BIOS image; expect instability");
        }
        let mut new: PsxSystem =
            postcard::from_bytes(body).map_err(|e| format!("deserialize failed: {e}"))?;
        new.bus.bios = std::mem::take(&mut self.bus.bios);
        new.bus.cdrom.set_disc(self.bus.cdrom.take_disc());
        new.bus.sio.memcard = std::mem::take(&mut self.bus.sio.memcard);
        new.bus.gpu.log_commands = self.bus.gpu.log_commands;
        *self = new;
        Ok(())
    }

    /// TTY output captured so far (kernel `putchar`).
    pub fn tty_output(&self) -> &str {
        &self.tty
    }

    /// Side-load a PS-X EXE image over the running machine.
    ///
    /// This is the shortcut the BIOS shell would otherwise take after
    /// reading the executable off a disc, so the caller must let the BIOS
    /// reach the shell entry point (`0x8003_0000`) first — the kernel must
    /// have set up its jump tables, or the loaded program has nothing to
    /// call. Used to run test executables that never ship as disc images.
    pub fn load_exe(&mut self, exe: &[u8]) -> Result<(), String> {
        const HEADER_SIZE: usize = 0x800;
        if exe.len() < HEADER_SIZE || &exe[..8] != b"PS-X EXE" {
            return Err("not a PS-X EXE image".into());
        }
        let word = |off: usize| u32::from_le_bytes(exe[off..off + 4].try_into().unwrap());

        let pc = word(0x10);
        let gp = word(0x14);
        let dest = word(0x18);
        let size = word(0x1c) as usize;
        let (bss, bss_size) = (word(0x28), word(0x2c) as usize);
        let sp = word(0x30).wrapping_add(word(0x34));

        let body = exe
            .get(HEADER_SIZE..HEADER_SIZE + size)
            .ok_or_else(|| format!("EXE declares {size} bytes of body but is truncated"))?;
        let base = (dest & (bus::RAM_SIZE as u32 - 1)) as usize;
        if base + size > bus::RAM_SIZE {
            return Err(format!("EXE at {dest:#010x} does not fit in RAM"));
        }
        self.bus.ram[base..base + size].copy_from_slice(body);

        // The shell zero-fills bss before jumping; programs rely on it.
        let bss_base = (bss & (bus::RAM_SIZE as u32 - 1)) as usize;
        if bss_size > 0 && bss_base + bss_size <= bus::RAM_SIZE {
            self.bus.ram[bss_base..bss_base + bss_size].fill(0);
        }

        self.cpu.set_pc(pc);
        self.cpu.regs[28] = gp;
        if sp != 0 {
            self.cpu.regs[29] = sp;
            self.cpu.regs[30] = sp;
        }
        Ok(())
    }

    pub fn insert_disc(&mut self, disc: cdrom::Disc) {
        self.bus.cdrom.insert_disc(disc);
    }

    /// Open the drive lid, stopping the drive and flagging the shell as
    /// open. Pair with [`PsxSystem::close_shell`] to swap a disc while a
    /// game runs — resetting instead would defeat the point.
    pub fn open_shell(&mut self) {
        self.bus.cdrom.open_shell(self.cycles);
    }

    /// Close the lid, optionally over a new disc (`None` keeps the current
    /// one, e.g. when the user cancels the picker).
    pub fn close_shell(&mut self, disc: Option<cdrom::Disc>) {
        self.bus.cdrom.close_shell(disc);
    }

    /// Update controller state (bits per [`sio::button`], set = pressed).
    pub fn set_buttons(&mut self, buttons: u16) {
        self.bus.sio.buttons = buttons;
    }

    /// Execute a single CPU instruction, then fire any due events.
    pub fn step(&mut self) {
        self.observe_tty();
        self.bus.now = self.cycles;
        self.cpu.step(&mut self.bus);
        // 1 pipeline cycle per instruction plus the wait states its bus
        // accesses accumulated (I-cache hits in cached segments are free)
        self.cycles += 1 + std::mem::take(&mut self.bus.penalty);

        let bus::Bus {
            cdrom,
            sio,
            spu,
            irq,
            ..
        } = &mut self.bus;
        cdrom.tick(self.cycles, irq);
        sio.tick(self.cycles, irq);
        // Route decoded XA audio into the SPU's CD input. Cap the SPU-side
        // level so backlog stays in the drive's buffer, where it throttles
        // further sector reads (see cdrom back-pressure).
        while spu.cd_in_level() < 9_408 {
            let Some(l) = cdrom.xa_out.pop_front() else {
                break;
            };
            let r = cdrom.xa_out.pop_front().unwrap_or(0);
            spu.push_cd_audio(l, r);
        }
        while self.cycles >= self.next_sample {
            spu.generate_sample(irq);
            self.next_sample += spu::CYCLES_PER_SAMPLE;
        }

        while let Some(event) = self.scheduler.pop_due(self.cycles) {
            self.handle_event(event);
        }
    }

    /// Run for roughly `cycles` CPU cycles.
    pub fn run_cycles(&mut self, cycles: u64) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            self.step();
        }
    }

    fn handle_event(&mut self, event: EventKind) {
        if event == EventKind::VBlank {
            let timing = self.bus.gpu.video_timing();
            self.bus.irq.raise(0);
            self.bus.gpu.vblank();
            // Keep lazily-synced components from lagging more than a frame,
            // then hand them the field boundary they measure blanking from
            self.bus
                .timers
                .sync_all(self.cycles, timing, &mut self.bus.irq);
            self.bus.timers.set_frame_origin(self.cycles);
            self.scheduler
                .schedule(self.cycles + timing.cycles_per_frame(), EventKind::VBlank);
        }
    }

    /// Observation-only PC watch on the kernel entry points (A0h/B0h/C0h).
    /// Mirrors TTY output into the log and debug UI and traces interesting
    /// kernel calls, without altering execution — safe for LLE BIOS bring-up.
    fn observe_tty(&mut self) {
        let pc = self.cpu.pc & 0x1fff_ffff;
        if pc != 0xa0 && pc != 0xb0 {
            return;
        }
        let call = self.cpu.regs[9]; // $t1 selects the kernel function
        if pc == 0xb0 {
            let (a0, a1) = (self.cpu.regs[4], self.cpu.regs[5]);
            match call {
                // OpenEvent(class, spec, mode, func) / EnableEvent(event)
                0x08 => tracing::debug!(target: "psx_core::kernel",
                        "OpenEvent(class={a0:#010x}, spec={a1:#06x})"),
                0x0c => tracing::debug!(target: "psx_core::kernel",
                        "EnableEvent({a0:#010x})"),
                _ => tracing::trace!(target: "psx_core::kernel", "B0({call:#04x}, {a0:#x})"),
            }
        }
        let is_putchar = (pc == 0xa0 && call == 0x3c) || (pc == 0xb0 && call == 0x3d);
        if is_putchar {
            let ch = self.cpu.regs[4] as u8 as char; // $a0
            self.tty.push(ch);
            if ch == '\n'
                && let Some(line) = self.tty.lines().last()
            {
                info!(target: "psx_core::tty", "{line}");
            }
        }
    }
}
