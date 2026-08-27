//! R3000A CPU interpreter.
//!
//! Models the architectural quirks games and BIOSes rely on:
//! - branch delay slots (one instruction after every branch/jump executes)
//! - load delay slots (a loaded value is not visible to the next instruction,
//!   and a direct write to the same register cancels the in-flight load)
//! - COP0 exception handling, including BD/EPC fix-up for delay slots
//! - data-cache isolation (stores swallowed while SR.IsC is set)
//!
//! Timing is one pipeline cycle per instruction plus the read wait states
//! the bus accumulates (`Bus::penalty`, drained by the system). Cache-line
//! behaviour and multiplier/divider latency are not modeled.

mod cop0;
mod gte;

pub use cop0::{Cop0, Exception};
pub use gte::Gte;

use crate::bus::Bus;
use tracing::{trace, warn};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub regs: [u32; 32],
    pub hi: u32,
    pub lo: u32,
    /// Address of the next instruction to execute.
    pub pc: u32,
    /// Address after `pc`; branches redirect this, giving delay-slot semantics.
    pub next_pc: u32,
    /// Address of the instruction currently executing (for exceptions).
    current_pc: u32,
    /// In-flight load: (register, value), applied after the next instruction.
    pending_load: Option<(usize, u32)>,
    /// Register directly written by the current instruction (cancels loads).
    written_reg: Option<usize>,
    /// The current instruction sits in a branch delay slot.
    in_delay_slot: bool,
    /// The current instruction is a branch/jump (the next one is a delay slot).
    is_branch: bool,
    pub cop0: Cop0,
    pub gte: Gte,
    /// Cycle the GTE finishes its current command at. The CPU runs on
    /// alongside it and only waits when an instruction touches the GTE.
    gte_done_at: u64,
}

pub const RESET_VECTOR: u32 = 0xbfc0_0000;

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: [0; 32],
            hi: 0,
            lo: 0,
            pc: RESET_VECTOR,
            next_pc: RESET_VECTOR.wrapping_add(4),
            current_pc: RESET_VECTOR,
            pending_load: None,
            written_reg: None,
            in_delay_slot: false,
            is_branch: false,
            cop0: Cop0::default(),
            gte: Gte::new(),
            gte_done_at: 0,
        }
    }

    /// Redirect execution to `pc`, dropping any in-flight branch or load.
    /// A loader that has just placed a program in RAM enters it this way.
    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.current_pc = pc;
        self.pending_load = None;
        self.written_reg = None;
        self.in_delay_slot = false;
        self.is_branch = false;
    }

    /// Execute one instruction.
    pub fn step(&mut self, bus: &mut Bus) {
        // Mirror the interrupt controller into CAUSE bit 10 (hardware line).
        if bus.irq.pending() {
            self.cop0.cause |= 1 << 10;
        } else {
            self.cop0.cause &= !(1 << 10);
        }

        self.current_pc = self.pc;
        self.in_delay_slot = self.is_branch;
        self.is_branch = false;

        // Interrupts are sampled before instruction execution.
        if self.cop0.interrupt_pending() {
            self.exception(Exception::Interrupt);
            // The pending branch (if any) re-executes after RFE via EPC.
            return;
        }

        if self.current_pc & 3 != 0 {
            self.cop0.bad_vaddr = self.current_pc;
            self.exception(Exception::AdEL);
            return;
        }

        if self.cop0.debug_enabled() && self.cop0.code_break(self.current_pc) {
            self.debug_break();
            return;
        }

        let instr = bus.fetch32(self.pc);
        self.pc = self.next_pc;
        self.next_pc = self.next_pc.wrapping_add(4);

        // Take the in-flight load; it becomes visible after this instruction
        // unless the instruction overwrites the same register.
        let inflight = self.pending_load.take();
        self.written_reg = None;

        self.execute(bus, instr, inflight);

        if let Some((r, v)) = inflight
            && self.written_reg != Some(r)
        {
            self.regs[r] = v;
            self.regs[0] = 0;
        }
    }

    fn set_reg(&mut self, r: usize, v: u32) {
        self.regs[r] = v;
        self.written_reg = Some(r);
        self.regs[0] = 0;
    }

    /// Schedule a delayed load into `r`.
    fn set_load(&mut self, r: usize, v: u32) {
        if r != 0 {
            self.pending_load = Some((r, v));
        }
    }

    fn exception(&mut self, cause: Exception) {
        let handler = self
            .cop0
            .enter_exception(cause, self.current_pc, self.in_delay_slot);
        trace!(target: "psx_core::cpu",
               "exception {cause:?} at {:#010x} -> {handler:#010x}", self.current_pc);
        self.pc = handler;
        self.next_pc = handler.wrapping_add(4);
    }

    /// Take a COP0 breakpoint exception in place of the current instruction.
    fn debug_break(&mut self) {
        let handler = self
            .cop0
            .enter_debug_break(self.current_pc, self.in_delay_slot);
        trace!(target: "psx_core::cpu",
               "cop0 debug break at {:#010x} -> {handler:#010x}", self.current_pc);
        self.pc = handler;
        self.next_pc = handler.wrapping_add(4);
    }

    fn branch_to(&mut self, target: u32) {
        self.next_pc = target;
    }

    fn execute(&mut self, bus: &mut Bus, instr: u32, inflight: Option<(usize, u32)>) {
        let op = instr >> 26;
        let rs = ((instr >> 21) & 0x1f) as usize;
        let rt = ((instr >> 16) & 0x1f) as usize;
        let rd = ((instr >> 11) & 0x1f) as usize;
        let shamt = (instr >> 6) & 0x1f;
        let imm = instr & 0xffff;
        let imm_se = imm as i16 as i32 as u32; // sign-extended immediate
        let target = instr & 0x03ff_ffff;

        // COP0 data breakpoints watch the effective address of every load
        // and store; all of them address memory as rs+imm.
        if self.cop0.debug_enabled()
            && let Some(is_write) = data_access(op)
            && self
                .cop0
                .data_break(self.regs[rs].wrapping_add(imm_se), is_write)
        {
            self.debug_break();
            return;
        }

        match op {
            0x00 => match instr & 0x3f {
                0x00 => self.set_reg(rd, self.regs[rt] << shamt), // SLL (incl. NOP)
                0x02 => self.set_reg(rd, self.regs[rt] >> shamt), // SRL
                0x03 => self.set_reg(rd, ((self.regs[rt] as i32) >> shamt) as u32), // SRA
                0x04 => self.set_reg(rd, self.regs[rt] << (self.regs[rs] & 0x1f)), // SLLV
                0x06 => self.set_reg(rd, self.regs[rt] >> (self.regs[rs] & 0x1f)), // SRLV
                0x07 => {
                    // SRAV
                    self.set_reg(
                        rd,
                        ((self.regs[rt] as i32) >> (self.regs[rs] & 0x1f)) as u32,
                    )
                }
                0x08 => {
                    // JR
                    self.is_branch = true;
                    self.branch_to(self.regs[rs]);
                }
                0x09 => {
                    // JALR
                    self.is_branch = true;
                    let ra = self.next_pc;
                    self.branch_to(self.regs[rs]);
                    self.set_reg(rd, ra);
                }
                0x0c => self.exception(Exception::Syscall),
                0x0d => self.exception(Exception::Break),
                0x10 => self.set_reg(rd, self.hi), // MFHI
                0x11 => self.hi = self.regs[rs],   // MTHI
                0x12 => self.set_reg(rd, self.lo), // MFLO
                0x13 => self.lo = self.regs[rs],   // MTLO
                0x18 => {
                    // MULT
                    let v = (self.regs[rs] as i32 as i64) * (self.regs[rt] as i32 as i64);
                    self.hi = (v >> 32) as u32;
                    self.lo = v as u32;
                }
                0x19 => {
                    // MULTU
                    let v = (self.regs[rs] as u64) * (self.regs[rt] as u64);
                    self.hi = (v >> 32) as u32;
                    self.lo = v as u32;
                }
                0x1a => {
                    // DIV: division by zero and i32::MIN / -1 have defined results
                    let n = self.regs[rs] as i32;
                    let d = self.regs[rt] as i32;
                    if d == 0 {
                        self.hi = n as u32;
                        self.lo = if n >= 0 { 0xffff_ffff } else { 1 };
                    } else if n == i32::MIN && d == -1 {
                        self.hi = 0;
                        self.lo = 0x8000_0000;
                    } else {
                        self.hi = (n % d) as u32;
                        self.lo = (n / d) as u32;
                    }
                }
                0x1b => {
                    // DIVU
                    let n = self.regs[rs];
                    let d = self.regs[rt];
                    if d == 0 {
                        self.hi = n;
                        self.lo = 0xffff_ffff;
                    } else {
                        self.hi = n % d;
                        self.lo = n / d;
                    }
                }
                0x20 => {
                    // ADD (traps on overflow)
                    match (self.regs[rs] as i32).checked_add(self.regs[rt] as i32) {
                        Some(v) => self.set_reg(rd, v as u32),
                        None => self.exception(Exception::Overflow),
                    }
                }
                0x21 => self.set_reg(rd, self.regs[rs].wrapping_add(self.regs[rt])), // ADDU
                0x22 => {
                    // SUB (traps on overflow)
                    match (self.regs[rs] as i32).checked_sub(self.regs[rt] as i32) {
                        Some(v) => self.set_reg(rd, v as u32),
                        None => self.exception(Exception::Overflow),
                    }
                }
                0x23 => self.set_reg(rd, self.regs[rs].wrapping_sub(self.regs[rt])), // SUBU
                0x24 => self.set_reg(rd, self.regs[rs] & self.regs[rt]),             // AND
                0x25 => self.set_reg(rd, self.regs[rs] | self.regs[rt]),             // OR
                0x26 => self.set_reg(rd, self.regs[rs] ^ self.regs[rt]),             // XOR
                0x27 => self.set_reg(rd, !(self.regs[rs] | self.regs[rt])),          // NOR
                0x2a => {
                    // SLT
                    self.set_reg(rd, ((self.regs[rs] as i32) < (self.regs[rt] as i32)) as u32)
                }
                0x2b => self.set_reg(rd, (self.regs[rs] < self.regs[rt]) as u32), // SLTU
                _ => self.illegal(instr),
            },
            0x01 => {
                // BcondZ: BLTZ/BGEZ/BLTZAL/BGEZAL. Hardware decodes any rt:
                // bit 0 selects >=, rt & 0x1e == 0x10 links (unconditionally).
                self.is_branch = true;
                let ge = rt & 1 != 0;
                let cond = ((self.regs[rs] as i32) < 0) != ge;
                if rt & 0x1e == 0x10 {
                    self.set_reg(31, self.next_pc);
                }
                if cond {
                    self.branch_to(self.pc.wrapping_add(imm_se << 2));
                }
            }
            0x02 => {
                // J
                self.is_branch = true;
                self.branch_to((self.pc & 0xf000_0000) | (target << 2));
            }
            0x03 => {
                // JAL
                self.is_branch = true;
                self.set_reg(31, self.next_pc);
                self.branch_to((self.pc & 0xf000_0000) | (target << 2));
            }
            0x04 => {
                // BEQ
                self.is_branch = true;
                if self.regs[rs] == self.regs[rt] {
                    self.branch_to(self.pc.wrapping_add(imm_se << 2));
                }
            }
            0x05 => {
                // BNE
                self.is_branch = true;
                if self.regs[rs] != self.regs[rt] {
                    self.branch_to(self.pc.wrapping_add(imm_se << 2));
                }
            }
            0x06 => {
                // BLEZ
                self.is_branch = true;
                if (self.regs[rs] as i32) <= 0 {
                    self.branch_to(self.pc.wrapping_add(imm_se << 2));
                }
            }
            0x07 => {
                // BGTZ
                self.is_branch = true;
                if (self.regs[rs] as i32) > 0 {
                    self.branch_to(self.pc.wrapping_add(imm_se << 2));
                }
            }
            0x08 => {
                // ADDI (traps on overflow)
                match (self.regs[rs] as i32).checked_add(imm_se as i32) {
                    Some(v) => self.set_reg(rt, v as u32),
                    None => self.exception(Exception::Overflow),
                }
            }
            0x09 => self.set_reg(rt, self.regs[rs].wrapping_add(imm_se)), // ADDIU
            0x0a => self.set_reg(rt, ((self.regs[rs] as i32) < (imm_se as i32)) as u32), // SLTI
            0x0b => self.set_reg(rt, (self.regs[rs] < imm_se) as u32),    // SLTIU
            0x0c => self.set_reg(rt, self.regs[rs] & imm),                // ANDI
            0x0d => self.set_reg(rt, self.regs[rs] | imm),                // ORI
            0x0e => self.set_reg(rt, self.regs[rs] ^ imm),                // XORI
            0x0f => self.set_reg(rt, imm << 16),                          // LUI

            0x10 => self.op_cop0(instr, rs, rt, rd),
            0x12 => self.op_cop2(bus, instr, rs, rt, rd),
            0x11 | 0x13 => self.exception(Exception::CoprocessorUnusable),

            0x20 => {
                // LB
                let v = bus.read8(self.regs[rs].wrapping_add(imm_se)) as i8 as i32 as u32;
                self.set_load(rt, v);
            }
            0x21 => {
                // LH
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdEL);
                } else {
                    self.set_load(rt, bus.read16(addr) as i16 as i32 as u32);
                }
            }
            0x22 => {
                // LWL: merge high bytes; chains with an in-flight load to rt
                let addr = self.regs[rs].wrapping_add(imm_se);
                let cur = self.load_chain_value(rt, inflight);
                let word = bus.read32(addr & !3);
                let v = match addr & 3 {
                    0 => (cur & 0x00ff_ffff) | (word << 24),
                    1 => (cur & 0x0000_ffff) | (word << 16),
                    2 => (cur & 0x0000_00ff) | (word << 8),
                    _ => word,
                };
                self.set_load(rt, v);
            }
            0x23 => {
                // LW
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdEL);
                } else {
                    self.set_load(rt, bus.read32(addr));
                }
            }
            0x24 => {
                // LBU
                let v = bus.read8(self.regs[rs].wrapping_add(imm_se)) as u32;
                self.set_load(rt, v);
            }
            0x25 => {
                // LHU
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdEL);
                } else {
                    self.set_load(rt, bus.read16(addr) as u32);
                }
            }
            0x26 => {
                // LWR: merge low bytes; chains with an in-flight load to rt
                let addr = self.regs[rs].wrapping_add(imm_se);
                let cur = self.load_chain_value(rt, inflight);
                let word = bus.read32(addr & !3);
                let v = match addr & 3 {
                    0 => word,
                    1 => (cur & 0xff00_0000) | (word >> 8),
                    2 => (cur & 0xffff_0000) | (word >> 16),
                    _ => (cur & 0xffff_ff00) | (word >> 24),
                };
                self.set_load(rt, v);
            }
            0x28 => {
                // SB
                let addr = self.regs[rs].wrapping_add(imm_se);
                if !self.cop0.cache_isolated() {
                    bus.write8(addr, self.regs[rt] as u8);
                }
            }
            0x29 => {
                // SH
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 1 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdES);
                } else if !self.cop0.cache_isolated() {
                    bus.write16(addr, self.regs[rt] as u16);
                }
            }
            0x2a => {
                // SWL
                let addr = self.regs[rs].wrapping_add(imm_se);
                if !self.cop0.cache_isolated() {
                    let mem = bus.read32(addr & !3);
                    let reg = self.regs[rt];
                    let v = match addr & 3 {
                        0 => (mem & 0xffff_ff00) | (reg >> 24),
                        1 => (mem & 0xffff_0000) | (reg >> 16),
                        2 => (mem & 0xff00_0000) | (reg >> 8),
                        _ => reg,
                    };
                    bus.write32(addr & !3, v);
                }
            }
            0x2b => {
                // SW
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdES);
                } else if !self.cop0.cache_isolated() {
                    bus.write32(addr, self.regs[rt]);
                }
            }
            0x2e => {
                // SWR
                let addr = self.regs[rs].wrapping_add(imm_se);
                if !self.cop0.cache_isolated() {
                    let mem = bus.read32(addr & !3);
                    let reg = self.regs[rt];
                    let v = match addr & 3 {
                        0 => reg,
                        1 => (mem & 0x0000_00ff) | (reg << 8),
                        2 => (mem & 0x0000_ffff) | (reg << 16),
                        _ => (mem & 0x00ff_ffff) | (reg << 24),
                    };
                    bus.write32(addr & !3, v);
                }
            }
            0x32 => {
                // LWC2: memory -> GTE data register (no CPU load delay)
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdEL);
                } else {
                    let v = bus.read32(addr);
                    self.gte_sync(bus);
                    self.gte.write_data(rt as u32, v);
                }
            }
            0x3a => {
                // SWC2: GTE data register -> memory
                let addr = self.regs[rs].wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.cop0.bad_vaddr = addr;
                    self.exception(Exception::AdES);
                } else if !self.cop0.cache_isolated() {
                    self.gte_sync(bus);
                    let v = self.gte.read_data(rt as u32);
                    bus.write32(addr, v);
                }
            }
            0x30 | 0x31 | 0x33 | 0x38 | 0x39 | 0x3b => {
                // LWC0/1/3, SWC0/1/3: coprocessor absent
                self.exception(Exception::CoprocessorUnusable);
            }
            _ => self.illegal(instr),
        }
    }

    /// Value LWL/LWR merge into: an in-flight load to the same register is
    /// forwarded (hardware bypasses the delay for LWL/LWR chains).
    fn load_chain_value(&self, rt: usize, inflight: Option<(usize, u32)>) -> u32 {
        match inflight {
            Some((r, v)) if r == rt => v,
            _ => self.regs[rt],
        }
    }

    fn op_cop0(&mut self, instr: u32, rs: usize, rt: usize, rd: usize) {
        match rs {
            0x00 => {
                // MFC0 has a load delay, like memory loads
                let v = self.cop0.read(rd as u32);
                self.set_load(rt, v);
            }
            0x04 => self.cop0.write(rd as u32, self.regs[rt]), // MTC0
            0x10 => {
                if instr & 0x3f == 0x10 {
                    self.cop0.return_from_exception();
                } else {
                    self.illegal(instr);
                }
            }
            _ => self.illegal(instr),
        }
    }

    /// Hold the CPU until the GTE has finished the command it is running.
    /// Every instruction that touches the GTE goes through here first.
    fn gte_sync(&mut self, bus: &mut Bus) {
        let now = bus.now + bus.penalty;
        bus.penalty += self.gte_done_at.saturating_sub(now);
    }

    fn op_cop2(&mut self, bus: &mut Bus, instr: u32, rs: usize, rt: usize, rd: usize) {
        self.gte_sync(bus);
        if instr & (1 << 25) != 0 {
            let busy = self.gte.execute(instr & 0x1ff_ffff);
            self.gte_done_at = bus.now + bus.penalty + busy;
            return;
        }
        match rs {
            0x00 => {
                // MFC2 has a load delay, like memory loads
                let v = self.gte.read_data(rd as u32);
                self.set_load(rt, v);
            }
            0x02 => {
                let v = self.gte.read_control(rd as u32);
                self.set_load(rt, v);
            }
            0x04 => self.gte.write_data(rd as u32, self.regs[rt]),
            0x06 => self.gte.write_control(rd as u32, self.regs[rt]),
            _ => self.illegal(instr),
        }
    }

    fn illegal(&mut self, instr: u32) {
        warn!(target: "psx_core::cpu",
              "illegal instruction {instr:#010x} at {:#010x}", self.current_pc);
        self.exception(Exception::ReservedInstruction);
    }
}

/// Whether `op` is a load or store, and if so whether it writes memory.
/// The coprocessor loads/stores that raise CoprocessorUnusable never reach
/// the bus, so they are not accesses.
fn data_access(op: u32) -> Option<bool> {
    match op {
        0x20..=0x26 | 0x32 => Some(false),
        0x28..=0x2b | 0x2e | 0x3a => Some(true),
        _ => None,
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::BIOS_SIZE;

    /// Build a system with `program` placed at physical 0 and PC at KSEG0 RAM.
    fn setup(program: &[u32]) -> (Cpu, Bus) {
        let mut bus = Bus::new(vec![0; BIOS_SIZE]).unwrap();
        for (i, w) in program.iter().enumerate() {
            bus.write32((i * 4) as u32, *w);
        }
        let mut cpu = Cpu::new();
        cpu.pc = 0x8000_0000;
        cpu.next_pc = 0x8000_0004;
        (cpu, bus)
    }

    /// Step once and return what it cost, draining the bus the way the
    /// system does between instructions.
    fn step_cycles(cpu: &mut Cpu, bus: &mut Bus) -> u64 {
        cpu.step(bus);
        let cost = 1 + std::mem::take(&mut bus.penalty);
        bus.now += cost;
        cost
    }

    /// The GTE runs alongside the CPU, so a command is free until an
    /// instruction wants the result; only then does the CPU wait.
    #[test]
    fn gte_holds_the_cpu_only_when_its_result_is_wanted() {
        const RTPS: u32 = 0x4a18_0001; // COP2 imm25 = 0180001h, 15 cycles
        const MFC2: u32 = 0x4808_0000; // mfc2 $8, $0

        let (mut cpu, mut bus) = setup(&[RTPS, MFC2]);
        assert_eq!(
            step_cycles(&mut cpu, &mut bus),
            1,
            "the command itself issues in one"
        );
        assert_eq!(step_cycles(&mut cpu, &mut bus), 15, "reading it back waits");

        // With enough unrelated work in between, the wait is already over.
        let mut program = vec![RTPS];
        program.extend([0; 15]); // nops
        program.push(MFC2);
        let (mut cpu, mut bus) = setup(&program);
        for _ in 0..16 {
            step_cycles(&mut cpu, &mut bus);
        }
        assert_eq!(
            step_cycles(&mut cpu, &mut bus),
            1,
            "nothing left to wait for"
        );
    }

    #[test]
    fn load_delay_slot_hides_value_for_one_instruction() {
        // lw $t0, 0x100($zero); move $t1, $t0 (sees old); move $t2, $t0 (sees new)
        let (mut cpu, mut bus) = setup(&[
            0x8c08_0100, // lw $8, 0x100($0)
            0x0008_4821, // addu $9, $0, $8
            0x0008_5021, // addu $10, $0, $8
        ]);
        bus.write32(0x100, 0xdead_beef);
        cpu.regs[8] = 0x1111_1111;
        cpu.step(&mut bus); // lw
        cpu.step(&mut bus); // delay slot: old value
        assert_eq!(cpu.regs[9], 0x1111_1111);
        cpu.step(&mut bus); // load has landed
        assert_eq!(cpu.regs[10], 0xdead_beef);
        assert_eq!(cpu.regs[8], 0xdead_beef);
    }

    #[test]
    fn direct_write_cancels_inflight_load() {
        // lw $t0, 0x100($zero); addiu $t0, $zero, 7 → the addiu wins
        let (mut cpu, mut bus) = setup(&[
            0x8c08_0100, // lw $8, 0x100($0)
            0x2408_0007, // addiu $8, $0, 7
        ]);
        bus.write32(0x100, 0xdead_beef);
        cpu.step(&mut bus);
        cpu.step(&mut bus);
        assert_eq!(cpu.regs[8], 7);
    }

    #[test]
    fn branch_delay_slot_executes() {
        let (mut cpu, mut bus) = setup(&[
            0x1000_0002, // beq $0, $0, +2 (target = 0x8000000c)
            0x2409_0001, // addiu $9, $0, 1   (delay slot, executes)
            0x240a_0001, // addiu $10, $0, 1  (skipped)
            0x240b_0001, // addiu $11, $0, 1  (branch target)
        ]);
        cpu.step(&mut bus);
        cpu.step(&mut bus);
        cpu.step(&mut bus);
        assert_eq!(cpu.regs[9], 1);
        assert_eq!(cpu.regs[10], 0);
        assert_eq!(cpu.regs[11], 1);
    }

    #[test]
    fn syscall_sets_epc_and_jumps_to_bev_handler() {
        let (mut cpu, mut bus) = setup(&[0x0000_000c]); // syscall
        cpu.cop0.sr = 1 << 22; // BEV=1
        cpu.step(&mut bus);
        assert_eq!(cpu.cop0.epc, 0x8000_0000);
        assert_eq!(cpu.pc, 0xbfc0_0180);
        // ExcCode = 8 (Syscall)
        assert_eq!((cpu.cop0.cause >> 2) & 0x1f, 8);
    }

    /// DCIC r7 value enabling kernel-mode breaks with the given extra bits.
    const fn dcic(extra: u32) -> u32 {
        (1 << 23) | (1 << 29) | extra // DE | KD
    }

    /// Arm the program-counter breakpoint on `pc` with an exact-match mask.
    fn arm_code_break(cpu: &mut Cpu, pc: u32, dcic_bits: u32) {
        cpu.cop0.write(3, pc); // BPC
        cpu.cop0.write(11, 0xffff_ffff); // BPCM
        cpu.cop0.write(7, dcic_bits);
    }

    #[test]
    fn code_breakpoint_traps_to_the_debug_vector() {
        let (mut cpu, mut bus) = setup(&[0, 0]);
        // PCE | TR
        arm_code_break(&mut cpu, 0x8000_0004, dcic((1 << 24) | (1 << 31)));
        cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8000_0004);
        cpu.step(&mut bus);
        // Its own vector, not the general 0x80000080 one
        assert_eq!(cpu.pc, 0x8000_0040);
        assert_eq!(cpu.cop0.epc, 0x8000_0004);
        // ExcCode = 9, as for the BREAK opcode
        assert_eq!((cpu.cop0.cause >> 2) & 0x1f, 9);
    }

    #[test]
    fn code_breakpoint_without_trap_only_records_the_hit() {
        let (mut cpu, mut bus) = setup(&[0, 0x2409_0001]); // nop; addiu $9, $0, 1
        arm_code_break(&mut cpu, 0x8000_0004, dcic(1 << 24)); // PCE, no TR
        cpu.step(&mut bus);
        cpu.step(&mut bus);
        assert_eq!(cpu.regs[9], 1, "execution must continue");
        // DB | PC
        assert_eq!(cpu.cop0.read(7) & 0b11, 0b11);
    }

    #[test]
    fn code_breakpoint_stays_disarmed_for_the_other_privilege_level() {
        let (mut cpu, mut bus) = setup(&[0, 0]);
        // UD only, while SR.KUc says kernel mode
        arm_code_break(
            &mut cpu,
            0x8000_0004,
            (1 << 23) | (1 << 30) | (1 << 24) | (1 << 31),
        );
        cpu.step(&mut bus);
        cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8000_0008);
    }

    #[test]
    fn data_breakpoint_traps_before_the_store_lands() {
        let (mut cpu, mut bus) = setup(&[0xac08_0100]); // sw $8, 0x100($0)
        cpu.regs[8] = 0x1234_5678;
        cpu.cop0.write(5, 0x100); // BDA
        cpu.cop0.write(9, 0xffff_ffff); // BDAM
        // DAE | DW | TR
        cpu.cop0.write(7, dcic((1 << 25) | (1 << 27) | (1 << 31)));
        cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8000_0040);
        assert_eq!(bus.read32(0x100), 0);
        // DB | DA | W
        assert_eq!(cpu.cop0.read(7) & 0b1_1101, 0b1_0101);
    }

    #[test]
    fn data_breakpoint_ignores_the_direction_it_was_not_armed_for() {
        let (mut cpu, mut bus) = setup(&[0xac08_0100]); // sw $8, 0x100($0)
        cpu.regs[8] = 0x1234_5678;
        cpu.cop0.write(5, 0x100);
        cpu.cop0.write(9, 0xffff_ffff);
        // DAE | DR | TR: reads only, so a store must pass through
        cpu.cop0.write(7, dcic((1 << 25) | (1 << 26) | (1 << 31)));
        cpu.step(&mut bus);
        assert_eq!(bus.read32(0x100), 0x1234_5678);
    }

    #[test]
    fn dcic_read_only_bits_stay_zero() {
        let mut cop0 = Cop0::default();
        cop0.write(7, 0xffff_ffff);
        assert_eq!(cop0.read(7), 0xff80_f03f);
    }

    #[test]
    fn stores_swallowed_while_cache_isolated() {
        let (mut cpu, mut bus) = setup(&[0xac08_0100]); // sw $8, 0x100($0)
        cpu.regs[8] = 0x1234_5678;
        cpu.cop0.sr = 1 << 16; // IsC
        cpu.step(&mut bus);
        assert_eq!(bus.read32(0x100), 0);
    }
}
