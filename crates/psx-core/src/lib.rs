//! psx-core: platform-independent PlayStation 1 emulator core.
//!
//! No windowing, graphics-API or I/O dependencies live here; frontends
//! (native, wasm) drive [`PsxSystem`] and present its output.

pub mod bus;
pub mod cpu;
pub mod scheduler;

use bus::Bus;
use cpu::Cpu;
use tracing::info;

/// CPU clock: 33.8688 MHz.
pub const CPU_CLOCK_HZ: u64 = 33_868_800;

pub struct PsxSystem {
    pub cpu: Cpu,
    pub bus: Bus,
    cycles: u64,
    tty: String,
}

impl PsxSystem {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        Ok(Self {
            cpu: Cpu::new(),
            bus: Bus::new(bios)?,
            cycles: 0,
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

    /// Execute a single CPU instruction.
    pub fn step(&mut self) {
        self.observe_tty();
        self.cpu.step(&mut self.bus);
        // TODO: refine with memory wait states and I-cache timing; drive the
        // scheduler from here once timers/GPU events exist.
        self.cycles += 1;
    }

    /// Run for roughly `cycles` CPU cycles.
    pub fn run_cycles(&mut self, cycles: u64) {
        let end = self.cycles + cycles;
        while self.cycles < end {
            self.step();
        }
    }

    /// Observation-only PC watch on the kernel `putchar` entry points
    /// (A0h:3Ch, B0h:3Dh). Mirrors TTY output into the log and debug UI
    /// without altering execution — safe for LLE BIOS bring-up.
    fn observe_tty(&mut self) {
        let pc = self.cpu.pc & 0x1fff_ffff;
        if pc != 0xa0 && pc != 0xb0 {
            return;
        }
        let call = self.cpu.regs[9]; // $t1 selects the kernel function
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
