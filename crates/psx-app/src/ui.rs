//! egui debug shell: run control, CPU registers, TTY console.

use crate::audio::Audio;
use crate::config::Config;
use eframe::egui;
use std::path::PathBuf;
use psx_core::sio::button;
use psx_core::{CPU_CLOCK_HZ, PsxSystem};

/// Keyboard -> digital pad mapping.
const KEYMAP: [(egui::Key, u16); 14] = [
    (egui::Key::ArrowUp, button::UP),
    (egui::Key::ArrowDown, button::DOWN),
    (egui::Key::ArrowLeft, button::LEFT),
    (egui::Key::ArrowRight, button::RIGHT),
    (egui::Key::X, button::CROSS),
    (egui::Key::C, button::CIRCLE),
    (egui::Key::S, button::SQUARE),
    (egui::Key::D, button::TRIANGLE),
    (egui::Key::Q, button::L1),
    (egui::Key::E, button::R1),
    (egui::Key::Num1, button::L2),
    (egui::Key::Num3, button::R2),
    (egui::Key::Enter, button::START),
    (egui::Key::Backspace, button::SELECT),
];

const REG_NAMES: [&str; 32] = [
    "zero", "at", "v0", "v1", "a0", "a1", "a2", "a3", //
    "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", //
    "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", //
    "t8", "t9", "k0", "k1", "gp", "sp", "fp", "ra",
];

pub struct App {
    sys: PsxSystem,
    /// Kept for Reset.
    bios: Vec<u8>,
    running: bool,
    show_vram: bool,
    vram_as_24bit: bool,
    display_tex: Option<egui::TextureHandle>,
    vram_tex: Option<egui::TextureHandle>,
    audio: Option<Audio>,
    sample_scratch: Vec<i16>,
    /// Master volume applied on top of the SPU output (0..=1).
    volume: f32,
    config: Config,
    config_path: Option<PathBuf>,
    memcard_path: PathBuf,
}

impl App {
    pub fn new(
        sys: PsxSystem,
        bios: Vec<u8>,
        config: Config,
        config_path: Option<PathBuf>,
        memcard_path: PathBuf,
    ) -> Self {
        Self {
            sys,
            bios,
            running: false,
            show_vram: false,
            vram_as_24bit: false,
            display_tex: None,
            vram_tex: None,
            audio: Audio::new(),
            sample_scratch: Vec::new(),
            volume: config.volume.clamp(0.0, 1.0),
            config,
            config_path,
            memcard_path,
        }
    }

    fn save_memcard_if_dirty(&mut self) {
        if self.sys.bus.sio.memcard.take_dirty() {
            if let Err(e) = std::fs::write(&self.memcard_path, &self.sys.bus.sio.memcard.data) {
                tracing::error!("failed to save memory card: {e}");
            } else {
                tracing::info!("memory card saved");
            }
        }
    }
}

impl Drop for App {
    /// Persist settings changed from the UI and any unsaved card writes.
    fn drop(&mut self) {
        self.save_memcard_if_dirty();
        if let Some(path) = &self.config_path {
            if (self.config.volume - self.volume).abs() > f32::EPSILON {
                self.config.volume = self.volume;
                self.config.save(path);
            }
        }
    }
}

impl App {
    /// The display area as an egui image, honoring the 24-bit display mode
    /// (packed RGB888 in VRAM, e.g. FMV frames).
    fn display_image(&self) -> egui::ColorImage {
        let gpu = &self.sys.bus.gpu;
        let (w, h) = gpu.display_resolution();
        let (sx, sy) = gpu.display_vram_start();
        if !gpu.is_24bit() {
            return self.vram_image(sx, sy, w, h);
        }
        let vram = &self.sys.bus.gpu.vram;
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            let row = (((sy + y) & 0x1ff) as usize) * 1024;
            for x in 0..w {
                // Pixel x starts at byte offset sx*2 + x*3 within the row
                let byte = sx as usize * 2 + x as usize * 3;
                let read = |b: usize| {
                    let half = vram[row + ((byte + b) / 2) % 1024];
                    (half >> (((byte + b) & 1) * 8)) as u8
                };
                pixels.push(egui::Color32::from_rgb(read(0), read(1), read(2)));
            }
        }
        egui::ColorImage {
            size: [w as usize, h as usize],
            source_size: egui::Vec2::new(w as f32, h as f32),
            pixels,
        }
    }

    /// Convert a VRAM rectangle (15-bit pixels) into an egui image.
    fn vram_image(&self, x0: u32, y0: u32, w: u32, h: u32) -> egui::ColorImage {
        let vram = &self.sys.bus.gpu.vram;
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            let row = (((y0 + y) & 0x1ff) as usize) * 1024;
            for x in 0..w {
                let px = vram[row + (((x0 + x) & 0x3ff) as usize)];
                // Expand 5-bit channels, replicating the top bits
                let e = |c: u16| ((c << 3) | (c >> 2)) as u8;
                pixels.push(egui::Color32::from_rgb(
                    e(px & 0x1f),
                    e((px >> 5) & 0x1f),
                    e((px >> 10) & 0x1f),
                ));
            }
        }
        egui::ColorImage {
            size: [w as usize, h as usize],
            source_size: egui::Vec2::new(w as f32, h as f32),
            pixels,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let buttons = ctx.input(|i| {
            KEYMAP
                .iter()
                .filter(|(k, _)| i.key_down(*k))
                .fold(0u16, |acc, (_, b)| acc | b)
        });
        self.sys.set_buttons(buttons);

        if self.running {
            // Pace by wall clock, not repaint rate (high-refresh monitors
            // would otherwise fast-forward the game). Nudge by the audio
            // buffer level to counteract clock drift.
            let dt = ctx.input(|i| i.stable_dt).clamp(0.001, 0.05) as f64;
            let mut cycles = (dt * CPU_CLOCK_HZ as f64) as u64;
            if let Some(audio) = &self.audio {
                let buffered = audio.buffered_frames();
                if buffered < 2205 {
                    cycles += cycles / 10; // <50ms buffered: catch up
                } else if buffered > 8820 {
                    cycles -= cycles / 10; // >200ms: back off
                }
            }
            self.sys.run_cycles(cycles);
            self.sample_scratch.clear();
            self.sys.bus.spu.drain_output(&mut self.sample_scratch);
            for s in &mut self.sample_scratch {
                *s = (*s as f32 * self.volume) as i16;
            }
            if let Some(audio) = &self.audio {
                audio.push_samples(&self.sample_scratch);
            }
            self.save_memcard_if_dirty();
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let label = if self.running { "⏸ Pause" } else { "▶ Run" };
                if ui.button(label).clicked() {
                    self.running = !self.running;
                }
                if ui.button("Step").clicked() {
                    self.running = false;
                    self.sys.step();
                }
                if ui.button("Reset").clicked() {
                    self.running = false;
                    self.sys = PsxSystem::new(self.bios.clone()).expect("reset failed");
                }
                ui.separator();
                ui.checkbox(&mut self.show_vram, "VRAM viewer");
                ui.separator();
                ui.label("🔊");
                ui.add(
                    egui::Slider::new(&mut self.volume, 0.0..=1.0)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.separator();
                ui.monospace(format!(
                    "pc {:#010x}   cycles {}",
                    self.sys.cpu.pc,
                    self.sys.cycles()
                ));
            });
        });

        egui::SidePanel::right("registers")
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("CPU");
                egui::Grid::new("regs").striped(true).show(ui, |ui| {
                    for (i, name) in REG_NAMES.iter().enumerate() {
                        ui.monospace(format!("{name:>4}"));
                        ui.monospace(format!("{:08x}", self.sys.cpu.regs[i]));
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                    ui.monospace("  hi");
                    ui.monospace(format!("{:08x}", self.sys.cpu.hi));
                    ui.monospace("  lo");
                    ui.monospace(format!("{:08x}", self.sys.cpu.lo));
                    ui.end_row();
                });
            });

        egui::TopBottomPanel::bottom("tty")
            .resizable(true)
            .default_height(160.0)
            .show(ctx, |ui| {
                ui.heading("TTY");
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.sys.tty_output().to_string())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let enabled = self.sys.bus.gpu.display_enabled();
            let image = self.display_image();
            let tex = match &mut self.display_tex {
                Some(t) => {
                    t.set(image, egui::TextureOptions::NEAREST);
                    t.clone()
                }
                None => {
                    let t = ui.ctx().load_texture(
                        "display",
                        image,
                        egui::TextureOptions::NEAREST,
                    );
                    self.display_tex = Some(t.clone());
                    t
                }
            };
            if enabled {
                // Fit the panel while keeping a 4:3 presentation aspect
                let avail = ui.available_size();
                let scale = (avail.x / 4.0).min(avail.y / 3.0);
                let size = egui::Vec2::new(scale * 4.0, scale * 3.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::new(&tex).fit_to_exact_size(size));
                });
            } else {
                ui.centered_and_justified(|ui| ui.label("display disabled"));
            }
        });

        if self.show_vram {
            let image = if self.vram_as_24bit {
                // Whole VRAM reinterpreted as packed RGB888 (682 px/row)
                let vram = &self.sys.bus.gpu.vram;
                let (w, h) = (682usize, 512usize);
                let mut pixels = Vec::with_capacity(w * h);
                for y in 0..h {
                    let row = y * 1024;
                    for x in 0..w {
                        let byte = x * 3;
                        let read = |b: usize| {
                            let half = vram[row + (byte + b) / 2];
                            (half >> (((byte + b) & 1) * 8)) as u8
                        };
                        pixels.push(egui::Color32::from_rgb(read(0), read(1), read(2)));
                    }
                }
                egui::ColorImage {
                    size: [w, h],
                    source_size: egui::Vec2::new(w as f32, h as f32),
                    pixels,
                }
            } else {
                self.vram_image(0, 0, 1024, 512)
            };
            let tex = match &mut self.vram_tex {
                Some(t) => {
                    t.set(image, egui::TextureOptions::NEAREST);
                    t.clone()
                }
                None => {
                    let t =
                        ctx.load_texture("vram", image, egui::TextureOptions::NEAREST);
                    self.vram_tex = Some(t.clone());
                    t
                }
            };
            egui::Window::new("VRAM (1024x512)")
                .default_width(1024.0)
                .show(ctx, |ui| {
                    ui.checkbox(&mut self.vram_as_24bit, "interpret as 24-bit RGB");
                    ui.add(egui::Image::new(&tex));
                });
        }
    }
}
