//! Save-state round-trip and determinism tests.

use psx_core::PsxSystem;

fn fresh() -> PsxSystem {
    // Zero BIOS: an endless nop sled — enough to exercise the CPU, timers,
    // scheduler and SPU sample generation deterministically.
    PsxSystem::new(vec![0; 512 * 1024]).unwrap()
}

/// Snapshot of externally observable machine state for equality checks.
fn observe(sys: &PsxSystem) -> (u32, [u32; 32], u64, u64) {
    let ram_sum = sys
        .ram()
        .iter()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(*b as u64));
    (sys.cpu.pc, sys.cpu.regs, sys.cycles(), ram_sum)
}

#[test]
fn round_trip_restores_execution_state() {
    let mut sys = fresh();
    sys.run_cycles(100_000);
    let state = sys.save_state().unwrap();
    let at_save = observe(&sys);

    sys.run_cycles(50_000);
    assert_ne!(observe(&sys).2, at_save.2);

    sys.load_state(&state).unwrap();
    assert_eq!(observe(&sys), at_save);
}

#[test]
fn resumed_execution_is_deterministic() {
    let mut sys = fresh();
    sys.run_cycles(100_000);
    let state = sys.save_state().unwrap();

    sys.run_cycles(77_777);
    let first_run = observe(&sys);

    sys.load_state(&state).unwrap();
    sys.run_cycles(77_777);
    assert_eq!(observe(&sys), first_run);
}

#[test]
fn bios_survives_load() {
    let mut sys = fresh();
    let state = sys.save_state().unwrap();
    sys.load_state(&state).unwrap();
    assert_eq!(sys.bios().len(), 512 * 1024);
    // The machine still runs after a load.
    sys.run_cycles(1_000);
}

#[test]
fn rejects_garbage_and_wrong_versions() {
    let mut sys = fresh();
    assert!(sys.load_state(b"nope").is_err());
    assert!(sys.load_state(b"XXXX\x01\x00\x00\x00\x00\x00rest").is_err());

    let mut state = sys.save_state().unwrap();
    state[4] = 0xff; // corrupt the version field
    assert!(sys.load_state(&state).is_err());
}

/// Cheats are frontend-owned: they ride in `Ambient`, so neither a state
/// load nor a reset may drop them, and they must keep firing afterwards.
#[test]
fn cheats_survive_a_state_load_and_a_reset() {
    use psx_core::cheats::CheatList;

    let mut sys = fresh();
    sys.set_cheats(CheatList::parse("[*t]\n80000100 BEEF\n"));
    let state = sys.save_state().unwrap();

    sys.load_state(&state).unwrap();
    assert_eq!(sys.cheats().cheats.len(), 1);

    sys.reset();
    assert_eq!(sys.cheats().cheats.len(), 1);

    // One frame is enough to reach a vblank, which is where they apply.
    sys.run_cycles(psx_core::CPU_CLOCK_HZ / 50);
    assert_eq!(sys.peek8(0x8000_0100), Some(0xEF));
    assert_eq!(sys.peek8(0x8000_0101), Some(0xBE));
}
