//! Test executables run over a booted BIOS.
//!
//! Nothing here is in the repository — `assets/` is ignored — so a missing
//! or unbootable image reports what it was looking for and returns instead
//! of failing. That second case is normal in this project: the BIOS under
//! development is not expected to reach the shell yet, so point
//! `PS1E_TEST_BIOS` at a retail image to exercise these.
//!
//! Third-party executables (Amidog's CPU and GTE tests, for instance) go in
//! `assets/tests/`, or wherever `PS1E_TEST_ROMS` points.

mod common;

use common::{EXE_RESULT_BASE, Program, T0, run_to_marker};
use psx_core::PsxSystem;
use std::path::PathBuf;

/// Where the BIOS hands control to the executable it has loaded.
const SHELL_ENTRY: u32 = 0x8003_0000;
/// Cycles allowed for the BIOS to reach the shell. A retail image gets there
/// in about 80 million; anything far beyond that is not going to arrive.
const BOOT_CAP: u64 = 200_000_000;
/// Cycles per observation chunk while a test executable runs.
const CHUNK: u64 = 2_000_000;
/// Chunks without new TTY output that mean the program has stopped talking.
const QUIET_CHUNKS: u32 = 8;
/// Ceiling on a single executable, so a hung program still ends the test.
const RUN_CAP: u64 = 400_000_000;

/// The crate sits two levels below the workspace root.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn bios_path() -> PathBuf {
    std::env::var_os("PS1E_TEST_BIOS")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("assets/bios.bin"))
}

fn rom_dir() -> PathBuf {
    std::env::var_os("PS1E_TEST_ROMS")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("assets/tests"))
}

/// Boot the BIOS to the point where it would enter a loaded executable.
/// `None` means there is nothing to test with, not that a test failed.
fn booted() -> Option<PsxSystem> {
    let path = bios_path();
    let Ok(image) = std::fs::read(&path) else {
        eprintln!("skipping: no BIOS image at {}", path.display());
        return None;
    };

    let mut sys = PsxSystem::new(image).expect("BIOS image");
    while sys.cycles() < BOOT_CAP {
        if sys.cpu.pc == SHELL_ENTRY {
            return Some(sys);
        }
        sys.step();
    }
    eprintln!(
        "skipping: {} does not reach the shell (pc {:#010x}, {} cycles); set PS1E_TEST_BIOS to a retail image",
        path.display(),
        sys.cpu.pc,
        sys.cycles()
    );
    None
}

/// Run until the program stops producing TTY output, then return all of it.
fn run_until_quiet(sys: &mut PsxSystem) -> String {
    let mut seen = 0;
    let mut quiet = 0;
    while sys.cycles() < RUN_CAP && quiet < QUIET_CHUNKS {
        sys.run_cycles(CHUNK);
        let len = sys.tty_output().len();
        quiet = if len == seen { quiet + 1 } else { 0 };
        seen = len;
    }
    sys.tty_output().to_owned()
}

/// Exercises the side-load path with a program built here, so the loader
/// stays covered even where no third-party executable is available.
#[test]
fn side_loaded_executable_runs() {
    let Some(mut sys) = booted() else { return };

    let mut program = Program::with_results(EXE_RESULT_BASE);
    program.li(T0, 0x1234_5678);
    program.store_result(0, T0);

    sys.load_exe(&program.into_exe(0x8001_0000, 0x801f_fff0))
        .expect("side-load");
    assert_eq!(run_to_marker(sys, EXE_RESULT_BASE).slot(0), 0x1234_5678);
}

#[test]
fn test_executables_report_no_failures() {
    let dir = rom_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: no test executables in {}", dir.display());
        return;
    };

    let mut roms: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("psexe"))
        })
        .collect();
    roms.sort();

    if roms.is_empty() {
        eprintln!("skipping: no test executables in {}", dir.display());
        return;
    }

    let mut failed = Vec::new();
    for rom in &roms {
        let name = rom.file_name().unwrap().to_string_lossy().into_owned();
        let exe = std::fs::read(rom).expect("test executable");
        let Some(mut sys) = booted() else { return };

        let boot_tty = sys.tty_output().len();
        if let Err(e) = sys.load_exe(&exe) {
            failed.push(format!("{name}: {e}"));
            continue;
        }

        let tty = run_until_quiet(&mut sys);
        let output = tty[boot_tty.min(tty.len())..].trim().to_owned();
        eprintln!("--- {name} ---\n{output}\n");

        if output.is_empty() {
            failed.push(format!("{name}: produced no output"));
        } else if output.to_ascii_lowercase().contains("fail") {
            failed.push(format!("{name}: reported a failure"));
        }
    }

    assert!(
        failed.is_empty(),
        "test executables failed:\n{}",
        failed.join("\n")
    );
}
