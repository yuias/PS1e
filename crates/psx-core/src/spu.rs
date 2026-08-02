//! SPU register skeleton.
//!
//! No audio is produced yet. This models just enough for sound drivers to
//! make progress: a raw register file (writes read back), SPU RAM with
//! manual/DMA transfers, SPUSTAT mirroring SPUCNT, and IRQ9 generation.
//!
//! IRQ9 sources modeled:
//! - a transfer write landing on the IRQ address (exact), and
//! - a periodic "heartbeat" while the IRQ is enabled and voices are keyed
//!   on — approximating a looping voice crossing the IRQ address, which
//!   sound drivers (e.g. Namco's) use as their tick. Real voice address
//!   progression replaces this once the SPU mixes audio.

use crate::bus::Irq;
use tracing::{debug, trace};

pub const SPU_RAM_SIZE: usize = 512 * 1024;

/// ~480 Hz heartbeat: fast enough for driver ticks, slow enough to be cheap.
const HEARTBEAT_CYCLES: u64 = 70_000;

const REG_BASE: u32 = 0x1f80_1c00;

pub struct Spu {
    /// Raw 16-bit register file for 0x1f801c00..0x1f801e00.
    regs: [u16; 0x100],
    pub ram: Box<[u8]>,
    /// Current transfer address in bytes (register value is in 8-byte units).
    xfer_addr: u32,
    irq_flag: bool,
    /// Bitmask of voices currently keyed on (24 voices).
    voices_on: u32,
    next_heartbeat: u64,
}

impl Spu {
    pub fn new() -> Self {
        Self {
            regs: [0; 0x100],
            ram: vec![0; SPU_RAM_SIZE].into_boxed_slice(),
            xfer_addr: 0,
            irq_flag: false,
            voices_on: 0,
            next_heartbeat: 0,
        }
    }

    fn spucnt(&self) -> u16 {
        self.regs[(0x1aa) / 2]
    }

    fn irq_enabled(&self) -> bool {
        let cnt = self.spucnt();
        cnt & (1 << 15) != 0 && cnt & (1 << 6) != 0
    }

    fn irq_addr(&self) -> u32 {
        self.regs[(0x1a4) / 2] as u32 * 8
    }

    fn raise_irq(&mut self, irq: &mut Irq) {
        if !self.irq_flag {
            trace!(target: "psx_core::spu", "SPU IRQ at {:#x}", self.irq_addr());
        }
        self.irq_flag = true;
        irq.raise(9);
    }

    /// Periodic heartbeat while voices play (see module docs).
    pub fn tick(&mut self, now: u64, irq: &mut Irq) {
        if now < self.next_heartbeat {
            return;
        }
        self.next_heartbeat = now + HEARTBEAT_CYCLES;
        if self.irq_enabled() && self.voices_on != 0 {
            self.raise_irq(irq);
        }
    }

    pub fn read16(&mut self, p: u32) -> u16 {
        let ofs = (p - REG_BASE) as usize;
        match ofs {
            // SPUSTAT: low 6 bits mirror SPUCNT; bit 6 is the IRQ flag.
            // Transfer-busy bits stay 0 (transfers complete instantly).
            0x1ae => (self.spucnt() & 0x3f) | (self.irq_flag as u16) << 6,
            // Voice ADSR current volume: report silence so drivers never
            // wait on a fading envelope
            _ if ofs < 0x180 && ofs & 0xf == 0xc => 0,
            _ => self.regs[ofs / 2],
        }
    }

    pub fn write16(&mut self, p: u32, val: u16, irq: &mut Irq) {
        let ofs = (p - REG_BASE) as usize;
        match ofs {
            0x1a6 => {
                self.xfer_addr = val as u32 * 8;
                self.regs[ofs / 2] = val;
            }
            0x1a8 => {
                // Data port: manual transfer into SPU RAM
                let a = (self.xfer_addr as usize) & (SPU_RAM_SIZE - 1);
                self.ram[a..a + 2].copy_from_slice(&val.to_le_bytes());
                if self.irq_enabled() {
                    let ia = self.irq_addr();
                    if self.xfer_addr <= ia + 1 && ia < self.xfer_addr + 2 {
                        self.raise_irq(irq);
                    }
                }
                self.xfer_addr = (self.xfer_addr + 2) & (SPU_RAM_SIZE as u32 - 1);
            }
            0x1aa => {
                // SPUCNT: clearing the IRQ-enable bit acknowledges the IRQ
                if val & (1 << 6) == 0 {
                    self.irq_flag = false;
                }
                self.regs[ofs / 2] = val;
            }
            0x188 => {
                self.voices_on |= val as u32;
                self.regs[ofs / 2] = val;
            }
            0x18a => {
                self.voices_on |= (val as u32) << 16;
                self.regs[ofs / 2] = val;
            }
            0x18c => {
                self.voices_on &= !(val as u32);
                self.regs[ofs / 2] = val;
            }
            0x18e => {
                self.voices_on &= !((val as u32) << 16);
                self.regs[ofs / 2] = val;
            }
            _ => self.regs[ofs / 2] = val,
        }
        if ofs == 0x1aa {
            debug!(target: "psx_core::spu", "SPUCNT = {val:#06x}");
        }
    }

    /// DMA channel 4: words to SPU RAM through the transfer address.
    pub fn dma_write_word(&mut self, w: u32, irq: &mut Irq) {
        self.write16(REG_BASE + 0x1a8, w as u16, irq);
        self.write16(REG_BASE + 0x1a8, (w >> 16) as u16, irq);
    }

    /// DMA channel 4 reads (SPU RAM -> CPU), rarely used.
    pub fn dma_read_word(&mut self) -> u32 {
        let a = (self.xfer_addr as usize) & (SPU_RAM_SIZE - 1);
        let w = u32::from_le_bytes(self.ram[a & !3..(a & !3) + 4].try_into().unwrap());
        self.xfer_addr = (self.xfer_addr + 4) & (SPU_RAM_SIZE as u32 - 1);
        w
    }
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_hitting_irq_address_raises_irq9() {
        let mut spu = Spu::new();
        let mut irq = Irq::default();
        spu.write16(REG_BASE + 0x1aa, 0x8040, &mut irq); // enable + IRQ enable
        spu.write16(REG_BASE + 0x1a4, 0x0002, &mut irq); // IRQ addr = 0x10
        spu.write16(REG_BASE + 0x1a6, 0x0000, &mut irq); // transfer addr = 0
        for i in 0..16 {
            spu.write16(REG_BASE + 0x1a8, i, &mut irq);
        }
        assert!(irq.stat & (1 << 9) != 0);
        // SPUSTAT reflects the IRQ flag until SPUCNT bit 6 is cleared
        assert!(spu.read16(REG_BASE + 0x1ae) & (1 << 6) != 0);
        spu.write16(REG_BASE + 0x1aa, 0x8000, &mut irq);
        assert!(spu.read16(REG_BASE + 0x1ae) & (1 << 6) == 0);
    }
}
