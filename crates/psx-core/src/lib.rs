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
pub mod tty;

use bus::Bus;
use cpu::Cpu;
use scheduler::{EventKind, Scheduler};
use tracing::info;
use tty::Tty;

/// CPU clock: 33.8688 MHz.
pub const CPU_CLOCK_HZ: u64 = 33_868_800;
/// NTSC field, in CPU cycles. A nominal unit for callers that need a
/// stable "one frame" figure; the running machine follows the display
/// mode's [`gpu::VideoTiming`], which is longer in PAL.
pub const CYCLES_PER_FRAME: u64 = gpu::VideoTiming::NTSC.cycles_per_frame();
/// Where the BIOS shell hands control to the executable it has loaded.
/// Reaching it means the kernel jump tables are up, so it is the earliest
/// point at which [`PsxSystem::load_exe`] has something to hand over to.
pub const SHELL_ENTRY: u32 = 0x8003_0000;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PsxSystem {
    pub cpu: Cpu,
    pub bus: Bus,
    scheduler: Scheduler,
    cycles: u64,
    next_sample: u64,
    #[serde(skip)]
    tty: Tty,
}

/// Everything the frontend owns and the machine merely holds: the BIOS
/// image, the disc in the drive, the memory card, the GPU command-log
/// switch and the TTY capture. None of it is machine state, so none of it
/// is serialized, and all of it must survive every rebuild of the machine
/// (state load, reset). [`PsxSystem::take_ambient`] and
/// [`PsxSystem::set_ambient`] are the single carry path; anything added
/// here is carried everywhere by construction.
#[derive(Default)]
pub struct Ambient {
    pub bios: Box<[u8]>,
    pub disc: Option<cdrom::Disc>,
    pub memcard: memcard::MemCard,
    pub log_gpu: bool,
    pub tty: Tty,
}

/// Save-state file magic + format version. Bump the version on any change
/// to a serialized struct.
const STATE_MAGIC: &[u8; 4] = b"PS1E";
const STATE_VERSION: u16 = 10;

/// Cheap content fingerprint (FNV-1a) to flag cross-BIOS state loads.
fn bios_fingerprint(bios: &[u8]) -> u32 {
    bios.iter().fold(0x811c_9dc5u32, |h, b| {
        (h ^ *b as u32).wrapping_mul(0x0100_0193)
    })
}

impl PsxSystem {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        Ok(Self::with_bus(Bus::new(bios)?))
    }

    fn with_bus(bus: Bus) -> Self {
        let mut scheduler = Scheduler::new();
        scheduler.schedule(CYCLES_PER_FRAME, EventKind::VBlank);
        Self {
            cpu: Cpu::new(),
            bus,
            scheduler,
            cycles: 0,
            next_sample: spu::CYCLES_PER_SAMPLE,
            tty: Tty::default(),
        }
    }

    /// Detach the frontend-owned assets, leaving defaults behind. Pair with
    /// [`PsxSystem::set_ambient`] around anything that replaces the machine.
    pub fn take_ambient(&mut self) -> Ambient {
        Ambient {
            bios: std::mem::take(&mut self.bus.bios),
            disc: self.bus.cdrom.take_disc(),
            memcard: std::mem::take(&mut self.bus.sio.memcard),
            log_gpu: self.bus.gpu.log_commands,
            tty: std::mem::take(&mut self.tty),
        }
    }

    /// Install the frontend-owned assets. Any disc already in the drive is
    /// replaced outright, not swapped through the lid (see
    /// [`PsxSystem::open_shell`]).
    pub fn set_ambient(&mut self, ambient: Ambient) {
        self.bus.bios = ambient.bios;
        self.bus.cdrom.set_disc(ambient.disc);
        self.bus.sio.memcard = ambient.memcard;
        self.bus.gpu.log_commands = ambient.log_gpu;
        self.tty = ambient.tty;
    }

    /// Power-cycle the machine, keeping the ambient assets.
    pub fn reset(&mut self) {
        let ambient = self.take_ambient();
        *self = Self::with_bus(Bus::build(Box::default()));
        self.set_ambient(ambient);
    }

    /// Switch GP0/GP1 command decoding to the log on or off.
    pub fn set_gpu_log(&mut self, on: bool) {
        self.bus.gpu.log_commands = on;
    }

    /// Total elapsed CPU cycles since reset.
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Serialize the complete machine state. The [`Ambient`] assets are
    /// *not* included: the frontend owns them, and rolling back the memory
    /// card would corrupt real saves.
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(8 * 1024 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&bios_fingerprint(&self.bus.bios).to_le_bytes());
        postcard::to_extend(self, out).map_err(|e| format!("serialize failed: {e}"))
    }

    /// Restore a state produced by [`PsxSystem::save_state`], carrying over
    /// the current [`Ambient`] assets.
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
        // Only after the decode succeeded: a bad file must not eat the disc
        new.set_ambient(self.take_ambient());
        *self = new;
        Ok(())
    }

    /// TTY output retained so far (kernel `putchar`); see [`Tty`] for the
    /// retention rule.
    pub fn tty_output(&self) -> &str {
        self.tty.text()
    }

    /// TTY output written after monotonic position `pos`, plus the next
    /// position. Survives state loads, resets and buffer trimming, unlike
    /// an index into [`PsxSystem::tty_output`].
    pub fn tty_since(&self, pos: u64) -> (&str, u64) {
        self.tty.since(pos)
    }

    /// Side-load a PS-X EXE image over the running machine.
    ///
    /// This is the shortcut the BIOS shell would otherwise take after
    /// reading the executable off a disc, so the caller must let the BIOS
    /// reach the shell entry point ([`SHELL_ENTRY`]) first — the kernel must
    /// have set up its jump tables, or the loaded program has nothing to
    /// call. [`PsxSystem::run_until_pc`] is the wait to put in front of it.
    /// Used to run test executables that never ship as disc images.
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

    /// Step until the pc reaches `target`, giving up after `max_cycles`.
    /// Returns whether it arrived.
    ///
    /// The pc only passes *through* a given address for one instruction, so
    /// a caller that advances in chunks cannot catch it with an equality
    /// test at a chunk boundary; this checks every instruction instead.
    /// Pair it with [`SHELL_ENTRY`] to park a fresh machine where
    /// [`PsxSystem::load_exe`] can take over. Note that a machine already
    /// past the shell entry will never reach it again — the BIOS executes
    /// it once — so this is for booting, not for re-synchronising.
    pub fn run_until_pc(&mut self, target: u32, max_cycles: u64) -> bool {
        let end = self.cycles + max_cycles;
        while self.cycles < end {
            if self.cpu.pc == target {
                return true;
            }
            self.step();
        }
        self.cpu.pc == target
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
            self.bus.gpu.vblank(self.cycles);
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
            if let Some(line) = self.tty.push(ch) {
                info!(target: "psx_core::tty", "{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system() -> PsxSystem {
        let mut sys = PsxSystem::new(vec![0; bus::BIOS_SIZE]).unwrap();
        sys.set_gpu_log(true);
        sys.insert_disc(cdrom::Disc::new(vec![0; 2352]).unwrap());
        sys.bus.sio.memcard.data[0] = 0x4d;
        for c in "boot\n".chars() {
            sys.tty.push(c);
        }
        sys
    }

    fn assert_ambient_intact(sys: &PsxSystem) {
        assert!(sys.bus.gpu.log_commands);
        assert!(sys.bus.cdrom.has_disc());
        assert_eq!(sys.bus.sio.memcard.data[0], 0x4d);
        assert_eq!(sys.bus.bios.len(), bus::BIOS_SIZE);
        assert_eq!(sys.tty_output(), "boot\n");
    }

    #[test]
    fn ambient_assets_survive_a_state_load() {
        let mut sys = system();
        sys.run_cycles(1000);
        let saved_at = sys.cycles();
        let state = sys.save_state().unwrap();
        sys.run_cycles(1000);
        sys.load_state(&state).unwrap();
        assert_ambient_intact(&sys);
        assert_eq!(sys.cycles(), saved_at);
    }

    #[test]
    fn a_bad_state_file_keeps_the_ambient_assets() {
        let mut sys = system();
        let mut state = sys.save_state().unwrap();
        state.truncate(state.len() / 2);
        assert!(sys.load_state(&state).is_err());
        assert_ambient_intact(&sys);
    }

    #[test]
    fn reset_keeps_the_ambient_assets_and_restarts_the_machine() {
        let mut sys = system();
        sys.run_cycles(1000);
        sys.reset();
        assert_ambient_intact(&sys);
        assert_eq!(sys.cycles(), 0);
        assert_eq!(sys.cpu.pc, 0xbfc0_0000);
    }

    #[test]
    fn tty_cursor_survives_load_and_reset() {
        let mut sys = system();
        let (first, pos) = sys.tty_since(0);
        assert_eq!(first, "boot\n");
        let state = sys.save_state().unwrap();
        sys.load_state(&state).unwrap();
        sys.reset();
        for c in "later\n".chars() {
            sys.tty.push(c);
        }
        assert_eq!(sys.tty_since(pos).0, "later\n");
    }
}
