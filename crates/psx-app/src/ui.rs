//! egui debug shell: a thin client over the emulator worker thread.
//!
//! All emulation (and audio) lives in [`crate::emu`]; this module only sends
//! commands, reads published snapshots and draws. Keeping it presentation-only
//! is deliberate — a wasm frontend can reuse the same snapshot types.

use crate::config;
use crate::config::Config;
use crate::disc;
use crate::disc::DiscInfo;
use crate::emu;
use crate::emu::{Command, DebuggerState, Emu, FrameSnapshot, Status};
use crate::gamepad::Gamepad;
use crate::scan;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

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

/// One tab of the side pane. Adding a page means an arm in each of
/// [`Page::ALL`], [`Page::label`], [`Page::panels`] and the dispatch in
/// `update`; the pane itself needs no other bookkeeping.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Page {
    #[default]
    Settings,
    Cheats,
    Memory,
    Registers,
}

impl Page {
    const ALL: [Page; 4] = [Page::Settings, Page::Cheats, Page::Memory, Page::Registers];

    fn label(self) -> &'static str {
        match self {
            Page::Settings => "Settings",
            Page::Cheats => "Cheats",
            Page::Memory => "Memory",
            Page::Registers => "Registers",
        }
    }

    /// What the worker has to publish for this page to have anything to
    /// show. A page not on screen asks for nothing.
    fn panels(self) -> u8 {
        match self {
            // The cheat list is held by the UI, so the worker owes it
            // nothing.
            Page::Settings | Page::Cheats => 0,
            Page::Memory => emu::PANEL_MEMORY,
            Page::Registers => emu::PANEL_REGS,
        }
    }
}

/// Display heights the View menu can size the window to, in physical
/// pixels. Every PS1 mode is presented 4:3, so the height is the whole
/// choice.
const DISPLAY_HEIGHTS: [u32; 3] = [480, 720, 1080];

/// RAM shows as KSEG0 addresses: what the BIOS, gdb and cheat codes all
/// use, and what makes an address recognizable at a glance.
const KSEG0: u32 = 0x8000_0000;

/// VRAM is a fixed 1024x512 grid of 16-bit words.
const VRAM_LEN: usize = 1024 * 512;

pub struct App {
    emu: Emu,
    show_vram: bool,
    vram_as_24bit: bool,
    /// Side pane visibility, the page it is showing, and the width it was
    /// last dragged to. egui's own persistence is not compiled in, so the
    /// width lives in `Config`.
    show_pane: bool,
    page: Page,
    pane_width: f32,
    show_tty: bool,
    fullscreen: bool,
    display_tex: Option<egui::TextureHandle>,
    vram_tex: Option<egui::TextureHandle>,
    /// Vblank count of the frame currently uploaded to `display_tex`.
    shown_frame: u64,
    /// Vblank count and colour interpretation behind `vram_tex`, so the
    /// 1 MiB expansion runs on a new copy rather than on every repaint.
    shown_vram: Option<(u64, bool)>,
    /// VRAM copied out from under the worker's mutex, reused each time.
    vram_scratch: Vec<u16>,
    /// Memory page: the address as typed, kept separate from the offset
    /// sent to the worker so a half-typed address does not move the view.
    mem_addr: String,
    /// Cheats for the disc in the drive, and the `.cht` they came from.
    /// The UI holds the list because it is what the checkboxes edit; the
    /// worker gets a copy through `Command::SetCheats`.
    cheats: psx_core::cheats::CheatList,
    cheat_file: Option<PathBuf>,
    /// Master switch, persisted in `Config`. Off by default so a `.cht`
    /// found beside an image cannot change a run on its own.
    cheats_on: bool,
    /// Scanner controls. The candidate list itself lives on the worker.
    scan_width: u8,
    scan_value: String,
    scan_started: bool,
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
    /// The disc currently in the drive, `None` when it is empty.
    disc: Option<DiscInfo>,
    /// Set whenever `disc` changes, so the next frame retitles the window.
    /// `App::new` has no `Context` yet, which is why this is not immediate.
    title_dirty: bool,
    /// Last failed disc pick, shown until the next one succeeds.
    disc_error: Option<String>,
    /// Path of the most recent screenshot, shown in the status bar.
    last_screenshot: Option<String>,
    /// Window size in egui points, sampled every non-fullscreen frame so
    /// the size at exit is the one that gets saved.
    window_size: egui::Vec2,
    /// Space the display got last frame, in points. The difference against
    /// the window is everything else on screen, which is what lets a
    /// display-size pick leave the pane and the TTY the size they are.
    central_size: egui::Vec2,
    /// Display height requested from the View menu, in physical pixels,
    /// applied on the next frame.
    resize_to: Option<u32>,
}

impl App {
    pub fn new(
        emu: Emu,
        config: Config,
        config_path: Option<PathBuf>,
        log_gpu: bool,
        disc: Option<DiscInfo>,
    ) -> Self {
        // Re-read the sidecar rather than threading the already-parsed
        // list down from main: the CLI and the picker then show the same
        // list by construction, and it is one small file at startup.
        let cheat_file = disc.as_ref().map(|d| disc::cheat_path(&d.path));
        let cheats = cheat_file
            .as_deref()
            .map(disc::load_cheats)
            .unwrap_or_default();
        let volume = config.volume.clamp(0.0, 1.0);
        let window_size = egui::vec2(config.window_width, config.window_height);
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
            show_pane: config.pane,
            page: config.page,
            pane_width: config.pane_width,
            show_tty: false,
            fullscreen: false,
            display_tex: None,
            vram_tex: None,
            shown_frame: 0,
            shown_vram: None,
            vram_scratch: Vec::new(),
            mem_addr: format!("{:08x}", KSEG0),
            cheats,
            cheat_file,
            cheats_on: config.cheats,
            scan_width: 4,
            scan_value: String::new(),
            scan_started: false,
            gpu_log: log_gpu,
            volume,
            config,
            config_path,
            keymap,
            hotkey_save,
            hotkey_load,
            gamepad,
            disc,
            title_dirty: true,
            disc_error: None,
            last_screenshot: None,
            window_size,
            central_size: egui::Vec2::ZERO,
            resize_to: None,
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
            Ok(loaded) => {
                self.disc_error = None;
                self.cheat_file = Some(disc::cheat_path(&loaded.info.path));
                self.cheats = loaded.cheats.clone();
                self.disc = Some(loaded.info.clone());
                self.title_dirty = true;
                Some(loaded)
            }
            Err(e) => {
                tracing::error!("{e}");
                self.disc_error = Some(e);
                None
            }
        });
        self.emu.send(Command::CloseShell(disc));
    }

    /// Name the window after the disc, so several instances stay tellable
    /// apart in the task switcher.
    fn apply_window_title(&mut self, ctx: &egui::Context) {
        self.title_dirty = false;
        let title = match &self.disc {
            Some(d) => format!("PS1e - {}", d.title),
            None => "PS1e".to_string(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
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

    /// Settings page: everything the shell can change while it runs, which
    /// is what used to be spread across the View and Audio menus.
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Audio");
        ui.add(
            egui::Slider::new(&mut self.volume, 0.0..=1.0)
                .text("volume")
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        );
        ui.separator();
        ui.heading("Debug");
        if ui
            .checkbox(&mut self.gpu_log, "GPU cmd log")
            .on_hover_text("decode every GP0/GP1 command to the log (debug level)")
            .changed()
        {
            self.emu.send(Command::SetGpuLog(self.gpu_log));
        }
    }

    /// Cheats page: one checkbox per cheat in the disc's `.cht`.
    /// Enable/disable only — the file stays the place codes are written.
    fn cheats_page(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.cheats_on, "Apply cheats")
            .on_hover_text(
                "off by default, so a .cht left beside an image does nothing until you say so",
            )
            .changed()
        {
            self.emu.send(Command::SetCheatsEnabled(self.cheats_on));
        }
        ui.separator();
        let Some(path) = self.cheat_file.clone() else {
            ui.label("No disc in the drive.");
            return;
        };
        if self.cheats.is_empty() {
            ui.label("No cheats for this disc.");
            ui.monospace(path.display().to_string());
            if ui.button("Reload").clicked() {
                self.reload_cheats();
            }
            return;
        }
        let mut changed = false;
        // The per-cheat boxes stay usable while the master switch is off:
        // setting up which ones you want before turning them on is the
        // normal order.
        for cheat in &mut self.cheats.cheats {
            let row = ui.checkbox(&mut cheat.enabled, &cheat.name);
            changed |= row.changed();
            if cheat.has_unsupported() {
                row.on_hover_text("contains code types this build does not apply");
            }
        }
        ui.separator();
        if ui
            .button("Reload")
            .on_hover_text(path.display().to_string())
            .clicked()
        {
            self.reload_cheats();
        }
        if changed {
            self.emu.send(Command::SetCheats(self.cheats.clone()));
            self.save_cheats();
        }
    }

    /// Re-read the `.cht` and hand the result to the worker. Used after
    /// editing the file by hand, which is how codes get added at all.
    fn reload_cheats(&mut self) {
        let Some(path) = &self.cheat_file else { return };
        self.cheats = disc::load_cheats(path);
        self.emu.send(Command::SetCheats(self.cheats.clone()));
    }

    /// The `.cht` is the only record of what is enabled, so a toggle
    /// writes it straight back.
    fn save_cheats(&mut self) {
        let Some(path) = &self.cheat_file else { return };
        if let Err(e) = std::fs::write(path, self.cheats.to_text()) {
            tracing::error!("could not save {}: {e}", path.display());
        }
    }

    /// Memory page: a window of RAM as hex and ASCII.
    fn memory_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("addr");
            let edit = egui::TextEdit::singleline(&mut self.mem_addr)
                .font(egui::TextStyle::Monospace)
                .desired_width(80.0);
            if ui.add(edit).changed()
                && let Ok(addr) = u32::from_str_radix(self.mem_addr.trim_start_matches("0x"), 16)
            {
                // Any of the three RAM mirrors is accepted; the worker
                // wants an offset.
                self.emu.shared.view_base.store(
                    addr & (psx_core::bus::RAM_SIZE as u32 - 1),
                    Ordering::Relaxed,
                );
            }
        });
        ui.separator();
        let view = self.emu.shared.memory.lock().unwrap().clone();
        if view.bytes.len() < emu::VIEW_BYTES {
            ui.label("waiting for the worker");
            return;
        }
        // One row per 16 bytes, wide enough that the pane usually scrolls
        // sideways; the alternative is a narrower row that reads worse.
        egui::ScrollArea::horizontal().show(ui, |ui| {
            for (i, row) in view.bytes.chunks(16).enumerate() {
                let addr = KSEG0 + view.base + (i * 16) as u32;
                let hex: String = row.iter().map(|b| format!("{b:02x} ")).collect();
                let ascii: String = row
                    .iter()
                    .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
                    .collect();
                ui.monospace(format!("{addr:08x}  {hex} {ascii}"));
            }
        });
        ui.separator();
        self.scanner(ui);
    }

    /// Scanner controls and the last pass's hits. Every pass is asked for
    /// explicitly -- nothing here runs per frame.
    fn scanner(&mut self, ui: &mut egui::Ui) {
        ui.heading("Scan");
        ui.horizontal(|ui| {
            for w in [1u8, 2, 4] {
                ui.selectable_value(&mut self.scan_width, w, format!("{}", w * 8));
            }
            ui.label("bit");
        });
        ui.horizontal(|ui| {
            ui.label("value");
            ui.add(
                egui::TextEdit::singleline(&mut self.scan_value)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(80.0),
            );
        });
        let value = self.scan_value.trim();
        let parsed = match value.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => value.parse::<u32>().ok(),
        };
        ui.horizontal(|ui| {
            if ui
                .add_enabled(parsed.is_some(), egui::Button::new("First scan"))
                .on_disabled_hover_text("decimal, or 0x-prefixed hex")
                .clicked()
                && let Some(v) = parsed
            {
                self.send_scan(scan::Filter::Exact(v), true);
            }
            if ui
                .button("Unknown")
                .on_hover_text("start with every address, then narrow by what moves")
                .clicked()
            {
                self.send_scan(scan::Filter::Unknown, true);
            }
        });
        ui.add_enabled_ui(self.scan_started, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(parsed.is_some(), egui::Button::new("= value"))
                    .clicked()
                    && let Some(v) = parsed
                {
                    self.send_scan(scan::Filter::Exact(v), false);
                }
                for (label, filter) in [
                    ("changed", scan::Filter::Changed),
                    ("same", scan::Filter::Unchanged),
                    ("up", scan::Filter::Increased),
                    ("down", scan::Filter::Decreased),
                ] {
                    if ui.button(label).clicked() {
                        self.send_scan(filter, false);
                    }
                }
            });
        });

        let outcome = self.emu.shared.scan.lock().unwrap().clone();
        let Some(outcome) = outcome else { return };
        ui.separator();
        let shown = outcome.hits.len();
        if outcome.count > shown {
            ui.label(format!("{} hits, first {shown}", outcome.count));
        } else {
            ui.label(format!("{} hits", outcome.count));
        }
        egui::ScrollArea::vertical()
            .max_height(240.0)
            .show(ui, |ui| {
                for (addr, value) in &outcome.hits {
                    let digits = 2 * outcome.width as usize;
                    let row = format!("{:08x}  {value:0digits$x}", KSEG0 + addr);
                    // Clicking a hit walks the viewer over to it.
                    if ui
                        .selectable_label(false, egui::RichText::new(row).monospace())
                        .clicked()
                    {
                        self.mem_addr = format!("{:08x}", KSEG0 + addr);
                        self.emu.shared.view_base.store(*addr, Ordering::Relaxed);
                    }
                }
            });
    }

    fn send_scan(&mut self, filter: scan::Filter, restart: bool) {
        self.emu.send(Command::Scan(scan::Request {
            width: self.scan_width,
            filter,
            restart,
        }));
        self.scan_started = true;
    }

    /// VRAM page: the whole 1024x512 grid, either as the 15-bit words the
    /// GPU stores or reinterpreted as packed 24-bit colour.
    ///
    /// Written to the same rule as the pane's pages -- it draws into the
    /// `Ui` it is handed and never builds its own container -- but it is
    /// not in [`Page::ALL`]: 1024 px does not fit a side pane, so the
    /// caller currently hands it a window. Putting it on a tab is adding
    /// the variant, nothing here.
    fn vram_page(&mut self, ui: &mut egui::Ui, tex: &egui::TextureHandle) {
        ui.checkbox(&mut self.vram_as_24bit, "interpret as 24-bit RGB");
        ui.add(egui::Image::new(tex));
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
                    ui.checkbox(&mut self.show_pane, "Side pane");
                    ui.checkbox(&mut self.show_tty, "TTY panel");
                    ui.checkbox(&mut self.show_vram, "VRAM viewer");
                    ui.separator();
                    ui.menu_button("Display size", |ui| {
                        for height in DISPLAY_HEIGHTS {
                            let label = format!("{height}p ({}x{height})", height * 4 / 3);
                            if ui.button(label).clicked() {
                                self.resize_to = Some(height);
                                ui.close();
                            }
                        }
                    });
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
                match &self.disc {
                    Some(d) => ui
                        .monospace(&d.file)
                        .on_hover_text(d.path.display().to_string()),
                    None => ui.weak("no disc"),
                };
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
    /// card itself when it stops.) Comparing whole configs rather than
    /// field by field means a new setting only has to be copied in here,
    /// not also added to a condition that is easy to forget.
    fn drop(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let mut cfg = self.config.clone();
        cfg.volume = self.volume;
        cfg.cheats = self.cheats_on;
        cfg.pane = self.show_pane;
        cfg.page = self.page;
        cfg.pane_width = self.pane_width;
        // Rounded: `screen_rect` is physical pixels over `pixels_per_point`,
        // so at fractional scaling it lands a hair off the size that was
        // requested, and an exact compare would rewrite the file on every
        // exit even when nothing was touched.
        cfg.window_width = self.window_size.x.round();
        cfg.window_height = self.window_size.y.round();
        if cfg != self.config {
            cfg.save(path);
        }
    }
}

/// Registers page: the CPU register file as last published. Nothing here
/// touches `App`, so it stays a free function.
fn registers_page(ui: &mut egui::Ui, status: &Status) {
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
        // A focused text field in the pane would otherwise type into the
        // pad as well. Only the pad is gated: the function-key shortcuts
        // below stay live, and Esc needs no handling because egui drops
        // focus in its own begin_pass before this runs.
        let typing = ctx.wants_keyboard_input();
        let buttons = ctx.input(|i| {
            self.keymap
                .iter()
                .filter(|(k, _)| !typing && i.key_down(*k))
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
        // What the worker has to publish this frame. Sampled before the
        // panels draw, so switching page reaches the worker one frame late
        // and the new page shows the previous snapshot until then.
        // Fullscreen hides every panel, so it asks for nothing.
        let panels = if chrome {
            let pane = if self.show_pane {
                self.page.panels()
            } else {
                0
            };
            let vram = if self.show_vram { emu::PANEL_VRAM } else { 0 };
            pane | vram
        } else {
            0
        };
        self.emu.shared.panels.store(panels, Ordering::Relaxed);
        // The worker only asks for a repaint when it publishes a frame, so
        // a page reading live state would freeze the moment emulation
        // pauses -- which is exactly when it is being read. Drive it here
        // instead, slow enough to stay legible.
        if panels != 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Fullscreen reports the screen, not the window the user chose.
        if chrome {
            self.window_size = ctx.screen_rect().size();
        }
        if self.title_dirty {
            self.apply_window_title(ctx);
        }

        // Resize the window so the display comes out exactly this tall.
        // The chrome is measured, not assumed: whatever the window has that
        // the display does not is the pane, the TTY and the bars, and they
        // keep their size while the difference lands on the display. Only
        // taken once a frame has been laid out, or the request would be
        // consumed with nothing to measure against.
        if self.central_size.x > 0.0
            && let Some(height) = self.resize_to.take()
        {
            let display =
                egui::vec2(height as f32 * 4.0 / 3.0, height as f32) / ctx.pixels_per_point();
            let window_chrome = ctx.screen_rect().size() - self.central_size;
            // A maximized window ignores InnerSize.
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(window_chrome + display));
        }

        if chrome {
            self.menu_bar(ctx, status.running, debugger_active);
            self.status_bar(ctx, &status);
        }

        // Declared before the TTY panel so the pane runs full height and
        // the TTY sits beside it, not under it.
        if chrome && self.show_pane {
            let pane = egui::SidePanel::right("pane")
                .resizable(true)
                .min_width(220.0)
                .default_width(self.pane_width)
                .show(ctx, |ui| {
                    // Registered before anything else so every widget sits
                    // on top of it: egui gives a click to the last widget
                    // added at that spot, so a hit here is a click on empty
                    // pane, which drops text focus and gives the keyboard
                    // back to the pad.
                    let background =
                        ui.interact(ui.max_rect(), ui.id().with("bg"), egui::Sense::click());
                    ui.horizontal(|ui| {
                        for page in Page::ALL {
                            ui.selectable_value(&mut self.page, page, page.label());
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| match self.page {
                        Page::Settings => self.settings_page(ui),
                        Page::Cheats => self.cheats_page(ui),
                        Page::Memory => self.memory_page(ui),
                        Page::Registers => registers_page(ui, &status),
                    });
                    if background.clicked() {
                        ui.memory_mut(|m| m.stop_text_input());
                    }
                });
            // Follow the drag rather than tracking the events behind it.
            self.pane_width = pane.response.rect.width();
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
            self.central_size = ui.max_rect().size();
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

        if panels & emu::PANEL_VRAM != 0 {
            let want = (
                self.emu.shared.vram_count.load(Ordering::Relaxed),
                self.vram_as_24bit,
            );
            if self.shown_vram != Some(want) || self.vram_tex.is_none() {
                // Copy out under the lock, expand outside it: the worker's
                // publish() blocks on this mutex, and turning 1024x512
                // 16-bit words into Color32 costs far more than the memcpy.
                {
                    let vram = self.emu.shared.vram.lock().unwrap();
                    if vram.len() == VRAM_LEN {
                        self.vram_scratch.clear();
                        self.vram_scratch.extend_from_slice(&vram);
                    }
                }
                if self.vram_scratch.len() == VRAM_LEN {
                    let image = vram_image(&self.vram_scratch, self.vram_as_24bit);
                    match &mut self.vram_tex {
                        Some(t) => t.set(image, egui::TextureOptions::NEAREST),
                        None => {
                            self.vram_tex =
                                Some(ctx.load_texture("vram", image, egui::TextureOptions::NEAREST))
                        }
                    }
                    self.shown_vram = Some(want);
                }
            }
            if let Some(tex) = self.vram_tex.clone() {
                // `open` takes its own bool so the body can still borrow self.
                let mut open = self.show_vram;
                egui::Window::new("VRAM (1024x512)")
                    .default_width(1024.0)
                    .open(&mut open)
                    .show(ctx, |ui| self.vram_page(ui, &tex));
                self.show_vram = open;
            }
        }
    }
}
