//! TOML configuration.
//!
//! Search order:
//! 1. `<exe_dir>/config/config.toml` (portable installs)
//! 2. `~/.config/PS1e/config.toml`
//!
//! When neither exists, a commented default is generated at the user
//! location (it survives `cargo clean`, unlike the target directory).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_TEMPLATE: &str = r#"# PS1e configuration

# Path to a 512 KiB PlayStation BIOS image (required to run).
# Absolute, or relative to the directory this file is in.
#bios = "path/to/bios.bin"

# Master volume, 0.0 .. 1.0
volume = 0.5
"#;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub bios: Option<PathBuf>,
    pub volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bios: None,
            volume: 0.5,
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
                        // Relative BIOS paths resolve against the config dir
                        if let (Some(bios), Some(dir)) = (&cfg.bios, path.parent()) {
                            if bios.is_relative() {
                                cfg.bios = Some(dir.join(bios));
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
