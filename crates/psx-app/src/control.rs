//! Interactive control port for headless automation.
//!
//! Designed for LLM/script operators: a line-based text protocol over TCP
//! where the emulator runs in *lockstep* — it only advances when a `run` or
//! `press` command says so, making every observation deterministic and
//! repeatable. One command per line; the reply is `ok`/`err <msg>` followed
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

use psx_core::{CPU_CLOCK_HZ, CYCLES_PER_FRAME, PsxSystem};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
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

/// Parse `10` (frames), `2s` (seconds) or `50000c` (cycles) into cycles.
fn parse_duration(s: &str) -> Result<u64, String> {
    let (num, unit) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], CPU_CLOCK_HZ),
        Some('c') => (&s[..s.len() - 1], 1),
        _ => (s, CYCLES_PER_FRAME),
    };
    let n: f64 = num.parse().map_err(|_| format!("bad duration '{s}'"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("bad duration '{s}'"));
    }
    Ok((n * unit as f64) as u64)
}

fn parse_addr(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| format!("bad address '{s}'"))
}

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

/// Command executor: protocol state independent of the transport, so the
/// whole command surface is unit-testable without sockets.
#[derive(Default)]
pub struct Controller {
    /// Buttons held across `run` commands (`input set`).
    held: u16,
    /// Byte offset into the TTY buffer already returned by `tty`.
    tty_read: usize,
    frames_run: u64,
}

impl Controller {
    /// Advance emulation, keeping the held-button state applied.
    fn advance(&mut self, sys: &mut PsxSystem, cycles: u64) {
        sys.set_buttons(self.held);
        sys.run_cycles(cycles);
        self.frames_run += cycles / CYCLES_PER_FRAME;
    }

    pub fn execute(&mut self, sys: &mut PsxSystem, line: &str, debugger_owns: bool) -> Reply {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let args: Vec<&str> = words.collect();
        // The debugger and the control port must not both drive execution
        // (loadstate mutates it just as much as running does).
        if debugger_owns && matches!(cmd, "run" | "press" | "reset" | "loadstate") {
            return Reply::err("debugger attached; execution is owned by the debugger");
        }
        match (cmd, args.as_slice()) {
            ("help", _) => Reply::ok(HELP.trim_end()),
            ("state", _) => {
                let frame = &sys.bus.gpu.frame;
                Reply::ok(format!(
                    "pc={:#010x} cycles={} frames={} held={} display={}x{}{}",
                    sys.cpu.pc,
                    sys.cycles(),
                    self.frames_run,
                    buttons_to_names(self.held),
                    frame.width,
                    frame.height,
                    if frame.enabled { "" } else { " (disabled)" },
                ))
            }
            ("run", [dur]) => match parse_duration(dur) {
                Ok(cycles) => {
                    self.advance(sys, cycles);
                    Reply::ok(format!("ran {cycles} cycles, pc={:#010x}", sys.cpu.pc))
                }
                Err(e) => Reply::err(e),
            },
            ("press", [buttons, dur]) => match (parse_buttons(buttons), parse_duration(dur)) {
                (Ok(mask), Ok(cycles)) => {
                    let prev = self.held;
                    self.held |= mask;
                    self.advance(sys, cycles);
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
            ("peek", [addr, len]) => {
                let (addr, len) = match (parse_addr(addr), len.parse::<u32>()) {
                    (Ok(a), Ok(l)) if l <= 4096 => (a, l),
                    (Err(e), _) => return Reply::err(e),
                    _ => return Reply::err("bad length (max 4096)"),
                };
                let mut out = String::new();
                for base in (0..len).step_by(16) {
                    let row: Vec<String> = (base..(base + 16).min(len))
                        .map(|i| match sys.bus.peek8(addr.wrapping_add(i)) {
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
                    if !sys.bus.poke8(addr.wrapping_add(i as u32), *b) {
                        return Reply::err(format!(
                            "address {:#010x} not writable",
                            addr.wrapping_add(i as u32)
                        ));
                    }
                }
                Reply::ok(format!("wrote {} bytes", bytes.len()))
            }
            ("tty", _) => {
                let all = sys.tty_output();
                let new = all[self.tty_read.min(all.len())..].to_string();
                self.tty_read = all.len();
                Reply::ok(new)
            }
            ("frame", [path]) => {
                let frame = &sys.bus.gpu.frame;
                if frame.width == 0 || frame.height == 0 {
                    return Reply::err("no frame captured yet (run at least one frame)");
                }
                crate::write_frame_bmp(path, frame);
                Reply::ok(format!("{}x{} -> {path}", frame.width, frame.height))
            }
            ("vram", [path]) => {
                crate::write_vram_bmp(path, &sys.bus.gpu.vram);
                Reply::ok(format!("1024x512 -> {path}"))
            }
            ("savestate", [path]) => match sys.save_state() {
                Ok(data) => match std::fs::write(path, &data) {
                    Ok(()) => Reply::ok(format!("saved {} bytes -> {path}", data.len())),
                    Err(e) => Reply::err(format!("write {path}: {e}")),
                },
                Err(e) => Reply::err(e),
            },
            ("loadstate", [path]) => match std::fs::read(path) {
                Ok(data) => match sys.load_state(&data) {
                    Ok(()) => Reply::ok(format!("loaded, pc={:#010x}", sys.cpu.pc)),
                    Err(e) => Reply::err(e),
                },
                Err(e) => Reply::err(format!("read {path}: {e}")),
            },
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
state                 pc, cycles, frames run, held buttons, display mode
run <n>[s|c]          advance n frames (s=seconds, c=cycles), inputs held
press <BTN+BTN> <n>   hold buttons for n frames on top of held set, release
input set <BTN+BTN>   hold buttons until changed (applied during run)
input clear           release all held buttons
frame <path>          dump the latched display frame as BMP
vram <path>           dump full 1024x512 VRAM as BMP
peek <hexaddr> <len>  hex dump memory (side-effect-free, MMIO shows --)
poke <hexaddr> <hex>  write bytes to RAM/scratchpad
tty                   TTY output accumulated since the last `tty`
savestate <path>      snapshot the full machine state to a file
loadstate <path>      restore a snapshot (BIOS/disc/memcard carry over)
quit                  shut the emulator down
";

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
        info!("control port listening on {}", listener.local_addr()?);
        Ok(Self {
            listener,
            client: None,
            buf: Vec::new(),
            controller: Controller::default(),
        })
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
        if let Some(stream) = &mut self.client
            && stream.write_all(out.as_bytes()).is_err()
        {
            self.client = None;
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
    fn press_restores_held_set() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "input set UP", false).ok);
        let r = c.execute(&mut sys, "press CROSS+START 1", false);
        assert!(r.ok, "{}", r.payload);
        // After the press, only the held set remains applied.
        assert_eq!(sys.bus.sio.buttons, psx_core::sio::button::UP);
        assert!(c.execute(&mut sys, "input clear", false).ok);
        assert_eq!(sys.bus.sio.buttons, 0);
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

    #[test]
    fn debugger_owns_execution() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(!c.execute(&mut sys, "run 1", true).ok);
        assert!(c.execute(&mut sys, "peek 80000000 4", true).ok); // observation is fine
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
        assert!(!c.execute(&mut sys, "press NOPE 1", false).ok);
        assert!(!c.execute(&mut sys, "peek xyz 4", false).ok);
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
    fn quit_flag_propagates() {
        let (mut sys, mut c) = (sys(), Controller::default());
        assert!(c.execute(&mut sys, "quit", false).quit);
    }
}
