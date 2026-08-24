//! TOML configuration.
//!
//! Search order:
//! 1. `<exe_dir>/config/config.toml` (portable installs)
//! 2. `~/.config/PS1e/config.toml`
//!
//! When neither exists, a commented default is generated at the user
//! location (it survives `cargo clean`, unlike the target directory).

use psx_core::sio::button;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_TEMPLATE: &str = r#"# PS1e configuration

# Path to a 512 KiB PlayStation BIOS image (required to run).
# Absolute, or relative to the directory this file is in.
#bios = "path/to/bios.bin"

# Master volume, 0.0 .. 1.0
volume = 0.5

# Memory card image (created and formatted automatically).
# Defaults to memcard0.mcr next to this file.
#memcard = "memcard0.mcr"

# Keyboard bindings for the digital pad. Values are egui key names:
# letters and digits as themselves ("X", "1"), arrows as "Up"/"Down"/
# "Left"/"Right", plus "Enter", "Backspace", "Space", "F1".."F20" and so
# on. An unrecognized name falls back to the default for that button.
#[keys]
#up = "Up"
#down = "Down"
#left = "Left"
#right = "Right"
#cross = "X"
#circle = "C"
#square = "S"
#triangle = "D"
#l1 = "Q"
#r1 = "E"
#l2 = "1"
#r2 = "3"
#start = "Enter"
#select = "Backspace"

# Frontend hotkeys (not part of the emulated pad).
#[hotkeys]
#save_state = "F5"
#load_state = "F9"
"#;

/// One host key name per digital-pad button.
///
/// Names are what `egui::Key::name` produces ("Up", "X", "1", ...);
/// parsing accepts the wider set `egui::Key::from_name` understands.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyBindings {
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
    pub cross: String,
    pub circle: String,
    pub square: String,
    pub triangle: String,
    pub l1: String,
    pub r1: String,
    pub l2: String,
    pub r2: String,
    pub start: String,
    pub select: String,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            up: "Up".into(),
            down: "Down".into(),
            left: "Left".into(),
            right: "Right".into(),
            cross: "X".into(),
            circle: "C".into(),
            square: "S".into(),
            triangle: "D".into(),
            l1: "Q".into(),
            r1: "E".into(),
            l2: "1".into(),
            r2: "3".into(),
            start: "Enter".into(),
            select: "Backspace".into(),
        }
    }
}

impl KeyBindings {
    /// Each binding paired with the pad bit it drives, in a fixed order.
    pub fn pairs(&self) -> [(&str, u16); 14] {
        [
            (&self.up, button::UP),
            (&self.down, button::DOWN),
            (&self.left, button::LEFT),
            (&self.right, button::RIGHT),
            (&self.cross, button::CROSS),
            (&self.circle, button::CIRCLE),
            (&self.square, button::SQUARE),
            (&self.triangle, button::TRIANGLE),
            (&self.l1, button::L1),
            (&self.r1, button::R1),
            (&self.l2, button::L2),
            (&self.r2, button::R2),
            (&self.start, button::START),
            (&self.select, button::SELECT),
        ]
    }
}

/// Frontend hotkeys. These drive the emulator shell, not the emulated pad.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Hotkeys {
    pub save_state: String,
    pub load_state: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            save_state: "F5".into(),
            load_state: "F9".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub bios: Option<PathBuf>,
    pub volume: f32,
    pub memcard: Option<PathBuf>,
    // Tables must stay last: TOML cannot emit a scalar after a table.
    pub keys: KeyBindings,
    pub hotkeys: Hotkeys,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bios: None,
            volume: 0.5,
            memcard: None,
            keys: KeyBindings::default(),
            hotkeys: Hotkeys::default(),
        }
    }
}

fn exe_local_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config").join("config.toml"))
}

fn user_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".config").join("PS1e").join("config.toml"))
}

impl Config {
    /// Load the configuration, generating a default file on first run.
    /// Returns the config and the path it lives at.
    pub fn load() -> (Self, Option<PathBuf>) {
        for path in [exe_local_path(), user_path()].into_iter().flatten() {
            match std::fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<Config>(&text) {
                    Ok(mut cfg) => {
                        // Relative paths resolve against the config dir
                        if let Some(dir) = path.parent() {
                            for p in [&mut cfg.bios, &mut cfg.memcard] {
                                if let Some(v) = p
                                    && v.is_relative()
                                {
                                    *v = dir.join(&v);
                                }
                            }
                        }
                        tracing::info!("loaded config from {}", path.display());
                        return (cfg, Some(path));
                    }
                    Err(e) => {
                        tracing::error!("ignoring malformed {}: {e}", path.display());
                        return (Config::default(), Some(path));
                    }
                },
                Err(_) => continue,
            }
        }

        // First run: write a commented template to the user location
        let path = user_path();
        if let Some(path) = &path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(path, DEFAULT_TEMPLATE) {
                Ok(()) => tracing::info!("created default config at {}", path.display()),
                Err(e) => tracing::warn!("could not write {}: {e}", path.display()),
            }
        }
        (Config::default(), path)
    }

    /// Memory card image location: configured path, or memcard0.mcr next
    /// to the config file.
    pub fn memcard_path(&self, cfg_path: Option<&PathBuf>) -> PathBuf {
        self.memcard.clone().unwrap_or_else(|| {
            cfg_path
                .and_then(|p| p.parent())
                .map(|d| d.join("memcard0.mcr"))
                .unwrap_or_else(|| PathBuf::from("memcard0.mcr"))
        })
    }

    /// Persist current settings (e.g. volume changed in the UI), keeping it
    /// simple: full re-serialize, comments in the template are lost once
    /// the user's settings are saved over it.
    pub fn save(&self, path: &PathBuf) {
        // Store the BIOS path as given; no attempt to re-relativize
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(path, text) {
                    tracing::warn!("failed to save config: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to serialize config: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string_pretty(&Config::default()).expect("serialize");
        let back: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.keys.cross, "X");
        assert_eq!(back.hotkeys.save_state, "F5");
    }

    #[test]
    fn partial_tables_fall_back_to_defaults() {
        let cfg: Config = toml::from_str("[keys]\ncross = \"Z\"\n").expect("deserialize");
        assert_eq!(cfg.keys.cross, "Z");
        assert_eq!(cfg.keys.circle, "C");
        assert_eq!(cfg.hotkeys.load_state, "F9");
    }
}
