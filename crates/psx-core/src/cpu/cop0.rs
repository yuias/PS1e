//! COP0: System Control Coprocessor (exception handling, cache control).

use tracing::warn;

/// Exception codes as encoded in CAUSE bits 2..6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    Interrupt = 0x0,
    /// Address error on load or instruction fetch
    AdEL = 0x4,
    /// Address error on store
    AdES = 0x5,
    Syscall = 0x8,
    Break = 0x9,
    /// Reserved (illegal) instruction
    ReservedInstruction = 0xa,
    CoprocessorUnusable = 0xb,
    Overflow = 0xc,
}

/// DCIC bits that hold a value; the rest read back as zero.
const DCIC_WRITABLE: u32 = 0xff80_f03f;

/// DCIC status bits, set when a break condition matches.
mod status {
    /// Any break
    pub const DB: u32 = 1 << 0;
    /// Program counter break
    pub const PC: u32 = 1 << 1;
    /// Data address break
    pub const DA: u32 = 1 << 2;
    /// Data read reference
    pub const R: u32 = 1 << 3;
    /// Data write reference
    pub const W: u32 = 1 << 4;
}

/// DCIC enable bits.
mod enable {
    /// Master debug enable; gates every bit below.
    pub const DE: u32 = 1 << 23;
    pub const PCE: u32 = 1 << 24;
    pub const DAE: u32 = 1 << 25;
    pub const DR: u32 = 1 << 26;
    pub const DW: u32 = 1 << 27;
    /// Break in kernel mode
    pub const KD: u32 = 1 << 29;
    /// Break in user mode
    pub const UD: u32 = 1 << 30;
    /// Jump to the debug vector rather than only setting the status bits
    pub const TR: u32 = 1 << 31;
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Cop0 {
    /// r12: Status register
    pub sr: u32,
    /// r13: CAUSE
    pub cause: u32,
    /// r14: Exception PC
    pub epc: u32,
    /// r8: bad virtual address (address-error exceptions)
    pub bad_vaddr: u32,
    /// Breakpoint registers (r3 BPC, r5 BDA, r7 DCIC, r9 BDAM, r11 BPCM).
    bpc: u32,
    bda: u32,
    dcic: u32,
    bdam: u32,
    bpcm: u32,
}

impl Cop0 {
    /// True while SR.IsC (bit 16) isolates the data cache from the bus.
    /// The BIOS sets this during cache flush; stores must then be swallowed.
    pub fn cache_isolated(&self) -> bool {
        self.sr & (1 << 16) != 0
    }

    /// True when interrupts are enabled and an enabled line is pending.
    pub fn interrupt_pending(&self) -> bool {
        let pending = self.cause & self.sr & 0x0000_ff00;
        self.sr & 1 != 0 && pending != 0
    }

    /// DCIC master enable. Checked before the breakpoint comparisons so the
    /// common case costs one test on the instruction hot path.
    pub fn debug_enabled(&self) -> bool {
        self.dcic & enable::DE != 0
    }

    /// Breakpoints only fire in the privilege level DCIC selects. SR.KUc
    /// (bit 1) is the current mode: set = user.
    fn armed(&self) -> bool {
        let level = if self.sr & (1 << 1) != 0 {
            enable::UD
        } else {
            enable::KD
        };
        self.debug_enabled() && self.dcic & level != 0
    }

    /// Test `pc` against the program-counter breakpoint. Records the hit in
    /// DCIC and reports whether it should trap to the debug vector — with
    /// DCIC.TR clear, a match only sets the status bits.
    pub fn code_break(&mut self, pc: u32) -> bool {
        if !self.armed() || self.dcic & enable::PCE == 0 {
            return false;
        }
        if (pc ^ self.bpc) & self.bpcm != 0 {
            return false;
        }
        self.dcic |= status::DB | status::PC;
        self.dcic & enable::TR != 0
    }

    /// Test a data access against the data-address breakpoint, as
    /// [`Cop0::code_break`] does for instruction fetches.
    pub fn data_break(&mut self, addr: u32, is_write: bool) -> bool {
        if !self.armed() || self.dcic & enable::DAE == 0 {
            return false;
        }
        let (side, reference) = if is_write {
            (enable::DW, status::W)
        } else {
            (enable::DR, status::R)
        };
        if self.dcic & side == 0 {
            return false;
        }
        if (addr ^ self.bda) & self.bdam != 0 {
            return false;
        }
        self.dcic |= status::DB | status::DA | reference;
        self.dcic & enable::TR != 0
    }

    pub fn read(&self, reg: u32) -> u32 {
        match reg {
            3 => self.bpc,
            5 => self.bda,
            6 => 0, // JUMPDEST, not implemented
            7 => self.dcic,
            8 => self.bad_vaddr,
            9 => self.bdam,
            11 => self.bpcm,
            12 => self.sr,
            13 => self.cause,
            14 => self.epc,
            15 => 0x0000_0002, // PRID: CXD8530 revision as reported on retail units
            _ => {
                warn!(target: "psx_core::cpu", "MFC0 from unhandled cop0 r{reg}");
                0
            }
        }
    }

    pub fn write(&mut self, reg: u32, val: u32) {
        match reg {
            3 => self.bpc = val,
            5 => self.bda = val,
            6 => {}
            7 => self.dcic = val & DCIC_WRITABLE,
            9 => self.bdam = val,
            11 => self.bpcm = val,
            12 => self.sr = val,
            // Only the two software-interrupt bits are writable
            13 => self.cause = (self.cause & !0x300) | (val & 0x300),
            _ => {
                if val != 0 {
                    warn!(target: "psx_core::cpu",
                          "MTC0 to unhandled cop0 r{reg} = {val:#010x}");
                }
            }
        }
    }

    /// Enter an exception: push the interrupt/kernel mode stack and record
    /// the cause. Returns the handler address.
    pub fn enter_exception(&mut self, cause: Exception, epc: u32, in_delay_slot: bool) -> u32 {
        self.push_exception(cause, epc, in_delay_slot);
        if self.sr & (1 << 22) != 0 {
            0xbfc0_0180 // BEV=1: ROM handler
        } else {
            0x8000_0080
        }
    }

    /// Enter a COP0 breakpoint exception. It reports the same cause as the
    /// BREAK opcode but has its own vector, so a debug handler can tell the
    /// two apart without decoding the faulting instruction.
    pub fn enter_debug_break(&mut self, epc: u32, in_delay_slot: bool) -> u32 {
        self.push_exception(Exception::Break, epc, in_delay_slot);
        if self.sr & (1 << 22) != 0 {
            0xbfc0_0140
        } else {
            0x8000_0040
        }
    }

    fn push_exception(&mut self, cause: Exception, epc: u32, in_delay_slot: bool) {
        // Push the 3-entry (KU, IE) stack: bits 5:0 <<= 2
        let mode = self.sr & 0x3f;
        self.sr = (self.sr & !0x3f) | ((mode << 2) & 0x3f);

        self.cause = (self.cause & !0x7c) | ((cause as u32) << 2);
        if in_delay_slot {
            // EPC points at the branch; BD tells the handler to compensate
            self.epc = epc.wrapping_sub(4);
            self.cause |= 1 << 31;
        } else {
            self.epc = epc;
            self.cause &= !(1 << 31);
        }
    }

    /// RFE: pop the (KU, IE) mode stack.
    pub fn return_from_exception(&mut self) {
        self.sr = (self.sr & !0xf) | ((self.sr >> 2) & 0xf);
    }
}
