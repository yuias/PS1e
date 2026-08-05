//! psxctl: one-shot client for the PS1e control port.
//!
//! Sends a single command to a `ps1e --headless --control-port <port>`
//! session and prints the reply, so shells and tool-using LLM agents can
//! drive the emulator statelessly:
//!
//! ```text
//! psxctl press START 30
//! psxctl frame shot.bmp
//! psxctl --port 9005 state
//! ```
//!
//! Exit code 0 on `ok`, 1 on `err`, 2 on usage/connection problems.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

const DEFAULT_PORT: u16 = 9002;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut port = DEFAULT_PORT;
    if args.first().is_some_and(|a| a == "--port") {
        args.remove(0);
        port = args
            .first()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| usage());
        args.remove(0);
    }
    let cmd = args.join(" ");
    if cmd.is_empty() {
        usage();
    }

    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("psxctl: cannot connect to 127.0.0.1:{port}: {e}");
        eprintln!("is `ps1e --headless --control-port {port}` running?");
        std::process::exit(2);
    });
    let mut writer = stream.try_clone().expect("clone stream");
    writer
        .write_all(format!("{cmd}\n").as_bytes())
        .expect("send command");

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).expect("read status");
    let ok = status.trim() == "ok";

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read reply") == 0 {
            eprintln!("psxctl: connection closed mid-reply");
            std::process::exit(2);
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l == "." {
            break;
        }
        // Reverse the server's dot-stuffing.
        println!("{}", l.strip_prefix('.').unwrap_or(l));
    }
    std::process::exit(if ok { 0 } else { 1 });
}

fn usage() -> ! {
    eprintln!("usage: psxctl [--port N] <command...>   (try: psxctl help)");
    std::process::exit(2);
}
