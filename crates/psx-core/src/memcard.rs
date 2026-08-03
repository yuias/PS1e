//! Memory card: the 128 KiB card protocol spoken over SIO0.
//!
//! Implements the Read (52h), Write (57h) and ID (53h) commands with the
//! FLAG byte and per-byte /ACK semantics. The backing image is plain bytes
//! (.mcr layout); the frontend owns loading/saving it — this module only
//! flips a dirty flag on committed writes.

use tracing::{debug, trace};

pub const CARD_SIZE: usize = 128 * 1024;
const SECTOR: usize = 128;

/// Transaction progress. `step` counts bytes since the 0x81 select.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    None,
    Read,
    Write,
    Id,
    /// Transaction finished or failed; ignore until deselect.
    Done,
}

pub struct MemCard {
    pub data: Box<[u8]>,
    /// Bit 3 = "directory unread" (fresh card); cleared by a write.
    flag: u8,
    dirty: bool,
    cmd: Command,
    step: u32,
    sector: u16,
    last_tx: u8,
    buf: [u8; SECTOR],
    /// Running XOR of address and data bytes.
    chk: u8,
    /// Checksum byte the console sent during a write.
    recv_chk: u8,
}

impl MemCard {
    /// A freshly formatted card (what the shell's "format" produces), so
    /// games see an empty card instead of an unformatted one.
    pub fn new() -> Self {
        let mut data = vec![0u8; CARD_SIZE].into_boxed_slice();
        let write_frame = |data: &mut [u8], frame: usize, init: &dyn Fn(&mut [u8])| {
            let f = &mut data[frame * SECTOR..(frame + 1) * SECTOR];
            init(f);
            f[127] = f[..127].iter().fold(0, |a, b| a ^ b);
        };
        // Frame 0: header block magic
        write_frame(&mut data, 0, &|f| {
            f[0] = b'M';
            f[1] = b'C';
        });
        // Frames 1..15: directory entries, all free
        for frame in 1..16 {
            write_frame(&mut data, frame, &|f| {
                f[0..4].copy_from_slice(&0xa0u32.to_le_bytes()); // free, fresh
                f[8] = 0xff; // no next block
                f[9] = 0xff;
            });
        }
        // Frames 16..36: broken-sector list, all unused
        for frame in 16..36 {
            write_frame(&mut data, frame, &|f| {
                f[0..4].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
                f[8] = 0xff;
                f[9] = 0xff;
            });
        }
        Self::with_data(data)
    }

    /// Wrap an existing card image (must be 128 KiB).
    pub fn with_data(data: Box<[u8]>) -> Self {
        assert_eq!(data.len(), CARD_SIZE, "memory card image must be 128 KiB");
        Self {
            data,
            flag: 0x08,
            dirty: false,
            cmd: Command::None,
            step: 0,
            sector: 0,
            last_tx: 0,
            buf: [0; SECTOR],
            chk: 0,
            recv_chk: 0,
        }
    }

    /// True once after a committed write; the frontend persists the image.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Reset transaction state (card deselected).
    pub fn deselect(&mut self) {
        self.cmd = Command::None;
        self.step = 0;
    }

    /// Exchange one byte; returns (reply, ack).
    pub fn exchange(&mut self, tx: u8) -> (u8, bool) {
        let step = self.step;
        self.step += 1;
        let pre = self.last_tx;
        self.last_tx = tx;

        match step {
            0 => (0xff, true), // reply to the 0x81 select
            1 => {
                self.cmd = match tx {
                    0x52 => Command::Read,
                    0x57 => Command::Write,
                    0x53 => Command::Id,
                    _ => Command::Done,
                };
                if self.cmd == Command::Done {
                    (0xff, false)
                } else {
                    trace!(target: "psx_core::memcard", "command {tx:#04x}");
                    (self.flag, true)
                }
            }
            2 => (0x5a, true),
            3 => (0x5d, true),
            _ => match self.cmd {
                Command::Read => self.read_step(step, tx),
                Command::Write => self.write_step(step, tx, pre),
                Command::Id => match step {
                    4 => (0x5c, true),
                    5 => (0x5d, true),
                    6 => (0x04, true),
                    7 => (0x00, true),
                    8 => (0x00, true),
                    _ => {
                        self.cmd = Command::Done;
                        (0x80, false)
                    }
                },
                _ => (0xff, false),
            },
        }
    }

    fn sector_valid(&self) -> bool {
        (self.sector as usize) < CARD_SIZE / SECTOR
    }

    fn read_step(&mut self, step: u32, tx: u8) -> (u8, bool) {
        match step {
            4 => {
                self.sector = (tx as u16) << 8;
                (0x00, true)
            }
            5 => {
                self.sector |= tx as u16;
                ((self.sector >> 8) as u8, true)
            }
            6 => (0x5c, true),
            7 => (0x5d, true),
            8 => ((self.sector >> 8) as u8, true),
            9 => {
                self.chk = ((self.sector >> 8) as u8) ^ (self.sector as u8);
                (self.sector as u8, true)
            }
            10..=137 => {
                let i = (step - 10) as usize;
                let b = if self.sector_valid() {
                    self.data[self.sector as usize * SECTOR + i]
                } else {
                    0xff
                };
                self.chk ^= b;
                (b, true)
            }
            138 => (self.chk, true),
            _ => {
                self.cmd = Command::Done;
                (0x47, false) // GOOD, final byte has no /ACK
            }
        }
    }

    fn write_step(&mut self, step: u32, tx: u8, pre: u8) -> (u8, bool) {
        match step {
            4 => {
                self.sector = (tx as u16) << 8;
                (0x00, true)
            }
            5 => {
                self.sector |= tx as u16;
                self.chk = ((self.sector >> 8) as u8) ^ (self.sector as u8);
                (pre, true)
            }
            6..=133 => {
                self.buf[(step - 6) as usize] = tx;
                self.chk ^= tx;
                (pre, true)
            }
            134 => {
                self.recv_chk = tx;
                (pre, true)
            }
            135 => (0x5c, true),
            136 => (0x5d, true),
            _ => {
                self.cmd = Command::Done;
                let status = if !self.sector_valid() {
                    0xff // bad sector
                } else if self.recv_chk == self.chk {
                    let s = self.sector as usize * SECTOR;
                    self.data[s..s + SECTOR].copy_from_slice(&self.buf);
                    self.dirty = true;
                    self.flag &= !0x08;
                    debug!(target: "psx_core::memcard", "wrote sector {:#05x}", self.sector);
                    0x47 // GOOD
                } else {
                    0x4e // bad checksum
                };
                (status, false) // final byte has no /ACK
            }
        }
    }
}

impl Default for MemCard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transact(mc: &mut MemCard, bytes: &[u8]) -> Vec<u8> {
        let out = bytes.iter().map(|b| mc.exchange(*b).0).collect();
        mc.deselect();
        out
    }

    #[test]
    fn read_sector0_returns_format_magic() {
        let mut mc = MemCard::new();
        let mut tx = vec![0x81, 0x52, 0x00, 0x00, 0x00, 0x00];
        tx.extend([0u8; 4 + 128 + 2]);
        let rx = transact(&mut mc, &tx);
        assert_eq!(&rx[2..4], &[0x5a, 0x5d]);
        assert_eq!(&rx[6..8], &[0x5c, 0x5d]); // ACKs
        assert_eq!(rx[10], b'M');
        assert_eq!(rx[11], b'C');
        assert_eq!(*rx.last().unwrap(), 0x47); // GOOD
    }

    #[test]
    fn write_commits_sector_and_sets_dirty() {
        let mut mc = MemCard::new();
        let sector = 0x41u16; // first data block
        let payload = [0xabu8; 128];
        let chk = (sector >> 8) as u8 ^ sector as u8 ^ payload.iter().fold(0, |a, b| a ^ b);
        let mut tx = vec![0x81, 0x57, 0x00, 0x00, (sector >> 8) as u8, sector as u8];
        tx.extend(payload);
        tx.push(chk);
        tx.extend([0, 0, 0]); // ACK1, ACK2, status
        let rx = transact(&mut mc, &tx);
        assert_eq!(*rx.last().unwrap(), 0x47);
        assert!(mc.take_dirty());
        assert_eq!(mc.data[sector as usize * 128], 0xab);
    }

    #[test]
    fn bad_checksum_rejected() {
        let mut mc = MemCard::new();
        let mut tx = vec![0x81, 0x57, 0x00, 0x00, 0x00, 0x41];
        tx.extend([0u8; 128]);
        tx.push(0x77); // wrong checksum
        tx.extend([0, 0, 0]);
        let rx = transact(&mut mc, &tx);
        assert_eq!(*rx.last().unwrap(), 0x4e);
        assert!(!mc.take_dirty());
    }
}
