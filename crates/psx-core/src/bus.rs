//! System bus: memory map, RAM/BIOS/scratchpad, MMIO dispatch and DMA
//! transfer execution.
//!
//! Addresses coming from the CPU are virtual (KUSEG/KSEG0/KSEG1); they are
//! masked down to physical addresses before dispatch. Unhandled MMIO accesses
//! are logged and stubbed so BIOS bring-up can make progress while components
//! are still missing.

use crate::cdrom::Cdrom;
use crate::dma::Dma;
use crate::gpu::Gpu;
use crate::mdec::Mdec;
use crate::sio::Sio;
use crate::spu::Spu;
use crate::timers::Timers;
use tracing::{debug, trace, warn};

pub const RAM_SIZE: usize = 2 * 1024 * 1024;

/// `mem_ctrl` slots holding a delay/size register, in [`Bus::access`] order.
const ACCESS_REGS: [usize; 5] = [2, 4, 5, 6, 7];
const EXP1: usize = 0;
const BIOS: usize = 1;
const SPU: usize = 2;
const CDROM: usize = 3;
const EXP2: usize = 4;
/// `mem_ctrl` slot of COM_DELAY (0x1f801020).
const COM_DELAY_IDX: usize = 8;
/// Reset value of a delay/size register: the slowest read and write the
/// hardware offers, over an 8-bit bus. The BIOS speeds them up immediately.
const DELAY_RESET: u32 = 0x0000_00ff;

/// What one access to a region behind the external bus costs, derived from
/// its delay/size register and COM_DELAY.
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct AccessCost {
    /// Cycles for the first access of a transfer.
    first: u64,
    /// Cycles for each further access, when the bus is narrower than the
    /// transfer and the CPU has to repeat it.
    seq: u64,
    /// Bytes moved per bus access: 1 for an 8-bit bus, 2 for a 16-bit one.
    bus_bytes: u32,
}

impl AccessCost {
    /// The psx-spx access-time formula. `delay` is a delay/size register,
    /// `com` is COM_DELAY; only COM0, COM2 and COM3 take part.
    fn derive(delay: u32, com: u32) -> Self {
        let (com0, com2, com3) = (com & 0xf, (com >> 8) & 0xf, (com >> 12) & 0xf);
        // Read delay, 0..0Fh encoding 1..16 cycles.
        let access = ((delay >> 4) & 0xf) + 1;

        let (mut first, mut seq, mut min) = (0, 0, 0);
        if delay & (1 << 8) != 0 {
            // Recovery period
            first += com0.saturating_sub(1);
            seq += com0.saturating_sub(1);
        }
        if delay & (1 << 10) != 0 {
            // Floating period
            first += com2;
            seq += com2;
        }
        if delay & (1 << 11) != 0 {
            // Pre-strobe period sets a floor rather than an offset.
            min = com3;
        }
        if first < 6 {
            first += 1;
        }
        first = (first + access + 2).max(min + 6);
        seq = (seq + access + 2).max(min + 2);

        Self {
            first: first as u64,
            seq: seq as u64,
            bus_bytes: if delay & (1 << 12) != 0 { 2 } else { 1 },
        }
    }

    /// Cycles for a transfer of `width` bytes. A transfer wider than the bus
    /// is split into repeated accesses with !CS held active throughout.
    fn cycles(&self, width: u32) -> u64 {
        let accesses = width.div_ceil(self.bus_bytes) as u64;
        self.first + self.seq * (accesses - 1)
    }
}

pub const BIOS_SIZE: usize = 512 * 1024;
pub const SCRATCHPAD_SIZE: usize = 1024;

/// KSEG2 cache-control register (not part of the 512 MiB physical map).
const CACHE_CONTROL: u32 = 0xfffe_0130;

/// Total words of a block-mode BCR value (0 fields mean the maximum).
fn dma_block_words(bcr: u32) -> u32 {
    let unit = match bcr & 0xffff {
        0 => 0x10000u32,
        n => n,
    };
    let blocks = match (bcr >> 16) & 0xffff {
        0 => 1u32,
        n => n,
    };
    unit * blocks
}

/// Interrupt controller (I_STAT / I_MASK).
#[derive(Default, serde::Serialize, serde::Deserialize)]
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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bus {
    pub ram: Box<[u8]>,
    pub scratchpad: Box<[u8]>,
    /// Not part of save states; the frontend re-injects it on load.
    #[serde(skip)]
    pub bios: Box<[u8]>,
    pub irq: Irq,
    pub gpu: Gpu,
    pub dma: Dma,
    pub timers: Timers,
    pub cdrom: Cdrom,
    pub sio: Sio,
    pub spu: Spu,
    pub mdec: Mdec,
    /// Current CPU cycle, updated by the system before each step; used by
    /// components that catch up lazily (timers).
    pub now: u64,
    /// Wait-state cycles accumulated by accesses since the last step;
    /// drained by the system into the cycle counter.
    pub penalty: u64,
    /// Expansion base / delay registers at 0x1f801000..0x1f801024, plus
    /// RAM_SIZE at 0x1f801060. Stored raw so BIOS read-back matches.
    mem_ctrl: [u32; 9],
    ram_size: u32,
    cache_control: u32,
    /// SIO1 (serial port) registers 0x1f801050..0x1f801060, stored raw
    /// (stub: nothing connected).
    sio1_regs: [u32; 4],
    /// Access costs derived from `mem_ctrl`, kept in step with it by
    /// [`Bus::refresh_access`].
    access: [AccessCost; 5],
}

impl Bus {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        if bios.len() != BIOS_SIZE {
            return Err(format!(
                "BIOS image must be {BIOS_SIZE} bytes, got {}",
                bios.len()
            ));
        }
        let mut mem_ctrl = [0; 9];
        for reg in ACCESS_REGS {
            mem_ctrl[reg] = DELAY_RESET;
        }
        let mut bus = Self {
            ram: vec![0; RAM_SIZE].into_boxed_slice(),
            scratchpad: vec![0; SCRATCHPAD_SIZE].into_boxed_slice(),
            bios: bios.into_boxed_slice(),
            irq: Irq::default(),
            gpu: Gpu::new(),
            dma: Dma::new(),
            timers: Timers::new(),
            cdrom: Cdrom::new(),
            sio: Sio::new(),
            spu: Spu::new(),
            mdec: Mdec::new(),
            now: 0,
            penalty: 0,
            mem_ctrl,
            ram_size: 0,
            cache_control: 0,
            sio1_regs: [0; 4],
            access: [AccessCost::default(); 5],
        };
        bus.refresh_access();
        Ok(bus)
    }

    /// Strip the virtual-memory segment, yielding a physical address.
    /// KSEG2 addresses are passed through (only CACHE_CONTROL lives there).
    pub fn mask_address(addr: u32) -> u32 {
        const REGION_MASK: [u32; 8] = [
            // KUSEG: 2 GiB mapped to the same physical space
            0xffff_ffff,
            0xffff_ffff,
            0xffff_ffff,
            0xffff_ffff,
            // KSEG0: 512 MiB, cached
            0x7fff_ffff,
            // KSEG1: 512 MiB, uncached
            0x1fff_ffff,
            // KSEG2: not translated
            0xffff_ffff,
            0xffff_ffff,
        ];
        addr & REGION_MASK[(addr >> 29) as usize]
    }

    /// Wait states charged on top of the one cycle every load already
    /// costs, so the totals match the figures measured on hardware: 1 cycle
    /// for scratchpad, 5 for on-die I/O, 7 for main RAM. Regions behind the
    /// external bus (BIOS, expansion, SPU, CD-ROM) come from their delay
    /// registers, which the BIOS reprograms early in boot. Stores go through
    /// the write queue and cost only their issue cycle.
    fn read_penalty(&self, p: u32, width: u32) -> u64 {
        let total = match p {
            0x0000_0000..0x0080_0000 => 7, // main RAM, plus DRAM refresh
            0x1f80_0000..0x1f80_0400 => 1, // scratchpad: on-chip SRAM
            0x1f00_0000..0x1f80_0000 => self.access[EXP1].cycles(width),
            0x1f80_1800..0x1f80_1804 => self.access[CDROM].cycles(width),
            0x1f80_1c00..0x1f80_2000 => self.access[SPU].cycles(width),
            0x1f80_2000..0x1fa0_0000 => self.access[EXP2].cycles(width),
            0x1fc0_0000..0x1fc8_0000 => self.access[BIOS].cycles(width),
            _ => 5, // on-die I/O shares one decoder
        };
        total - 1
    }

    /// Recompute the external-bus access costs. Call after any write to the
    /// delay registers, which the BIOS reprograms during boot.
    fn refresh_access(&mut self) {
        let com = self.mem_ctrl[COM_DELAY_IDX];
        for (slot, reg) in ACCESS_REGS.iter().enumerate() {
            self.access[slot] = AccessCost::derive(self.mem_ctrl[*reg], com);
        }
    }

    /// Instruction fetch: cached segments (KUSEG/KSEG0) are treated as
    /// I-cache hits; uncached fetches pay the full bus latency per word.
    pub fn fetch32(&mut self, addr: u32) -> u32 {
        let p = Self::mask_address(addr);
        if addr >= 0xa000_0000 {
            self.penalty += self.read_penalty(p, 4);
        }
        self.read32_at(p)
    }

    /// Side-effect-free byte read for debuggers: plain memories only.
    /// MMIO is refused (`None`) — a debugger draining the CD-ROM FIFO or
    /// acknowledging IRQs by *looking* at them would corrupt emulation.
    pub fn peek8(&self, addr: u32) -> Option<u8> {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => Some(self.ram[(p as usize) & (RAM_SIZE - 1)]),
            0x1f80_0000..0x1f80_0400 => Some(self.scratchpad[(p - 0x1f80_0000) as usize]),
            0x1fc0_0000..0x1fc8_0000 => Some(self.bios[(p - 0x1fc0_0000) as usize]),
            _ => None,
        }
    }

    /// Side-effect-free byte write for debuggers: RAM and scratchpad only.
    /// Returns false for ROM and MMIO.
    pub fn poke8(&mut self, addr: u32, val: u8) -> bool {
        let p = Self::mask_address(addr);
        match p {
            0x0000_0000..0x0080_0000 => self.ram[(p as usize) & (RAM_SIZE - 1)] = val,
            0x1f80_0000..0x1f80_0400 => self.scratchpad[(p - 0x1f80_0000) as usize] = val,
            _ => return false,
        }
        true
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
        self.penalty += self.read_penalty(p, 1);
        match p {
            0x0000_0000..0x0080_0000 => self.ram[(p as usize) & (RAM_SIZE - 1)],
            0x1f80_0000..0x1f80_0400 => self.scratchpad[(p - 0x1f80_0000) as usize],
            0x1fc0_0000..0x1fc8_0000 => self.bios[(p - 0x1fc0_0000) as usize],
            // Expansion 1 (parallel port): open bus reads as 0xff
            0x1f00_0000..0x1f80_0000 => 0xff,
            0x1f80_1800..0x1f80_1804 => self.cdrom.read8(p),
            0x1f80_1040 => self.sio.read_data(),
            0x1f80_1050 => 0xff, // SIO1 data: nothing connected
            _ => {
                let w = self.read32_io(p & !3);
                (w >> ((p & 3) * 8)) as u8
            }
        }
    }

    pub fn read16(&mut self, addr: u32) -> u16 {
        let p = Self::mask_address(addr);
        self.penalty += self.read_penalty(p, 2);
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
        self.penalty += self.read_penalty(p, 4);
        self.read32_at(p)
    }

    fn read32_at(&mut self, p: u32) -> u32 {
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
            0x1f80_1040 => self.sio.read_data() as u32,
            0x1f80_1044 => self.sio.read_stat(),
            0x1f80_1048 => {
                (self.sio.read_reg16(0x8) as u32) | (self.sio.read_reg16(0xa) as u32) << 16
            }
            0x1f80_104c => (self.sio.read_reg16(0xe) as u32) << 16,
            // SIO1 status: TX ready, RX empty
            0x1f80_1054 => 0b101,
            0x1f80_1050..0x1f80_1060 => self.sio1_regs[((p - 0x1f80_1050) / 4) as usize],
            0x1f80_1080..0x1f80_1100 => self.dma.read_reg(p),
            0x1f80_1100..0x1f80_1130 => {
                let Bus {
                    timers,
                    gpu,
                    irq,
                    now,
                    ..
                } = self;
                timers.read(p, *now, gpu.video_timing(), irq)
            }
            0x1f80_1810 => self.gpu.gpuread(),
            0x1f80_1814 => self.gpu.status(),
            0x1f80_1820 => self.mdec.read_data(),
            0x1f80_1824 => self.mdec.status(),
            // SPU registers are 16-bit; a 32-bit read combines two
            0x1f80_1c00..0x1f80_1e00 => {
                (self.spu.read16(p) as u32) | (self.spu.read16(p + 2) as u32) << 16
            }
            0x1f80_1e00..0x1f80_2000 => 0,
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
                self.refresh_access();
            }
            0x1f80_1060 => self.ram_size = val,
            0x1f80_1070 => self.irq.stat &= val, // write-0-to-acknowledge
            0x1f80_1074 => self.irq.mask = val,
            0x1f80_1040 => self.sio.write_data(val as u8, self.now),
            0x1f80_1044..0x1f80_1050 => {
                if width == 4 && p == 0x1f80_1048 {
                    self.sio.write_reg16(0x8, val as u16);
                    self.sio.write_reg16(0xa, (val >> 16) as u16);
                } else {
                    self.sio.write_reg16(p, val as u16);
                }
            }
            0x1f80_1050..0x1f80_1060 => {
                trace!(target: "psx_core::sio", "SIO1 stub write {p:#010x} = {val:#x}");
                let i = ((p - 0x1f80_1050) / 4) as usize;
                self.sio1_regs[i] = val;
            }
            0x1f80_1080..0x1f80_1100 => {
                // Merge sub-word writes (games write DICR halves separately).
                let aligned = p & !3;
                let val32 = if width == 4 {
                    val
                } else {
                    // For DICR, exclude the flag bits from the merge: they
                    // are write-1-to-acknowledge and must not be re-written.
                    let cur = self.dma.read_reg(aligned)
                        & if aligned == 0x1f80_10f4 {
                            0x00ff_ffff
                        } else {
                            !0
                        };
                    let shift = (p & 3) * 8;
                    let mask = (if width == 2 { 0xffffu32 } else { 0xff }) << shift;
                    (cur & !mask) | ((val << shift) & mask)
                };
                if let Some(ch) = self.dma.write_reg(aligned, val32) {
                    self.run_dma(ch);
                }
            }
            0x1f80_1100..0x1f80_1130 => {
                let Bus {
                    timers,
                    gpu,
                    irq,
                    now,
                    ..
                } = self;
                timers.write(p, val, *now, gpu.video_timing(), irq);
            }
            0x1f80_1800..0x1f80_1804 => self.cdrom.write8(p, val as u8, self.now),
            0x1f80_1810 => self.gpu.gp0(val),
            0x1f80_1814 => self.gpu.gp1(val),
            0x1f80_1820 => self.mdec.write_data(val),
            0x1f80_1824 => self.mdec.write_control(val),
            0x1f80_1c00..0x1f80_1e00 => {
                let Bus { spu, irq, .. } = self;
                spu.write16(p, val as u16, irq);
                if width == 4 {
                    spu.write16(p + 2, (val >> 16) as u16, irq);
                }
            }
            0x1f80_1e00..0x1f80_2000 => {
                trace!(target: "psx_core::spu", "write past register area {p:#010x}");
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

    /// Run a whole transfer for `ch` at once, then charge the time it would
    /// have taken. Hardware lets the CPU carry on out of cache, scratchpad,
    /// COP0 and GTE, so a cache-resident loop running alongside a transfer
    /// is charged more here than it costs on the console.
    fn run_dma(&mut self, ch: usize) {
        let words = match ch {
            0 => self.dma_mdec_in(),
            1 => self.dma_mdec_out(),
            2 => self.dma_gpu(),
            3 => self.dma_cdrom(),
            4 => self.dma_spu(),
            6 => self.dma_otc(),
            _ => {
                warn!(target: "psx_core::dma", "DMA{ch} not implemented");
                0
            }
        };
        self.penalty += self.dma_cycles(ch, words);
        // Always mark finished so nothing spins on the busy bit
        self.dma.ch[ch].finish();
        if self.dma.complete(ch) {
            self.irq.raise(3);
        }
    }

    /// How long a transfer takes, from the clocks-per-100h-words figures
    /// measured on hardware. The channels that just stream to or from RAM
    /// land a little above one clock per word: DRAM hyper-page mode still
    /// has to reload the row address every 16 words.
    fn dma_cycles(&self, ch: usize, words: u32) -> u64 {
        let per_256: u64 = match ch {
            // The BIOS leaves the drive at 24 clocks per word; most games
            // enable the recovery period, which brings it to 40.
            3 if self.mem_ctrl[6] & (1 << 8) != 0 => 0x2800,
            3 => 0x1800,
            4 => 0x420,
            5 => 0x1400,
            _ => 0x110,
        };
        (words as u64 * per_256).div_ceil(0x100)
    }

    /// Channel 3: CD-ROM sector data into RAM.
    fn dma_cdrom(&mut self) -> u32 {
        let c = self.dma.ch[3];
        let words = match c.bcr & 0xffff {
            0 => 0x10000u32,
            n => n,
        };
        trace!(target: "psx_core::dma", "CDROM dma {words} words to {:#08x}", c.madr);
        let mut addr = c.madr;
        for _ in 0..words {
            let w = self.cdrom.dma_read_word();
            self.write_ram32(addr, w);
            addr = addr.wrapping_add(4);
        }
        words
    }

    /// Channel 0: compressed data into the MDEC.
    fn dma_mdec_in(&mut self) -> u32 {
        let c = self.dma.ch[0];
        let words = dma_block_words(c.bcr);
        trace!(target: "psx_core::dma", "MDEC-in {words} words");
        let mut addr = c.madr;
        for _ in 0..words {
            let w = self.read_ram32(addr);
            self.mdec.write_data(w);
            addr = addr.wrapping_add(4);
        }
        words
    }

    /// Channel 1: decoded pixels out of the MDEC.
    fn dma_mdec_out(&mut self) -> u32 {
        let c = self.dma.ch[1];
        let words = dma_block_words(c.bcr);
        trace!(target: "psx_core::dma", "MDEC-out {words} words");
        let mut addr = c.madr;
        for _ in 0..words {
            let w = self.mdec.read_data();
            self.write_ram32(addr, w);
            addr = addr.wrapping_add(4);
        }
        words
    }

    /// Channel 4: SPU RAM transfers through the data port.
    fn dma_spu(&mut self) -> u32 {
        let c = self.dma.ch[4];
        let unit = match c.bcr & 0xffff {
            0 => 0x10000u32,
            n => n,
        };
        let blocks = match (c.bcr >> 16) & 0xffff {
            0 => 1u32,
            n => n,
        };
        let words = unit * blocks;
        trace!(target: "psx_core::dma", "SPU dma {words} words {} RAM",
               if c.from_ram() { "from" } else { "to" });
        let mut addr = c.madr;
        for _ in 0..words {
            if c.from_ram() {
                let w = self.read_ram32(addr);
                let Bus { spu, irq, .. } = self;
                spu.dma_write_word(w, irq);
            } else {
                let w = self.spu.dma_read_word();
                self.write_ram32(addr, w);
            }
            addr = addr.wrapping_add(4);
        }
        words
    }

    /// Channel 6: build the GPU ordering table, walking backwards.
    fn dma_otc(&mut self) -> u32 {
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
        count
    }

    /// Channel 2: GPU command lists and image data.
    fn dma_gpu(&mut self) -> u32 {
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
                words
            }
            // Linked list of GP0 packets
            2 => {
                let mut addr = c.madr & 0x001f_fffc;
                let mut guard = 0u32;
                // Every node costs its header word as well as its payload.
                let mut words = 0u32;
                loop {
                    let header = self.read_ram32(addr);
                    let count = header >> 24;
                    words += 1 + count;
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
                words
            }
            _ => {
                warn!(target: "psx_core::dma", "GPU dma sync mode 3 invalid");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values the BIOS programs, against the range measured on hardware
    /// for a `lw` from ROM (27..33 cycles).
    #[test]
    fn bios_read_matches_hardware() {
        let cost = AccessCost::derive(0x0013_243f, 0x0003_1125);
        assert_eq!(cost.bus_bytes, 1, "the BIOS ROM sits on an 8-bit bus");
        assert_eq!(cost.cycles(4), 29);
        assert_eq!(cost.cycles(1), cost.first);
    }

    /// Until the BIOS reprograms them, every region uses the slowest timing.
    #[test]
    fn reset_timing_is_the_slowest_available() {
        let slow = AccessCost::derive(DELAY_RESET, 0);
        let fast = AccessCost::derive(0x0013_243f, 0x0003_1125);
        assert!(slow.cycles(4) > fast.cycles(4));
    }

    /// A 16-bit bus halves the accesses a word costs.
    #[test]
    fn bus_width_sets_the_access_count() {
        let eight = AccessCost::derive(0x0000_0033, 0);
        let sixteen = AccessCost::derive(0x0000_1033, 0);
        assert_eq!(eight.cycles(4), eight.first + eight.seq * 3);
        assert_eq!(sixteen.cycles(4), sixteen.first + sixteen.seq);
        assert_eq!(sixteen.cycles(2), sixteen.first);
    }

    /// Per-channel transfer rates, against the clocks-per-100h-words
    /// figures measured on hardware.
    #[test]
    fn dma_rates_match_hardware() {
        let mut bus = Bus::new(vec![0; BIOS_SIZE]).unwrap();
        for streaming in [0, 1, 2, 6] {
            assert_eq!(
                bus.dma_cycles(streaming, 0x100),
                0x110,
                "channel {streaming}"
            );
        }
        assert_eq!(bus.dma_cycles(4, 0x100), 0x420, "SPU");
        assert_eq!(bus.dma_cycles(5, 0x100), 0x1400, "PIO");

        assert_eq!(
            bus.dma_cycles(3, 0x100),
            0x1800,
            "CD-ROM, as the BIOS leaves it"
        );
        bus.mem_ctrl[6] = 0x0002_0943; // what most games program
        assert_eq!(bus.dma_cycles(3, 0x100), 0x2800, "CD-ROM with recovery");
    }

    /// The regions the CPU reaches without leaving the chip are fixed.
    #[test]
    fn on_die_regions_ignore_the_delay_registers() {
        let bus = Bus::new(vec![0; BIOS_SIZE]).unwrap();
        assert_eq!(bus.read_penalty(0x0000_0000, 4), 6, "main RAM totals 7");
        assert_eq!(bus.read_penalty(0x1f80_0000, 4), 0, "scratchpad totals 1");
        assert_eq!(bus.read_penalty(0x1f80_1070, 4), 4, "on-die I/O totals 5");
    }
}
