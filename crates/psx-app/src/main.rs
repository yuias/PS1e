//! Native frontend: egui debug shell (registers, TTY console, run control).
//!
//! `--headless` runs the core without a window for CI and BIOS bring-up:
//!   ps1e --headless --cycles 30000000 [--bios assets/SCPH-1000.bin]
//!
//! Headless game progression: `--mash-start` taps START/CROSS periodically;
//! `--input <file>` replays a script of timed button holds (see
//! [`parse_input_script`]). Inspect results with `--dump-frame`/`--dump-vram`
//! and `--log-gpu` (decoded GP0/GP1 command log).
//!
//! `--debug-port <port>` opens an LLDB/GDB gdb-remote stub (both GUI and
//! headless); `--wait-debugger` additionally holds execution at the reset
//! vector until a debugger attaches:
//!   (lldb) gdb-remote localhost:9001
//!
//! `--control-port <port>` (headless only) runs the emulator in lockstep,
//! driven interactively through the `psxctl` client — designed for scripted
//! or LLM-driven game analysis (see [`control`]).

mod audio;
mod config;
mod control;
mod disc;
mod emu;
mod gamepad;
mod ui;

use psx_core::PsxSystem;

struct Args {
    bios: Option<String>,
    disc: Option<String>,
    headless: bool,
    cycles: u64,
    dump_vram: Option<String>,
    dump_wav: Option<String>,
    dump_frame: Option<String>,
    peek: Option<u32>,
    /// Headless: tap START every 2 seconds to get past title screens.
    mash_start: bool,
    /// Headless: input script replayed during the run (overrides mash-start).
    input: Option<String>,
    /// Decode every GP0/GP1 command to the log.
    log_gpu: bool,
    /// gdb-remote stub port (LLDB-first; see psx-debug).
    debug_port: Option<u16>,
    /// Hold execution at the reset vector until a debugger attaches.
    wait_debugger: bool,
    /// Lockstep control port for interactive automation (headless only).
    control_port: Option<u16>,
}

fn parse_args() -> Args {
    let mut args = Args {
        bios: None,
        disc: None,
        headless: false,
        cycles: 30_000_000,
        dump_vram: None,
        dump_wav: None,
        dump_frame: None,
        peek: None,
        mash_start: false,
        input: None,
        log_gpu: false,
        debug_port: None,
        wait_debugger: false,
        control_port: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => args.headless = true,
            "--mash-start" => args.mash_start = true,
            "--log-gpu" => args.log_gpu = true,
            "--wait-debugger" => args.wait_debugger = true,
            "--debug-port" => {
                args.debug_port = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .expect("--debug-port needs a port number"),
                )
            }
            "--control-port" => {
                args.control_port = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .expect("--control-port needs a port number"),
                )
            }
            "--input" => args.input = Some(it.next().expect("--input needs a path")),
            "--bios" => args.bios = Some(it.next().expect("--bios needs a path")),
            "--disc" => args.disc = Some(it.next().expect("--disc needs a path")),
            "--cycles" => {
                args.cycles = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--cycles needs a number")
            }
            "--dump-vram" => args.dump_vram = Some(it.next().expect("--dump-vram needs a path")),
            "--dump-frame" => args.dump_frame = Some(it.next().expect("--dump-frame needs a path")),
            "--dump-wav" => args.dump_wav = Some(it.next().expect("--dump-wav needs a path")),
            "--peek" => {
                args.peek = Some(
                    u32::from_str_radix(
                        it.next()
                            .expect("--peek needs a hex address")
                            .trim_start_matches("0x"),
                        16,
                    )
                    .expect("--peek needs a hex address"),
                )
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
    let args = parse_args();
    // The GPU command log emits at debug level under its own target so it can
    // be toggled at runtime (UI checkbox / --log-gpu) without drowning `info`.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into())
        .add_directive("psx_core::gpu::cmd=debug".parse().unwrap());
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let (cfg, cfg_path) = config::Config::load();

    // CLI takes precedence over the config file
    let bios_path = args
        .bios
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| cfg.bios.clone())
        .unwrap_or_else(|| {
            eprintln!("No BIOS configured.");
            eprintln!("Set `bios = \"...\"` in the config file or pass --bios <path>.");
            if let Some(p) = &cfg_path {
                eprintln!("Config file: {}", p.display());
            }
            std::process::exit(2);
        });
    let bios = std::fs::read(&bios_path)
        .unwrap_or_else(|e| panic!("failed to read BIOS '{}': {e}", bios_path.display()));
    let mut sys = PsxSystem::new(bios.clone()).expect("failed to create system");
    sys.bus.gpu.log_commands = args.log_gpu;
    if let Some(path) = &args.disc {
        match disc::load_disc(std::path::Path::new(path)) {
            Ok(d) => sys.insert_disc(d),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
    }

    // Memory card: load the image, or create a freshly formatted one
    let memcard_path = cfg.memcard_path(cfg_path.as_ref());
    match std::fs::read(&memcard_path) {
        Ok(data) if data.len() == psx_core::memcard::CARD_SIZE => {
            sys.bus.sio.memcard = psx_core::memcard::MemCard::with_data(data.into_boxed_slice());
            tracing::info!("memory card: {}", memcard_path.display());
        }
        Ok(_) => {
            tracing::error!(
                "memory card {} has wrong size; using a fresh card (not saved over it)",
                memcard_path.display()
            );
        }
        Err(_) => {
            if let Some(dir) = memcard_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let card = psx_core::memcard::MemCard::new();
            if let Err(e) = std::fs::write(&memcard_path, &card.data) {
                tracing::warn!("could not create memory card image: {e}");
            } else {
                tracing::info!("created memory card: {}", memcard_path.display());
            }
            sys.bus.sio.memcard = card;
        }
    }

    let debugger = args
        .debug_port
        .map(|port| psx_debug::DebugServer::bind(port).expect("failed to bind debug port"));
    if args.control_port.is_some() && !args.headless {
        eprintln!("--control-port requires --headless (lockstep control needs no window)");
        std::process::exit(2);
    }

    if args.headless {
        let script = args
            .input
            .as_deref()
            .map(parse_input_script)
            .unwrap_or_default();
        run_headless(sys, &args, &script, debugger);
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
        Box::new(move |cc| {
            let worker_cfg = emu::WorkerConfig {
                bios,
                state_path: memcard_path.with_file_name("state0.sst"),
                memcard_path,
                debugger,
                wait_debugger: args.wait_debugger,
                volume: cfg.volume,
                log_gpu: args.log_gpu,
            };
            let emu = emu::spawn(sys, worker_cfg, cc.egui_ctx.clone());
            Ok(Box::new(ui::App::new(emu, cfg, cfg_path, args.log_gpu)))
        }),
    )
}

/// One scripted input span: hold `buttons` for cycles `start..end`.
struct InputSpan {
    start: u64,
    end: u64,
    buttons: u16,
}

/// Parse an input script: one span per line, `<start-sec> <dur-sec> <BUTTONS>`
/// where BUTTONS is `+`-separated names (START, CROSS, UP, ...). `#` starts a
/// comment. Overlapping spans are OR-ed together.
fn parse_input_script(path: &str) -> Vec<InputSpan> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read input script '{path}': {e}"));
    let mut spans = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let bad = || -> ! {
            panic!(
                "{path}:{}: expected '<start-sec> <dur-sec> <BUTTONS>'",
                n + 1
            )
        };
        let mut parts = line.split_whitespace();
        let start: f64 = parts
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| bad());
        let dur: f64 = parts
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| bad());
        let buttons = parts
            .next()
            .unwrap_or_else(|| bad())
            .split('+')
            .map(|name| {
                control::button_by_name(name)
                    .unwrap_or_else(|| panic!("{path}:{}: unknown button '{name}'", n + 1))
            })
            .fold(0u16, |acc, b| acc | b);
        let hz = psx_core::CPU_CLOCK_HZ as f64;
        spans.push(InputSpan {
            start: (start * hz) as u64,
            end: ((start + dur) * hz) as u64,
            buttons,
        });
    }
    spans
}

fn run_headless(
    mut sys: PsxSystem,
    args: &Args,
    script: &[InputSpan],
    mut debugger: Option<psx_debug::DebugServer>,
) {
    let cycles = args.cycles;
    let dump_wav = args.dump_wav.as_deref();
    // Chunked run so we can inject input and collect audio along the way
    const CHUNK: u64 = psx_core::CPU_CLOCK_HZ / 10;
    let mut wav_samples: Vec<i16> = Vec::new();

    if let Some(port) = args.control_port {
        // Lockstep control mode: the emulator advances only on `run`/`press`
        // commands, so --cycles and input scripts do not apply.
        let mut ctl = control::ControlServer::bind(port).expect("failed to bind control port");
        tracing::info!("lockstep control mode; drive with: psxctl --port {port} help");
        loop {
            let debugger_owns = debugger.as_ref().is_some_and(|d| d.attached());
            if let Some(dbg) = &mut debugger {
                dbg.pump(&mut sys, CHUNK);
            }
            if !ctl.pump(&mut sys, debugger_owns) {
                break;
            }
            if dump_wav.is_some() {
                sys.bus.spu.drain_output(&mut wav_samples);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        finish_headless(&mut sys, args, wav_samples);
        return;
    }

    tracing::info!("running headless for {cycles} cycles");
    let mut debugger_seen = false;
    let mut done = 0u64;
    while done < cycles {
        // While a debugger is attached (or awaited), it owns execution; only
        // the cycles it actually ran count against the --cycles budget.
        if let Some(dbg) = &mut debugger {
            let before = sys.cycles();
            dbg.pump(&mut sys, CHUNK.min(cycles - done));
            debugger_seen |= dbg.attached();
            done += sys.cycles() - before;
            if dbg.attached() || (args.wait_debugger && !debugger_seen) {
                if dbg.halted() || !dbg.attached() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                if dump_wav.is_some() {
                    sys.bus.spu.drain_output(&mut wav_samples);
                }
                continue;
            }
        }
        if !script.is_empty() {
            let buttons = script
                .iter()
                .filter(|s| s.start <= done && done < s.end)
                .fold(0u16, |acc, s| acc | s.buttons);
            sys.set_buttons(buttons);
        } else if args.mash_start {
            // Alternate START and CROSS taps (0.5s each out of every 2s)
            // so both title screens and menus advance
            let phase = done / CHUNK % 40;
            sys.set_buttons(match phase {
                0..5 => psx_core::sio::button::START,
                20..25 => psx_core::sio::button::CROSS,
                _ => 0,
            });
        }
        let n = CHUNK.min(cycles - done);
        sys.run_cycles(n);
        done += n;
        if dump_wav.is_some() {
            sys.bus.spu.drain_output(&mut wav_samples);
        }
    }
    finish_headless(&mut sys, args, wav_samples);
}

/// End-of-run summary and dumps, shared by budgeted and control-mode runs.
fn finish_headless(sys: &mut PsxSystem, args: &Args, wav_samples: Vec<i16>) {
    let (dump_vram, dump_wav) = (args.dump_vram.as_deref(), args.dump_wav.as_deref());
    if let Some(path) = dump_wav {
        write_wav(path, &wav_samples);
        tracing::info!(
            "wrote {} ({:.1}s of audio)",
            path,
            wav_samples.len() as f64 / 2.0 / 44_100.0
        );
    }
    tracing::info!(
        "done: pc={:#010x} cycles={} frames={} sr={:#010x} cause={:#010x} i_stat={:#06x} i_mask={:#06x}",
        sys.cpu.pc,
        sys.cycles(),
        sys.bus.gpu.frame_count,
        sys.cpu.cop0.sr,
        sys.cpu.cop0.cause,
        sys.bus.irq.stat,
        sys.bus.irq.mask,
    );
    let mut samples = Vec::new();
    sys.bus.spu.drain_output(&mut samples);
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    tracing::info!("audio: {} samples buffered, peak {peak}", samples.len() / 2);
    tracing::info!(
        "xa: {} sectors decoded, {} frames pushed ({:.1}/sector), dropped {} (cd_in {})",
        sys.bus.cdrom.xa_sectors,
        sys.bus.cdrom.xa_frames,
        sys.bus.cdrom.xa_frames as f64 / sys.bus.cdrom.xa_sectors.max(1) as f64,
        sys.bus.cdrom.xa_dropped,
        sys.bus.spu.cd_dropped,
    );
    print!("--- TTY ---\n{}\n-----------\n", sys.tty_output());
    // Dump the instructions around PC to identify wait loops during bring-up
    let pc = (sys.cpu.pc & 0x001f_ffff) as usize;
    for ofs in (pc.saturating_sub(16)..pc + 16).step_by(4) {
        let w = u32::from_le_bytes(sys.bus.ram[ofs..ofs + 4].try_into().unwrap());
        println!(
            "{:#010x}: {w:08x}{}",
            ofs,
            if ofs == pc { "  <- pc" } else { "" }
        );
    }
    // Dump the kernel event table (EvCB pointer at 0x120): one line per
    // entry as [index] class spec status — resolves TestEvent handles.
    let ram = &sys.bus.ram;
    let word =
        |a: usize| u32::from_le_bytes(ram[a & 0x1f_fffc..(a & 0x1f_fffc) + 4].try_into().unwrap());
    let evcb = word(0x120) as usize & 0x001f_ffff;
    let evcb_size = word(0x124) as usize / 0x1c;
    if evcb != 0 {
        println!("--- events (EvCB at {evcb:#x}, {evcb_size} entries) ---");
        for i in 0..evcb_size.min(32) {
            let base = evcb + i * 0x1c;
            let (class, status, spec) = (word(base), word(base + 4), word(base + 8));
            if class != 0 {
                println!("[{i:#04x}] class={class:#010x} spec={spec:#06x} status={status:#06x}");
            }
        }
    }
    if let Some(addr) = args.peek {
        println!("--- peek {addr:#010x} ---");
        let base = (addr & 0x001f_fffc) as usize;
        for ofs in (base..base + 96).step_by(4) {
            let w = u32::from_le_bytes(sys.bus.ram[ofs..ofs + 4].try_into().unwrap());
            println!("{:#010x}: {w:08x}", ofs);
        }
    }
    if let Some(path) = dump_vram {
        write_vram_bmp(path, &sys.bus.gpu.vram);
        tracing::info!("VRAM dumped to {path}");
    }
    if let Some(path) = args.dump_frame.as_deref() {
        let frame = &sys.bus.gpu.frame;
        if frame.width == 0 || frame.height == 0 {
            tracing::warn!("no frame captured yet; skipping --dump-frame");
        } else {
            write_frame_bmp(path, frame);
            tracing::info!(
                "frame dumped to {path} ({}x{}{}{})",
                frame.width,
                frame.height,
                if frame.is_24bit { ", 24-bit" } else { "" },
                if frame.enabled {
                    ""
                } else {
                    ", display disabled"
                },
            );
        }
    }
}

/// 44.1kHz stereo 16-bit WAV writer for offline listening tests.
fn write_wav(path: &str, samples: &[i16]) {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&44_100u32.to_le_bytes());
    out.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).expect("failed to write WAV");
}

/// Dump the last vblank-latched display frame (what the TV shows) as a
/// 24-bit BMP, decoding 15-bit or packed 24-bit rows as appropriate.
fn write_frame_bmp(path: &str, frame: &psx_core::gpu::Frame) {
    let (w, h, stride) = (
        frame.width as usize,
        frame.height as usize,
        frame.stride as usize,
    );
    let pad = (4 - (w * 3) % 4) % 4;
    let data_size = (w * 3 + pad) * h;
    let mut out = Vec::with_capacity(54 + data_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + data_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // no compression, default resolution
    for y in (0..h).rev() {
        let row = &frame.pixels[y * stride..(y + 1) * stride];
        for x in 0..w {
            let (r, g, b) = if frame.is_24bit {
                let byte = x * 3;
                let read = |b: usize| (row[(byte + b) / 2] >> (((byte + b) & 1) * 8)) as u8;
                (read(0), read(1), read(2))
            } else {
                let px = row[x];
                let e = |c: u16| ((c << 3) | (c >> 2)) as u8;
                (e(px & 0x1f), e((px >> 5) & 0x1f), e((px >> 10) & 0x1f))
            };
            out.extend_from_slice(&[b, g, r]);
        }
        out.extend_from_slice(&[0, 0, 0][..pad]);
    }
    std::fs::write(path, out).expect("failed to write BMP");
}

/// Dump the full 1024x512 VRAM as a 24-bit BMP for offline inspection.
fn write_vram_bmp(path: &str, vram: &[u16]) {
    const W: usize = 1024;
    const H: usize = 512;
    let row = W * 3; // already a multiple of 4, no padding needed
    let data_size = row * H;
    let mut out = Vec::with_capacity(54 + data_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + data_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(W as u32).to_le_bytes());
    out.extend_from_slice(&(H as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // no compression, default resolution
    for y in (0..H).rev() {
        for x in 0..W {
            let px = vram[y * W + x];
            let r = ((px & 0x1f) << 3) as u8;
            let g = (((px >> 5) & 0x1f) << 3) as u8;
            let b = (((px >> 10) & 0x1f) << 3) as u8;
            out.extend_from_slice(&[b, g, r]);
        }
    }
    std::fs::write(path, out).expect("failed to write BMP");
}
