//! egui debug shell: a thin client over the emulator worker thread.
//!
//! All emulation (and audio) lives in [`crate::emu`]; this module only sends
//! commands, reads published snapshots and draws. Keeping it presentation-only
//! is deliberate — a wasm frontend can reuse the same snapshot types.

use crate::config;
use crate::config::Config;
use crate::disc;
use crate::emu::{Command, DebuggerState, Emu, FrameSnapshot};
use crate::gamepad::Gamepad;
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

/// Resolve configured key names to egui keys, paired with the pad bit each
/// one drives. An unrecognized name falls back to the built-in default.
fn resolve_keymap(keys: &config::KeyBindings) -> Vec<(egui::Key, u16)> {
    let fallback = config::KeyBindings::default();
    keys.pairs()
        .into_iter()
        .zip(fallback.pairs())
        .filter_map(
            |((name, bit), (default_name, _))| match egui::Key::from_name(name) {
                Some(key) => Some((key, bit)),
                None => {
                    tracing::warn!("unknown key name '{name}'; using '{default_name}'");
                    egui::Key::from_name(default_name).map(|key| (key, bit))
                }
            },
        )
        .collect()
}

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
    /// Key -> pad bit, resolved from the config once at startup.
    keymap: Vec<(egui::Key, u16)>,
    hotkey_save: Option<egui::Key>,
    hotkey_load: Option<egui::Key>,
    /// Absent when no gamepad backend is available.
    gamepad: Option<Gamepad>,
    /// Last failed disc pick, shown until the next one succeeds.
    disc_error: Option<String>,
}

impl App {
    pub fn new(emu: Emu, config: Config, config_path: Option<PathBuf>, log_gpu: bool) -> Self {
        let volume = config.volume.clamp(0.0, 1.0);
        let keymap = resolve_keymap(&config.keys);
        let gamepad = Gamepad::new(&config.pad);
        let hotkey_save = egui::Key::from_name(&config.hotkeys.save_state);
        let hotkey_load = egui::Key::from_name(&config.hotkeys.load_state);
        for (name, key) in [
            (&config.hotkeys.save_state, hotkey_save),
            (&config.hotkeys.load_state, hotkey_load),
        ] {
            if key.is_none() {
                tracing::warn!("unknown hotkey name '{name}'; that hotkey is disabled");
            }
        }
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
            keymap,
            hotkey_save,
            hotkey_load,
            gamepad,
            disc_error: None,
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
            self.keymap
                .iter()
                .filter(|(k, _)| i.key_down(*k))
                .fold(0u16, |acc, (_, b)| acc | b)
        });
        let buttons = buttons | self.gamepad.as_mut().map_or(0, Gamepad::poll);
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
        let (save, load) = ctx.input(|i| {
            let pressed = |k: Option<egui::Key>| k.is_some_and(|k| i.key_pressed(k));
            (pressed(self.hotkey_save), pressed(self.hotkey_load))
        });
        if save {
            self.emu.send(Command::SaveState);
        }
        if load {
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
                    if ui
                        .button("💿 Open disc…")
                        .on_hover_text(
                            "load a disc image and reset; mid-game disc swaps are not modeled",
                        )
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("PlayStation disc image", &["cue", "bin", "img"])
                            .pick_file()
                    {
                        match disc::load_disc(&path) {
                            Ok(d) => {
                                self.disc_error = None;
                                self.emu.send(Command::InsertDisc(d));
                                self.emu.send(Command::Reset);
                            }
                            Err(e) => {
                                tracing::error!("{e}");
                                self.disc_error = Some(e);
                            }
                        }
                    }
                    ui.separator();
                    let save_key = &self.config.hotkeys.save_state;
                    if ui.button(format!("💾 Save ({save_key})")).clicked() {
                        self.emu.send(Command::SaveState);
                    }
                    let load_key = &self.config.hotkeys.load_state;
                    if ui.button(format!("📂 Load ({load_key})")).clicked() {
                        self.emu.send(Command::LoadState);
                    }
                });
                if let Some(err) = &self.disc_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
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
