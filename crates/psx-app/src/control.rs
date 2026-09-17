//! Interactive control port for headless automation.
//!
//! Designed for LLM/script operators: a line-based text protocol over TCP
//! where the emulator runs in *lockstep* — it only advances when a `run` or
//! `press` command says so, making every observation deterministic and
//! repeatable. Duration arguments take an `s` (seconds), `c` (cycles) or
//! `v` (whole vblanks, stopping right after the edge) suffix, or default to
//! frames. One command per line; the reply is `ok`/`err <msg>` followed
//! by payload lines, terminated by a single `.` line (payload lines starting
//! with `.` are dot-stuffed, SMTP-style).
//!
//! The bundled `psxctl` binary wraps one command per invocation, so a shell
//! (or a tool-using LLM) can drive a session statelessly:
//!
//! ```text
//! psxctl press START 30     # hold START for 30 frames
//! psxctl frame shot.bmp     # dump what the TV shows
//! psxctl peek 801ffc38 64   # inspect memory, side-effect-free
//! ```

use psx_core::{CPU_CLOCK_HZ, CYCLES_PER_FRAME, PsxSystem, SHELL_ENTRY};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use tracing::info;

/// Digital pad button by script/protocol name.
pub fn button_by_name(name: &str) -> Option<u16> {
    use psx_core::sio::button::*;
    Some(match name.to_ascii_uppercase().as_str() {
        "SELECT" => SELECT,
        "START" => START,
        "UP" => UP,
        "DOWN" => DOWN,
        "LEFT" => LEFT,
        "RIGHT" => RIGHT,
        "L1" => L1,
        "R1" => R1,
        "L2" => L2,
        "R2" => R2,
        "TRIANGLE" => TRIANGLE,
        "CIRCLE" => CIRCLE,
        "CROSS" => CROSS,
        "SQUARE" => SQUARE,
        _ => return None,
    })
}

const BUTTON_NAMES: [(&str, u16); 14] = {
    use psx_core::sio::button::*;
    [
        ("SELECT", SELECT),
        ("START", START),
        ("UP", UP),
        ("DOWN", DOWN),
        ("LEFT", LEFT),
        ("RIGHT", RIGHT),
        ("L1", L1),
        ("R1", R1),
        ("L2", L2),
        ("R2", R2),
        ("TRIANGLE", TRIANGLE),
        ("CIRCLE", CIRCLE),
        ("CROSS", CROSS),
        ("SQUARE", SQUARE),
    ]
};

fn buttons_to_names(mask: u16) -> String {
    let names: Vec<&str> = BUTTON_NAMES
        .iter()
        .filter(|(_, b)| mask & b != 0)
        .map(|(n, _)| *n)
        .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join("+")
    }
}

/// Parse `A+B+C` into a button mask.
fn parse_buttons(s: &str) -> Result<u16, String> {
    s.split('+').try_fold(0u16, |acc, name| {
        button_by_name(name)
            .map(|b| acc | b)
            .ok_or_else(|| format!("unknown button '{name}'"))
    })
}

/// How far a `run`-style command advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunLength {
    Cycles(u64),
    /// Whole vblanks; the machine stops right after the edge.
    Vblanks(u64),
}

/// Parse `10` (frames), `2s`, `50000c` or `3v` (vblanks; integer >= 1).
fn parse_duration(s: &str) -> Result<RunLength, String> {
    if let Some(num) = s.strip_suffix('v') {
        // Vblanks are a whole-edge unit, not a fraction of a field, so this
        // parses as an integer rather than sharing the f64 path below.
        return match num.parse::<u64>() {
            Ok(n) if n >= 1 => Ok(RunLength::Vblanks(n)),
            _ => Err(format!("bad duration '{s}'")),
        };
    }
    let (num, unit) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], CPU_CLOCK_HZ),
        Some('c') => (&s[..s.len() - 1], 1),
        _ => (s, CYCLES_PER_FRAME),
    };
    let n: f64 = num.parse().map_err(|_| format!("bad duration '{s}'"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("bad duration '{s}'"));
    }
    Ok(RunLength::Cycles((n * unit as f64) as u64))
}

fn parse_addr(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| format!("bad address '{s}'"))
}

/// Scanner width, restricted to what `scan.rs` supports.
fn parse_scan_width(s: &str) -> Result<u8, String> {
    match s {
        "1" | "2" | "4" => Ok(s.parse().unwrap()),
        _ => Err(format!("bad width '{s}' (1, 2 or 4)")),
    }
}

/// Largest reply a `peekb`/`peekm` will encode, per command (summed over
/// ranges for `peekm`). Bounds the base64 line the client has to buffer.
const PEEK_BYTES_MAX: u32 = 2 * 1024 * 1024;

/// Standard base64 with `=` padding.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (b0 as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Inverse of [`base64_encode`], used only by the tests that round-trip it.
#[cfg(test)]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.trim_end_matches('=');
    let mut bits: u32 = 0;
    let mut nbits: u32 = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

/// Read `len` bytes starting at `addr`, one `peek8` at a time, failing on
/// the first byte that is not RAM/scratchpad/BIOS (base64 has no room for a
/// per-byte "unmapped" marker).
fn read_range(sys: &PsxSystem, addr: u32, len: u32) -> Result<Vec<u8>, String> {
    (0..len)
        .map(|i| {
            let a = addr.wrapping_add(i);
            sys.peek8(a)
                .ok_or_else(|| format!("address {a:#010x} not readable"))
        })
        .collect()
}

/// Encode top-down RGB24 pixels as a PNG, for `frameb png`.
fn encode_png(out: &mut Vec<u8>, w: u32, h: u32, rgb: &[u8]) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(out, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgb)
}

/// Cycles allowed for the BIOS to reach the shell entry on `loadexe`. A
/// retail image gets there in about 80 million; well past that means the
/// image is not going to arrive, and the port must not wedge waiting.
const BOOT_CAP: u64 = 200_000_000;

pub struct Reply {
    pub ok: bool,
    /// Payload lines (without the status line or terminator).
    pub payload: String,
    pub quit: bool,
}

impl Reply {
    fn ok(payload: impl Into<String>) -> Self {
        Reply {
            ok: true,
            payload: payload.into(),
            quit: false,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Reply {
            ok: false,
            payload: msg.into(),
            quit: false,
        }
    }
}

/// Number of in-memory save-state slots addressable as `@0`..`@15`.
const STATE_SLOTS: usize = 16;

/// Ceiling on `scan list <max>`, so a huge max cannot build a reply covering
/// the whole of RAM; the header already reports the true hit count.
const SCAN_LIST_MAX: usize = 4096;

/// Command executor: protocol state independent of the transport, so the
/// whole command surface is unit-testable without sockets.
pub struct Controller {
    /// Buttons held across `run` commands (`input set`).
    held: u16,
    /// Monotonic TTY position already returned by `tty`.
    tty_read: u64,
    frames_run: u64,
    /// The `.cht` the current cheats came from, so `cheat on|off` can
    /// write the enable marker back and `cheat reload` can re-read it.
    /// `None` when no disc with a cheat file has been opened.
    cheat_file: Option<PathBuf>,
    /// In-memory save-state slots `@0`..`@15`. `psxctl` opens a new TCP
    /// connection per command, so a slot cleared on reconnect would be
    /// unusable from it; slots therefore persist until overwritten or the
    /// process exits, never on reconnect.
    slots: Vec<Option<Vec<u8>>>,
    /// Scanner session; persists across client connections, since `psxctl`
    /// opens a new TCP connection per command and a session cleared on
    /// reconnect would be unusable from it.
    scan: Option<crate::scan::Scan>,
}

impl Default for Controller {
    fn default() -> Self {
        Controller {
            held: 0,
            tty_read: 0,
            frames_run: 0,
            cheat_file: None,
            slots: vec![None; STATE_SLOTS],
            scan: None,
        }
    }
}

/// Parse `@<n>` (0-15) into a slot index.
fn parse_slot(s: &str) -> Option<usize> {
    let n: usize = s.strip_prefix('@')?.parse().ok()?;
    (n < STATE_SLOTS).then_some(n)
}

impl Controller {
    /// Point `cheat on|off|reload` at the sidecar for a disc that was
    /// opened before the server started, i.e. from `--disc`.
    pub fn set_cheat_file(&mut self, path: PathBuf) {
        self.cheat_file = Some(path);
    }

    /// Write the enable markers back to the `.cht`. The file is the only
    /// record of what is on, so a toggle that cannot be saved has to say
    /// so rather than look like it stuck.
    fn save_cheats(&self, sys: &PsxSystem) -> String {
        let Some(path) = &self.cheat_file else {
            return " (in memory only: no cheat file)".into();
        };
        match std::fs::write(path, sys.cheats().to_text()) {
            Ok(()) => String::new(),
            Err(e) => format!(" (not saved: {e})"),
        }
    }

    /// Advance emulation, keeping the held-button state applied; returns
    /// cycles elapsed.
    fn advance(&mut self, sys: &mut PsxSystem, len: RunLength) -> u64 {
        sys.set_buttons(self.held);
        let before = sys.cycles();
        match len {
            RunLength::Cycles(cycles) => {
                sys.run_cycles(cycles);
                self.frames_run += cycles / CYCLES_PER_FRAME;
            }
            RunLength::Vblanks(n) => {
                sys.run_vblanks(n);
                self.frames_run = self.frames_run.saturating_add(n);
            }
        }
        sys.cycles() - before
    }

    /// Run one scanner pass, the same rule the GUI scanner uses
    /// (`crate::scan::Scan::pass`), and report the hit count.
    fn scan_pass(&mut self, sys: &PsxSystem, req: crate::scan::Request) -> Reply {
        let (scan, outcome) = crate::scan::Scan::pass(self.scan.take(), req, sys.ram());
        self.scan = Some(scan);
        Reply::ok(format!("{} hits (width {})", outcome.count, outcome.width))
    }

    /// List the current scan's candidates, addressed the way `peek` expects
    /// them back (KSEG0), current value and value at the last pass.
    fn scan_list(&self, sys: &PsxSystem, max: usize) -> Reply {
        let Some(scan) = &self.scan else {
            return Reply::err("no scan (use scan start)");
        };
        let hits = scan.list(sys.ram(), max);
        let digits = 2 * scan.width() as usize;
        let mut out = format!("{} hits, showing {}\n", scan.count(), hits.len());
        for (addr, value, previous) in hits {
            out.push_str(&format!(
                "{:08x} {value:0digits$x} {previous:0digits$x}\n",
                0x8000_0000u32 + addr
            ));
        }
        Reply::ok(out.trim_end().to_string())
    }

    /// Read a PS-X EXE and hand it to the machine, optionally booting the
    /// BIOS to the shell entry first.
    fn load_exe(&mut self, sys: &mut PsxSystem, path: &str, wait: bool) -> Reply {
        let exe = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => return Reply::err(format!("read {path}: {e}")),
        };
        let mut waited = 0;
        if wait {
            let before = sys.cycles();
            if !sys.run_until_pc(SHELL_ENTRY, BOOT_CAP) {
                let pc = sys.cpu.pc;
                return Reply::err(format!(
                    "no shell entry in {BOOT_CAP} cycles (pc={pc:#010x}); append `now` if already booted"
                ));
            }
            waited = sys.cycles() - before;
            self.frames_run += waited / CYCLES_PER_FRAME;
        }
        match sys.load_exe(&exe) {
            Ok(()) => Reply::ok(format!(
                "loaded {} bytes from {path}, pc={:#010x}{}",
                exe.len(),
                sys.cpu.pc,
                if wait {
                    format!(", booted in {waited} cycles")
                } else {
                    String::new()
                }
            )),
            Err(e) => Reply::err(e),
        }
    }

    pub fn execute(&mut self, sys: &mut PsxSystem, line: &str, debugger_owns: bool) -> Reply {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let args: Vec<&str> = words.collect();
        // The debugger and the control port must not both drive execution
        // (loadstate mutates it just as much as running does).
        if debugger_owns && matches!(cmd, "run" | "press" | "loadstate" | "loadexe" | "reset") {
            return Reply::err("debugger attached; execution is owned by the debugger");
        }
        match (cmd, args.as_slice()) {
            ("help", _) => Reply::ok(HELP.trim_end()),
            ("state", _) => {
                let frame = &sys.gpu().frame;
                let video = if sys.gpu().is_pal() { "PAL" } else { "NTSC" };
                let field_hz =
                    CPU_CLOCK_HZ as f64 / sys.gpu().video_timing().cycles_per_frame() as f64;
                Reply::ok(format!(
                    "pc={:#010x} cycles={} frames={} vblanks={} video={video} field_hz={field_hz:.2} held={} display={}x{}{}",
                    sys.cpu.pc,
                    sys.cycles(),
                    self.frames_run,
                    sys.vblanks(),
                    buttons_to_names(self.held),
                    frame.width,
                    frame.height,
                    if frame.enabled { "" } else { " (disabled)" },
                ))
            }
            ("run", [dur]) => match parse_duration(dur) {
                Ok(len @ RunLength::Cycles(cycles)) => {
                    self.advance(sys, len);
                    Reply::ok(format!("ran {cycles} cycles, pc={:#010x}", sys.cpu.pc))
                }
                Ok(len @ RunLength::Vblanks(n)) => {
                    let cycles = self.advance(sys, len);
                    Reply::ok(format!(
                        "ran {n} vblanks ({cycles} cycles), vblanks={}, pc={:#010x}",
                        sys.vblanks(),
                        sys.cpu.pc
                    ))
                }
                Err(e) => Reply::err(e),
            },
            ("press", [buttons, dur]) => match (parse_buttons(buttons), parse_duration(dur)) {
                (Ok(mask), Ok(len)) => {
                    let prev = self.held;
                    self.held |= mask;
                    let cycles = self.advance(sys, len);
                    self.held = prev;
                    sys.set_buttons(self.held);
                    Reply::ok(format!(
                        "pressed {} for {cycles} cycles",
                        buttons_to_names(mask)
                    ))
                }
                (Err(e), _) | (_, Err(e)) => Reply::err(e),
            },
            ("input", ["set", buttons]) => match parse_buttons(buttons) {
                Ok(mask) => {
                    self.held = mask;
                    sys.set_buttons(mask);
                    Reply::ok(format!("holding {}", buttons_to_names(mask)))
                }
                Err(e) => Reply::err(e),
            },
            ("input", ["clear"]) => {
                self.held = 0;
                sys.set_buttons(0);
                Reply::ok("holding none")
            }
            // Power-cycle: `PsxSystem::reset` keeps disc, memory card, BIOS,
            // TTY and cheats (they are ambient), but held buttons are the
            // control port's own state and must not survive into the fresh
            // machine.
            ("reset", []) => {
                sys.reset();
                self.held = 0;
                sys.set_buttons(0);
                self.frames_run = 0;
                Reply::ok(format!("reset, pc={:#010x}", sys.cpu.pc))
            }
            ("peek", [addr, len]) => {
                let (addr, len) = match (parse_addr(addr), len.parse::<u32>()) {
                    (Ok(a), Ok(l)) if l <= 4096 => (a, l),
                    (Err(e), _) => return Reply::err(e),
                    _ => return Reply::err("bad length (max 4096)"),
                };
                let mut out = String::new();
                for base in (0..len).step_by(16) {
                    let row: Vec<String> = (base..(base + 16).min(len))
                        .map(|i| match sys.peek8(addr.wrapping_add(i)) {
                            Some(b) => format!("{b:02x}"),
                            None => "--".into(),
                        })
                        .collect();
                    out.push_str(&format!(
                        "{:#010x}: {}\n",
                        addr.wrapping_add(base),
                        row.join(" ")
                    ));
                }
                Reply::ok(out.trim_end().to_string())
            }
            ("peekb", [addr, len]) => {
                let (addr, len) = match (parse_addr(addr), len.parse::<u32>()) {
                    (Ok(a), Ok(l)) if l > 0 && l <= PEEK_BYTES_MAX => (a, l),
                    (Err(e), _) => return Reply::err(e),
                    _ => return Reply::err(format!("bad length (1-{PEEK_BYTES_MAX})")),
                };
                match read_range(sys, addr, len) {
                    Ok(bytes) => Reply::ok(base64_encode(&bytes)),
                    Err(e) => Reply::err(e),
                }
            }
            ("peekm", []) => Reply::err("peekm needs at least one <hexaddr>:<len>"),
            ("peekm", ranges @ [_, ..]) => {
                // Validate and size every range before reading any of them,
                // so a bad range further down the list changes nothing.
                let mut parsed = Vec::with_capacity(ranges.len());
                let mut total: u64 = 0;
                for r in ranges {
                    let Some((addr, len)) = r.split_once(':') else {
                        return Reply::err(format!("bad range '{r}' (want <hexaddr>:<len>)"));
                    };
                    let addr = match parse_addr(addr) {
                        Ok(a) => a,
                        Err(e) => return Reply::err(e),
                    };
                    let len: u32 = match len.parse() {
                        Ok(l) if l > 0 => l,
                        _ => return Reply::err(format!("bad length (1-{PEEK_BYTES_MAX})")),
                    };
                    total += len as u64;
                    parsed.push((addr, len));
                }
                if total > PEEK_BYTES_MAX as u64 {
                    return Reply::err(format!("bad length (max {PEEK_BYTES_MAX})"));
                }
                let mut lines = Vec::with_capacity(parsed.len());
                for (addr, len) in parsed {
                    match read_range(sys, addr, len) {
                        Ok(bytes) => lines.push(base64_encode(&bytes)),
                        Err(e) => return Reply::err(e),
                    }
                }
                Reply::ok(lines.join("\n"))
            }
            ("poke", [addr, hex]) => {
                let addr = match parse_addr(addr) {
                    Ok(a) => a,
                    Err(e) => return Reply::err(e),
                };
                if hex.len() % 2 != 0 {
                    return Reply::err("odd hex length");
                }
                let bytes: Option<Vec<u8>> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                    .collect();
                let Some(bytes) = bytes else {
                    return Reply::err("bad hex");
                };
                for (i, b) in bytes.iter().enumerate() {
                    if !sys.poke8(addr.wrapping_add(i as u32), *b) {
                        return Reply::err(format!(
                            "address {:#010x} not writable",
                            addr.wrapping_add(i as u32)
                        ));
                    }
                }
                Reply::ok(format!("wrote {} bytes", bytes.len()))
            }
            // Observation, not execution: a scan never advances the machine,
            // so it is not gated by `debugger_owns`.
            ("scan", ["start", w]) | ("scan", ["start", w, "unknown"]) => match parse_scan_width(w)
            {
                Ok(width) => self.scan_pass(
                    sys,
                    crate::scan::Request {
                        width,
                        filter: crate::scan::Filter::Unknown,
                        restart: true,
                    },
                ),
                Err(e) => Reply::err(e),
            },
            ("scan", ["start", w, "exact", v]) => {
                match (parse_scan_width(w), crate::scan::parse_value(v)) {
                    (Ok(width), Some(value)) => self.scan_pass(
                        sys,
                        crate::scan::Request {
                            width,
                            filter: crate::scan::Filter::Exact(value),
                            restart: true,
                        },
                    ),
                    (Err(e), _) => Reply::err(e),
                    (_, None) => Reply::err(format!("bad value '{v}'")),
                }
            }
            ("scan", ["filter", "exact", v]) => {
                let Some(width) = self.scan.as_ref().map(|s| s.width()) else {
                    return Reply::err("no scan (use scan start)");
                };
                match crate::scan::parse_value(v) {
                    Some(value) => self.scan_pass(
                        sys,
                        crate::scan::Request {
                            width,
                            filter: crate::scan::Filter::Exact(value),
                            restart: false,
                        },
                    ),
                    None => Reply::err(format!("bad value '{v}'")),
                }
            }
            ("scan", ["filter", f]) => {
                let Some(width) = self.scan.as_ref().map(|s| s.width()) else {
                    return Reply::err("no scan (use scan start)");
                };
                let filter = match *f {
                    "changed" => crate::scan::Filter::Changed,
                    "unchanged" => crate::scan::Filter::Unchanged,
                    "increased" => crate::scan::Filter::Increased,
                    "decreased" => crate::scan::Filter::Decreased,
                    _ => return Reply::err(format!("bad filter '{f}'")),
                };
                self.scan_pass(
                    sys,
                    crate::scan::Request {
                        width,
                        filter,
                        restart: false,
                    },
                )
            }
            ("scan", ["list"]) => self.scan_list(sys, 100),
            ("scan", ["list", max]) => match max.parse::<usize>() {
                Ok(max) => self.scan_list(sys, max.min(SCAN_LIST_MAX)),
                Err(_) => Reply::err(format!("bad max '{max}'")),
            },
            ("scan", ["clear"]) => {
                self.scan = None;
                Reply::ok("scan cleared")
            }
            // Disc swap, split into the two halves a real drive has, so a
            // script can leave the lid open across several `run`s and watch
            // how the game reacts before the new disc goes in.
            ("disc", ["open"]) => {
                sys.open_shell();
                Reply::ok("drive open")
            }
            ("disc", ["close"]) => {
                sys.close_shell(None);
                Reply::ok("drive closed")
            }
            ("disc", ["close", path]) => match crate::disc::load_disc(std::path::Path::new(path)) {
                Ok(loaded) => {
                    let cheats = loaded.cheats.cheats.len();
                    self.cheat_file = Some(crate::disc::cheat_path(std::path::Path::new(path)));
                    sys.set_cheats(loaded.cheats);
                    sys.close_shell(Some(loaded.disc));
                    Reply::ok(format!(
                        "drive closed on {} ({path}); {cheats} cheats",
                        loaded.info.title
                    ))
                }
                Err(e) => Reply::err(e),
            },
            ("cheat", ["apply", state @ ("on" | "off")]) => {
                sys.set_cheats_enabled(*state == "on");
                Reply::ok(format!("cheats {state}"))
            }
            ("cheat", ["list"]) => {
                let mut out = format!(
                    "apply: {}
",
                    if sys.cheats_enabled() { "on" } else { "off" }
                );
                for (i, cheat) in sys.cheats().cheats.iter().enumerate() {
                    let mark = if cheat.enabled { "on " } else { "off" };
                    let partial = if cheat.has_unsupported() {
                        " (partial)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "{i:>3}  {mark}  {} [{} codes]{partial}
",
                        cheat.name,
                        cheat.codes.len()
                    ));
                }
                Reply::ok(out.trim_end())
            }
            ("cheat", [state @ ("on" | "off"), index]) => {
                let Ok(i) = index.parse::<usize>() else {
                    return Reply::err(format!("bad cheat index '{index}'"));
                };
                let mut list = sys.cheats().clone();
                let Some(cheat) = list.cheats.get_mut(i) else {
                    return Reply::err(format!("no cheat {i} (see `cheat list`)"));
                };
                cheat.enabled = *state == "on";
                let name = cheat.name.clone();
                sys.set_cheats(list);
                let saved = self.save_cheats(sys);
                Reply::ok(format!("cheat {i} '{name}' {state}{saved}"))
            }
            ("cheat", ["reload"]) => {
                let Some(path) = self.cheat_file.clone() else {
                    return Reply::err("no cheat file (open a disc first)");
                };
                let list = crate::disc::load_cheats(&path);
                let n = list.cheats.len();
                sys.set_cheats(list);
                Reply::ok(format!("{n} cheats from {}", path.display()))
            }
            ("tty", _) => {
                let (new, pos) = sys.tty_since(self.tty_read);
                let new = new.to_string();
                self.tty_read = pos;
                Reply::ok(new)
            }
            ("frame", [path]) => {
                let frame = &sys.gpu().frame;
                if frame.width == 0 || frame.height == 0 {
                    return Reply::err("no frame captured yet (run at least one frame)");
                }
                match crate::write_frame_bmp(
                    path,
                    frame.width,
                    frame.height,
                    frame.stride,
                    frame.is_24bit,
                    &frame.pixels,
                ) {
                    Ok(()) => Reply::ok(format!("{}x{} -> {path}", frame.width, frame.height)),
                    Err(e) => Reply::err(format!("write {path}: {e}")),
                }
            }
            ("frameb", fmt_arg @ ([] | [_])) => {
                let fmt = fmt_arg.first().copied().unwrap_or("rgb24");
                if fmt != "rgb24" && fmt != "png" {
                    return Reply::err(format!("bad format '{fmt}' (rgb24 or png)"));
                }
                let frame = &sys.gpu().frame;
                if frame.width == 0 || frame.height == 0 {
                    return Reply::err("no frame captured yet (run at least one frame)");
                }
                let (w, h) = (frame.width, frame.height);
                let rgb = crate::frame_rgb24(w, h, frame.stride, frame.is_24bit, &frame.pixels);
                if fmt == "rgb24" {
                    Reply::ok(format!("{w} {h} rgb24\n{}", base64_encode(&rgb)))
                } else {
                    let mut png_bytes = Vec::new();
                    match encode_png(&mut png_bytes, w, h, &rgb) {
                        Ok(()) => Reply::ok(format!("{w} {h} png\n{}", base64_encode(&png_bytes))),
                        Err(e) => Reply::err(format!("png encode: {e}")),
                    }
                }
            }
            ("vram", [path]) => {
                crate::write_vram_bmp(path, &sys.gpu().vram);
                Reply::ok(format!("1024x512 -> {path}"))
            }
            ("savestate", [path]) if path.starts_with('@') => match parse_slot(path) {
                Some(slot) => match sys.save_state() {
                    Ok(data) => {
                        let len = data.len();
                        self.slots[slot] = Some(data);
                        Reply::ok(format!("saved {len} bytes -> @{slot}"))
                    }
                    Err(e) => Reply::err(e),
                },
                None => Reply::err(format!("bad slot '{path}' (0-15)")),
            },
            ("savestate", [path]) => match sys.save_state() {
                Ok(data) => match std::fs::write(path, &data) {
                    Ok(()) => Reply::ok(format!("saved {} bytes -> {path}", data.len())),
                    Err(e) => Reply::err(format!("write {path}: {e}")),
                },
                Err(e) => Reply::err(e),
            },
            ("loadstate", [path]) if path.starts_with('@') => match parse_slot(path) {
                Some(slot) => match &self.slots[slot] {
                    Some(data) => match sys.load_state(data) {
                        Ok(()) => Reply::ok(format!("loaded @{slot}, pc={:#010x}", sys.cpu.pc)),
                        Err(e) => Reply::err(e),
                    },
                    None => Reply::err(format!("slot @{slot} is empty")),
                },
                None => Reply::err(format!("bad slot '{path}' (0-15)")),
            },
            ("loadstate", [path]) => match std::fs::read(path) {
                Ok(data) => match sys.load_state(&data) {
                    Ok(()) => Reply::ok(format!("loaded, pc={:#010x}", sys.cpu.pc)),
                    Err(e) => Reply::err(e),
                },
                Err(e) => Reply::err(format!("read {path}: {e}")),
            },
            // Side-load a program over the running machine, the shortcut the
            // shell would take after reading it off a disc. `now` skips the
            // boot wait for a machine the caller has already run past it.
            ("loadexe", [path]) => self.load_exe(sys, path, true),
            ("loadexe", [path, "now"]) => self.load_exe(sys, path, false),
            ("quit", _) => Reply {
                ok: true,
                payload: "bye".into(),
                quit: true,
            },
            _ => Reply::err(format!("unknown command '{line}' (try 'help')")),
        }
    }
}

const HELP: &str = "\
state                 pc, cycles, frames run, vblanks, video standard and
                      field rate, held buttons, display mode
run <n>[s|c|v]        advance n frames (s=seconds, c=cycles, v=vblanks:
                      stop right after the edge), inputs held
press <BTN+BTN> <n>[s|c|v]
                      hold buttons for n frames on top of held set, release
input set <BTN+BTN>   hold buttons until changed (applied during run)
input clear           release all held buttons
reset                 power-cycle: disc and memory card stay in, held buttons cleared
frame <path>          dump the latched display frame as BMP
frameb [rgb24|png]    latched display frame over the socket: <w> <h> <fmt>
                      then one base64 line (top-down RGB24, or a PNG file)
vram <path>           dump full 1024x512 VRAM as BMP
peek <hexaddr> <len>  hex dump memory (side-effect-free, MMIO shows --)
peekb <hexaddr> <len> memory as one base64 line (up to 2 MiB; err if any
                      byte is not RAM/scratchpad/BIOS)
peekm <hexaddr>:<len> ...
                      one base64 line per range, 2 MiB in total, same rule
                      as peekb
poke <hexaddr> <hex>  write bytes to RAM/scratchpad
scan start <1|2|4> [exact <value>|unknown]
                      new RAM scan (value: decimal or 0x hex, unsigned);
                      reports the hit count
scan filter exact <value>|changed|unchanged|increased|decreased
                      narrow the candidates against the last pass
scan list [max]       `<addr> <value> <previous>` per hit, hex, default
                      100, capped at 4096; previous = value at the last pass
scan clear            drop the scan session (it otherwise survives
                      reconnects)
disc open             open the drive lid (stops the drive, flags shell open)
disc close [path]     close the lid, on a new image if given, else the old one
cheat list            cheats from the disc's .cht, with their enable state
cheat apply on|off    master switch (off unless `cheats = true` in the config)
cheat on|off <n>      toggle cheat n and write the marker back to the file
cheat reload          re-read the .cht for the disc in the drive
tty                   TTY output accumulated since the last `tty`
loadexe <path> [now]  side-load a PS-X EXE (boots to the shell first unless
                      `now`, for a machine already run past it)
savestate <path>|@<n>
                      snapshot the full machine state to a file, or to
                      in-memory slot n (0-15)
loadstate <path>|@<n>
                      restore a snapshot (BIOS/disc/memcard carry over)
quit                  shut the emulator down
";

/// Ceiling on the blocking reply write in [`ControlServer::pump`], so a
/// client that never drains its socket gets dropped instead of wedging the
/// emulator forever.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// TCP transport: accepts one client at a time, reads newline-terminated
/// commands, writes dot-terminated replies.
pub struct ControlServer {
    listener: TcpListener,
    client: Option<TcpStream>,
    buf: Vec<u8>,
    pub controller: Controller,
}

impl ControlServer {
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let server = Self {
            listener,
            client: None,
            buf: Vec::new(),
            controller: Controller::default(),
        };
        info!("control port listening on {}", server.local_addr()?);
        Ok(server)
    }

    /// Address the listener bound to (tests bind port 0 and read back the
    /// assigned one).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Service the connection; executes at most one command per call.
    /// Returns false once a `quit` command has been executed.
    pub fn pump(&mut self, sys: &mut PsxSystem, debugger_owns: bool) -> bool {
        if self.client.is_none() {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).ok();
                    stream.set_nodelay(true).ok();
                    self.client = Some(stream);
                    self.buf.clear();
                }
                Err(_) => return true, // includes WouldBlock: nothing to do
            }
        }
        let Some(stream) = &mut self.client else {
            return true;
        };
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    self.client = None;
                    return true;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.client = None;
                    return true;
                }
            }
        }
        let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else {
            return true;
        };
        let line: Vec<u8> = self.buf.drain(..nl + 1).collect();
        let line = String::from_utf8_lossy(&line).trim().to_string();
        if line.is_empty() {
            return true;
        }
        let reply = self.controller.execute(sys, &line, debugger_owns);
        let mut out = String::new();
        out.push_str(if reply.ok { "ok\n" } else { "err\n" });
        for l in reply.payload.lines() {
            // Dot-stuff payload lines so `.` can never terminate early.
            if l.starts_with('.') {
                out.push('.');
            }
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(".\n");
        // Blocking write: under lockstep the client is always reading its
        // reply, so this costs nothing, and it is the only way a multi-MB
        // reply (peekb/peekm/frameb) survives instead of hitting WouldBlock
        // mid-write and dropping the client. A write timeout bounds the
        // remaining risk, a client that connects and never reads its
        // reply: instead of wedging the emulator forever, that client gets
        // dropped after `WRITE_TIMEOUT`.
        if let Some(stream) = &mut self.client {
            stream.set_nonblocking(false).ok();
            stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
            let sent = stream.write_all(out.as_bytes());
            stream.set_write_timeout(None).ok();
            stream.set_nonblocking(true).ok();
            if sent.is_err() {
                self.client = None;
            }
        }
        !reply.quit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> PsxSystem {
        PsxSystem::new(vec![0; 512 * 1024]).unwrap()
    }

    /// Poke a program into RAM and point the CPU at it.
    fn load_program(sys: &mut PsxSystem, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            for (j, b) in w.to_le_bytes().iter().enumerate() {
                assert!(sys.poke8(0x8001_0000 + (i * 4 + j) as u32, *b));
            }
        }
        sys.cpu.set_pc(0x8001_0000);
    }

    /// GP0(02h) fill 16x16 red at (0,0), then spin.
    const FILL_PROGRAM: [u32; 11] = [
        0x3c08_1f80, // lui   $t0, 0x1f80
        0x3508_1810, // ori   $t0, $t0, 0x1810      GP0
        0x3c09_0200, // lui   $t1, 0x0200
        0x3529_00ff, // ori   $t1, $t1, 0x00ff      fill, colour R=0xff
        0xad09_0000, // sw    $t1, 0($t0)
        0xad00_0000, // sw    $zero, 0($t0)         top-left (0,0)
        0x3c09_0010, // lui   $t1, 0x0010
        0x3529_0010, // ori   $t1, $t1, 0x0010      16x16
        0xad09_0000, // sw    $t1, 0($t0)
        0x0800_4009, // loop: j loop                (0x80010024)
        0x0000_0000, // nop
    ];

    /// GP1(08h) with the PAL bit, then spin.
    const PAL_PROGRAM: [u32; 7] = [
        0x3c08_1f80, // lui   $t0, 0x1f80
        0x3508_1814, // ori   $t0, $t0, 0x1814      GP1
        0x3c09_0800, // lui   $t1, 0x0800
        0x3529_0008, // ori   $t1, $t1, 0x0008      display mode 320x240, PAL
        0xad09_0000, // sw    $t1, 0($t0)
        0x0800_4005, // loop: j loop                (0x80010014)
        0x0000_0000, // nop
    ];

    /// Cost of one uncached nop from KSEG1 with the reset-value bus delays:
    /// the most a single instruction can overshoot a vblank deadline by,
    /// since the step that crosses it always completes first.
    fn measure_slack() -> u64 {
        let mut probe = sys();
        probe.step();
        probe.cycles()
    }

    #[test]
    fn run_advances_by_frames() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let r = c.execute(&mut sys, "run 2", false);
        assert!(r.ok, "{}", r.payload);
        assert!(sys.cycles() >= 2 * CYCLES_PER_FRAME);
        assert!(
            c.execute(&mut sys, "state", false)
                .payload
                .contains("frames=2")
        );
    }

    #[test]
    fn run_v_advances_exactly_one_vblank() {
        let slack = measure_slack();
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "run 1v", false).ok);
        assert_eq!(sys.vblanks(), 1);

        let before = sys.cycles();
        assert!(c.execute(&mut sys, "run 1v", false).ok);
        assert_eq!(sys.vblanks(), 2);
        let cpf = psx_core::gpu::VideoTiming::NTSC.cycles_per_frame();
        let delta = sys.cycles() - before;
        assert!(cpf <= delta && delta < cpf + slack, "delta={delta}");

        let r = c.execute(&mut sys, "state", false);
        assert!(r.payload.contains("vblanks=2"), "{}", r.payload);
        assert!(r.payload.contains("video=NTSC"), "{}", r.payload);
        assert!(r.payload.contains("field_hz=59.81"), "{}", r.payload);
        assert!(r.payload.contains("frames=2"), "{}", r.payload);
    }

    #[test]
    fn run_v_follows_pal_timing() {
        let slack = measure_slack();
        let (mut sys, mut c) = (sys(), Controller::default());
        load_program(&mut sys, &PAL_PROGRAM);

        // This vblank was scheduled with NTSC timing before the mode write.
        assert!(c.execute(&mut sys, "run 1v", false).ok);
        let before = sys.cycles();
        assert!(c.execute(&mut sys, "run 1v", false).ok);
        let pal_cpf = psx_core::gpu::VideoTiming::PAL.cycles_per_frame();
        let delta = sys.cycles() - before;
        assert!(pal_cpf <= delta && delta < pal_cpf + slack, "delta={delta}");

        let r = c.execute(&mut sys, "state", false);
        assert!(r.payload.contains("video=PAL"), "{}", r.payload);
        assert!(r.payload.contains("field_hz=49.75"), "{}", r.payload);
    }

    #[test]
    fn press_restores_held_set() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "input set UP", false).ok);
        let r = c.execute(&mut sys, "press CROSS+START 1", false);
        assert!(r.ok, "{}", r.payload);
        // After the press, only the held set remains applied.
        assert_eq!(sys.sio().buttons, psx_core::sio::button::UP);
        assert!(c.execute(&mut sys, "input clear", false).ok);
        assert_eq!(sys.sio().buttons, 0);
    }

    #[test]
    fn press_accepts_vblank_lengths() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "input set UP", false).ok);
        let r = c.execute(&mut sys, "press CROSS 1v", false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(sys.vblanks(), 1);
        assert_eq!(sys.sio().buttons, psx_core::sio::button::UP);
    }

    #[test]
    fn peek_poke_round_trip() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "poke 80100000 deadbeef", false).ok);
        let r = c.execute(&mut sys, "peek 80100000 4", false);
        assert!(r.payload.contains("de ad be ef"), "{}", r.payload);
        // MMIO reads render as -- instead of touching the device.
        let r = c.execute(&mut sys, "peek 1f801800 4", false);
        assert!(r.payload.contains("--"), "{}", r.payload);
        // ROM is not writable.
        assert!(!c.execute(&mut sys, "poke bfc00000 ff", false).ok);
    }

    /// The whole cheat feature, end to end without a game: load a code,
    /// run a frame, and see the write land where `peek` can read it.
    #[test]
    fn a_cheat_applies_once_the_machine_reaches_a_vblank() {
        let (mut sys, mut c) = (sys(), Controller::default());
        sys.set_cheats(psx_core::cheats::CheatList::parse(
            "[*Health]
80100000 0063
[Lives]
30100004 0009
",
        ));

        let r = c.execute(&mut sys, "cheat list", false);
        assert!(r.payload.contains("apply: on"), "{}", r.payload);
        assert!(r.payload.contains("on "), "{}", r.payload);
        assert!(r.payload.contains("Health"), "{}", r.payload);

        // Only the enabled one fires.
        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, "peek 80100000 8", false);
        assert!(r.payload.contains("63 00"), "{}", r.payload);
        assert!(r.payload.contains("00 00 00 00"), "{}", r.payload);

        // Turning the second one on makes it fire on the next frame.
        assert!(c.execute(&mut sys, "cheat on 1", false).ok);
        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, "peek 80100004 1", false);
        assert!(r.payload.contains("09"), "{}", r.payload);

        // The master switch stops everything without touching the list.
        assert!(c.execute(&mut sys, "cheat apply off", false).ok);
        assert!(c.execute(&mut sys, "poke 80100000 00", false).ok);
        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, "peek 80100000 1", false);
        assert!(r.payload.contains("00"), "{}", r.payload);
        assert!(c.execute(&mut sys, "cheat apply on", false).ok);

        // And off again stops it: poke over the value, run, still ours.
        assert!(c.execute(&mut sys, "cheat off 1", false).ok);
        assert!(c.execute(&mut sys, "poke 80100004 00", false).ok);
        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, "peek 80100004 1", false);
        assert!(r.payload.contains("00"), "{}", r.payload);
    }

    #[test]
    fn cheat_commands_reject_what_they_cannot_do() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "cheat on 0", false).ok);
        assert!(!c.execute(&mut sys, "cheat on nope", false).ok);
        // No disc has been opened, so there is nothing to reload from.
        assert!(!c.execute(&mut sys, "cheat reload", false).ok);
    }

    #[test]
    fn debugger_owns_execution() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "run 1", true).ok);
        assert!(c.execute(&mut sys, "peek 80000000 4", true).ok); // observation is fine
    }

    #[test]
    fn reset_restarts_the_machine_and_clears_held_buttons() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "input set UP", false).ok);
        assert!(c.execute(&mut sys, "run 2", false).ok);
        let r = c.execute(&mut sys, "reset", false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(sys.cycles(), 0);
        assert_eq!(sys.cpu.pc, 0xbfc0_0000);
        assert_eq!(sys.sio().buttons, 0);
        let r = c.execute(&mut sys, "state", false);
        assert!(r.payload.contains("frames=0"), "{}", r.payload);
        assert!(r.payload.contains("held=none"), "{}", r.payload);
    }

    #[test]
    fn reset_is_refused_while_the_debugger_owns_execution() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "reset", true).ok);
    }

    #[test]
    fn tty_returns_only_new_output() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert_eq!(c.execute(&mut sys, "tty", false).payload, "");
        sys.run_cycles(1000);
        // Zero BIOS produces no TTY; the delta must stay empty, not error.
        assert_eq!(c.execute(&mut sys, "tty", false).payload, "");
    }

    #[test]
    fn unknown_and_malformed_commands_error() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "dance", false).ok);
        assert!(!c.execute(&mut sys, "run zero", false).ok);
        assert!(!c.execute(&mut sys, "run -5", false).ok);
        assert!(!c.execute(&mut sys, "run 0v", false).ok);
        assert!(!c.execute(&mut sys, "run 1.5v", false).ok);
        assert!(!c.execute(&mut sys, "run v", false).ok);
        assert!(!c.execute(&mut sys, "press NOPE 1", false).ok);
        assert!(!c.execute(&mut sys, "peek xyz 4", false).ok);
    }

    /// Minimal but valid PS-X EXE: header, one word of body at `dest`.
    fn exe_image(pc: u32, gp: u32, dest: u32, sp_base: u32, body: &[u8]) -> Vec<u8> {
        let mut exe = vec![0u8; 0x800];
        exe[..8].copy_from_slice(b"PS-X EXE");
        let mut put = |off: usize, v: u32| exe[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(0x10, pc);
        put(0x14, gp);
        put(0x18, dest);
        put(0x1c, body.len() as u32);
        put(0x30, sp_base);
        exe.extend_from_slice(body);
        exe
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ps1e-ctl-test-{}-{tag}", std::process::id()))
    }

    #[test]
    fn loadexe_now_takes_pc_and_registers() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let path = temp_path("probe.exe");
        let p = path.to_str().unwrap();
        let image = exe_image(
            0x8001_0000,
            0x8002_0000,
            0x8001_0000,
            0x801f_ff00,
            &[0xef, 0xbe, 0xad, 0xde],
        );
        std::fs::write(&path, &image).unwrap();

        // `now` skips the boot wait, which a zero BIOS would never satisfy.
        let r = c.execute(&mut sys, &format!("loadexe {p} now"), false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(sys.cpu.pc, 0x8001_0000);
        assert_eq!(sys.cpu.regs[28], 0x8002_0000);
        assert_eq!(sys.cpu.regs[29], 0x801f_ff00);
        assert_eq!(sys.cpu.regs[30], 0x801f_ff00);
        let r = c.execute(&mut sys, "peek 80010000 4", false);
        assert!(r.payload.contains("ef be ad de"), "{}", r.payload);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loadexe_rejects_bad_input() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let path = temp_path("garbage.exe");
        let p = path.to_str().unwrap();
        std::fs::write(&path, b"not an executable").unwrap();

        assert!(!c.execute(&mut sys, &format!("loadexe {p} now"), false).ok);
        assert!(!c.execute(&mut sys, "loadexe /no/such/file now", false).ok);
        // Loading mutates execution, so the debugger owns it exclusively.
        assert!(!c.execute(&mut sys, &format!("loadexe {p} now"), true).ok);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loadexe_reports_a_bios_that_never_reaches_the_shell() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let path = temp_path("wait.exe");
        let p = path.to_str().unwrap();
        std::fs::write(&path, exe_image(0x8001_0000, 0, 0x8001_0000, 0, &[0; 4])).unwrap();

        // The zero BIOS never gets to the shell; the wait must give up and
        // point at the `now` form rather than wedge the port.
        let r = c.execute(&mut sys, &format!("loadexe {p}"), false);
        assert!(!r.ok);
        assert!(r.payload.contains("now"), "{}", r.payload);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn savestate_loadstate_round_trip() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let path = std::env::temp_dir().join(format!("ps1e-ctl-test-{}.sst", std::process::id()));
        let p = path.to_str().unwrap();

        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, &format!("savestate {p}"), false);
        assert!(r.ok, "{}", r.payload);
        let cycles_at_save = sys.cycles();

        assert!(c.execute(&mut sys, "run 1", false).ok);
        assert_ne!(sys.cycles(), cycles_at_save);

        let r = c.execute(&mut sys, &format!("loadstate {p}"), false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(sys.cycles(), cycles_at_save);

        // While a debugger owns execution, loading is refused (saving is ok).
        assert!(c.execute(&mut sys, &format!("savestate {p}"), true).ok);
        assert!(!c.execute(&mut sys, &format!("loadstate {p}"), true).ok);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn slot_savestate_loadstate_round_trip() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "run 1", false).ok);
        let r = c.execute(&mut sys, "savestate @3", false);
        assert!(r.ok, "{}", r.payload);
        let cycles_at_save = sys.cycles();

        assert!(c.execute(&mut sys, "run 1", false).ok);
        assert_ne!(sys.cycles(), cycles_at_save);

        let r = c.execute(&mut sys, "loadstate @3", false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(sys.cycles(), cycles_at_save);

        // While a debugger owns execution, loading is refused (saving is ok).
        assert!(c.execute(&mut sys, "savestate @3", true).ok);
        assert!(!c.execute(&mut sys, "loadstate @3", true).ok);
    }

    #[test]
    fn slot_errors() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "loadstate @4", false).ok);
        assert!(!c.execute(&mut sys, "savestate @16", false).ok);
        assert!(!c.execute(&mut sys, "savestate @x", false).ok);
    }

    #[test]
    fn base64_round_trips() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0xff; 3],
            (0..1000).map(|i| (i % 251) as u8).collect(),
        ];
        for data in cases {
            let encoded = base64_encode(&data);
            assert_eq!(base64_decode(&encoded).unwrap(), data, "{encoded}");
        }
    }

    #[test]
    fn peekb_matches_peek_and_covers_all_of_ram() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "poke 80100000 deadbeef", false).ok);

        let r = c.execute(&mut sys, "peekb 80100000 4", false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(
            base64_decode(r.payload.trim()).unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );

        let r = c.execute(&mut sys, "peekb 80000000 2097152", false);
        assert!(r.ok, "{}", r.payload);
        assert_eq!(base64_decode(r.payload.trim()).unwrap(), sys.ram());

        assert!(!c.execute(&mut sys, "peekb 80000000 2097153", false).ok);
        assert!(!c.execute(&mut sys, "peekb 1f801800 4", false).ok);
        // A zero-length reply would encode to an empty base64 line, which
        // `payload.lines()` drops, breaking the one-line reply contract.
        assert!(!c.execute(&mut sys, "peekb 80100000 0", false).ok);
    }

    #[test]
    fn peekm_returns_one_line_per_range() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "poke 80100000 deadbeef", false).ok);

        let r = c.execute(&mut sys, "peekm 80100000:4 bfc00000:2 1f800000:1", false);
        assert!(r.ok, "{}", r.payload);
        let lines: Vec<&str> = r.payload.lines().collect();
        assert_eq!(lines.len(), 3, "{}", r.payload);
        assert_eq!(base64_decode(lines[0]).unwrap().len(), 4);
        assert_eq!(base64_decode(lines[1]).unwrap().len(), 2);
        assert_eq!(base64_decode(lines[2]).unwrap().len(), 1);

        assert!(!c.execute(&mut sys, "peekm", false).ok);
        assert!(!c.execute(&mut sys, "peekm 1f801800:4", false).ok);
        // Same zero-length rejection as peekb, for each range.
        assert!(!c.execute(&mut sys, "peekm 80100000:0", false).ok);
        assert!(!c.execute(&mut sys, "peekm 80100000:4 bfc00000:0", false).ok);
    }

    /// Regression for the nonblocking `write_all` that dropped the client
    /// mid-reply: this fails against that version because the client sees
    /// the connection close before the base64 line completes.
    #[test]
    fn a_full_ram_reply_reaches_the_client() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpStream;
        use std::time::{Duration, Instant};

        let mut server = ControlServer::bind(0).unwrap();
        let addr = server.local_addr().unwrap();
        let mut sys = sys();

        let handle = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            // A hang here (e.g. a regression in the transport) must fail
            // the test rather than block the run indefinitely.
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream.write_all(b"peekb 80000000 2097152\n").unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut payload = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line.trim_end() == "." {
                    break;
                }
                if line.trim_end() != "ok" {
                    payload.push_str(&line);
                }
            }
            base64_decode(payload.trim_end()).unwrap().len()
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "pump loop did not finish");
            server.pump(&mut sys, false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(handle.join().unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn quit_flag_propagates() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "quit", false).quit);
    }

    #[test]
    fn frameb_matches_the_bmp_written_by_frame() {
        let (mut sys, mut c) = (sys(), Controller::default());
        load_program(&mut sys, &FILL_PROGRAM);
        assert!(c.execute(&mut sys, "run 1", false).ok);

        let r = c.execute(&mut sys, "frameb", false);
        assert!(r.ok, "{}", r.payload);
        let mut lines = r.payload.lines();
        assert_eq!(lines.next().unwrap(), "320 240 rgb24");
        let rgb = base64_decode(lines.next().unwrap()).unwrap();
        assert_eq!(rgb.len(), 320 * 240 * 3);
        assert_eq!(&rgb[..3], &[0xff, 0, 0]);
        let px = |x: usize, y: usize| &rgb[(y * 320 + x) * 3..(y * 320 + x) * 3 + 3];
        assert_eq!(px(100, 100), &[0, 0, 0]);

        let path = temp_path("frameb.bmp");
        let p = path.to_str().unwrap();
        assert!(c.execute(&mut sys, &format!("frame {p}"), false).ok);
        let bmp = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let (w, h) = (320usize, 240usize);
        let pad = (4 - (w * 3) % 4) % 4;
        let row_bytes = w * 3 + pad;
        for y in 0..h {
            let bmp_row = &bmp[54 + (h - 1 - y) * row_bytes..54 + (h - 1 - y) * row_bytes + w * 3];
            let rgb_row = &rgb[y * w * 3..(y + 1) * w * 3];
            for x in 0..w {
                let bgr = &bmp_row[x * 3..x * 3 + 3];
                let rgb_px = &rgb_row[x * 3..x * 3 + 3];
                assert_eq!(
                    [bgr[2], bgr[1], bgr[0]],
                    [rgb_px[0], rgb_px[1], rgb_px[2]],
                    "row {y} col {x}"
                );
            }
        }
    }

    #[test]
    fn frameb_png_decodes_to_the_same_pixels() {
        let (mut sys, mut c) = (sys(), Controller::default());
        load_program(&mut sys, &FILL_PROGRAM);
        assert!(c.execute(&mut sys, "run 1", false).ok);

        let r = c.execute(&mut sys, "frameb", false);
        assert!(r.ok, "{}", r.payload);
        let mut lines = r.payload.lines();
        lines.next();
        let rgb = base64_decode(lines.next().unwrap()).unwrap();

        let r = c.execute(&mut sys, "frameb png", false);
        assert!(r.ok, "{}", r.payload);
        let mut lines = r.payload.lines();
        assert_eq!(lines.next().unwrap(), "320 240 png");
        let png_bytes = base64_decode(lines.next().unwrap()).unwrap();

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..info.buffer_size()], rgb.as_slice());
    }

    #[test]
    fn frameb_rejects_unknown_formats() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "frameb", false).ok);
        load_program(&mut sys, &FILL_PROGRAM);
        assert!(c.execute(&mut sys, "run 1", false).ok);
        assert!(!c.execute(&mut sys, "frameb bmp", false).ok);
    }

    #[test]
    fn scan_over_the_control_port_matches_the_scanner() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "poke 80100000 64000000", false).ok);
        assert!(c.execute(&mut sys, "poke 80100040 64000000", false).ok);

        let r = c.execute(&mut sys, "scan start 4 exact 100", false);
        assert_eq!(r.payload, "2 hits (width 4)");

        assert!(c.execute(&mut sys, "poke 80100000 5a000000", false).ok);
        let r = c.execute(&mut sys, "scan filter decreased", false);
        assert_eq!(r.payload, "1 hits (width 4)");

        // Right after a pass, the snapshot was just refreshed with current
        // RAM, so the two columns still agree.
        let r = c.execute(&mut sys, "scan list", false);
        assert!(r.ok, "{}", r.payload);
        let mut lines = r.payload.lines();
        assert_eq!(lines.next().unwrap(), "1 hits, showing 1");
        assert_eq!(lines.next().unwrap(), "80100000 0000005a 0000005a");

        // A poke with no further pass leaves the snapshot behind.
        assert!(c.execute(&mut sys, "poke 80100000 50000000", false).ok);
        let r = c.execute(&mut sys, "scan list", false);
        assert_eq!(
            r.payload.lines().nth(1).unwrap(),
            "80100000 00000050 0000005a"
        );

        assert!(c.execute(&mut sys, "scan clear", false).ok);
        assert!(!c.execute(&mut sys, "scan list", false).ok);
    }

    #[test]
    fn scan_start_unknown_then_changed() {
        let (mut sys, mut c) = (sys(), Controller::default());
        // `scan start 1` with no filter defaults to unknown.
        let r = c.execute(&mut sys, "scan start 1", false);
        assert_eq!(r.payload, "2097152 hits (width 1)");

        assert!(c.execute(&mut sys, "poke 80100000 ff", false).ok);
        let r = c.execute(&mut sys, "scan filter changed", false);
        assert_eq!(r.payload, "1 hits (width 1)");
    }

    #[test]
    fn scan_filter_exact_needs_a_session_and_narrows_when_present() {
        let (mut sys, mut c) = (sys(), Controller::default());
        // No session yet: nothing to narrow.
        assert!(!c.execute(&mut sys, "scan filter exact 100", false).ok);

        assert!(c.execute(&mut sys, "poke 80100000 64000000", false).ok);
        assert!(c.execute(&mut sys, "poke 80100040 64000000", false).ok);
        let r = c.execute(&mut sys, "scan start 4 unknown", false);
        assert_eq!(r.payload, "524288 hits (width 4)");

        let r = c.execute(&mut sys, "scan filter exact 100", false);
        assert_eq!(r.payload, "2 hits (width 4)");
    }

    #[test]
    fn scan_filter_rejects_a_bad_name() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "scan start 1", false).ok);
        assert!(!c.execute(&mut sys, "scan filter bogus", false).ok);
    }

    /// Each comparative filter keeps only the candidate it names, so a
    /// mapping swapped between `Increased`/`Decreased`/`Unchanged` in the
    /// dispatch would show up as the wrong address surviving.
    #[test]
    fn scan_filter_increased_decreased_and_unchanged_pick_the_right_candidate() {
        let (mut sys, mut c) = (sys(), Controller::default());
        fn restart_with_three_hundreds(c: &mut Controller, sys: &mut PsxSystem) {
            assert!(c.execute(sys, "poke 80100000 64000000", false).ok); // 100
            assert!(c.execute(sys, "poke 80100004 64000000", false).ok); // 100
            assert!(c.execute(sys, "poke 80100008 64000000", false).ok); // 100
            let r = c.execute(sys, "scan start 4 exact 100", false);
            assert_eq!(r.payload, "3 hits (width 4)");
        }

        restart_with_three_hundreds(&mut c, &mut sys);
        assert!(c.execute(&mut sys, "poke 80100000 6e000000", false).ok); // 110: up
        assert!(c.execute(&mut sys, "poke 80100004 5a000000", false).ok); // 90: down
        let r = c.execute(&mut sys, "scan filter increased", false);
        assert_eq!(r.payload, "1 hits (width 4)");
        let list = c.execute(&mut sys, "scan list", false);
        assert!(list.payload.contains("80100000"), "{}", list.payload);

        restart_with_three_hundreds(&mut c, &mut sys);
        assert!(c.execute(&mut sys, "poke 80100000 6e000000", false).ok);
        assert!(c.execute(&mut sys, "poke 80100004 5a000000", false).ok);
        let r = c.execute(&mut sys, "scan filter decreased", false);
        assert_eq!(r.payload, "1 hits (width 4)");
        let list = c.execute(&mut sys, "scan list", false);
        assert!(list.payload.contains("80100004"), "{}", list.payload);

        restart_with_three_hundreds(&mut c, &mut sys);
        assert!(c.execute(&mut sys, "poke 80100000 6e000000", false).ok);
        assert!(c.execute(&mut sys, "poke 80100004 5a000000", false).ok);
        let r = c.execute(&mut sys, "scan filter unchanged", false);
        assert_eq!(r.payload, "1 hits (width 4)");
        let list = c.execute(&mut sys, "scan list", false);
        assert!(list.payload.contains("80100008"), "{}", list.payload);
    }

    #[test]
    fn scan_list_clamps_to_a_maximum() {
        let (mut sys, mut c) = (sys(), Controller::default());
        let r = c.execute(&mut sys, "scan start 1", false);
        assert_eq!(r.payload, "2097152 hits (width 1)");

        let r = c.execute(&mut sys, "scan list 999999999", false);
        assert!(r.ok, "{}", r.payload);
        let mut lines = r.payload.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("2097152 hits, showing {SCAN_LIST_MAX}")
        );
        assert_eq!(lines.count(), SCAN_LIST_MAX);
    }

    #[test]
    fn scan_rejects_bad_input() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "scan start 3", false).ok);
        assert!(!c.execute(&mut sys, "scan start 4 exact zz", false).ok);
        // No session yet: a filter has nothing to narrow.
        assert!(!c.execute(&mut sys, "scan filter changed", false).ok);
        assert!(c.execute(&mut sys, "scan start 4 exact 1", false).ok);
        assert!(!c.execute(&mut sys, "scan list -1", false).ok);
    }
}
