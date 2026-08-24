//! Gamepad input via gilrs.
//!
//! Digital buttons only: the emulated pad is a standard controller, so
//! there is nothing for the analog sticks to drive yet. Bits produced here
//! are OR-ed with the keyboard, letting either drive any button.

use crate::config::PadBindings;
use gilrs::{Button, Gilrs};

pub struct Gamepad {
    gilrs: Gilrs,
    /// Gamepad button -> pad bit, resolved from the config once at startup.
    map: Vec<(Button, u16)>,
}

impl Gamepad {
    /// Open the gamepad subsystem. Returns `None` when it is unavailable
    /// (no driver, no permission) — the frontend then runs keyboard-only.
    pub fn new(bindings: &PadBindings) -> Option<Self> {
        let gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("gamepad support unavailable: {e}");
                return None;
            }
        };
        let fallback = PadBindings::default();
        let map = bindings
            .pairs()
            .into_iter()
            .zip(fallback.pairs())
            .filter_map(
                |((name, bit), (default_name, _))| match parse_button(name) {
                    Some(btn) => Some((btn, bit)),
                    None => {
                        tracing::warn!("unknown gamepad button '{name}'; using '{default_name}'");
                        parse_button(default_name).map(|btn| (btn, bit))
                    }
                },
            )
            .collect();
        Some(Self { gilrs, map })
    }

    /// Pad bits held on any connected gamepad.
    pub fn poll(&mut self) -> u16 {
        // gilrs refreshes its cached button state from the event queue, so
        // the queue has to be drained before the state is worth reading.
        while self.gilrs.next_event().is_some() {}
        let mut bits = 0;
        for (_id, pad) in self.gilrs.gamepads() {
            for (btn, bit) in &self.map {
                if pad.is_pressed(*btn) {
                    bits |= bit;
                }
            }
        }
        bits
    }
}

/// Parse a `gilrs::Button` variant name. `Unknown` is rejected: it names no
/// physical button and would silently swallow the binding.
fn parse_button(name: &str) -> Option<Button> {
    Some(match name {
        "South" => Button::South,
        "East" => Button::East,
        "North" => Button::North,
        "West" => Button::West,
        "C" => Button::C,
        "Z" => Button::Z,
        "LeftTrigger" => Button::LeftTrigger,
        "LeftTrigger2" => Button::LeftTrigger2,
        "RightTrigger" => Button::RightTrigger,
        "RightTrigger2" => Button::RightTrigger2,
        "Select" => Button::Select,
        "Start" => Button::Start,
        "Mode" => Button::Mode,
        "LeftThumb" => Button::LeftThumb,
        "RightThumb" => Button::RightThumb,
        "DPadUp" => Button::DPadUp,
        "DPadDown" => Button::DPadDown,
        "DPadLeft" => Button::DPadLeft,
        "DPadRight" => Button::DPadRight,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_all_parse() {
        let defaults = PadBindings::default();
        for (name, _) in defaults.pairs() {
            assert!(parse_button(name).is_some(), "'{name}' should parse");
        }
    }

    #[test]
    fn unknown_is_not_a_valid_binding() {
        assert!(parse_button("Unknown").is_none());
        assert!(parse_button("south").is_none());
    }
}
