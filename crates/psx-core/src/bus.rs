//! System bus: memory map, RAM/BIOS/scratchpad and MMIO dispatch.
//!
//! Addresses coming from the CPU are virtual (KUSEG/KSEG0/KSEG1); they are
//! masked down to physical addresses before dispatch. Unhandled MMIO accesses
//! are logged and stubbed so BIOS bring-up can make progress while components
//! are still missing.

use tracing::{debug, trace, warn};

pub const RAM_SIZE: usize = 2 * 1024 * 1024;
pub const BIOS_SIZE: usize = 512 * 1024;
pub const SCRATCHPAD_SIZE: usize = 1024;

/// KSEG2 cache-control register (not part of the 512 MiB physical map).
const CACHE_CONTROL: u32 = 0xfffe_0130;

/// Interrupt controller (I_STAT / I_MASK).
#[derive(Default)]
pub struct Irq {
    pub stat: u32,
    pub mask: u32,
}

impl Irq {
    /// True when any enabled interrupt is pending (drives CAUSE bit 10).
    pub fn pending(&self) -> bool {
        self.stat & self.mask & 0x7ff != 0
    }

    pub fn raise(&mut self, line: u32) {
        self.stat |= 1 << line;
    }
}

pub struct Bus {
    pub ram: Box<[u8]>,
    pub scratchpad: Box<[u8]>,
    pub bios: Box<[u8]>,
    pub irq: Irq,
    /// Expansion base / delay registers at 0x1f801000..0x1f801024, plus
    /// RAM_SIZE at 0x1f801060. Stored raw so BIOS read-back matches.
    mem_ctrl: [u32; 9],
    ram_size: u32,
    cache_control: u32,
    /// DMA registers 0x1f801080..0x1f801100, stored raw until the DMA
    /// component lands (the BIOS only initializes DPCR/DICR at boot).
    dma_regs: [u32; 32],
    /// Timer registers 0x1f801100..0x1f801130, stored raw (stub).
    timer_regs: [u32; 12],
}

impl Bus {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        if bios.len() != BIOS_SIZE {
            return Err(format!(
                "BIOS image must be {BIOS_SIZE} bytes, got {}",
                bios.len()
            ));
        }
        Ok(Self {
            ram: vec![0; RAM_SIZE].into_boxed_slice(),
            scratchpad: vec![0; SCRATCHPAD_SIZE].into_boxed_slice(),
            bios: bios.into_boxed_slice(),
            irq: Irq::default(),
            mem_ctrl: [0; 9],
            ram_size: 0,
            cache_control: 0,
            dma_regs: [0; 32],
            timer_regs: [0; 12],
        })
    }

    /// Strip the virtual-memory segment, yielding a physical address.
    /// KSEG2 addresses are passed through (only CACHE_CONTROL lives there).
    pub fn mask_address(addr: u32) -> u32 {
        const REGION_MASK: [u32; 8] = [
            // KUSEG: 2 GiB mapped to the same physical space
            0xffff_ffff, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff,
            // KSEG0: 512 MiB, cached
            0x7fff_ffff,
            // KSEG1: 512 MiB, uncached
            0x1fff_ffff,
            // KSEG2: not translated
            0xffff_ffff, 0xffff_ffff,
        ];
        addr & REGION_MASK[(addr >> 29) as usize]
    }

    // --- Loads ---------------------------------------------------------

    pub fn read8(&mut self, addr: u32) -> u8 {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => self.ram[(p as usize) & (RAM_SIZE - 1)],
            0x1f80_0000..0x1f80_0400 => self.scratchpad[(p - 0x1f80_0000) as usize],
            0x1fc0_0000..0x1fc8_0000 => self.bios[(p - 0x1fc0_0000) as usize],
            // Expansion 1 (parallel port): open bus reads as 0xff
            0x1f00_0000..0x1f80_0000 => 0xff,
            _ => {
                let w = self.read32_io(p & !3);
                (w >> ((p & 3) * 8)) as u8
            }
        }
    }

    pub fn read16(&mut self, addr: u32) -> u16 {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => {
                let i = (p as usize) & (RAM_SIZE - 1);
                u16::from_le_bytes([self.ram[i], self.ram[i + 1]])
            }
            0x1f80_0000..0x1f80_0400 => {
                let i = (p - 0x1f80_0000) as usize;
                u16::from_le_bytes([self.scratchpad[i], self.scratchpad[i + 1]])
            }
            0x1fc0_0000..0x1fc8_0000 => {
                let i = (p - 0x1fc0_0000) as usize;
                u16::from_le_bytes([self.bios[i], self.bios[i + 1]])
            }
            0x1f00_0000..0x1f80_0000 => 0xffff,
            _ => {
                let w = self.read32_io(p & !3);
                (w >> ((p & 2) * 8)) as u16
            }
        }
    }

    pub fn read32(&mut self, addr: u32) -> u32 {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => {
                let i = (p as usize) & (RAM_SIZE - 1);
                u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
            }
            0x1f80_0000..0x1f80_0400 => {
                let i = (p - 0x1f80_0000) as usize;
                u32::from_le_bytes(self.scratchpad[i..i + 4].try_into().unwrap())
            }
            0x1fc0_0000..0x1fc8_0000 => {
                let i = (p - 0x1fc0_0000) as usize;
                u32::from_le_bytes(self.bios[i..i + 4].try_into().unwrap())
            }
            0x1f00_0000..0x1f80_0000 => 0xffff_ffff,
            _ => self.read32_io(p),
        }
    }

    /// MMIO / KSEG2 loads (32-bit granularity).
    fn read32_io(&mut self, p: u32) -> u32 {
        match p {
            0x1f80_1000..0x1f80_1024 => self.mem_ctrl[((p - 0x1f80_1000) / 4) as usize],
            0x1f80_1060 => self.ram_size,
            0x1f80_1070 => self.irq.stat,
            0x1f80_1074 => self.irq.mask,
            0x1f80_1080..0x1f80_1100 => self.dma_regs[((p - 0x1f80_1080) / 4) as usize],
            0x1f80_1100..0x1f80_1130 => {
                trace!(target: "psx_core::timers", "stub read {p:#010x}");
                self.timer_regs[((p - 0x1f80_1100) / 4) as usize]
            }
            // GPUREAD: no GPU yet
            0x1f80_1810 => 0,
            // GPUSTAT: report "ready to receive cmd/DMA, ready to send VRAM"
            // so the BIOS does not spin waiting for the GPU.
            0x1f80_1814 => 0x1c00_0000,
            // SPU registers: read back as 0 until the SPU exists
            0x1f80_1c00..0x1f80_2000 => {
                trace!(target: "psx_core::spu", "stub read {p:#010x}");
                0
            }
            // Expansion 2 (DUART/POST)
            0x1f80_2000..0x1f80_2080 => 0,
            CACHE_CONTROL => self.cache_control,
            _ => {
                warn!(target: "psx_core::bus", "unhandled read {p:#010x}");
                0
            }
        }
    }

    // --- Stores --------------------------------------------------------

    pub fn write8(&mut self, addr: u32, val: u8) {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => self.ram[(p as usize) & (RAM_SIZE - 1)] = val,
            0x1f80_0000..0x1f80_0400 => self.scratchpad[(p - 0x1f80_0000) as usize] = val,
            _ => self.write_io(p, val as u32, 1),
        }
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => {
                let i = (p as usize) & (RAM_SIZE - 1);
                self.ram[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x1f80_0000..0x1f80_0400 => {
                let i = (p - 0x1f80_0000) as usize;
                self.scratchpad[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            _ => self.write_io(p, val as u32, 2),
        }
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => {
                let i = (p as usize) & (RAM_SIZE - 1);
                self.ram[i..i + 4].copy_from_slice(&val.to_le_bytes());
            }
            0x1f80_0000..0x1f80_0400 => {
                let i = (p - 0x1f80_0000) as usize;
                self.scratchpad[i..i + 4].copy_from_slice(&val.to_le_bytes());
            }
            _ => self.write_io(p, val, 4),
        }
    }

    /// MMIO / KSEG2 stores. `width` only matters for logging; the stubbed
    /// registers below are all word-sized in practice during BIOS boot.
    fn write_io(&mut self, p: u32, val: u32, width: u32) {
        match p {
            0x1f80_1000..0x1f80_1024 => {
                debug!(target: "psx_core::bus", "mem-ctrl write {p:#010x} = {val:#010x}");
                self.mem_ctrl[((p - 0x1f80_1000) / 4) as usize] = val;
            }
            0x1f80_1060 => self.ram_size = val,
            0x1f80_1070 => self.irq.stat &= val, // write-0-to-acknowledge
            0x1f80_1074 => self.irq.mask = val,
            0x1f80_1080..0x1f80_1100 => {
                debug!(target: "psx_core::dma", "stub write {p:#010x} = {val:#010x}");
                self.dma_regs[((p - 0x1f80_1080) / 4) as usize] = val;
            }
            0x1f80_1100..0x1f80_1130 => {
                trace!(target: "psx_core::timers", "stub write {p:#010x} = {val:#010x}");
                self.timer_regs[((p - 0x1f80_1100) / 4) as usize] = val;
            }
            0x1f80_1810 | 0x1f80_1814 => {
                debug!(target: "psx_core::gpu", "stub GP{} cmd {val:#010x}",
                       if p == 0x1f80_1810 { 0 } else { 1 });
            }
            0x1f80_1c00..0x1f80_2000 => {
                trace!(target: "psx_core::spu", "stub write {p:#010x} = {val:#06x}");
            }
            0x1f80_2000..0x1f80_2080 => {
                // Expansion 2: 0x1f802041 is the 7-segment POST display
                if p == 0x1f80_2041 {
                    debug!(target: "psx_core::bus", "POST = {val:#x}");
                }
            }
            CACHE_CONTROL => self.cache_control = val,
            _ => {
                warn!(target: "psx_core::bus",
                      "unhandled write{} {p:#010x} = {val:#010x}", width * 8);
            }
        }
    }
}
