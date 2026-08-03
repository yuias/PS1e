//! Native frontend: egui debug shell (registers, TTY console, run control).
//!
//! `--headless` runs the core without a window for CI and BIOS bring-up:
//!   psx-app --headless --cycles 30000000 [--bios assets/SCPH-1000.bin]

mod audio;
mod config;
mod ui;

use psx_core::PsxSystem;

struct Args {
    bios: Option<String>,
    disc: Option<String>,
    headless: bool,
    cycles: u64,
    dump_vram: Option<String>,
    dump_wav: Option<String>,
    peek: Option<u32>,
    /// Headless: tap START every 2 seconds to get past title screens.
    mash_start: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        bios: None,
        disc: None,
        headless: false,
        cycles: 30_000_000,
        dump_vram: None,
        dump_wav: None,
        peek: None,
        mash_start: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => args.headless = true,
            "--mash-start" => args.mash_start = true,
            "--bios" => args.bios = Some(it.next().expect("--bios needs a path")),
            "--disc" => args.disc = Some(it.next().expect("--disc needs a path")),
            "--cycles" => {
                args.cycles = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--cycles needs a number")
            }
            "--dump-vram" => args.dump_vram = Some(it.next().expect("--dump-vram needs a path")),
            "--dump-wav" => args.dump_wav = Some(it.next().expect("--dump-wav needs a path")),
            "--peek" => {
                args.peek = Some(
                    u32::from_str_radix(
                        it.next().expect("--peek needs a hex address").trim_start_matches("0x"),
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args();
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
    if let Some(path) = &args.disc {
        sys.insert_disc(load_disc(path));
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

    if args.headless {
        run_headless(
            sys,
            args.cycles,
            args.dump_vram.as_deref(),
            args.dump_wav.as_deref(),
            args.peek,
            args.mash_start,
        );
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
        Box::new(move |_cc| Ok(Box::new(ui::App::new(sys, bios, cfg, cfg_path, memcard_path)))),
    )
}

/// Load a disc image: either a raw .bin, or a .cue referencing one .bin.
fn load_disc(path: &str) -> psx_core::cdrom::Disc {
    let p = std::path::Path::new(path);
    let bin_path = if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
        let cue = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("failed to read cue '{path}': {e}"));
        // Minimal parse: take the first FILE "..." line
        let name = cue
            .lines()
            .find_map(|l| {
                let l = l.trim();
                l.strip_prefix("FILE ")?.split('"').nth(1).map(String::from)
            })
            .unwrap_or_else(|| panic!("no FILE entry in cue '{path}'"));
        if cue.matches("FILE ").count() > 1 {
            tracing::warn!("multi-file cue sheets not supported; using first file only");
        }
        p.parent().unwrap_or(std::path::Path::new(".")).join(name)
    } else {
        p.to_path_buf()
    };
    let data = std::fs::read(&bin_path)
        .unwrap_or_else(|e| panic!("failed to read disc image '{}': {e}", bin_path.display()));
    psx_core::cdrom::Disc::new(data).expect("invalid disc image")
}

fn run_headless(
    mut sys: PsxSystem,
    cycles: u64,
    dump_vram: Option<&str>,
    dump_wav: Option<&str>,
    peek: Option<u32>,
    mash_start: bool,
) {
    tracing::info!("running headless for {cycles} cycles");
    // Chunked run so we can inject input and collect audio along the way
    const CHUNK: u64 = psx_core::CPU_CLOCK_HZ / 10;
    let mut wav_samples: Vec<i16> = Vec::new();
    let mut done = 0u64;
    while done < cycles {
        if mash_start {
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
    if let Some(path) = dump_wav {
        write_wav(path, &wav_samples);
        tracing::info!(
            "wrote {} ({:.1}s of audio)",
            path,
            wav_samples.len() as f64 / 2.0 / 44_100.0
        );
    }
    tracing::info!(
        "done: pc={:#010x} cycles={} sr={:#010x} cause={:#010x} i_stat={:#06x} i_mask={:#06x}",
        sys.cpu.pc,
        sys.cycles(),
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
        println!("{:#010x}: {w:08x}{}", ofs, if ofs == pc { "  <- pc" } else { "" });
    }
    // Dump the kernel event table (EvCB pointer at 0x120): one line per
    // entry as [index] class spec status — resolves TestEvent handles.
    let ram = &sys.bus.ram;
    let word = |a: usize| u32::from_le_bytes(ram[a & 0x1f_fffc..(a & 0x1f_fffc) + 4].try_into().unwrap());
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
    if let Some(addr) = peek {
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
