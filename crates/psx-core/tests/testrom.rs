//! CPU behaviour checked with generated test ROMs.
//!
//! These assert on what a program computes, never on how many cycles it
//! took, so they stay valid while the timing model is refined.

mod common;

use common::*;

#[test]
fn arithmetic_and_logic() {
    let mut p = Program::new();
    p.li(T0, 0x0000_0007);
    p.li(T1, 0xffff_fffe); // -2
    p.emit_all([
        addu(T2, T0, T1), // 5
        subu(T3, T0, T1), // 9
        and(T4, T0, T1),  // 6
        or(T5, T0, T1),   // 0xffff_ffff
        xor(T6, T0, T1),  // 0xffff_fff9
        nor(T7, T0, T1),  // 0
    ]);
    p.store_result(0, T2);
    p.store_result(1, T3);
    p.store_result(2, T4);
    p.store_result(3, T5);
    p.store_result(4, T6);
    p.store_result(5, T7);

    // Signedness: -2 < 7 signed, but 0xfffffffe > 7 unsigned.
    p.emit_all([slt(T2, T1, T0), sltu(T3, T1, T0)]);
    p.store_result(6, T2);
    p.store_result(7, T3);

    // Shifts, including the arithmetic/logical split on a negative value.
    p.emit_all([sll(T2, T1, 4), srl(T3, T1, 4), sra(T4, T1, 4)]);
    p.store_result(8, T2);
    p.store_result(9, T3);
    p.store_result(10, T4);

    let r = p.run();
    assert_eq!(r.slot(0), 5, "addu");
    assert_eq!(r.slot(1), 9, "subu");
    assert_eq!(r.slot(2), 6, "and");
    assert_eq!(r.slot(3), 0xffff_ffff, "or");
    assert_eq!(r.slot(4), 0xffff_fff9, "xor");
    assert_eq!(r.slot(5), 0, "nor");
    assert_eq!(r.slot(6), 1, "slt is signed");
    assert_eq!(r.slot(7), 0, "sltu is unsigned");
    assert_eq!(r.slot(8), 0xffff_ffe0, "sll");
    assert_eq!(r.slot(9), 0x0fff_ffff, "srl zero-extends");
    assert_eq!(r.slot(10), 0xffff_ffff, "sra sign-extends");
}

#[test]
fn r0_stays_zero() {
    let mut p = Program::new();
    p.li(T0, 0x1234_5678);
    p.emit(addu(ZERO, T0, T0));
    p.store_result(0, ZERO);

    assert_eq!(p.run().slot(0), 0, "writes to $zero are discarded");
}

#[test]
fn branch_delay_slot_executes() {
    let mut p = Program::new();
    p.li(T0, 0);
    // Taken branch over the instruction following the delay slot.
    p.emit_all([
        beq(ZERO, ZERO, 2),
        addiu(T0, T0, 5),   // delay slot: runs even though the branch is taken
        addiu(T0, T0, 100), // skipped
    ]);
    p.store_result(0, T0);

    // The same for a not-taken branch: the delay slot runs, and so does the
    // instruction after it.
    p.li(T1, 0);
    p.emit_all([bne(ZERO, ZERO, 2), addiu(T1, T1, 5), addiu(T1, T1, 100)]);
    p.store_result(1, T1);

    let r = p.run();
    assert_eq!(r.slot(0), 5, "taken branch runs its delay slot only");
    assert_eq!(r.slot(1), 105, "not-taken branch falls through");
}

#[test]
fn load_delay_slot_sees_the_old_register() {
    let mut p = Program::new();
    p.li(T0, 0x1234_5678);
    p.store_result(0, T0); // park the value in RAM to load it back

    p.li(T1, 0x0000_aaaa);
    p.emit_all([
        lui(AT, 0x8000),
        lw(T1, 0x1000, AT),
        addu(T2, T1, ZERO), // load delay slot: T1 still holds the old value
        nop(),
    ]);
    p.store_result(1, T2);
    p.store_result(2, T1);

    let r = p.run();
    assert_eq!(r.slot(1), 0x0000_aaaa, "the delay slot reads the old value");
    assert_eq!(
        r.slot(2),
        0x1234_5678,
        "the load lands one instruction later"
    );
}

#[test]
fn multiply_and_divide() {
    let mut p = Program::new();
    p.li(T0, 7);
    p.li(T1, 0xffff_fffa); // -6

    p.emit_all([mult(T0, T1), mflo(T2), mfhi(T3)]);
    p.store_result(0, T2);
    p.store_result(1, T3);

    p.emit_all([multu(T0, T1), mflo(T4), mfhi(T5)]);
    p.store_result(2, T4);
    p.store_result(3, T5);

    p.emit_all([div(T0, T1), mflo(T6), mfhi(T7)]);
    p.store_result(4, T6);
    p.store_result(5, T7);

    let r = p.run();
    assert_eq!(r.slot(0), (-42i32) as u32, "mult lo");
    assert_eq!(r.slot(1), 0xffff_ffff, "mult hi sign-extends");
    // The low word is the same for both; only the high word sees the sign.
    assert_eq!(r.slot(2), (-42i32) as u32, "multu lo");
    assert_eq!(r.slot(3), 6, "multu hi treats the operand as unsigned");
    assert_eq!(
        r.slot(4),
        (-1i32) as u32,
        "div quotient truncates toward zero"
    );
    assert_eq!(r.slot(5), 1, "div remainder takes the dividend's sign");
}
