//! LLDB-first gdb-remote debug stub for the PS1e core.
//!
//! [`DebugServer`] listens on TCP, speaks the gdb-remote serial protocol and
//! drives a [`PsxSystem`] owned by the frontend. LLDB is the primary client
//! (`qHostInfo`, `qProcessInfo`, `qRegisterInfo` and `target.xml` are all
//! implemented); plain GDB works via the same `target.xml`.
//!
//! Integration model: the frontend keeps calling [`DebugServer::pump`] with a
//! cycle budget. With no client attached, `pump` returns immediately and the
//! frontend runs the emulator itself. Once a client attaches, execution is
//! *owned by the debugger*: the frontend must stop stepping the system and
//! `pump` executes instructions only while the client says "continue".

mod packet;
mod registers;

use packet::{Item, Receiver};
use psx_core::PsxSystem;
use psx_core::bus::Bus;
use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

const TRIPLE: &str = "mipsel-unknown-unknown";

pub struct DebugServer {
    listener: TcpListener,
    client: Option<Client>,
    /// Breakpoint addresses, stored physically-masked so a breakpoint set on
    /// a KSEG0 address also hits its KSEG1/KUSEG aliases.
    breakpoints: HashSet<u32>,
    /// Execution is suspended, waiting for debugger commands.
    halted: bool,
    /// Skip the breakpoint check for the first instruction after a resume so
    /// continuing from a breakpointed pc makes progress.
    resume_skip: bool,
    no_ack: bool,
}

struct Client {
    stream: TcpStream,
    rx: Receiver,
}

impl DebugServer {
    /// Bind the stub to `127.0.0.1:port`. Port 0 picks a free port
    /// (see [`DebugServer::port`]).
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        info!(target: "psx_debug", "gdb-remote stub listening on {}", listener.local_addr()?);
        Ok(Self {
            listener,
            client: None,
            breakpoints: HashSet::new(),
            halted: false,
            resume_skip: false,
            no_ack: false,
        })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// A client is currently attached (the debugger owns execution).
    pub fn attached(&self) -> bool {
        self.client.is_some()
    }

    /// Execution is suspended at a debug stop.
    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Service the connection and, while the client has us running, execute
    /// up to `budget_cycles` of emulation with breakpoint checks.
    pub fn pump(&mut self, sys: &mut PsxSystem, budget_cycles: u64) {
        self.accept_new_client();
        self.service_client(sys);
        if self.client.is_some() && !self.halted {
            self.run_slice(sys, budget_cycles);
        }
    }

    fn accept_new_client(&mut self) {
        if self.client.is_some() {
            return;
        }
        match self.listener.accept() {
            Ok((stream, addr)) => {
                info!(target: "psx_debug", "debugger attached from {addr}");
                stream.set_nonblocking(true).ok();
                stream.set_nodelay(true).ok();
                self.client = Some(Client {
                    stream,
                    rx: Receiver::default(),
                });
                // Attaching halts the target, like gdbserver attaching to a
                // live process. The client's `?` query finds us stopped.
                self.halted = true;
                self.no_ack = false;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => warn!(target: "psx_debug", "accept failed: {e}"),
        }
    }

    fn detach(&mut self, reason: &str) {
        info!(target: "psx_debug", "debugger detached ({reason})");
        self.client = None;
        self.breakpoints.clear();
        self.halted = false;
        self.no_ack = false;
    }

    /// Read pending bytes and handle every complete protocol item.
    fn service_client(&mut self, sys: &mut PsxSystem) {
        let Some(client) = &mut self.client else {
            return;
        };
        let mut buf = [0u8; 4096];
        loop {
            match client.stream.read(&mut buf) {
                Ok(0) => {
                    self.detach("connection closed");
                    return;
                }
                Ok(n) => client.rx.push_bytes(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    let msg = format!("read failed: {e}");
                    self.detach(&msg);
                    return;
                }
            }
        }
        while let Some(item) = self.client.as_mut().and_then(|c| c.rx.next_item()) {
            match item {
                Item::Packet(payload) => {
                    self.send_raw(b"+");
                    self.handle_packet(sys, &payload);
                }
                Item::Corrupt => self.send_raw(b"-"),
                Item::Interrupt => {
                    if !self.halted {
                        self.halted = true;
                        // SIGINT
                        self.send_reply(b"T02thread:01;");
                    }
                }
                Item::Ack | Item::Nak => {}
            }
            if self.client.is_none() {
                break; // detached while handling (D / k)
            }
        }
    }

    /// Execute instructions until the budget runs out or a breakpoint hits.
    fn run_slice(&mut self, sys: &mut PsxSystem, budget_cycles: u64) {
        let end = sys.cycles() + budget_cycles;
        while sys.cycles() < end {
            if !std::mem::take(&mut self.resume_skip)
                && self.breakpoints.contains(&Bus::mask_address(sys.cpu.pc))
            {
                self.halted = true;
                self.send_reply(b"T05thread:01;");
                return;
            }
            sys.step();
        }
    }

    fn send_raw(&mut self, data: &[u8]) {
        if let Some(client) = &mut self.client
            && let Err(e) = client.stream.write_all(data)
        {
            let msg = format!("write failed: {e}");
            self.detach(&msg);
        }
    }

    fn send_reply(&mut self, payload: &[u8]) {
        debug!(target: "psx_debug", "reply: {}", String::from_utf8_lossy(payload));
        let frame = packet::frame(payload);
        self.send_raw(&frame);
    }

    fn handle_packet(&mut self, sys: &mut PsxSystem, payload: &[u8]) {
        // `X` carries escaped binary data; handle it before any utf8 view.
        if payload.first() == Some(&b'X') {
            let reply = handle_binary_write(sys, &payload[1..]);
            self.send_reply(&reply);
            return;
        }
        let text = String::from_utf8_lossy(payload).into_owned();
        debug!(target: "psx_debug", "packet: {text}");
        // Commands that reply asynchronously (on the next stop) or change
        // connection state are handled here; everything else returns a reply.
        match text.as_bytes() {
            [b'c', ..] | [b'C', ..] => {
                self.halted = false;
                self.resume_skip = true;
                return; // reply comes when we stop
            }
            [b's', ..] | [b'S', ..] => {
                sys.step();
                self.send_reply(b"T05thread:01;");
                return;
            }
            b"D" => {
                self.send_reply(b"OK");
                self.detach("D packet");
                return;
            }
            b"k" => {
                // No process to kill: drop the connection, emulation resumes.
                self.detach("k packet");
                return;
            }
            _ => {}
        }
        if let Some(rest) = text.strip_prefix("vCont;") {
            match rest.as_bytes().first() {
                Some(b'c') | Some(b'C') => {
                    self.halted = false;
                    self.resume_skip = true;
                }
                Some(b's') | Some(b'S') => {
                    sys.step();
                    self.send_reply(b"T05thread:01;");
                }
                _ => self.send_reply(b""),
            }
            return;
        }
        let reply = self.reply_for(sys, &text);
        self.send_reply(&reply);
        if text == "QStartNoAckMode" {
            self.no_ack = true;
        }
    }

    /// Synchronous request/reply commands.
    fn reply_for(&mut self, sys: &mut PsxSystem, text: &str) -> Vec<u8> {
        if text == "?" {
            return b"T05thread:01;".to_vec();
        }
        if text.starts_with("qSupported") {
            return b"PacketSize=4096;qXfer:features:read+;QStartNoAckMode+;\
                     swbreak+;vContSupported+"
                .to_vec();
        }
        match text {
            "QStartNoAckMode" => b"OK".to_vec(),
            "qHostInfo" => format!(
                "triple:{};ptrsize:4;endian:little;hostname:{};",
                packet::to_hex(TRIPLE.as_bytes()),
                packet::to_hex(b"ps1e")
            )
            .into_bytes(),
            "qProcessInfo" => format!(
                "pid:1;parent-pid:1;real-uid:0;real-gid:0;effective-uid:0;\
                 effective-gid:0;triple:{};ostype:unknown;endian:little;ptrsize:4;",
                packet::to_hex(TRIPLE.as_bytes())
            )
            .into_bytes(),
            "qC" => b"QC1".to_vec(),
            "qAttached" => b"1".to_vec(),
            "qfThreadInfo" => b"m1".to_vec(),
            "qsThreadInfo" => b"l".to_vec(),
            "vCont?" => b"vCont;c;C;s;S".to_vec(),
            "g" => {
                let mut hex = String::with_capacity(registers::NUM_REGS * 8);
                for i in 0..registers::NUM_REGS {
                    hex.push_str(&packet::to_hex(&registers::read(sys, i).to_le_bytes()));
                }
                hex.into_bytes()
            }
            _ => self.reply_for_prefixed(sys, text),
        }
    }

    fn reply_for_prefixed(&mut self, sys: &mut PsxSystem, text: &str) -> Vec<u8> {
        if let Some(rest) = text.strip_prefix("qRegisterInfo") {
            return match usize::from_str_radix(rest, 16)
                .ok()
                .and_then(registers::register_info)
            {
                Some(info) => info.into_bytes(),
                None => b"E45".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("qXfer:features:read:target.xml:") {
            return xfer_chunk(&registers::target_xml(), rest);
        }
        if let Some(rest) = text.strip_prefix("G") {
            return match packet::from_hex(rest) {
                Some(bytes) if bytes.len() == registers::NUM_REGS * 4 => {
                    for (i, w) in bytes.chunks_exact(4).enumerate() {
                        registers::write(sys, i, u32::from_le_bytes(w.try_into().unwrap()));
                    }
                    b"OK".to_vec()
                }
                _ => b"E01".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("p") {
            return match usize::from_str_radix(rest, 16) {
                Ok(i) if i < registers::NUM_REGS => {
                    packet::to_hex(&registers::read(sys, i).to_le_bytes()).into_bytes()
                }
                _ => b"E45".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("P") {
            let parsed = rest.split_once('=').and_then(|(idx, val)| {
                let i = usize::from_str_radix(idx, 16).ok()?;
                let bytes = packet::from_hex(val)?;
                let v = u32::from_le_bytes(bytes.try_into().ok()?);
                Some((i, v))
            });
            return match parsed {
                Some((i, v)) if i < registers::NUM_REGS => {
                    registers::write(sys, i, v);
                    b"OK".to_vec()
                }
                _ => b"E01".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("m") {
            return match parse_addr_len(rest) {
                Some((addr, len)) => read_memory(sys, addr, len),
                None => b"E01".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("M") {
            let parsed = rest
                .split_once(':')
                .and_then(|(range, hex)| Some((parse_addr_len(range)?, packet::from_hex(hex)?)));
            return match parsed {
                Some(((addr, len), bytes)) if bytes.len() as u32 == len => {
                    write_memory(sys, addr, &bytes)
                }
                _ => b"E01".to_vec(),
            };
        }
        if let Some(rest) = text.strip_prefix("Z") {
            return self.handle_breakpoint(rest, true);
        }
        if let Some(rest) = text.strip_prefix("z") {
            return self.handle_breakpoint(rest, false);
        }
        if text.starts_with("H") || text == "T1" {
            return b"OK".to_vec();
        }
        // Unknown packet: empty reply means "unsupported".
        b"".to_vec()
    }

    /// `Z<type>,<addr>,<kind>` / `z<type>,<addr>,<kind>`. Software and
    /// hardware breakpoints share one implementation (the interpreter checks
    /// pc every instruction); watchpoints are unsupported.
    fn handle_breakpoint(&mut self, rest: &str, insert: bool) -> Vec<u8> {
        let mut parts = rest.split(',');
        let (ty, addr) = match (
            parts.next(),
            parts.next().and_then(|a| u32::from_str_radix(a, 16).ok()),
        ) {
            (Some(ty), Some(addr)) => (ty, addr),
            _ => return b"E01".to_vec(),
        };
        match ty {
            "0" | "1" => {
                let key = Bus::mask_address(addr);
                if insert {
                    self.breakpoints.insert(key);
                } else {
                    self.breakpoints.remove(&key);
                }
                b"OK".to_vec()
            }
            _ => b"".to_vec(), // watchpoints unsupported
        }
    }
}

/// Serve one `qXfer` window (`<offset>,<length>` in hex) of a document:
/// `l` prefixes the final chunk, `m` a chunk with more data following.
fn xfer_chunk(doc: &str, range: &str) -> Vec<u8> {
    let Some((off, len)) = parse_addr_len(range) else {
        return b"E01".to_vec();
    };
    let bytes = doc.as_bytes();
    let start = (off as usize).min(bytes.len());
    let end = (start + len as usize).min(bytes.len());
    let mut out = Vec::with_capacity(end - start + 1);
    out.push(if end == bytes.len() { b'l' } else { b'm' });
    out.extend_from_slice(&bytes[start..end]);
    out
}

/// `X<addr>,<len>:<escaped binary>` — the write path LLDB prefers.
fn handle_binary_write(sys: &mut PsxSystem, rest: &[u8]) -> Vec<u8> {
    let Some(colon) = rest.iter().position(|&b| b == b':') else {
        return b"E01".to_vec();
    };
    let header = String::from_utf8_lossy(&rest[..colon]);
    let Some((addr, len)) = parse_addr_len(&header) else {
        return b"E01".to_vec();
    };
    let bytes = packet::unescape_binary(&rest[colon + 1..]);
    if bytes.len() as u32 != len {
        return b"E01".to_vec();
    }
    write_memory(sys, addr, &bytes)
}

/// Parse `<addr>,<len>` (both hex).
fn parse_addr_len(s: &str) -> Option<(u32, u32)> {
    let (a, l) = s.split_once(',')?;
    Some((
        u32::from_str_radix(a, 16).ok()?,
        u32::from_str_radix(l, 16).ok()?,
    ))
}

fn read_memory(sys: &PsxSystem, addr: u32, len: u32) -> Vec<u8> {
    let mut hex = String::with_capacity(len as usize * 2);
    for i in 0..len {
        match sys.bus.peek8(addr.wrapping_add(i)) {
            Some(b) => hex.push_str(&format!("{b:02x}")),
            // Partial reads are legal; an unmapped first byte is an error.
            None if i == 0 => return b"E01".to_vec(),
            None => break,
        }
    }
    hex.into_bytes()
}

fn write_memory(sys: &mut PsxSystem, addr: u32, bytes: &[u8]) -> Vec<u8> {
    for (i, b) in bytes.iter().enumerate() {
        if !sys.bus.poke8(addr.wrapping_add(i as u32), *b) {
            return b"E01".to_vec();
        }
    }
    b"OK".to_vec()
}
