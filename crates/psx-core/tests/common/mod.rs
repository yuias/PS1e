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

/// Physical RAM address of result slot 0. Slots are consecutive words.
const RESULT_BASE: u32 = 0x0000_1000;
/// Physical RAM address of the completion flag.
const DONE_ADDR: u32 = 0x0000_1ffc;
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

#[derive(Default)]
pub struct Program {
    words: Vec<u32>,
}

impl Program {
    pub fn new() -> Self {
        Self::default()
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
        let off = RESULT_BASE + slot * 4;
        assert!(off < DONE_ADDR, "result slot {slot} overruns the done flag");
        self.emit(lui(AT, 0x8000)).emit(sw(rt, off as i16, AT))
    }

    /// Address of the next instruction, for computing branch targets.
    pub fn here(&self) -> u32 {
        psx_core::cpu::RESET_VECTOR + self.words.len() as u32 * 4
    }

    /// Signal completion and spin, then pad out to a full BIOS image.
    fn into_image(mut self) -> Vec<u8> {
        self.emit(lui(AT, 0x8000));
        self.li(V0, DONE_MARKER);
        self.emit(sw(V0, DONE_ADDR as i16, AT));
        // Branch to self; the delay slot runs on every iteration.
        self.emit(beq(ZERO, ZERO, -1)).emit(nop());

        let mut image = vec![0u8; BIOS_SIZE];
        assert!(
            self.words.len() * 4 <= BIOS_SIZE,
            "program does not fit in a BIOS image"
        );
        for (i, w) in self.words.iter().enumerate() {
            image[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        image
    }

    /// Run the program to its completion marker and return the results.
    pub fn run(self) -> Results {
        let mut sys = PsxSystem::new(self.into_image()).expect("test ROM image");
        while sys.cycles() < CYCLE_CAP {
            sys.run_cycles(10_000);
            if read_ram(&sys, DONE_ADDR) == DONE_MARKER {
                return Results { sys };
            }
        }
        panic!(
            "test ROM never signalled completion (pc = {:#010x})",
            sys.cpu.pc
        );
    }
}

fn read_ram(sys: &PsxSystem, addr: u32) -> u32 {
    let a = addr as usize;
    u32::from_le_bytes(sys.bus.ram[a..a + 4].try_into().unwrap())
}

pub struct Results {
    sys: PsxSystem,
}

impl Results {
    /// The word the program stored into result slot `slot`.
    pub fn slot(&self, slot: u32) -> u32 {
        read_ram(&self.sys, RESULT_BASE + slot * 4)
    }
}
