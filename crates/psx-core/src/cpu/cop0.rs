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
    /// Stored for read-back only; hardware breakpoints are not implemented.
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
            7 => self.dcic = val,
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

        if self.sr & (1 << 22) != 0 {
            0xbfc0_0180 // BEV=1: ROM handler
        } else {
            0x8000_0080
        }
    }

    /// RFE: pop the (KU, IE) mode stack.
    pub fn return_from_exception(&mut self) {
        self.sr = (self.sr & !0xf) | ((self.sr >> 2) & 0xf);
    }
}
