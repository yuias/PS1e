//! gdb-remote serial protocol framing: `$<payload>#<checksum>` packets,
//! `+`/`-` acknowledgements and the 0x03 interrupt byte.

/// Modulo-256 sum of the payload bytes.
pub fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Frame a payload into a wire packet.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(b'$');
    out.extend_from_slice(payload);
    out.push(b'#');
    out.extend_from_slice(format!("{:02x}", checksum(payload)).as_bytes());
    out
}

/// One item extracted from the receive stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    /// A complete, checksum-verified packet payload.
    Packet(Vec<u8>),
    /// A packet whose checksum did not match (sender should retransmit).
    Corrupt,
    /// 0x03: the client requests an interrupt (Ctrl-C).
    Interrupt,
    Ack,
    Nak,
}

/// Incremental receive buffer that extracts complete protocol items.
#[derive(Default)]
pub struct Receiver {
    buf: Vec<u8>,
}

impl Receiver {
    pub fn push_bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Extract the next complete item, or None if more bytes are needed.
    pub fn next_item(&mut self) -> Option<Item> {
        loop {
            match *self.buf.first()? {
                b'+' => {
                    self.buf.remove(0);
                    return Some(Item::Ack);
                }
                b'-' => {
                    self.buf.remove(0);
                    return Some(Item::Nak);
                }
                0x03 => {
                    self.buf.remove(0);
                    return Some(Item::Interrupt);
                }
                b'$' => {
                    let hash = self.buf.iter().position(|&b| b == b'#')?;
                    if self.buf.len() < hash + 3 {
                        return None; // checksum digits not in yet
                    }
                    let payload: Vec<u8> = self.buf[1..hash].to_vec();
                    let sum_hex = std::str::from_utf8(&self.buf[hash + 1..hash + 3])
                        .ok()
                        .and_then(|s| u8::from_str_radix(s, 16).ok());
                    self.buf.drain(..hash + 3);
                    return Some(if sum_hex == Some(checksum(&payload)) {
                        Item::Packet(payload)
                    } else {
                        Item::Corrupt
                    });
                }
                // Noise between packets (e.g. stray newlines): skip.
                _ => {
                    self.buf.remove(0);
                }
            }
        }
    }
}

/// Decode the binary-escape encoding used by `X` packets:
/// `0x7d` escapes the next byte, which is XORed with `0x20`.
pub fn unescape_binary(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        if b == 0x7d {
            if let Some(&e) = it.next() {
                out.push(e ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_with_checksum() {
        assert_eq!(frame(b"OK"), b"$OK#9a".to_vec());
        assert_eq!(frame(b""), b"$#00".to_vec());
    }

    #[test]
    fn extracts_packets_and_control_bytes() {
        let mut rx = Receiver::default();
        rx.push_bytes(b"+$qC#b4\x03");
        assert_eq!(rx.next_item(), Some(Item::Ack));
        assert_eq!(rx.next_item(), Some(Item::Packet(b"qC".to_vec())));
        assert_eq!(rx.next_item(), Some(Item::Interrupt));
        assert_eq!(rx.next_item(), None);
    }

    #[test]
    fn waits_for_partial_packets() {
        let mut rx = Receiver::default();
        rx.push_bytes(b"$qSupported#3");
        assert_eq!(rx.next_item(), None);
        rx.push_bytes(b"7");
        assert_eq!(rx.next_item(), Some(Item::Packet(b"qSupported".to_vec())));
    }

    #[test]
    fn flags_bad_checksums() {
        let mut rx = Receiver::default();
        rx.push_bytes(b"$qC#00");
        assert_eq!(rx.next_item(), Some(Item::Corrupt));
    }

    #[test]
    fn unescapes_x_packet_bodies() {
        assert_eq!(
            unescape_binary(&[0x41, 0x7d, 0x5d, 0x42]),
            vec![0x41, 0x7d, 0x42]
        );
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0xde, 0xad]), "dead");
        assert_eq!(from_hex("dead"), Some(vec![0xde, 0xad]));
        assert_eq!(from_hex("xy"), None);
    }
}
