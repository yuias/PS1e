//! End-to-end protocol tests: a real TCP client speaks gdb-remote to a
//! [`DebugServer`] pumped on a background thread, the way a frontend would.

use psx_core::PsxSystem;
use psx_debug::DebugServer;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct Harness {
    client: Client,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Zero BIOS = an endless nop sled from the reset vector; perfect for
    /// deterministic step/breakpoint tests.
    fn start() -> Self {
        let mut sys = PsxSystem::new(vec![0; 512 * 1024]).unwrap();
        let mut server = DebugServer::bind(0).unwrap();
        let port = server.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                server.pump(&mut sys, 100_000);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Harness {
            client: Client { stream, ack: true },
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

struct Client {
    stream: TcpStream,
    ack: bool,
}

impl Client {
    fn checksum(data: &[u8]) -> u8 {
        data.iter().fold(0u8, |a, b| a.wrapping_add(*b))
    }

    /// Send a packet; consume the server's `+` ack when ack mode is on.
    fn send(&mut self, payload: &str) {
        let frame = format!("${payload}#{:02x}", Self::checksum(payload.as_bytes()));
        self.stream.write_all(frame.as_bytes()).unwrap();
        if self.ack {
            let mut b = [0u8; 1];
            self.stream.read_exact(&mut b).unwrap();
            assert_eq!(b[0], b'+', "expected ack for {payload}");
        }
    }

    /// Receive one packet payload (skipping stray acks), acknowledging it.
    fn recv(&mut self) -> String {
        let mut raw = Vec::new();
        let mut b = [0u8; 1];
        loop {
            self.stream.read_exact(&mut b).unwrap();
            match b[0] {
                b'$' => break,
                b'+' => continue,
                other => panic!("unexpected byte {other:#04x} while waiting for packet"),
            }
        }
        loop {
            self.stream.read_exact(&mut b).unwrap();
            if b[0] == b'#' {
                break;
            }
            raw.push(b[0]);
        }
        let mut sum = [0u8; 2];
        self.stream.read_exact(&mut sum).unwrap();
        if self.ack {
            self.stream.write_all(b"+").unwrap();
        }
        String::from_utf8(raw).unwrap()
    }

    fn cmd(&mut self, payload: &str) -> String {
        self.send(payload);
        self.recv()
    }

    fn interrupt(&mut self) {
        self.stream.write_all(&[0x03]).unwrap();
    }

    /// Read a register by gdb regnum, decoding the little-endian hex reply.
    fn reg(&mut self, regnum: usize) -> u32 {
        let hex = self.cmd(&format!("p{regnum:x}"));
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        u32::from_le_bytes(bytes.try_into().unwrap())
    }
}

const PC: usize = 37;

#[test]
fn lldb_handshake_queries() {
    let mut h = Harness::start();
    let c = &mut h.client;

    let supported = c.cmd("qSupported:xmlRegisters=i386;multiprocess+");
    assert!(supported.contains("qXfer:features:read+"), "{supported}");
    assert!(supported.contains("QStartNoAckMode+"), "{supported}");

    // Attaching halts the target; `?` must report a stop.
    assert!(c.cmd("?").starts_with("T05"));

    let host = c.cmd("qHostInfo");
    let triple_hex = host
        .split(';')
        .find_map(|f| f.strip_prefix("triple:"))
        .expect("qHostInfo has no triple");
    let triple: Vec<u8> = (0..triple_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&triple_hex[i..i + 2], 16).unwrap())
        .collect();
    assert_eq!(triple, b"mipsel-unknown-unknown");

    let proc = c.cmd("qProcessInfo");
    assert!(proc.contains("ptrsize:4;"), "{proc}");
    assert!(proc.contains("endian:little;"), "{proc}");

    assert_eq!(c.cmd("qC"), "QC1");
    assert_eq!(c.cmd("qfThreadInfo"), "m1");
    assert_eq!(c.cmd("qsThreadInfo"), "l");
    assert_eq!(c.cmd("vCont?"), "vCont;c;C;s;S");
    // Unknown packets must draw an empty reply, not an error.
    assert_eq!(c.cmd("vMustReplyEmpty"), "");
}

#[test]
fn register_info_and_target_xml_agree() {
    let mut h = Harness::start();
    let c = &mut h.client;

    // qRegisterInfo enumerates exactly 38 registers, then E45.
    let mut names = Vec::new();
    for i in 0.. {
        let info = c.cmd(&format!("qRegisterInfo{i:x}"));
        if info == "E45" {
            break;
        }
        names.push(
            info.split(';')
                .find_map(|f| f.strip_prefix("name:"))
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(names.len(), 38);
    assert_eq!(names[29], "sp");
    assert_eq!(names[37], "pc");

    let xml = c.cmd("qXfer:features:read:target.xml:0,1000");
    assert!(xml.starts_with('l') || xml.starts_with('m'));
    // Fetch the whole document across windows and sanity-check it.
    let mut doc = String::new();
    let mut off = 0;
    loop {
        let chunk = c.cmd(&format!("qXfer:features:read:target.xml:{off:x},200"));
        let (kind, data) = chunk.split_at(1);
        doc.push_str(data);
        off += data.len();
        if kind == "l" {
            break;
        }
    }
    assert!(doc.contains("<architecture>mips</architecture>"), "{doc}");
    assert!(doc.contains("org.gnu.gdb.mips.cpu"), "{doc}");
    assert!(doc.contains("org.gnu.gdb.mips.cp0"), "{doc}");
}

#[test]
fn registers_read_write() {
    let mut h = Harness::start();
    let c = &mut h.client;

    let g = c.cmd("g");
    assert_eq!(g.len(), 38 * 8);
    // Freshly reset: pc is the reset vector.
    assert_eq!(c.reg(PC), 0xbfc0_0000);

    // P: write $a0 (r4) and read it back both ways.
    assert_eq!(c.cmd("P4=78563412"), "OK");
    assert_eq!(c.reg(4), 0x1234_5678);
    let g = c.cmd("g");
    assert_eq!(&g[4 * 8..4 * 8 + 8], "78563412");

    // r0 stays hardwired to zero.
    assert_eq!(c.cmd("P0=ffffffff"), "OK");
    assert_eq!(c.reg(0), 0);
}

#[test]
fn memory_read_write() {
    let mut h = Harness::start();
    let c = &mut h.client;

    // RAM round-trip through KSEG0.
    assert_eq!(c.cmd("M80001000,4:deadbeef"), "OK");
    assert_eq!(c.cmd("m80001000,4"), "deadbeef");
    // The same bytes are visible through the KSEG1 mirror.
    assert_eq!(c.cmd("ma0001000,4"), "deadbeef");

    // X: binary write (0x7d escape decoding).
    c.send("X80002000,4:\x41\x7d\x5d\x42\x43");
    assert_eq!(c.recv(), "OK");
    assert_eq!(c.cmd("m80002000,4"), "417d4243");

    // BIOS is readable but not writable.
    assert_eq!(c.cmd("mbfc00000,4"), "00000000");
    assert_eq!(c.cmd("Mbfc00000,1:ff"), "E01");
    // MMIO must be refused, not dispatched.
    assert_eq!(c.cmd("m1f801800,1"), "E01");
}

#[test]
fn breakpoint_continue_step() {
    let mut h = Harness::start();
    let c = &mut h.client;

    // Single step from reset: one nop, pc advances by 4.
    assert!(c.cmd("s").starts_with("T05"));
    assert_eq!(c.reg(PC), 0xbfc0_0004);

    // Breakpoint set through the KSEG1 address, hit via masked compare.
    assert_eq!(c.cmd("Z0,bfc00020,4"), "OK");
    c.send("c");
    assert!(c.recv().starts_with("T05"));
    assert_eq!(c.reg(PC), 0xbfc0_0020);

    // Resuming from a breakpointed pc must make progress, and removing the
    // breakpoint means the next stop can only come from an interrupt.
    assert_eq!(c.cmd("z0,bfc00020,4"), "OK");
    c.send("c");
    std::thread::sleep(Duration::from_millis(50));
    c.interrupt();
    assert!(c.recv().starts_with("T02"));
    assert!(c.reg(PC) > 0xbfc0_0020);

    // vCont step works like `s`.
    let pc = c.reg(PC);
    c.send("vCont;s:1");
    assert!(c.recv().starts_with("T05"));
    assert_eq!(c.reg(PC), pc + 4);
}

#[test]
fn no_ack_mode() {
    let mut h = Harness::start();
    let c = &mut h.client;
    assert_eq!(c.cmd("QStartNoAckMode"), "OK");
    c.ack = false;
    assert_eq!(c.cmd("qC"), "QC1");
    assert_eq!(c.reg(PC), 0xbfc0_0000);
}
