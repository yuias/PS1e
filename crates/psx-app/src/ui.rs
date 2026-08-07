//! egui debug shell: a thin client over the emulator worker thread.
//!
//! All emulation (and audio) lives in [`crate::emu`]; this module only sends
//! commands, reads published snapshots and draws. Keeping it presentation-only
//! is deliberate — a wasm frontend can reuse the same snapshot types.

use crate::config::Config;
use crate::emu::{Command, DebuggerState, Emu, FrameSnapshot};
use eframe::egui;
use psx_core::sio::button;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

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
    emu: Emu,
    show_vram: bool,
    vram_as_24bit: bool,
    display_tex: Option<egui::TextureHandle>,
    vram_tex: Option<egui::TextureHandle>,
    /// Vblank count of the frame currently uploaded to `display_tex`.
    shown_frame: u64,
    gpu_log: bool,
    /// Master volume applied on top of the SPU output (0..=1).
    volume: f32,
    config: Config,
    config_path: Option<PathBuf>,
}

impl App {
    pub fn new(emu: Emu, config: Config, config_path: Option<PathBuf>, log_gpu: bool) -> Self {
        let volume = config.volume.clamp(0.0, 1.0);
        Self {
            emu,
            show_vram: false,
            vram_as_24bit: false,
            display_tex: None,
            vram_tex: None,
            shown_frame: 0,
            gpu_log: log_gpu,
            volume,
            config,
            config_path,
        }
    }
}

impl Drop for App {
    /// Persist settings changed from the UI. (The worker flushes the memory
    /// card itself when it stops.)
    fn drop(&mut self) {
        if let Some(path) = &self.config_path
            && (self.config.volume - self.volume).abs() > f32::EPSILON
        {
            self.config.volume = self.volume;
            self.config.save(path);
        }
    }
}

/// Convert a frame snapshot (15-bit or packed RGB888 rows) to an egui image.
fn frame_image(frame: &FrameSnapshot) -> egui::ColorImage {
    let (w, h, stride) = (
        frame.width as usize,
        frame.height as usize,
        frame.stride as usize,
    );
    if frame.pixels.len() < stride * h {
        return egui::ColorImage::default(); // no frame captured yet
    }
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = &frame.pixels[y * stride..(y + 1) * stride];
        for x in 0..w {
            pixels.push(if frame.is_24bit {
                let byte = x * 3;
                let read = |b: usize| (row[(byte + b) / 2] >> (((byte + b) & 1) * 8)) as u8;
                egui::Color32::from_rgb(read(0), read(1), read(2))
            } else {
                let px = row[x];
                let e = |c: u16| ((c << 3) | (c >> 2)) as u8;
                egui::Color32::from_rgb(e(px & 0x1f), e((px >> 5) & 0x1f), e((px >> 10) & 0x1f))
            });
        }
    }
    egui::ColorImage {
        size: [w, h],
        source_size: egui::Vec2::new(w as f32, h as f32),
        pixels,
    }
}

/// Convert a VRAM snapshot to an egui image, either as 15-bit pixels or
/// reinterpreted as packed 24-bit RGB (682 px/row).
fn vram_image(vram: &[u16], as_24bit: bool) -> egui::ColorImage {
    let (w, h) = if as_24bit {
        (682usize, 512usize)
    } else {
        (1024, 512)
    };
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = y * 1024;
        for x in 0..w {
            pixels.push(if as_24bit {
                let byte = x * 3;
                let read = |b: usize| (vram[row + (byte + b) / 2] >> (((byte + b) & 1) * 8)) as u8;
                egui::Color32::from_rgb(read(0), read(1), read(2))
            } else {
                let px = vram[row + x];
                // Expand 5-bit channels, replicating the top bits
                let e = |c: u16| ((c << 3) | (c >> 2)) as u8;
                egui::Color32::from_rgb(e(px & 0x1f), e((px >> 5) & 0x1f), e((px >> 10) & 0x1f))
            });
        }
    }
    egui::ColorImage {
        size: [w, h],
        source_size: egui::Vec2::new(w as f32, h as f32),
        pixels,
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
        self.emu.shared.buttons.store(buttons, Ordering::Relaxed);
        self.emu
            .shared
            .volume
            .store(self.volume.to_bits(), Ordering::Relaxed);

        let status = self.emu.shared.status.lock().unwrap().clone();
        let debugger_active = matches!(
            status.debugger,
            DebuggerState::Running | DebuggerState::Halted
        ) || status.debugger == DebuggerState::Waiting;

        // Save-state hotkeys; gating (debugger owns loads) is in the worker
        let (f5, f9) = ctx.input(|i| (i.key_pressed(egui::Key::F5), i.key_pressed(egui::Key::F9)));
        if f5 {
            self.emu.send(Command::SaveState);
        }
        if f9 {
            self.emu.send(Command::LoadState);
        }

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // The debugger owns run control while attached
                ui.add_enabled_ui(!debugger_active, |ui| {
                    let label = if status.running {
                        "⏸ Pause"
                    } else {
                        "▶ Run"
                    };
                    if ui.button(label).clicked() {
                        self.emu.send(Command::SetRunning(!status.running));
                    }
                    if ui.button("Step").clicked() {
                        self.emu.send(Command::Step);
                    }
                    if ui.button("Reset").clicked() {
                        self.emu.send(Command::Reset);
                    }
                    ui.separator();
                    if ui.button("💾 Save (F5)").clicked() {
                        self.emu.send(Command::SaveState);
                    }
                    if ui.button("📂 Load (F9)").clicked() {
                        self.emu.send(Command::LoadState);
                    }
                });
                if status.debugger != DebuggerState::None {
                    ui.separator();
                    ui.label(match status.debugger {
                        DebuggerState::Halted => "🔌 debugger: halted",
                        DebuggerState::Running => "🔌 debugger: running",
                        DebuggerState::Waiting => "🔌 waiting for debugger",
                        _ => "🔌 debugger: listening",
                    });
                }
                ui.separator();
                ui.checkbox(&mut self.show_vram, "VRAM viewer");
                if ui
                    .checkbox(&mut self.gpu_log, "GPU cmd log")
                    .on_hover_text("decode every GP0/GP1 command to the log (debug level)")
                    .changed()
                {
                    self.emu.send(Command::SetGpuLog(self.gpu_log));
                }
                ui.separator();
                ui.label("🔊");
                ui.add(
                    egui::Slider::new(&mut self.volume, 0.0..=1.0)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.separator();
                ui.monospace(format!(
                    "pc {:#010x}   cycles {}   🔉{:3}ms{}",
                    status.pc,
                    status.cycles,
                    status.audio_buffered * 1000 / 44_100,
                    if status.audio_underruns > 0 {
                        format!("   underruns {}", status.audio_underruns)
                    } else {
                        String::new()
                    }
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
                        ui.monospace(format!("{:08x}", status.regs[i]));
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                    ui.monospace("  hi");
                    ui.monospace(format!("{:08x}", status.hi));
                    ui.monospace("  lo");
                    ui.monospace(format!("{:08x}", status.lo));
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
                        let tty = self.emu.shared.tty.lock().unwrap().clone();
                        ui.add(
                            egui::TextEdit::multiline(&mut tty.as_str())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let (enabled, image) = {
                let frame = self.emu.shared.frame.lock().unwrap();
                // Convert only when the worker published a new frame
                let image = if frame.count != self.shown_frame || self.display_tex.is_none() {
                    self.shown_frame = frame.count;
                    Some(frame_image(&frame))
                } else {
                    None
                };
                (frame.enabled, image)
            };
            let has_frame = image
                .as_ref()
                .map(|i| i.size[0] > 0 && i.size[1] > 0)
                .unwrap_or(self.display_tex.is_some());
            if !(enabled && has_frame) {
                ui.centered_and_justified(|ui| ui.label("display disabled"));
                return;
            }
            let tex = match (&mut self.display_tex, image) {
                (Some(t), Some(image)) => {
                    t.set(image, egui::TextureOptions::NEAREST);
                    t.clone()
                }
                (Some(t), None) => t.clone(),
                (None, Some(image)) => {
                    // Zero-sized textures are a wgpu validation error; the
                    // has_frame check above already excluded them
                    let t = ui
                        .ctx()
                        .load_texture("display", image, egui::TextureOptions::NEAREST);
                    self.display_tex = Some(t.clone());
                    t
                }
                (None, None) => unreachable!(),
            };
            // Fit the panel while keeping a 4:3 presentation aspect
            let avail = ui.available_size();
            let scale = (avail.x / 4.0).min(avail.y / 3.0);
            let size = egui::Vec2::new(scale * 4.0, scale * 3.0);
            ui.centered_and_justified(|ui| {
                // maintain_aspect_ratio(false): the framebuffer's pixel aspect
                // (e.g. 320x480 interlace, 512x240) rarely matches the 4:3
                // output; egui would otherwise letterbox to the texture aspect.
                ui.add(
                    egui::Image::new(&tex)
                        .fit_to_exact_size(size)
                        .maintain_aspect_ratio(false),
                );
            });
        });

        self.emu
            .shared
            .vram_requested
            .store(self.show_vram, Ordering::Relaxed);
        if self.show_vram {
            let vram = self.emu.shared.vram.lock().unwrap();
            if vram.len() == 1024 * 512 {
                let image = vram_image(&vram, self.vram_as_24bit);
                drop(vram);
                let tex = match &mut self.vram_tex {
                    Some(t) => {
                        t.set(image, egui::TextureOptions::NEAREST);
                        t.clone()
                    }
                    None => {
                        let t = ctx.load_texture("vram", image, egui::TextureOptions::NEAREST);
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
}
