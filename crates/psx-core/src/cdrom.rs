//! CD-ROM controller.
//!
//! Command/response FIFO model with delayed interrupt delivery: each command
//! queues one or more (INTn, response) pairs; a pending pair is delivered
//! only after its deadline passes AND the previous interrupt was acknowledged,
//! which matches how the BIOS drives the drive. Timings are rough
//! approximations for now.
//!
//! The disc is a raw 2352-byte/sector image (track 1). Audio playback (CD-DA)
//! is not implemented yet.

use crate::bus::Irq;
use std::collections::VecDeque;
use tracing::{debug, trace, warn};

/// Raw sector size of BIN images.
pub const RAW_SECTOR: usize = 2352;

const CPU_HZ: u64 = 33_868_800;
/// Rough latency between accepting a command and its first response.
const ACK_DELAY: u64 = 25_000;
/// Extra latency for the second response of two-phase commands.
const COMPLETE_DELAY: u64 = 120_000;

pub struct Disc {
    /// Raw image, 2352-byte sectors, starting at LBA 0 (= MSF 00:02:00).
    data: Vec<u8>,
}

impl Disc {
    pub fn new(data: Vec<u8>) -> Result<Self, String> {
        if data.is_empty() || data.len() % RAW_SECTOR != 0 {
            return Err(format!(
                "disc image size {} is not a multiple of {RAW_SECTOR} bytes",
                data.len()
            ));
        }
        Ok(Self { data })
    }

    fn sector(&self, lba: u32) -> Option<&[u8]> {
        let ofs = lba as usize * RAW_SECTOR;
        self.data.get(ofs..ofs + RAW_SECTOR)
    }

    fn sector_count(&self) -> u32 {
        (self.data.len() / RAW_SECTOR) as u32
    }
}

/// Drive status byte bits.
mod stat {
    pub const MOTOR_ON: u8 = 1 << 1;
    pub const READING: u8 = 1 << 5;
}

pub struct Cdrom {
    disc: Option<Disc>,
    index: u8,
    params: VecDeque<u8>,
    response: VecDeque<u8>,
    /// Queued interrupts: (deadline, INT number, response bytes).
    pending: VecDeque<(u64, u8, Vec<u8>)>,
    int_enable: u8,
    /// Low 3 bits: currently asserted INT number (0 = none).
    int_flag: u8,
    mode: u8,
    motor_on: bool,
    reading: bool,
    /// Seek target (LBA) latched by Setloc, applied by Seek/Read.
    seek_target: u32,
    read_lba: u32,
    /// Payload of the most recently announced sector (INT1).
    sector_buffer: Vec<u8>,
    /// Data FIFO exposed at register 2 / DMA channel 3.
    data: Vec<u8>,
    data_pos: usize,
}

impl Cdrom {
    pub fn new() -> Self {
        Self {
            disc: None,
            index: 0,
            params: VecDeque::new(),
            response: VecDeque::new(),
            pending: VecDeque::new(),
            int_enable: 0,
            int_flag: 0,
            mode: 0,
            motor_on: true,
            reading: false,
            seek_target: 0,
            read_lba: 0,
            sector_buffer: Vec::new(),
            data: Vec::new(),
            data_pos: 0,
        }
    }

    pub fn insert_disc(&mut self, disc: Disc) {
        debug!(target: "psx_core::cdrom", "disc inserted: {} sectors", disc.sector_count());
        self.disc = Some(disc);
    }

    fn stat_byte(&self) -> u8 {
        let mut s = 0;
        if self.motor_on {
            s |= stat::MOTOR_ON;
        }
        if self.reading {
            s |= stat::READING;
        }
        s
    }

    /// Cycles per sector at the current speed (mode bit 7 = double).
    fn sector_period(&self) -> u64 {
        if self.mode & 0x80 != 0 {
            CPU_HZ / 150
        } else {
            CPU_HZ / 75
        }
    }

    // --- Interrupt delivery -------------------------------------------

    /// Deliver a due interrupt if the previous one has been acknowledged.
    /// Called every instruction; the front-of-queue check is cheap.
    pub fn tick(&mut self, now: u64, irq: &mut Irq) {
        if self.int_flag & 7 != 0 {
            return;
        }
        let Some((deadline, _, _)) = self.pending.front() else {
            return;
        };
        if *deadline > now {
            return;
        }
        let (_, int, resp) = self.pending.pop_front().unwrap();
        trace!(target: "psx_core::cdrom", "deliver INT{int} {resp:02x?}");
        self.response.clear();
        self.response.extend(resp);
        self.int_flag = (self.int_flag & !7) | int;
        if self.int_enable & (1 << (int - 1)) != 0 {
            irq.raise(2);
        }

        // Streaming: announcing a sector (INT1) stages its payload and
        // schedules the next one.
        if int == 1 && self.reading {
            self.stage_sector();
            let next = now + self.sector_period();
            let st = self.stat_byte();
            self.pending.push_back((next, 1, vec![st]));
        }
    }

    fn stage_sector(&mut self) {
        let Some(disc) = &self.disc else { return };
        let Some(raw) = disc.sector(self.read_lba) else {
            warn!(target: "psx_core::cdrom", "read past end of disc at LBA {}", self.read_lba);
            self.reading = false;
            return;
        };
        // Mode bit 5: 0x924 bytes from the header, else 0x800 of user data
        // (mode2 form1: 12 sync + 4 header + 8 subheader = offset 0x18).
        self.sector_buffer = if self.mode & 0x20 != 0 {
            raw[0xc..0xc + 0x924].to_vec()
        } else {
            raw[0x18..0x18 + 0x800].to_vec()
        };
        trace!(target: "psx_core::cdrom", "staged LBA {}", self.read_lba);
        self.read_lba += 1;
    }

    fn push_int(&mut self, now: u64, delay: u64, int: u8, resp: Vec<u8>) {
        // Chain after any queued response so orders stay FIFO
        let base = self
            .pending
            .back()
            .map(|(d, _, _)| *d)
            .unwrap_or(now)
            .max(now);
        self.pending.push_back((base + delay, int, resp));
    }

    // --- Register interface -------------------------------------------

    pub fn read8(&mut self, p: u32) -> u8 {
        match p & 3 {
            0 => {
                let mut s = self.index & 3;
                if self.params.is_empty() {
                    s |= 1 << 3;
                }
                if self.params.len() < 16 {
                    s |= 1 << 4;
                }
                if !self.response.is_empty() {
                    s |= 1 << 5;
                }
                if self.data_pos < self.data.len() {
                    s |= 1 << 6;
                }
                s
            }
            1 => self.response.pop_front().unwrap_or(0),
            2 => self.read_data_byte(),
            _ => match self.index {
                0 | 2 => self.int_enable | 0xe0,
                _ => self.int_flag | 0xe0,
            },
        }
    }

    fn read_data_byte(&mut self) -> u8 {
        let b = self.data.get(self.data_pos).copied().unwrap_or(0);
        self.data_pos += 1;
        b
    }

    /// 32-bit read for DMA channel 3.
    pub fn dma_read_word(&mut self) -> u32 {
        u32::from_le_bytes([
            self.read_data_byte(),
            self.read_data_byte(),
            self.read_data_byte(),
            self.read_data_byte(),
        ])
    }

    pub fn write8(&mut self, p: u32, val: u8, now: u64) {
        match (p & 3, self.index) {
            (0, _) => self.index = val & 3,
            (1, 0) => self.command(val, now),
            (2, 0) => {
                if self.params.len() < 16 {
                    self.params.push_back(val);
                }
            }
            (2, 1) => self.int_enable = val & 0x1f,
            (3, 0) => {
                // Request register: bit 7 (BFRD) latches the staged sector
                // into the data FIFO; clearing it empties the FIFO.
                if val & 0x80 != 0 {
                    if self.data_pos >= self.data.len() {
                        self.data = std::mem::take(&mut self.sector_buffer);
                        self.data_pos = 0;
                    }
                } else {
                    self.data.clear();
                    self.data_pos = 0;
                }
            }
            (3, 1) => {
                // Interrupt flag acknowledge (write-1-to-clear)
                self.int_flag &= !(val & 0x1f);
                if val & 0x40 != 0 {
                    self.params.clear();
                }
            }
            (1, _) | (2, _) | (3, _) => {
                // Sound map / volume registers: no SPU routing yet
                trace!(target: "psx_core::cdrom", "audio reg write idx{} r{} = {val:#04x}",
                       self.index, p & 3);
            }
            _ => unreachable!(),
        }
    }

    // --- Commands ------------------------------------------------------

    fn command(&mut self, cmd: u8, now: u64) {
        let params: Vec<u8> = self.params.drain(..).collect();
        self.response.clear();
        debug!(target: "psx_core::cdrom", "cmd {cmd:#04x} {params:02x?}");
        let st = self.stat_byte();
        match cmd {
            0x01 => self.push_int(now, ACK_DELAY, 3, vec![st]), // Getstat
            0x02 => {
                // Setloc(mm, ss, ff) in BCD
                let bcd = |v: u8| ((v >> 4) * 10 + (v & 0xf)) as u32;
                if params.len() >= 3 {
                    let (mm, ss, ff) = (bcd(params[0]), bcd(params[1]), bcd(params[2]));
                    self.seek_target = (mm * 60 + ss) * 75 + ff - 150;
                }
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x06 | 0x1b => {
                // ReadN / ReadS
                self.read_lba = self.seek_target;
                self.reading = true;
                self.motor_on = true;
                let st = self.stat_byte();
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, self.sector_period(), 1, vec![st]);
            }
            0x08 => {
                // Stop
                self.reading = false;
                self.push_int(now, ACK_DELAY, 3, vec![self.stat_byte()]);
                self.motor_on = false;
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            0x09 => {
                // Pause
                self.push_int(now, ACK_DELAY, 3, vec![self.stat_byte()]);
                self.reading = false;
                // Drop not-yet-delivered sector announcements
                self.pending.retain(|(_, int, _)| *int != 1);
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            0x0a => {
                // Init: reset mode, stop reading
                self.mode = 0;
                self.reading = false;
                self.motor_on = true;
                let st = self.stat_byte();
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![st]);
            }
            0x0b | 0x0c => self.push_int(now, ACK_DELAY, 3, vec![st]), // Mute / Demute
            0x0d => {
                // Setfilter(file, channel)
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x0e => {
                // Setmode
                if let Some(&m) = params.first() {
                    self.mode = m;
                }
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x10 => {
                // GetlocL: header of the last read sector (approximate)
                let (mm, ss, ff) = lba_to_bcd_msf(self.read_lba);
                self.push_int(now, ACK_DELAY, 3, vec![mm, ss, ff, self.mode, 0, 0, 0, 0]);
            }
            0x11 => {
                // GetlocP: track position (single data track assumed)
                let (mm, ss, ff) = lba_to_bcd_msf(self.read_lba);
                self.push_int(now, ACK_DELAY, 3, vec![1, 1, mm, ss, ff, mm, ss, ff]);
            }
            0x13 => {
                // GetTN: single data track
                self.push_int(now, ACK_DELAY, 3, vec![st, 0x01, 0x01]);
            }
            0x14 => {
                // GetTD: track start (track 1 = 00:02, end-of-disc for 0)
                let lba = match params.first() {
                    Some(0) => self.disc.as_ref().map_or(0, Disc::sector_count),
                    _ => 0,
                };
                let (mm, ss, _) = lba_to_bcd_msf(lba);
                self.push_int(now, ACK_DELAY, 3, vec![st, mm, ss]);
            }
            0x15 | 0x16 => {
                // SeekL / SeekP
                self.read_lba = self.seek_target;
                self.reading = false;
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            0x19 => {
                // Test: only the BIOS-version sub-command is meaningful here
                match params.first() {
                    Some(0x20) => self.push_int(now, ACK_DELAY, 3, vec![0x94, 0x09, 0x19, 0xc0]),
                    sub => {
                        warn!(target: "psx_core::cdrom", "Test sub-command {sub:02x?} stubbed");
                        self.push_int(now, ACK_DELAY, 3, vec![st]);
                    }
                }
            }
            0x1a => {
                // GetID
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                if self.disc.is_some() {
                    // Licensed NTSC-J disc
                    self.push_int(
                        now,
                        COMPLETE_DELAY,
                        2,
                        vec![0x02, 0x00, 0x20, 0x00, b'S', b'C', b'E', b'I'],
                    );
                } else {
                    // Door closed, no disc
                    self.push_int(now, COMPLETE_DELAY, 5, vec![0x08, 0x40, 0, 0, 0, 0, 0, 0]);
                }
            }
            0x1e => {
                // ReadTOC
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            _ => {
                warn!(target: "psx_core::cdrom", "unknown command {cmd:#04x}");
                self.push_int(now, ACK_DELAY, 5, vec![0x11, 0x40]);
            }
        }
    }
}

impl Default for Cdrom {
    fn default() -> Self {
        Self::new()
    }
}

/// LBA -> BCD (mm, ss, ff), including the 2-second lead-in offset.
fn lba_to_bcd_msf(lba: u32) -> (u8, u8, u8) {
    let abs = lba + 150;
    let to_bcd = |v: u32| (((v / 10) << 4) | (v % 10)) as u8;
    (
        to_bcd(abs / (60 * 75)),
        to_bcd(abs / 75 % 60),
        to_bcd(abs % 75),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acked(cd: &mut Cdrom, irq: &mut Irq, now: u64) -> (u8, Vec<u8>) {
        cd.tick(now, irq);
        let int = cd.int_flag & 7;
        let resp: Vec<u8> = cd.response.iter().copied().collect();
        cd.write8(3, 0x1f, now); // ack (index must be 1)
        (int, resp)
    }

    #[test]
    fn getstat_yields_int3() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.write8(1, 0x01, 0); // Getstat (index 0)
        cd.write8(0, 1, 0); // switch to index 1 for the flag register
        let (int, resp) = acked(&mut cd, &mut irq, ACK_DELAY + 1);
        assert_eq!(int, 3);
        assert_eq!(resp, vec![stat::MOTOR_ON]);
        assert!(irq.stat & (1 << 2) != 0);
    }

    #[test]
    fn getid_without_disc_reports_int5() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.write8(1, 0x1a, 0);
        cd.write8(0, 1, 0);
        let (int, _) = acked(&mut cd, &mut irq, ACK_DELAY + 1);
        assert_eq!(int, 3);
        let (int, resp) = acked(&mut cd, &mut irq, ACK_DELAY + COMPLETE_DELAY + 2);
        assert_eq!(int, 5);
        assert_eq!(resp[0], 0x08);
    }

    #[test]
    fn readn_streams_sector_data() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        // Two-sector disc, marker byte at the start of sector 0's payload
        let mut img = vec![0u8; RAW_SECTOR * 2];
        img[0x18] = 0xab;
        cd.insert_disc(Disc::new(img).unwrap());
        // Setloc 00:02:00 (LBA 0), then ReadN
        cd.write8(2, 0x00, 0);
        cd.write8(2, 0x02, 0);
        cd.write8(2, 0x00, 0);
        cd.write8(1, 0x02, 0);
        cd.write8(0, 1, 0);
        let (int, _) = acked(&mut cd, &mut irq, ACK_DELAY + 1);
        assert_eq!(int, 3);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 100_000); // ReadN
        cd.write8(0, 1, 0);
        let t = 100_000 + ACK_DELAY + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 3);
        let t = t + CPU_HZ / 75 + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 1); // first sector announced
        // Latch it into the data FIFO and read the marker
        cd.write8(0, 0, 0);
        cd.write8(3, 0x80, t);
        assert_eq!(cd.read8(2), 0xab);
    }
}
