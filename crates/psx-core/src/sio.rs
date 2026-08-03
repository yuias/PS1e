//! SIO0: controller / memory-card port.
//!
//! Implements the digital pad protocol on slot 1. Memory cards and slot 2
//! respond as absent (0xff, no /ACK). Byte exchange completes instantly;
//! the /ACK interrupt is latched immediately after each non-final byte.

use crate::bus::Irq;
use crate::memcard::MemCard;
use tracing::trace;

/// /ACK arrives roughly 100us after the byte transfer on real hardware.
/// Raising it during the JOY_DATA write is too early: the kernel ISR writes
/// TX first and acknowledges the previous IRQ afterwards, which would wipe
/// an instantly-raised interrupt and stall the transaction.
const ACK_DELAY_CYCLES: u64 = 1500;

/// Button bits (active low on the wire). Bit set in `buttons` = pressed.
pub mod button {
    pub const SELECT: u16 = 1 << 0;
    pub const START: u16 = 1 << 3;
    pub const UP: u16 = 1 << 4;
    pub const RIGHT: u16 = 1 << 5;
    pub const DOWN: u16 = 1 << 6;
    pub const LEFT: u16 = 1 << 7;
    pub const L2: u16 = 1 << 8;
    pub const R2: u16 = 1 << 9;
    pub const L1: u16 = 1 << 10;
    pub const R1: u16 = 1 << 11;
    pub const TRIANGLE: u16 = 1 << 12;
    pub const CIRCLE: u16 = 1 << 13;
    pub const CROSS: u16 = 1 << 14;
    pub const SQUARE: u16 = 1 << 15;
}

/// Transaction target decided by the first byte on the wire.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Device {
    None,
    Pad,
    MemCard,
    Absent,
}

pub struct Sio {
    ctrl: u16,
    mode: u16,
    baud: u16,
    rx: Option<u8>,
    /// Byte position within the current transaction.
    seq: u8,
    device: Device,
    irq_flag: bool,
    /// Cycle at which the pending /ACK interrupt fires.
    ack_at: Option<u64>,
    /// Currently pressed buttons (host convention: set = pressed).
    pub buttons: u16,
    /// Memory card in slot 1.
    pub memcard: MemCard,
}

impl Sio {
    pub fn new() -> Self {
        Self {
            ctrl: 0,
            mode: 0,
            baud: 0,
            rx: None,
            seq: 0,
            device: Device::None,
            irq_flag: false,
            ack_at: None,
            buttons: 0,
            memcard: MemCard::new(),
        }
    }

    /// Fire a due /ACK interrupt. Called every instruction; cheap check.
    pub fn tick(&mut self, now: u64, irq: &mut Irq) {
        if let Some(at) = self.ack_at
            && now >= at
        {
            self.ack_at = None;
            self.irq_flag = true;
            if self.ctrl & (1 << 12) != 0 {
                irq.raise(7);
            }
        }
    }

    pub fn read_data(&mut self) -> u8 {
        self.rx.take().unwrap_or(0xff)
    }

    pub fn read_stat(&self) -> u32 {
        let mut s = 0u32;
        s |= 1 << 0; // TX FIFO not full
        if self.rx.is_some() {
            s |= 1 << 1;
        }
        s |= 1 << 2; // TX idle
        if self.irq_flag {
            s |= 1 << 9;
        }
        s
    }

    pub fn read_reg16(&self, p: u32) -> u16 {
        match p & 0xf {
            0x8 => self.mode,
            0xa => self.ctrl,
            0xe => self.baud,
            _ => 0,
        }
    }

    pub fn write_reg16(&mut self, p: u32, val: u16) {
        match p & 0xf {
            0x8 => self.mode = val,
            0xa => {
                self.ctrl = val & !0x50; // ack/reset bits don't latch
                if val & (1 << 4) != 0 {
                    self.irq_flag = false;
                }
                if val & (1 << 6) != 0 {
                    // Reset
                    self.rx = None;
                    self.seq = 0;
                    self.device = Device::None;
                    self.irq_flag = false;
                    self.ack_at = None;
                }
                // Deselecting /JOYn ends the transaction
                if val & (1 << 1) == 0 {
                    self.seq = 0;
                    self.device = Device::None;
                    self.memcard.deselect();
                }
            }
            0xe => self.baud = val,
            _ => {}
        }
    }

    /// TX write: exchange one byte with the selected device.
    pub fn write_data(&mut self, tx: u8, now: u64) {
        // Needs TX enable and a selected device to reach anything
        if self.ctrl & 1 == 0 || self.ctrl & (1 << 1) == 0 {
            self.rx = Some(0xff);
            return;
        }
        // Slot 2 (ctrl bit 13) has nothing plugged in
        if self.ctrl & (1 << 13) != 0 {
            self.rx = Some(0xff);
            return;
        }

        if self.seq == 0 {
            self.device = match tx {
                0x01 => Device::Pad,
                0x81 => Device::MemCard,
                _ => Device::Absent,
            };
        }
        let (response, ack) = match self.device {
            Device::Pad => self.pad_exchange(tx),
            Device::MemCard => self.memcard.exchange(tx),
            _ => (0xff, false),
        };
        trace!(target: "psx_core::sio", "tx {tx:#04x} -> rx {response:#04x} ack={ack}");
        self.rx = Some(response);
        self.seq += 1;
        if ack {
            self.ack_at = Some(now + ACK_DELAY_CYCLES);
        }
    }

    /// Digital pad: 01 42 -> 41 5A <buttons lo> <buttons hi> (active low).
    fn pad_exchange(&mut self, tx: u8) -> (u8, bool) {
        let wire = !self.buttons; // active low
        match self.seq {
            0 => (0xff, true),
            1 => {
                if tx == 0x42 {
                    (0x41, true)
                } else {
                    self.device = Device::Absent;
                    (0xff, false)
                }
            }
            2 => (0x5a, true),
            3 => (wire as u8, true),
            4 => ((wire >> 8) as u8, false), // final byte: no /ACK
            _ => (0xff, false),
        }
    }
}

impl Default for Sio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digital_pad_handshake() {
        let mut sio = Sio::new();
        let mut irq = Irq::default();
        let mut now = 0u64;
        sio.buttons = button::CROSS | button::START;
        sio.write_reg16(0xa, 0x1003); // TX enable, select, ack IRQ enable
        fn exchange(sio: &mut Sio, irq: &mut Irq, now: &mut u64, tx: u8) -> u8 {
            sio.write_data(tx, *now);
            *now += ACK_DELAY_CYCLES + 1;
            sio.tick(*now, irq);
            sio.read_data()
        }
        assert_eq!(exchange(&mut sio, &mut irq, &mut now, 0x01), 0xff);
        assert!(irq.stat & (1 << 7) != 0, "ACK IRQ arrives after a delay");
        assert_eq!(exchange(&mut sio, &mut irq, &mut now, 0x42), 0x41);
        assert_eq!(exchange(&mut sio, &mut irq, &mut now, 0x00), 0x5a);
        assert_eq!(
            exchange(&mut sio, &mut irq, &mut now, 0x00),
            !(button::START as u8)
        );
        assert_eq!(
            exchange(&mut sio, &mut irq, &mut now, 0x00),
            (!(button::CROSS) >> 8) as u8
        );
    }
}
