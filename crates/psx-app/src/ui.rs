//! egui debug shell: a thin client over the emulator worker thread.
//!
//! All emulation (and audio) lives in [`crate::emu`]; this module only sends
//! commands, reads published snapshots and draws. Keeping it presentation-only
//! is deliberate — a wasm frontend can reuse the same snapshot types.

use crate::config;
use crate::config::Config;
use crate::disc;
use crate::emu::{Command, DebuggerState, Emu, FrameSnapshot, Status};
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

/// Pad button names, index-aligned with [`config::KeyBindings::pairs`], for
/// listing the configured bindings in the Help menu.
const BUTTON_NAMES: [&str; 14] = [
    "up", "down", "left", "right", "cross", "circle", "square", "triangle", "L1", "R1", "L2", "R2",
    "start", "select",
];

pub struct App {
    emu: Emu,
    show_vram: bool,
    vram_as_24bit: bool,
    show_regs: bool,
    show_tty: bool,
    fullscreen: bool,
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
    /// Path of the most recent screenshot, shown in the status bar.
    last_screenshot: Option<String>,
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
            show_regs: false,
            show_tty: false,
            fullscreen: false,
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
            last_screenshot: None,
        }
    }

    /// Swap the disc the way the console does it: lid open, pick, lid shut.
    /// The emulator keeps running throughout — the picker is up for exactly
    /// as long as the drive is open, which is the window a game watches for.
    /// Cancelling or picking an unreadable image just shuts the lid again on
    /// the disc that was already in there; the failure stays visible in the
    /// status bar until the next pick succeeds.
    fn open_disc(&mut self) {
        self.emu.send(Command::OpenShell);
        let picked = rfd::FileDialog::new()
            .add_filter("PlayStation disc image", &["cue", "bin", "img"])
            .pick_file();
        let disc = picked.and_then(|path| match disc::load_disc(&path) {
            Ok(d) => {
                self.disc_error = None;
                Some(d)
            }
            Err(e) => {
                tracing::error!("{e}");
                self.disc_error = Some(e);
                None
            }
        });
        self.emu.send(Command::CloseShell(disc));
    }

    /// Dump the currently displayed frame to a timestamped BMP in the working
    /// directory, mirroring the headless `--dump-frame` writer.
    fn take_screenshot(&mut self) {
        let frame = self.emu.shared.frame.lock().unwrap();
        if frame.width == 0 || frame.height == 0 {
            tracing::warn!("no frame to screenshot yet");
            return;
        }
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = format!("screenshot_{epoch}.bmp");
        let written = crate::write_frame_bmp(
            &path,
            frame.width,
            frame.height,
            frame.stride,
            frame.is_24bit,
            &frame.pixels,
        );
        drop(frame);
        match written {
            Ok(()) => {
                tracing::info!("screenshot written to {path}");
                self.last_screenshot = Some(path);
            }
            Err(e) => tracing::error!("screenshot failed: {e}"),
        }
    }

    /// Menu bar: every command the shell offers, grouped by what it acts on.
    fn menu_bar(&mut self, ctx: &egui::Context, running: bool, debugger_active: bool) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Emulation", |ui| {
                    // The debugger owns run control while attached.
                    ui.add_enabled_ui(!debugger_active, |ui| {
                        let label = if running { "Pause" } else { "Run" };
                        if ui.button(label).clicked() {
                            self.emu.send(Command::SetRunning(!running));
                            ui.close();
                        }
                        if ui.button("Step").clicked() {
                            self.emu.send(Command::Step);
                            ui.close();
                        }
                        if ui
                            .button("Hardware reset")
                            .on_hover_text(
                                "power-cycle the console; the disc and memory card stay in",
                            )
                            .clicked()
                        {
                            self.emu.send(Command::Reset);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Insert disc...")
                            .on_hover_text(
                                "opens the drive and closes it on the new image; swapping mid-game works, no reset needed",
                            )
                            .clicked()
                        {
                            self.open_disc();
                            ui.close();
                        }
                        ui.separator();
                        let save_key = self.config.hotkeys.save_state.clone();
                        if ui.button(format!("Save state\t{save_key}")).clicked() {
                            self.emu.send(Command::SaveState);
                            ui.close();
                        }
                        let load_key = self.config.hotkeys.load_state.clone();
                        if ui.button(format!("Load state\t{load_key}")).clicked() {
                            self.emu.send(Command::LoadState);
                            ui.close();
                        }
                    });
                    ui.separator();
                    if ui.button("Screenshot\tF12").clicked() {
                        self.take_screenshot();
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Fullscreen\tF11").clicked() {
                        self.fullscreen = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                        ui.close();
                    }
                    ui.separator();
                    ui.checkbox(&mut self.show_regs, "Registers panel");
                    ui.checkbox(&mut self.show_tty, "TTY panel");
                    ui.checkbox(&mut self.show_vram, "VRAM viewer");
                    ui.separator();
                    if ui
                        .checkbox(&mut self.gpu_log, "GPU cmd log")
                        .on_hover_text("decode every GP0/GP1 command to the log (debug level)")
                        .changed()
                    {
                        self.emu.send(Command::SetGpuLog(self.gpu_log));
                    }
                });
                ui.menu_button("Audio", |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.volume, 0.0..=1.0)
                            .text("volume")
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    );
                });
                ui.menu_button("Help", |ui| {
                    ui.label("Pad, as bound in the config file:");
                    for (name, (key, _)) in BUTTON_NAMES.iter().zip(self.config.keys.pairs()) {
                        ui.monospace(format!("{name:>8} = {key}"));
                    }
                    ui.separator();
                    ui.monospace(format!("    save = {}", self.config.hotkeys.save_state));
                    ui.monospace(format!("    load = {}", self.config.hotkeys.load_state));
                    ui.separator();
                    ui.label("F11 fullscreen (Esc leaves), F12 screenshot.");
                });
            });
        });
    }

    /// Status bar: what the emulator is doing right now, plus the last
    /// one-shot result worth reporting.
    fn status_bar(&self, ctx: &egui::Context, status: &Status) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let state = match status.debugger {
                    DebuggerState::Halted => "debugger: halted",
                    DebuggerState::Running => "debugger: running",
                    DebuggerState::Waiting => "waiting for debugger",
                    DebuggerState::None if status.running => "running",
                    DebuggerState::None => "paused",
                    _ => "debugger: listening",
                };
                ui.monospace(state);
                ui.separator();
                ui.monospace(format!(
                    "pc {:#010x}   cycles {}   audio {:3} ms{}",
                    status.pc,
                    status.cycles,
                    status.audio_buffered * 1000 / 44_100,
                    if status.audio_underruns > 0 {
                        format!("   underruns {}", status.audio_underruns)
                    } else {
                        String::new()
                    }
                ));
                if let Some(path) = &self.last_screenshot {
                    ui.separator();
                    ui.monospace(format!("saved {path}"));
                }
                if let Some(err) = &self.disc_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
            });
        });
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
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            self.take_screenshot();
        }

        // F11 toggles fullscreen; the chrome (menu, status bar, panels) hides
        // while fullscreen so only the display shows.
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        let chrome = !self.fullscreen;

        if chrome {
            self.menu_bar(ctx, status.running, debugger_active);
            self.status_bar(ctx, &status);
        }

        if chrome && self.show_regs {
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
        }

        if chrome && self.show_tty {
            egui::TopBottomPanel::bottom("tty")
                .resizable(true)
                .default_height(160.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("TTY");
                        if ui.button("Clear").clicked() {
                            self.emu.shared.tty.lock().unwrap().clear();
                        }
                    });
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
        }

        let central = if self.fullscreen {
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
        } else {
            egui::CentralPanel::default()
        };
        central.show(ctx, |ui| {
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

        // Fullscreen shows the display alone, so stop paying for VRAM copies.
        let want_vram = chrome && self.show_vram;
        self.emu
            .shared
            .vram_requested
            .store(want_vram, Ordering::Relaxed);
        if want_vram {
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
                    .open(&mut self.show_vram)
                    .show(ctx, |ui| {
                        ui.checkbox(&mut self.vram_as_24bit, "interpret as 24-bit RGB");
                        ui.add(egui::Image::new(&tex));
                    });
            }
        }
    }
}
