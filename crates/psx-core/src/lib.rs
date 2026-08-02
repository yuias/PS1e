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
/// NTSC field: 263 scanlines.
pub const CYCLES_PER_FRAME: u64 = timers::CYCLES_PER_LINE * 263;

pub struct PsxSystem {
    pub cpu: Cpu,
    pub bus: Bus,
    scheduler: Scheduler,
    cycles: u64,
    next_sample: u64,
    tty: String,
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

    /// TTY output captured so far (kernel `putchar`).
    pub fn tty_output(&self) -> &str {
        &self.tty
    }

    pub fn insert_disc(&mut self, disc: cdrom::Disc) {
        self.bus.cdrom.insert_disc(disc);
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
        // TODO: refine with memory wait states and I-cache timing
        self.cycles += 1;

        let bus::Bus { cdrom, sio, spu, irq, .. } = &mut self.bus;
        cdrom.tick(self.cycles, irq);
        sio.tick(self.cycles, irq);
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
        match event {
            EventKind::VBlank => {
                self.bus.irq.raise(0);
                self.bus.gpu.vblank();
                // Keep lazily-synced components from lagging more than a frame
                self.bus.timers.sync_all(self.cycles, &mut self.bus.irq);
                self.scheduler
                    .schedule(self.cycles + CYCLES_PER_FRAME, EventKind::VBlank);
            }
            _ => {}
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
            if ch == '\n' {
                if let Some(line) = self.tty.lines().last() {
                    info!(target: "psx_core::tty", "{line}");
                }
            }
        }
    }
}
