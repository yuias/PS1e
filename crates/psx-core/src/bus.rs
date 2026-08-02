//! System bus: memory map, RAM/BIOS/scratchpad, MMIO dispatch and DMA
//! transfer execution.
//!
//! Addresses coming from the CPU are virtual (KUSEG/KSEG0/KSEG1); they are
//! masked down to physical addresses before dispatch. Unhandled MMIO accesses
//! are logged and stubbed so BIOS bring-up can make progress while components
//! are still missing.

use crate::dma::Dma;
use crate::gpu::Gpu;
use crate::timers::Timers;
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
    pub gpu: Gpu,
    pub dma: Dma,
    pub timers: Timers,
    /// Current CPU cycle, updated by the system before each step; used by
    /// components that catch up lazily (timers).
    pub now: u64,
    /// Expansion base / delay registers at 0x1f801000..0x1f801024, plus
    /// RAM_SIZE at 0x1f801060. Stored raw so BIOS read-back matches.
    mem_ctrl: [u32; 9],
    ram_size: u32,
    cache_control: u32,
    /// SIO0/SIO1 registers 0x1f801040..0x1f801060, stored raw (stub: no
    /// controller is connected, JOY_DATA reads 0xff and never ACKs).
    sio_regs: [u32; 8],
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
            gpu: Gpu::new(),
            dma: Dma::new(),
            timers: Timers::new(),
            now: 0,
            mem_ctrl: [0; 9],
            ram_size: 0,
            cache_control: 0,
            sio_regs: [0; 8],
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

    fn read_ram32(&self, addr: u32) -> u32 {
        let i = (addr as usize) & (RAM_SIZE - 1) & !3;
        u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
    }

    fn write_ram32(&mut self, addr: u32, val: u32) {
        let i = (addr as usize) & (RAM_SIZE - 1) & !3;
        self.ram[i..i + 4].copy_from_slice(&val.to_le_bytes());
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
            // CD-ROM controller stub: byte registers. Status reports the
            // parameter FIFO empty/writable so command writes proceed.
            0x1f80_1800..0x1f80_1804 => {
                trace!(target: "psx_core::cdrom", "stub read {p:#010x}");
                if p & 3 == 0 { 0x18 } else { 0 }
            }
            0x1f80_1040 | 0x1f80_1050 => 0xff, // JOY/SIO data: nothing connected
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
            // JOY/SIO status: TX ready, RX empty, no /ACK
            0x1f80_1044 | 0x1f80_1054 => 0b101,
            0x1f80_1040..0x1f80_1060 => self.sio_regs[((p - 0x1f80_1040) / 4) as usize],
            0x1f80_1080..0x1f80_1100 => self.dma.read_reg(p),
            0x1f80_1100..0x1f80_1130 => {
                let Bus { timers, irq, now, .. } = self;
                timers.read(p, *now, irq)
            }
            0x1f80_1810 => self.gpu.gpuread(),
            0x1f80_1814 => self.gpu.status(),
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

    /// MMIO / KSEG2 stores. `width` is in bytes (1/2/4); sub-word writes
    /// only matter for the raw-stored SIO block and are merged there.
    fn write_io(&mut self, p: u32, val: u32, width: u32) {
        match p {
            0x1f80_1000..0x1f80_1024 => {
                debug!(target: "psx_core::bus", "mem-ctrl write {p:#010x} = {val:#010x}");
                self.mem_ctrl[((p - 0x1f80_1000) / 4) as usize] = val;
            }
            0x1f80_1060 => self.ram_size = val,
            0x1f80_1070 => self.irq.stat &= val, // write-0-to-acknowledge
            0x1f80_1074 => self.irq.mask = val,
            0x1f80_1040..0x1f80_1060 => {
                trace!(target: "psx_core::sio", "stub write {p:#010x} = {val:#x}");
                let i = ((p - 0x1f80_1040) / 4) as usize;
                let shift = (p & 3) * 8;
                let mask = match width {
                    1 => 0xffu32 << shift,
                    2 => 0xffffu32 << shift,
                    _ => 0xffff_ffff,
                };
                self.sio_regs[i] = (self.sio_regs[i] & !mask) | ((val << shift) & mask);
            }
            0x1f80_1080..0x1f80_1100 => {
                if let Some(ch) = self.dma.write_reg(p, val) {
                    self.run_dma(ch);
                }
            }
            0x1f80_1100..0x1f80_1130 => {
                let Bus { timers, irq, now, .. } = self;
                timers.write(p, val, *now, irq);
            }
            0x1f80_1800..0x1f80_1804 => {
                debug!(target: "psx_core::cdrom", "stub write {p:#010x} = {val:#04x}");
            }
            0x1f80_1810 => self.gpu.gp0(val),
            0x1f80_1814 => self.gpu.gp1(val),
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

    // --- DMA execution -------------------------------------------------

    /// Run a whole transfer for `ch` immediately (no bus timing yet).
    fn run_dma(&mut self, ch: usize) {
        match ch {
            2 => self.dma_gpu(),
            6 => self.dma_otc(),
            _ => warn!(target: "psx_core::dma", "DMA{ch} not implemented"),
        }
        // Always mark finished so nothing spins on the busy bit
        self.dma.ch[ch].finish();
        if self.dma.complete(ch) {
            self.irq.raise(3);
        }
    }

    /// Channel 6: build the GPU ordering table, walking backwards.
    fn dma_otc(&mut self) {
        let c = self.dma.ch[6];
        let count = match c.bcr & 0xffff {
            0 => 0x10000,
            n => n,
        };
        let mut addr = c.madr;
        trace!(target: "psx_core::dma", "OTC {count} entries ending {addr:#08x}");
        for i in 0..count {
            let val = if i == count - 1 {
                0x00ff_ffff // end marker
            } else {
                addr.wrapping_sub(4) & 0x001f_fffc
            };
            self.write_ram32(addr, val);
            addr = addr.wrapping_sub(4);
        }
    }

    /// Channel 2: GPU command lists and image data.
    fn dma_gpu(&mut self) {
        let c = self.dma.ch[2];
        match c.sync_mode() {
            // Manual / request mode: linear block of words
            0 | 1 => {
                let unit = match c.bcr & 0xffff {
                    0 => 0x10000u32,
                    n => n,
                };
                let words = if c.sync_mode() == 1 {
                    let blocks = match (c.bcr >> 16) & 0xffff {
                        0 => 0x10000u32,
                        n => n,
                    };
                    unit * blocks
                } else {
                    unit
                };
                let back = c.chcr & 2 != 0;
                let mut addr = c.madr;
                trace!(target: "psx_core::dma",
                       "GPU block dma {words} words {} RAM", if c.from_ram() {"from"} else {"to"});
                for _ in 0..words {
                    if c.from_ram() {
                        let w = self.read_ram32(addr);
                        self.gpu.gp0(w);
                    } else {
                        let w = self.gpu.gpuread();
                        self.write_ram32(addr, w);
                    }
                    addr = if back {
                        addr.wrapping_sub(4)
                    } else {
                        addr.wrapping_add(4)
                    };
                }
            }
            // Linked list of GP0 packets
            2 => {
                let mut addr = c.madr & 0x001f_fffc;
                let mut guard = 0u32;
                loop {
                    let header = self.read_ram32(addr);
                    let count = header >> 24;
                    let mut a = addr;
                    for _ in 0..count {
                        a = a.wrapping_add(4) & 0x001f_fffc;
                        let w = self.read_ram32(a);
                        self.gpu.gp0(w);
                    }
                    if header & 0x0080_0000 != 0 {
                        break; // end marker
                    }
                    addr = header & 0x001f_fffc;
                    guard += 1;
                    if guard > 0x40000 {
                        warn!(target: "psx_core::dma", "GPU linked list runaway, aborting");
                        break;
                    }
                }
            }
            _ => warn!(target: "psx_core::dma", "GPU dma sync mode 3 invalid"),
        }
    }
}
