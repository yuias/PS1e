//! DMA controller (7 channels).
//!
//! A transfer is carried out in full the moment CHCR starts it, and its
//! measured duration is then charged to the CPU; interleaving the two, and
//! the chopping windows that make it visible, are a later milestone. Channels are
//! implemented in `bus`: MDEC in/out (0/1), GPU (2), CD-ROM (3), SPU (4) and
//! OTC (6). PIO (5) has no device behind it.

use tracing::{debug, warn};

pub const DPCR_RESET: u32 = 0x0765_4321;

#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Channel {
    pub madr: u32,
    pub bcr: u32,
    pub chcr: u32,
}

impl Channel {
    pub fn sync_mode(&self) -> u32 {
        (self.chcr >> 9) & 3
    }

    /// From-RAM when set, to-RAM when clear.
    pub fn from_ram(&self) -> bool {
        self.chcr & 1 != 0
    }

    pub fn active(&self) -> bool {
        let start = self.chcr & (1 << 24) != 0;
        let trigger = self.chcr & (1 << 28) != 0;
        // Manual sync mode additionally needs the trigger bit
        start && (self.sync_mode() != 0 || trigger)
    }

    pub fn finish(&mut self) {
        self.chcr &= !((1 << 24) | (1 << 28));
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Dma {
    pub ch: [Channel; 7],
    pub dpcr: u32,
    dicr: u32,
}

impl Dma {
    pub fn new() -> Self {
        Self {
            ch: [Channel::default(); 7],
            dpcr: DPCR_RESET,
            dicr: 0,
        }
    }

    pub fn read_reg(&self, p: u32) -> u32 {
        let ofs = p - 0x1f80_1080;
        let ch = (ofs >> 4) as usize;
        match (ch, ofs & 0xf) {
            (0..7, 0x0) => self.ch[ch].madr,
            (0..7, 0x4) => self.ch[ch].bcr,
            (0..7, 0x8) => self.ch[ch].chcr,
            (7, 0x0) => self.dpcr,
            (7, 0x4) => self.dicr | (self.irq_asserted() as u32) << 31,
            _ => {
                warn!(target: "psx_core::dma", "unhandled DMA read {p:#010x}");
                0
            }
        }
    }

    /// Returns the channel number if the write made a channel active.
    pub fn write_reg(&mut self, p: u32, val: u32) -> Option<usize> {
        let ofs = p - 0x1f80_1080;
        let ch = (ofs >> 4) as usize;
        match (ch, ofs & 0xf) {
            (0..7, 0x0) => self.ch[ch].madr = val & 0x00ff_ffff,
            (0..7, 0x4) => self.ch[ch].bcr = val,
            (0..7, 0x8) => {
                self.ch[ch].chcr = val;
                if self.ch[ch].active() && self.dpcr & (8 << (ch * 4)) != 0 {
                    return Some(ch);
                }
            }
            (7, 0x0) => self.dpcr = val,
            (7, 0x4) => {
                // Bits 24..31 are write-1-to-acknowledge; 31 is read-only
                let ack = val & 0x7f00_0000;
                self.dicr = (val & 0x00ff_803f) | (self.dicr & 0x7f00_0000 & !ack);
            }
            _ => warn!(target: "psx_core::dma", "unhandled DMA write {p:#010x} = {val:#010x}"),
        }
        None
    }

    fn irq_asserted(&self) -> bool {
        let force = self.dicr & (1 << 15) != 0;
        let master = self.dicr & (1 << 23) != 0;
        let flags = (self.dicr >> 24) & 0x7f;
        let enables = (self.dicr >> 16) & 0x7f;
        force || (master && flags & enables != 0)
    }

    /// Latch completion of `ch`. Returns true when this assertion should
    /// raise IRQ3 (rising edge of the master flag).
    pub fn complete(&mut self, ch: usize) -> bool {
        debug!(target: "psx_core::dma", "DMA{ch} complete");
        let before = self.irq_asserted();
        if self.dicr & (1 << (16 + ch)) != 0 {
            self.dicr |= 1 << (24 + ch);
        }
        !before && self.irq_asserted()
    }
}

impl Default for Dma {
    fn default() -> Self {
        Self::new()
    }
}
