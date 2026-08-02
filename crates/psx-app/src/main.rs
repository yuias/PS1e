//! Native frontend: egui debug shell (registers, TTY console, run control).
//!
//! `--headless` runs the core without a window for CI and BIOS bring-up:
//!   psx-app --headless --cycles 30000000 [--bios assets/SCPH-1000.bin]

mod ui;

use psx_core::PsxSystem;

const DEFAULT_BIOS: &str = "assets/SCPH-1000.bin";

struct Args {
    bios: String,
    headless: bool,
    cycles: u64,
}

fn parse_args() -> Args {
    let mut args = Args {
        bios: DEFAULT_BIOS.to_string(),
        headless: false,
        cycles: 30_000_000,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => args.headless = true,
            "--bios" => args.bios = it.next().expect("--bios needs a path"),
            "--cycles" => {
                args.cycles = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--cycles needs a number")
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args();
    let bios = std::fs::read(&args.bios)
        .unwrap_or_else(|e| panic!("failed to read BIOS '{}': {e}", args.bios));
    let sys = PsxSystem::new(bios.clone()).expect("failed to create system");

    if args.headless {
        run_headless(sys, args.cycles);
        return Ok(());
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_title("PS1e"),
        ..Default::default()
    };
    eframe::run_native(
        "PS1e",
        options,
        Box::new(move |_cc| Ok(Box::new(ui::App::new(sys, bios)))),
    )
}

fn run_headless(mut sys: PsxSystem, cycles: u64) {
    tracing::info!("running headless for {cycles} cycles");
    sys.run_cycles(cycles);
    tracing::info!(
        "done: pc={:#010x} cycles={}",
        sys.cpu.pc,
        sys.cycles()
    );
    print!("--- TTY ---\n{}\n-----------\n", sys.tty_output());
}
