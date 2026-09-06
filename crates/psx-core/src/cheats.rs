//! GameShark / Pro Action Replay cheat codes.
//!
//! Codes are `TTaaaaaa vvvv`: a type byte, a 24-bit address and a 16-bit
//! operand. They are applied once per frame, at the vblank edge, which is
//! where the real cartridge got control.
//!
//! Every write goes through [`Bus::poke8`] and every read through
//! [`Bus::peek8`]. That is what does the KSEG masking and the scratchpad
//! routing, and what refuses ROM and MMIO — a cheat that names an MMIO
//! address must do nothing rather than drain a FIFO. It also makes cheat
//! writes indistinguishable from debugger pokes, which is what the
//! control port and the gdb server already issue.
//!
//! Code types follow psx-spx "Cheat Devices - Datel Cheat Code Format".
//! Types this does not implement are kept as [`Code::Unsupported`] so a
//! file that contains them still loads and still shows its other codes.

use crate::bus::Bus;

/// One code line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// `30aaaaaa 00dd`
    Write8 { addr: u32, value: u8 },
    /// `80aaaaaa dddd`
    Write16 { addr: u32, value: u16 },
    /// A well-formed line of a type that is not implemented. Kept so the
    /// rest of the cheat still works and the UI can say what was skipped.
    Unsupported { kind: u8, addr: u32, value: u16 },
}

impl Code {
    /// Parse one `TTaaaaaa vvvv` line. Whitespace between the words is
    /// free-form; anything else is a parse error, not an `Unsupported`.
    pub fn parse(line: &str) -> Result<Code, ParseError> {
        let mut words = line.split_whitespace();
        let (Some(hi), Some(lo), None) = (words.next(), words.next(), words.next()) else {
            return Err(ParseError::Shape);
        };
        if hi.len() != 8 || lo.len() != 4 {
            return Err(ParseError::Shape);
        }
        let hi = u32::from_str_radix(hi, 16).map_err(|_| ParseError::Hex)?;
        let value = u16::from_str_radix(lo, 16).map_err(|_| ParseError::Hex)?;
        let kind = (hi >> 24) as u8;
        let addr = hi & 0x00ff_ffff;
        Ok(match kind {
            0x30 => Code::Write8 {
                addr,
                value: value as u8,
            },
            0x80 => Code::Write16 { addr, value },
            _ => Code::Unsupported { kind, addr, value },
        })
    }

    /// Apply this code. Returns false when the line was not applied, so a
    /// future conditional type can gate the line after it.
    fn apply(self, bus: &mut Bus) -> bool {
        match self {
            Code::Write8 { addr, value } => bus.poke8(addr, value),
            // Little-endian, byte at a time: poke8 already resolves the
            // region, and a 16-bit code straddling the end of a region is
            // not worth a second address decode.
            Code::Write16 { addr, value } => {
                let lo = bus.poke8(addr, value as u8);
                let hi = bus.poke8(addr.wrapping_add(1), (value >> 8) as u8);
                lo && hi
            }
            Code::Unsupported { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Not two hex words of 8 and 4 digits.
    Shape,
    Hex,
}

/// One named cheat: the `[Name]` section of a `.cht` file and its codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cheat {
    pub name: String,
    pub enabled: bool,
    pub codes: Vec<Code>,
}

impl Cheat {
    /// True when the cheat contains a line this build cannot apply, which
    /// is what the UI greys out or marks.
    pub fn has_unsupported(&self) -> bool {
        self.codes
            .iter()
            .any(|c| matches!(c, Code::Unsupported { .. }))
    }
}

/// The cheats loaded for the disc in the drive.
///
/// Frontend-owned: it rides in [`crate::Ambient`], so it survives a state
/// load and a reset without any further plumbing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheatList {
    pub cheats: Vec<Cheat>,
}

impl CheatList {
    /// Parse a `.cht` file as PCSX-Reloaded and DuckStation write them:
    /// `[Name]` section headers, one code per line, `#` and `;` comments,
    /// blank lines ignored. A leading `*` on the section name means the
    /// cheat is enabled — the file is the only record of that, so there is
    /// no per-disc enable map anywhere else.
    ///
    /// Malformed code lines are dropped with a warning rather than failing
    /// the load: one bad line in a community file should not cost the user
    /// every other cheat in it.
    pub fn parse(text: &str) -> CheatList {
        let mut cheats: Vec<Cheat> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let (enabled, name) = match name.strip_prefix('*') {
                    Some(rest) => (true, rest),
                    None => (false, name),
                };
                cheats.push(Cheat {
                    name: name.trim().to_string(),
                    enabled,
                    codes: Vec::new(),
                });
                continue;
            }
            let Some(cheat) = cheats.last_mut() else {
                tracing::warn!("cheat line before any [section]: {line}");
                continue;
            };
            match Code::parse(line) {
                Ok(code) => cheat.codes.push(code),
                Err(e) => tracing::warn!("ignoring cheat line {line:?} in {}: {e:?}", cheat.name),
            }
        }
        CheatList { cheats }
    }

    /// Serialize back to `.cht`, which is how an enable toggle is stored.
    /// Comments in the original file are not preserved.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for cheat in &self.cheats {
            let mark = if cheat.enabled { "*" } else { "" };
            out.push_str(&format!("[{mark}{}]\n", cheat.name));
            for code in &cheat.codes {
                let line = match *code {
                    Code::Write8 { addr, value } => format!("30{addr:06X} 00{value:02X}"),
                    Code::Write16 { addr, value } => format!("80{addr:06X} {value:04X}"),
                    Code::Unsupported { kind, addr, value } => {
                        format!("{kind:02X}{addr:06X} {value:04X}")
                    }
                };
                out.push_str(&line);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.cheats.is_empty()
    }

    /// Apply every enabled cheat, in file order.
    pub fn apply(&self, bus: &mut Bus) {
        for cheat in self.cheats.iter().filter(|c| c.enabled) {
            for code in &cheat.codes {
                code.apply(bus);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u32 = 0x8000_0100;

    fn bus() -> Bus {
        Bus::new(vec![0; crate::bus::BIOS_SIZE]).expect("a blank BIOS is the right size")
    }

    #[test]
    fn both_write_widths_land_little_endian() {
        let mut bus = bus();
        let list = CheatList::parse("[*t]\n30000100 00AB\n80000102 1234\n");
        list.apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0xAB));
        assert_eq!(bus.peek8(RAM + 2), Some(0x34));
        assert_eq!(bus.peek8(RAM + 3), Some(0x12));
    }

    #[test]
    fn a_disabled_cheat_writes_nothing() {
        let mut bus = bus();
        CheatList::parse("[t]\n30000100 00AB\n").apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0x00));
    }

    #[test]
    fn headers_comments_and_blank_lines() {
        let list = CheatList::parse(
            "# a comment\n\
             ; another\n\
             \n\
             [ Infinite health ]\n\
             80000100 0063\n\
             \n\
             [*Infinite lives]\n\
             30000200 0009\n",
        );
        assert_eq!(list.cheats.len(), 2);
        assert_eq!(list.cheats[0].name, "Infinite health");
        assert!(!list.cheats[0].enabled);
        assert_eq!(list.cheats[1].name, "Infinite lives");
        assert!(list.cheats[1].enabled);
        assert_eq!(list.cheats[1].codes.len(), 1);
    }

    #[test]
    fn an_unknown_type_is_kept_and_the_rest_still_applies() {
        let mut bus = bus();
        let list = CheatList::parse("[*t]\nD0000100 0001\n30000100 00AB\n");
        assert!(list.cheats[0].has_unsupported());
        assert_eq!(list.cheats[0].codes.len(), 2);
        list.apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0xAB));
    }

    #[test]
    fn a_malformed_line_is_dropped_and_the_cheat_survives() {
        let list = CheatList::parse("[*t]\nnot a code\n30 00AB\n80000100 00AB\n");
        assert_eq!(list.cheats.len(), 1);
        assert_eq!(
            list.cheats[0].codes,
            [Code::Write16 {
                addr: 0x100,
                value: 0x00AB
            }]
        );
    }

    #[test]
    fn addresses_reach_ram_through_any_mirror() {
        // The type byte is stripped, so a code is written against the
        // 24-bit address whatever segment the author had in mind.
        assert_eq!(
            Code::parse("80800100 0001"),
            Ok(Code::Write16 {
                addr: 0x0080_0100 & 0x00ff_ffff,
                value: 1
            })
        );
    }

    #[test]
    fn an_address_outside_ram_is_refused_rather_than_written() {
        let mut bus = bus();
        // The 24-bit address field only ever reaches the low 16 MiB, of
        // which only the first 8 MiB decodes as RAM. poke8 declines the
        // rest, so a code cannot reach MMIO or ROM however it is written.
        assert!(!bus.poke8(0x0080_1814, 1));
        CheatList::parse("[*t]\n30801814 0001\n").apply(&mut bus);
        assert_eq!(bus.peek8(0x0080_1814), None);
    }

    #[test]
    fn text_round_trips_including_the_enable_marker() {
        let text = "[*On]\n30000100 00AB\n80000102 1234\n\n[Off]\nD0000100 0001\n\n";
        let list = CheatList::parse(text);
        assert_eq!(CheatList::parse(&list.to_text()), list);
        assert_eq!(list.to_text(), text);
    }
}
