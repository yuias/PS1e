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

/// Codes name a 24-bit address; published lists write the continuation
/// lines of the two-line types in KSEG0 form, and so does `to_text`.
const KSEG0: u32 = 0x8000_0000;

/// How a conditional compares. psx-spx words every one of them with the
/// code's own operand on the left: `D2` is "If dddd<[aaaaaa]". It also
/// says outright that the direction is unconfirmed, so this is the
/// documented reading rather than a measured one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    /// operand < memory
    Lt,
    /// operand > memory
    Gt,
}

impl Cmp {
    fn holds(self, operand: u32, memory: u32) -> bool {
        match self {
            Cmp::Eq => operand == memory,
            Cmp::Ne => operand != memory,
            Cmp::Lt => operand < memory,
            Cmp::Gt => operand > memory,
        }
    }

    /// The low two bits of a `Dx` / `Ex` type byte.
    fn from_low_nibble(n: u8) -> Cmp {
        match n {
            0 => Cmp::Eq,
            1 => Cmp::Ne,
            2 => Cmp::Lt,
            _ => Cmp::Gt,
        }
    }

    fn low_nibble(self) -> u8 {
        match self {
            Cmp::Eq => 0,
            Cmp::Ne => 1,
            Cmp::Lt => 2,
            Cmp::Gt => 3,
        }
    }
}

/// One code. Most are one line; [`Code::Slide`] and [`Code::Copy`] are
/// the two that eat the line after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// `30aaaaaa 00dd`
    Write8 { addr: u32, value: u8 },
    /// `80aaaaaa dddd`
    Write16 { addr: u32, value: u16 },
    /// `20aaaaaa 00dd` / `21aaaaaa 00dd`, wrapping at 8 bits.
    Add8 { addr: u32, delta: i16 },
    /// `10aaaaaa dddd` / `11aaaaaa dddd`, wrapping at 16 bits.
    Add16 { addr: u32, delta: i32 },
    /// `E0aaaaaa 00dd`..`E3`: run the next code only if this holds.
    If8 { addr: u32, cmp: Cmp, value: u8 },
    /// `D0aaaaaa dddd`..`D3`.
    If16 { addr: u32, cmp: Cmp, value: u16 },
    /// `D4000000 dddd`: run the next code while exactly these buttons are
    /// held. The operand is the raw pad halfword, so it is active low —
    /// nothing held is `FFFF`, Cross alone is `BFFF`.
    IfButtons { value: u16 },
    /// ```text
    /// 5000nnbb dddd
    /// aaaaaaaa ??ee   for i in 0..nn: [addr + i*bb] = value + i*step
    /// ```
    /// psx-spx does not say what width the writes are; `dddd` is 16 bits,
    /// so they are taken as 16-bit.
    Slide {
        addr: u32,
        count: u8,
        stride: u8,
        value: u16,
        step: u16,
    },
    /// ```text
    /// C2ssssss nnnn
    /// 80tttttt 0000   copy nnnn bytes from ssssss to tttttt
    /// ```
    /// psx-spx names the length `nnnn` in the layout but calls it `ssss`
    /// in the prose; the operand word is the only field that can hold it.
    Copy { src: u32, dst: u32, len: u16 },
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
            0x10 => Code::Add16 {
                addr,
                delta: i32::from(value),
            },
            0x11 => Code::Add16 {
                addr,
                delta: -i32::from(value),
            },
            0x20 => Code::Add8 {
                addr,
                delta: i16::from(value as u8),
            },
            0x21 => Code::Add8 {
                addr,
                delta: -i16::from(value as u8),
            },
            0xd4 => Code::IfButtons { value },
            0xd0..=0xd3 => Code::If16 {
                addr,
                cmp: Cmp::from_low_nibble(kind & 3),
                value,
            },
            0xe0..=0xe3 => Code::If8 {
                addr,
                cmp: Cmp::from_low_nibble(kind & 3),
                value: value as u8,
            },
            // 0x50 and 0xc2 need the line after them; the caller pairs
            // them up and calls `parse_pair`.
            _ => Code::Unsupported { kind, addr, value },
        })
    }

    /// The type byte of a line, without committing to parsing it. Tells
    /// the reader whether a continuation line has to be pulled in.
    fn kind_of(line: &str) -> Option<u8> {
        let word = line.split_whitespace().next()?;
        (word.len() == 8).then(|| u8::from_str_radix(&word[..2], 16).ok())?
    }

    /// True for the two types that are written across two lines.
    fn takes_a_continuation(kind: u8) -> bool {
        matches!(kind, 0x50 | 0xc2)
    }

    /// Assemble one of the two-line codes. Their second line is a bare
    /// 32-bit address plus a 16-bit word, not a typed code line, so it is
    /// split here rather than run through [`Code::parse`].
    fn parse_pair(head: &str, tail: &str) -> Result<Code, ParseError> {
        let (head_hi, head_lo) = words(head)?;
        let (tail_hi, tail_lo) = words(tail)?;
        match (head_hi >> 24) as u8 {
            0x50 => Ok(Code::Slide {
                addr: tail_hi & 0x00ff_ffff,
                count: (head_hi >> 8) as u8,
                stride: head_hi as u8,
                value: head_lo,
                step: tail_lo,
            }),
            // The second word of the destination line is documented as
            // 0000 and carries nothing, so it is not kept.
            _ => Ok(Code::Copy {
                src: head_hi & 0x00ff_ffff,
                dst: tail_hi & 0x00ff_ffff,
                len: head_lo,
            }),
        }
    }

    /// Run this code. Returns how many of the codes after it to skip,
    /// which is 1 for a conditional that does not hold and 0 otherwise.
    /// A GameShark conditional gates exactly the line after it, so there
    /// is no nesting to track — a chain of them nests by construction.
    fn run(self, bus: &mut Bus) -> usize {
        match self {
            Code::Write8 { addr, value } => {
                bus.poke8(addr, value);
                0
            }
            Code::Write16 { addr, value } => {
                write16(bus, addr, value);
                0
            }
            Code::Add8 { addr, delta } => {
                if let Some(cur) = bus.peek8(addr) {
                    bus.poke8(addr, (i16::from(cur).wrapping_add(delta)) as u8);
                }
                0
            }
            Code::Add16 { addr, delta } => {
                if let Some(cur) = read16(bus, addr) {
                    write16(bus, addr, (i32::from(cur).wrapping_add(delta)) as u16);
                }
                0
            }
            // An address that cannot be read makes the condition false:
            // the alternative is running a write against state nobody
            // could observe.
            Code::If8 { addr, cmp, value } => {
                let held = bus
                    .peek8(addr)
                    .is_some_and(|m| cmp.holds(u32::from(value), u32::from(m)));
                usize::from(!held)
            }
            Code::If16 { addr, cmp, value } => {
                let held =
                    read16(bus, addr).is_some_and(|m| cmp.holds(u32::from(value), u32::from(m)));
                usize::from(!held)
            }
            // The pad halfword is active low on the wire; the core keeps
            // it set-means-pressed, so invert before comparing.
            Code::IfButtons { value } => usize::from(value != !bus.sio.buttons),
            Code::Slide {
                addr,
                count,
                stride,
                value,
                step,
            } => {
                for i in 0..u32::from(count) {
                    let at = addr.wrapping_add(i.wrapping_mul(u32::from(stride)));
                    let v = value.wrapping_add((i as u16).wrapping_mul(step));
                    write16(bus, at, v);
                }
                0
            }
            // Byte at a time so the region check happens per address:
            // a copy that runs off the end of RAM stops writing rather
            // than wrapping into something else.
            Code::Copy { src, dst, len } => {
                for i in 0..u32::from(len) {
                    let Some(b) = bus.peek8(src.wrapping_add(i)) else {
                        break;
                    };
                    if !bus.poke8(dst.wrapping_add(i), b) {
                        break;
                    }
                }
                0
            }
            Code::Unsupported { .. } => 0,
        }
    }
}

/// Split a `xxxxxxxx yyyy` line into its two words.
fn words(line: &str) -> Result<(u32, u16), ParseError> {
    let mut w = line.split_whitespace();
    let (Some(hi), Some(lo), None) = (w.next(), w.next(), w.next()) else {
        return Err(ParseError::Shape);
    };
    if hi.len() != 8 || lo.len() != 4 {
        return Err(ParseError::Shape);
    }
    Ok((
        u32::from_str_radix(hi, 16).map_err(|_| ParseError::Hex)?,
        u16::from_str_radix(lo, 16).map_err(|_| ParseError::Hex)?,
    ))
}

/// Little-endian halfword through the debugger accessors: `poke8` already
/// resolves the region, so a second address decode buys nothing.
fn write16(bus: &mut Bus, addr: u32, value: u16) {
    bus.poke8(addr, value as u8);
    bus.poke8(addr.wrapping_add(1), (value >> 8) as u8);
}

fn read16(bus: &Bus, addr: u32) -> Option<u16> {
    let lo = bus.peek8(addr)?;
    let hi = bus.peek8(addr.wrapping_add(1))?;
    Some(u16::from(lo) | u16::from(hi) << 8)
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
        // Filtered up front so a slide can reach its continuation line
        // without tripping over a comment or a blank line between them.
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !(l.is_empty() || l.starts_with('#') || l.starts_with(';')));
        while let Some(line) = lines.next() {
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
            // Two types are written across two lines; the rest are one.
            let paired = Code::kind_of(line).is_some_and(Code::takes_a_continuation);
            let parsed = if paired {
                match lines.next() {
                    Some(tail) => Code::parse_pair(line, tail),
                    None => Err(ParseError::Shape),
                }
            } else {
                Code::parse(line)
            };
            match parsed {
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
                    Code::Add8 { addr, delta } => {
                        let kind = if delta < 0 { 0x21 } else { 0x20 };
                        format!("{kind:02X}{addr:06X} 00{:02X}", delta.unsigned_abs())
                    }
                    Code::Add16 { addr, delta } => {
                        let kind = if delta < 0 { 0x11 } else { 0x10 };
                        format!("{kind:02X}{addr:06X} {:04X}", delta.unsigned_abs())
                    }
                    Code::If8 { addr, cmp, value } => {
                        format!("E{}{addr:06X} 00{value:02X}", cmp.low_nibble())
                    }
                    Code::If16 { addr, cmp, value } => {
                        format!("D{}{addr:06X} {value:04X}", cmp.low_nibble())
                    }
                    Code::IfButtons { value } => format!("D4000000 {value:04X}"),
                    Code::Slide {
                        addr,
                        count,
                        stride,
                        value,
                        step,
                    } => format!(
                        // The continuation address goes out in the KSEG0
                        // form published codes are written in; parsing
                        // masks the segment off again.
                        "5000{count:02X}{stride:02X} {value:04X}\n{:08X} {step:04X}",
                        KSEG0 | addr
                    ),
                    Code::Copy { src, dst, len } => {
                        format!("C2{src:06X} {len:04X}\n{:08X} 0000", KSEG0 | dst)
                    }
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

    /// Apply every enabled cheat, in file order. Within a cheat the codes
    /// run as a little program: a conditional that does not hold skips the
    /// code after it, which is the whole of GameShark's control flow.
    pub fn apply(&self, bus: &mut Bus) {
        for cheat in self.cheats.iter().filter(|c| c.enabled) {
            let mut i = 0;
            while let Some(code) = cheat.codes.get(i) {
                i += 1 + code.run(bus);
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
        let list = CheatList::parse("[*t]\nC1000000 4000\n30000100 00AB\n");
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
    fn increments_wrap_at_their_own_width() {
        let mut bus = bus();
        // 8-bit: FF + 2 wraps to 01. 16-bit: 0001 - 2 wraps to FFFF.
        assert!(bus.poke8(RAM, 0xFF));
        write16(&mut bus, RAM + 2, 0x0001);
        CheatList::parse(
            "[*t]
20000100 0002
11000102 0002
",
        )
        .apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0x01));
        assert_eq!(read16(&bus, RAM + 2), Some(0xFFFF));
    }

    /// psx-spx writes every comparison with the code's operand on the
    /// left: `D2` is "If dddd<[aaaaaa]". It also says the direction is
    /// unconfirmed, so this test is what pins the reading we chose.
    #[test]
    fn a_conditional_gates_exactly_the_code_after_it() {
        let mut bus = bus();
        write16(&mut bus, RAM, 0x0064); // 100

        // 100 == 100 -> the write runs. 5 < 100 -> the write runs.
        CheatList::parse(
            "[*t]
D0000100 0064
80000110 1111
D2000100 0005
80000112 2222
",
        )
        .apply(&mut bus);
        assert_eq!(read16(&bus, RAM + 0x10), Some(0x1111));
        assert_eq!(read16(&bus, RAM + 0x12), Some(0x2222));

        // 200 < 100 is false, so the write after it is skipped and the
        // one after that still runs.
        CheatList::parse(
            "[*t]
D2000100 00C8
80000114 3333
80000116 4444
",
        )
        .apply(&mut bus);
        assert_eq!(read16(&bus, RAM + 0x14), Some(0x0000));
        assert_eq!(read16(&bus, RAM + 0x16), Some(0x4444));
    }

    #[test]
    fn a_conditional_with_nothing_after_it_just_ends_the_cheat() {
        let mut bus = bus();
        CheatList::parse(
            "[*t]
D1000100 0000
",
        )
        .apply(&mut bus);
    }

    #[test]
    fn an_eight_bit_conditional_reads_one_byte() {
        let mut bus = bus();
        assert!(bus.poke8(RAM, 0x07));
        // E1 is not-equal: 07 != 07 is false, so the write is skipped.
        CheatList::parse(
            "[*t]
E1000100 0007
30000120 00FF
",
        )
        .apply(&mut bus);
        assert_eq!(bus.peek8(RAM + 0x20), Some(0x00));
        CheatList::parse(
            "[*t]
E0000100 0007
30000120 00FF
",
        )
        .apply(&mut bus);
        assert_eq!(bus.peek8(RAM + 0x20), Some(0xFF));
    }

    /// The operand is the pad halfword as the hardware presents it, so it
    /// is active low: nothing held is FFFF, Cross alone is BFFF.
    #[test]
    fn the_button_conditional_compares_the_active_low_halfword() {
        let mut bus = bus();
        CheatList::parse(
            "[*t]
D4000000 FFFF
30000100 0011
",
        )
        .apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0x11));

        bus.sio.buttons = crate::sio::button::CROSS;
        CheatList::parse(
            "[*t]
D4000000 FFFF
30000100 0022
",
        )
        .apply(&mut bus);
        assert_eq!(
            bus.peek8(RAM),
            Some(0x11),
            "released-only code must not fire"
        );
        CheatList::parse(
            "[*t]
D4000000 BFFF
30000100 0022
",
        )
        .apply(&mut bus);
        assert_eq!(bus.peek8(RAM), Some(0x22));
    }

    /// `5000nnbb dddd` / `aaaaaaaa ??ee`: nn writes, address stepping by
    /// bb bytes and value by ee each time.
    #[test]
    fn a_slide_writes_its_whole_run() {
        let mut bus = bus();
        CheatList::parse(
            "[*t]
50000304 0010
80000100 0001
",
        )
        .apply(&mut bus);
        assert_eq!(read16(&bus, RAM), Some(0x0010));
        assert_eq!(read16(&bus, RAM + 4), Some(0x0011));
        assert_eq!(read16(&bus, RAM + 8), Some(0x0012));
        assert_eq!(
            read16(&bus, RAM + 12),
            Some(0x0000),
            "count of 3 stops at 3"
        );
    }

    #[test]
    fn a_copy_moves_its_run_of_bytes() {
        let mut bus = bus();
        for (i, b) in [1u8, 2, 3, 4].iter().enumerate() {
            assert!(bus.poke8(RAM + i as u32, *b));
        }
        CheatList::parse(
            "[*t]
C2000100 0004
80000200 0000
",
        )
        .apply(&mut bus);
        for (i, b) in [1u8, 2, 3, 4].iter().enumerate() {
            assert_eq!(bus.peek8(0x8000_0200 + i as u32), Some(*b));
        }
        assert_eq!(bus.peek8(0x8000_0204), Some(0x00), "length of 4 stops at 4");
    }

    #[test]
    fn a_copy_stops_where_the_memory_does() {
        let mut bus = bus();
        // Source runs off the end of RAM part way through; the code has
        // to stop rather than wrap around to the start of it.
        let near_end = (crate::bus::RAM_SIZE as u32) - 2;
        assert!(bus.poke8(near_end, 0xAA));
        CheatList::parse(&format!(
            "[*t]
C2{near_end:06X} 0010
80000200 0000
"
        ))
        .apply(&mut bus);
        assert_eq!(bus.peek8(0x8000_0200), Some(0xAA));
        assert_eq!(bus.peek8(0x8000_0202), Some(0x00));
    }

    #[test]
    fn a_slide_without_its_second_line_is_dropped() {
        let list = CheatList::parse(
            "[*t]
50000304 0010
",
        );
        assert!(list.cheats[0].codes.is_empty());
    }

    /// A comment between the two halves of a slide must not break it: the
    /// parser filters comments before it pairs the lines up.
    #[test]
    fn a_slide_survives_a_comment_between_its_lines() {
        let list = CheatList::parse(
            "[*t]
50000304 0010
; here
80000100 0001
",
        );
        assert_eq!(list.cheats[0].codes.len(), 1);
    }

    /// The two-line types survive a rewrite of the file, which is what a
    /// toggle does. Their continuation goes back out in the KSEG0 form
    /// published codes are written in.
    #[test]
    fn the_two_line_codes_round_trip() {
        let text = "[*Two]
5000030A 0010
80000100 0001
C2000100 0004
80000200 0000

";
        let list = CheatList::parse(text);
        assert_eq!(list.cheats[0].codes.len(), 2);
        assert_eq!(list.to_text(), text);
        assert_eq!(CheatList::parse(&list.to_text()), list);
    }

    #[test]
    fn text_round_trips_including_the_enable_marker() {
        let text = "[*On]\n30000100 00AB\n80000102 1234\n\n[Off]\nC1000000 4000\n\n";
        let list = CheatList::parse(text);
        assert_eq!(CheatList::parse(&list.to_text()), list);
        assert_eq!(list.to_text(), text);
    }
}
