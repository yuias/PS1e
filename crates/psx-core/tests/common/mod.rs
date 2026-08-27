//! Generated test ROMs: a minimal MIPS encoder plus a runner.
//!
//! Test programs are assembled here instead of being checked in as binaries,
//! so the suite runs on a clean checkout where `assets/` — BIOS images and
//! third-party test ROMs alike — is absent. A program is placed at the reset
//! vector as a 512 KiB image and executed as if it were the BIOS.
//!
//! Programs report by storing words into RAM rather than through the TTY:
//! TTY capture watches the kernel `putchar` entry points, and a generated
//! ROM has no kernel behind them.

#![allow(dead_code)]

use psx_core::PsxSystem;

const BIOS_SIZE: usize = 512 * 1024;

/// Physical RAM address of result slot 0 in a BIOS-image program. Slots are
/// consecutive words; the flag sits one word below the region's end.
const RESULT_BASE: u32 = 0x0000_1000;
/// Result base for programs loaded as an executable, which must stay clear
/// of the kernel the BIOS has already put at the bottom of RAM.
pub const EXE_RESULT_BASE: u32 = 0x0010_0000;
/// Words of results before the completion flag.
const SLOTS: u32 = 0x3ff;
/// Word a program stores at `DONE_ADDR` once it has finished.
const DONE_MARKER: u32 = 0xc0de_d09e;

/// Cycle ceiling for a generated program. Deliberately far above what any
/// of them need: assertions must survive timing changes, so completion is
/// detected by the marker, never by a cycle count.
const CYCLE_CAP: u64 = 5_000_000;

// Register numbers used by the test programs.
pub const ZERO: u32 = 0;
pub const AT: u32 = 1;
pub const V0: u32 = 2;
pub const T0: u32 = 8;
pub const T1: u32 = 9;
pub const T2: u32 = 10;
pub const T3: u32 = 11;
pub const T4: u32 = 12;
pub const T5: u32 = 13;
pub const T6: u32 = 14;
pub const T7: u32 = 15;

// --- Instruction encoders ----------------------------------------------

fn i_type(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
    op << 26 | rs << 21 | rt << 16 | imm as u32
}

fn r_type(rs: u32, rt: u32, rd: u32, shamt: u32, funct: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | shamt << 6 | funct
}

pub fn lui(rt: u32, imm: u16) -> u32 {
    i_type(0x0f, 0, rt, imm)
}
pub fn ori(rt: u32, rs: u32, imm: u16) -> u32 {
    i_type(0x0d, rs, rt, imm)
}
pub fn andi(rt: u32, rs: u32, imm: u16) -> u32 {
    i_type(0x0c, rs, rt, imm)
}
pub fn addiu(rt: u32, rs: u32, imm: i16) -> u32 {
    i_type(0x09, rs, rt, imm as u16)
}
pub fn lw(rt: u32, off: i16, base: u32) -> u32 {
    i_type(0x23, base, rt, off as u16)
}
pub fn sw(rt: u32, off: i16, base: u32) -> u32 {
    i_type(0x2b, base, rt, off as u16)
}
/// Branch on equal; `off` counts instructions from the delay slot.
pub fn beq(rs: u32, rt: u32, off: i16) -> u32 {
    i_type(0x04, rs, rt, off as u16)
}
/// Branch on not equal; `off` counts instructions from the delay slot.
pub fn bne(rs: u32, rt: u32, off: i16) -> u32 {
    i_type(0x05, rs, rt, off as u16)
}
pub fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x21)
}
pub fn subu(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x23)
}
pub fn and(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x24)
}
pub fn or(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x25)
}
pub fn xor(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x26)
}
pub fn nor(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x27)
}
pub fn slt(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x2a)
}
pub fn sltu(rd: u32, rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, rd, 0, 0x2b)
}
pub fn sll(rd: u32, rt: u32, shamt: u32) -> u32 {
    r_type(0, rt, rd, shamt, 0x00)
}
pub fn srl(rd: u32, rt: u32, shamt: u32) -> u32 {
    r_type(0, rt, rd, shamt, 0x02)
}
pub fn sra(rd: u32, rt: u32, shamt: u32) -> u32 {
    r_type(0, rt, rd, shamt, 0x03)
}
pub fn mult(rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, 0, 0, 0x18)
}
pub fn multu(rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, 0, 0, 0x19)
}
pub fn div(rs: u32, rt: u32) -> u32 {
    r_type(rs, rt, 0, 0, 0x1a)
}
pub fn mfhi(rd: u32) -> u32 {
    r_type(0, 0, rd, 0, 0x10)
}
pub fn mflo(rd: u32) -> u32 {
    r_type(0, 0, rd, 0, 0x12)
}
pub fn nop() -> u32 {
    0
}

// --- Program builder ---------------------------------------------------

pub struct Program {
    words: Vec<u32>,
    /// Physical address of result slot 0.
    result_base: u32,
}

impl Program {
    /// A program to be run as a BIOS image, from the reset vector.
    pub fn new() -> Self {
        Self::with_results(RESULT_BASE)
    }

    /// A program whose results land at `result_base`, for when the default
    /// region is already occupied.
    pub fn with_results(result_base: u32) -> Self {
        Self {
            words: Vec::new(),
            result_base,
        }
    }

    fn done_addr(&self) -> u32 {
        self.result_base + SLOTS * 4
    }

    pub fn emit(&mut self, word: u32) -> &mut Self {
        self.words.push(word);
        self
    }

    pub fn emit_all(&mut self, words: impl IntoIterator<Item = u32>) -> &mut Self {
        self.words.extend(words);
        self
    }

    /// Load a 32-bit constant, collapsing to one instruction when it fits.
    pub fn li(&mut self, rt: u32, value: u32) -> &mut Self {
        if value >> 16 == 0 {
            self.emit(ori(rt, ZERO, value as u16))
        } else {
            self.emit(lui(rt, (value >> 16) as u16))
                .emit(ori(rt, rt, value as u16))
        }
    }

    /// Store `rt` into result slot `slot`. Clobbers `AT`.
    pub fn store_result(&mut self, slot: u32, rt: u32) -> &mut Self {
        assert!(slot < SLOTS, "result slot {slot} overruns the done flag");
        self.store_word(self.result_base + slot * 4, rt)
    }

    /// Store `rt` at a physical address, through KSEG0. Clobbers `AT`.
    fn store_word(&mut self, phys: u32, rt: u32) -> &mut Self {
        let addr = 0x8000_0000 | phys;
        // `sw` sign-extends its offset, so a high half-word borrows a 1.
        let (lo, hi) = (addr as u16, (addr >> 16) as u16);
        let hi = if lo & 0x8000 != 0 {
            hi.wrapping_add(1)
        } else {
            hi
        };
        self.emit(lui(AT, hi)).emit(sw(rt, lo as i16, AT))
    }

    /// Address of the next instruction, for computing branch targets.
    pub fn here(&self) -> u32 {
        psx_core::cpu::RESET_VECTOR + self.words.len() as u32 * 4
    }

    /// Append the completion marker and a spin, and flatten to bytes.
    fn finish(mut self) -> Vec<u8> {
        self.li(V0, DONE_MARKER);
        self.store_word(self.done_addr(), V0);
        // Branch to self; the delay slot runs on every iteration.
        self.emit(beq(ZERO, ZERO, -1)).emit(nop());
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Pad the program out to a full BIOS image, to run from the reset vector.
    fn into_image(self) -> Vec<u8> {
        let code = self.finish();
        assert!(
            code.len() <= BIOS_SIZE,
            "program does not fit in a BIOS image"
        );
        let mut image = vec![0u8; BIOS_SIZE];
        image[..code.len()].copy_from_slice(&code);
        image
    }

    /// Wrap the program in a PS-X EXE, to be side-loaded over a booted BIOS.
    pub fn into_exe(self, load_addr: u32, sp: u32) -> Vec<u8> {
        const HEADER_SIZE: usize = 0x800;
        let code = self.finish();
        // The body is stored in whole 2 KiB blocks, as the shell expects.
        let size = code.len().next_multiple_of(HEADER_SIZE);

        let mut exe = vec![0u8; HEADER_SIZE + size];
        exe[..8].copy_from_slice(b"PS-X EXE");
        let mut put = |off: usize, v: u32| exe[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(0x10, load_addr); // entry point
        put(0x18, load_addr);
        put(0x1c, size as u32);
        put(0x30, sp);
        exe[HEADER_SIZE..HEADER_SIZE + code.len()].copy_from_slice(&code);
        exe
    }

    /// Run the program as a BIOS image, to its completion marker.
    pub fn run(self) -> Results {
        let result_base = self.result_base;
        let sys = PsxSystem::new(self.into_image()).expect("test ROM image");
        run_to_marker(sys, result_base)
    }
}

/// Advance `sys` until the program stores its completion marker.
pub fn run_to_marker(mut sys: PsxSystem, result_base: u32) -> Results {
    let done = result_base + SLOTS * 4;
    let deadline = sys.cycles() + CYCLE_CAP;
    while sys.cycles() < deadline {
        sys.run_cycles(10_000);
        if read_ram(&sys, done) == DONE_MARKER {
            return Results { sys, result_base };
        }
    }
    panic!(
        "test program never signalled completion (pc = {:#010x})",
        sys.cpu.pc
    );
}

fn read_ram(sys: &PsxSystem, addr: u32) -> u32 {
    let a = addr as usize;
    u32::from_le_bytes(sys.bus.ram[a..a + 4].try_into().unwrap())
}

pub struct Results {
    sys: PsxSystem,
    result_base: u32,
}

impl Results {
    /// The word the program stored into result slot `slot`.
    pub fn slot(&self, slot: u32) -> u32 {
        read_ram(&self.sys, self.result_base + slot * 4)
    }
}
