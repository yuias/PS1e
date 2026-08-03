//! CD-ROM controller.
//!
//! Command/response FIFO model with delayed interrupt delivery: each command
//! queues one or more (INTn, response) pairs; a pending pair is delivered
//! only after its deadline passes AND the previous interrupt was
//! acknowledged, which matches how the BIOS drives the drive.
//!
//! Sector streaming is evaluated per sector at delivery time: realtime XA
//! audio sectors are decoded to PCM (drained into the SPU's CD input by the
//! system) instead of being announced as data, exactly like the real drive.
//!
//! Seek and pause latencies model the mechanics coarsely (distance-based),
//! so boot/loading pacing resembles real hardware.

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

const ADPCM_POS: [i32; 5] = [0, 60, 115, 98, 122];
const ADPCM_NEG: [i32; 5] = [0, 0, -52, -55, -60];

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
    /// Queued command interrupts: (deadline, INT number, response bytes).
    pending: VecDeque<(u64, u8, Vec<u8>)>,
    int_enable: u8,
    /// Low 3 bits: currently asserted INT number (0 = none).
    int_flag: u8,
    mode: u8,
    filter_file: u8,
    filter_channel: u8,
    motor_on: bool,
    reading: bool,
    /// CD audio mute (Mute/Demute commands). Muted XA decodes are
    /// discarded — games mute right before Pause to cut the tail.
    muted: bool,
    /// Seek target (LBA) latched by Setloc, applied by Seek/Read.
    seek_target: u32,
    read_lba: u32,
    /// Head position used for distance-based seek latency.
    head_lba: u32,
    /// When the next sector passes under the head during reading.
    next_sector_at: u64,
    /// Header + subheader (mm,ss,ff,mode,file,channel,submode,coding) of
    /// the last sector that passed under the head — GetlocL returns this.
    /// Games poll it during XA playback to spot the EOF submode flag that
    /// terminates a voice clip.
    last_header: [u8; 8],
    /// Payload of the most recently announced data sector (INT1).
    sector_buffer: Vec<u8>,
    /// Data FIFO exposed at register 2 / DMA channel 3.
    data: Vec<u8>,
    data_pos: usize,
    /// Decoded XA-ADPCM audio, interleaved stereo at 44.1kHz, drained into
    /// the SPU by the system.
    pub xa_out: VecDeque<i16>,
    xa_hist: [(i32, i32); 2],
    /// Last-seen XA coding byte, for change logging.
    xa_last_coding: u8,
    /// Bring-up statistics: decoded sectors, pushed output frames, and
    /// frames lost to the buffer cap (indicates production outpacing
    /// consumption — heard as fast-forward garble).
    pub xa_sectors: u64,
    pub xa_frames: u64,
    pub xa_dropped: u64,
    /// Resampler state: previous input frame and output phase in [0, 1)
    /// scaled by the source rate.
    xa_prev: (i16, i16),
    xa_phase: u32,
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
            filter_file: 0,
            filter_channel: 0,
            motor_on: true,
            reading: false,
            muted: false,
            seek_target: 0,
            read_lba: 0,
            head_lba: 0,
            next_sector_at: 0,
            last_header: [0; 8],
            sector_buffer: Vec::new(),
            data: Vec::new(),
            data_pos: 0,
            xa_out: VecDeque::new(),
            xa_hist: [(0, 0); 2],
            xa_last_coding: 0xff,
            xa_sectors: 0,
            xa_frames: 0,
            xa_dropped: 0,
            xa_prev: (0, 0),
            xa_phase: 0,
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

    /// Coarse mechanical seek latency: 15ms base plus up to ~330ms of
    /// distance-dependent sled travel.
    fn seek_cycles(&self, target: u32) -> u64 {
        let dist = target.abs_diff(self.head_lba) as u64;
        let ms = 15 + (dist / 1000).min(330);
        CPU_HZ / 1000 * ms
    }

    // --- Interrupt delivery -------------------------------------------

    fn deliver(&mut self, int: u8, resp: &[u8], irq: &mut Irq) {
        trace!(target: "psx_core::cdrom", "deliver INT{int} {resp:02x?}");
        self.response.clear();
        self.response.extend(resp);
        self.int_flag = (self.int_flag & !7) | int;
        if self.int_enable & (1 << (int - 1)) != 0 {
            irq.raise(2);
        }
    }

    /// Called every instruction; front-of-queue checks are cheap.
    pub fn tick(&mut self, now: u64, irq: &mut Irq) {
        // Queued command responses (need the previous INT acknowledged)
        if self.int_flag & 7 == 0 {
            if let Some((deadline, _, _)) = self.pending.front() {
                if *deadline <= now {
                    let (_, int, resp) = self.pending.pop_front().unwrap();
                    self.deliver(int, &resp, irq);
                    return;
                }
            }
        }

        // Sector streaming
        if self.reading && now >= self.next_sector_at {
            self.process_sector(now, irq);
        }
    }

    /// Handle the sector currently under the head: route realtime XA audio
    /// to the decoder, announce data sectors via INT1.
    fn process_sector(&mut self, now: u64, irq: &mut Irq) {
        enum Action {
            Xa(Vec<u8>),
            XaFiltered,
            Data(Vec<u8>),
            End,
        }
        let mut header = [0u8; 8];
        let action = match self.disc.as_ref().and_then(|d| d.sector(self.read_lba)) {
            None => Action::End,
            Some(raw) => {
                header.copy_from_slice(&raw[0x0c..0x14]);
                let (file, channel, submode) = (raw[0x10], raw[0x11], raw[0x12]);
                // Realtime + audio submode bits, with the XA mode enabled
                if self.mode & 0x40 != 0 && submode & 0x44 == 0x44 {
                    let pass = self.mode & 0x08 == 0
                        || (file == self.filter_file && channel == self.filter_channel);
                    if pass {
                        Action::Xa(raw.to_vec())
                    } else {
                        Action::XaFiltered
                    }
                } else if self.mode & 0x20 != 0 {
                    Action::Data(raw[0xc..0xc + 0x924].to_vec())
                } else {
                    Action::Data(raw[0x18..0x18 + 0x800].to_vec())
                }
            }
        };
        match action {
            Action::End => {
                warn!(target: "psx_core::cdrom",
                      "read past end of disc at LBA {}", self.read_lba);
                self.reading = false;
            }
            Action::Xa(raw) => {
                if self.muted {
                    // Muted playback doesn't reach the SPU; the drive keeps
                    // its normal pace (nothing gates it)
                    self.last_header = header;
                    self.advance_sector(now);
                    return;
                }
                // Back-pressure: voice files are often stored WITHOUT
                // interleave (every sector is audio), which arrives ~14x
                // faster than playback. The real decoder chain gates the
                // drive through its buffering; model that by holding the
                // sector until the decoded backlog drains below ~2 sectors.
                // The header (GetlocL) only updates once the sector is
                // actually consumed.
                if self.xa_out.len() / 2 > 9_408 {
                    return;
                }
                self.last_header = header;
                trace!(target: "psx_core::cdrom",
                       "XA sector LBA {} file {} ch {}", self.read_lba, raw[0x10], raw[0x11]);
                self.decode_xa_sector(&raw);
                self.advance_sector(now);
            }
            Action::XaFiltered => {
                self.last_header = header;
                self.advance_sector(now);
            }
            Action::Data(payload) => {
                // Hold until the previous interrupt is acknowledged
                if self.int_flag & 7 != 0 || !self.pending.is_empty() {
                    return;
                }
                self.last_header = header;
                trace!(target: "psx_core::cdrom", "staged LBA {}", self.read_lba);
                self.sector_buffer = payload;
                let st = self.stat_byte();
                self.deliver(1, &[st], irq);
                self.advance_sector(now);
            }
        }
    }

    fn advance_sector(&mut self, now: u64) {
        self.read_lba += 1;
        self.head_lba = self.read_lba;
        self.next_sector_at = now + self.sector_period();
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

    // --- XA-ADPCM ------------------------------------------------------

    /// Decode one form2 realtime audio sector (18 sound groups) into
    /// 44.1kHz stereo PCM.
    fn decode_xa_sector(&mut self, raw: &[u8]) {
        let coding = raw[0x13];
        let stereo = coding & 3 == 1;
        let rate = if coding & 0x0c == 0x04 { 18_900 } else { 37_800 };
        let bits8 = coding & 0x30 == 0x10;
        if coding != self.xa_last_coding {
            self.xa_last_coding = coding;
            debug!(target: "psx_core::cdrom",
                   "XA coding {coding:#04x}: {} {rate}Hz {}bit (file={} ch={})",
                   if stereo { "stereo" } else { "mono" },
                   if bits8 { 8 } else { 4 },
                   raw[0x10], raw[0x11]);
        }
        let data = &raw[0x18..0x18 + 2304];
        self.xa_sectors += 1;

        let mut unit_buf = [[0i32; 28]; 2];
        for group in data.chunks(128) {
            let params = &group[..16];
            let d = &group[16..];
            let units = if bits8 { 4 } else { 8 };
            for u in 0..units {
                let hdr = params[4 + u];
                let shift = (hdr & 0xf).min(12) as i32;
                let filter = ((hdr >> 4) & 3) as usize;
                let ch = if stereo { u & 1 } else { 0 };
                for i in 0..28 {
                    let s = if bits8 {
                        (d[i * 4 + u] as i8 as i32) << 8
                    } else {
                        let b = d[i * 4 + u / 2];
                        let n = (b >> ((u & 1) * 4)) & 0xf;
                        (((n as i32) << 28) >> 28) << 12
                    } >> shift;
                    let h = &mut self.xa_hist[ch];
                    let v = (s + (h.0 * ADPCM_POS[filter] + h.1 * ADPCM_NEG[filter] + 32) / 64)
                        .clamp(-0x8000, 0x7fff);
                    h.1 = h.0;
                    h.0 = v;
                    unit_buf[ch][i] = v;
                }
                let complete_pair = !stereo || u & 1 == 1;
                if complete_pair {
                    for i in 0..28 {
                        let l = unit_buf[0][i];
                        let r = if stereo { unit_buf[1][i] } else { l };
                        self.push_xa_frame(l as i16, r as i16, rate);
                    }
                }
            }
        }
        // Bound the buffer (~2s) in case nothing drains it
        while self.xa_out.len() > 88_200 * 2 {
            self.xa_out.pop_front();
            self.xa_out.pop_front();
            self.xa_dropped += 1;
            if self.xa_dropped == 1 || self.xa_dropped % 44_100 == 0 {
                warn!(target: "psx_core::cdrom",
                      "XA output overflowing ({} frames dropped) — production \
                       outpacing 44.1kHz consumption", self.xa_dropped);
            }
        }
    }

    /// Linear resample from the XA rate to the SPU's 44100 Hz. Output
    /// samples between the previous and current input frame interpolate
    /// on the phase accumulator.
    fn push_xa_frame(&mut self, l: i16, r: i16, src_rate: u32) {
        while self.xa_phase < 44_100 {
            let t = self.xa_phase as i32;
            let lerp = |a: i16, b: i16| {
                (a as i32 + (b as i32 - a as i32) * t / 44_100) as i16
            };
            self.xa_out.push_back(lerp(self.xa_prev.0, l));
            self.xa_out.push_back(lerp(self.xa_prev.1, r));
            self.xa_frames += 1;
            self.xa_phase += src_rate;
        }
        self.xa_phase -= 44_100;
        self.xa_prev = (l, r);
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
                // into the data FIFO; clearing it empties the FIFO. A newly
                // staged sector replaces any partially-read remainder.
                if val & 0x80 != 0 {
                    if !self.sector_buffer.is_empty() {
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
            0x07 => {
                // MotorOn
                self.motor_on = true;
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            0x06 | 0x1b => {
                // ReadN / ReadS: implicit seek to the Setloc target.
                // Undelivered XA from a previous stream is flushed so clips
                // don't bleed into each other.
                let seek = self.seek_cycles(self.seek_target);
                self.read_lba = self.seek_target;
                self.reading = true;
                self.motor_on = true;
                self.xa_out.clear();
                self.xa_hist = [(0, 0); 2];
                self.xa_prev = (0, 0);
                self.xa_phase = 0;
                self.next_sector_at = now + ACK_DELAY + seek + self.sector_period();
                let st = self.stat_byte();
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x08 => {
                // Stop
                self.reading = false;
                self.push_int(now, ACK_DELAY, 3, vec![self.stat_byte()]);
                self.motor_on = false;
                self.push_int(now, CPU_HZ / 2, 2, vec![self.stat_byte()]);
            }
            0x09 => {
                // Pause: ~70ms at single speed, half at double
                self.push_int(now, ACK_DELAY, 3, vec![self.stat_byte()]);
                self.reading = false;
                let pause = if self.mode & 0x80 != 0 {
                    CPU_HZ / 1000 * 35
                } else {
                    CPU_HZ / 1000 * 70
                };
                self.push_int(now, pause, 2, vec![self.stat_byte()]);
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
            0x0b => {
                // Mute: silence CD audio immediately, including anything
                // already decoded but not yet mixed
                self.muted = true;
                self.xa_out.clear();
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x0c => {
                self.muted = false;
                self.push_int(now, ACK_DELAY, 3, vec![st]);
            }
            0x0d => {
                // Setfilter(file, channel)
                if params.len() >= 2 {
                    self.filter_file = params[0];
                    self.filter_channel = params[1];
                }
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
                // GetlocL: real header + subheader of the last read sector.
                // The subheader matters: XA voice clips end with an
                // EOF-flagged sector that games poll for here.
                self.push_int(now, ACK_DELAY, 3, self.last_header.to_vec());
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
                // SeekL / SeekP with distance-based latency
                let seek = self.seek_cycles(self.seek_target);
                self.read_lba = self.seek_target;
                self.head_lba = self.seek_target;
                self.reading = false;
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, seek, 2, vec![self.stat_byte()]);
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
                // ReadTOC: a full TOC scan takes about a second
                self.push_int(now, ACK_DELAY, 3, vec![st]);
                self.push_int(now, CPU_HZ, 2, vec![self.stat_byte()]);
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
        // Includes the implicit-seek latency before the first sector
        let t = t + cd.seek_cycles(0) + CPU_HZ / 75 + ACK_DELAY + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 1); // first sector announced
        cd.write8(0, 0, 0);
        cd.write8(3, 0x80, t);
        assert_eq!(cd.read8(2), 0xab);
    }

    #[test]
    fn getlocl_reports_real_subheader_including_eof() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        let mut img = vec![0u8; RAW_SECTOR];
        img[0x0c..0x0f].copy_from_slice(&[0x00, 0x02, 0x00]); // header MSF
        img[0x10] = 2; // file
        img[0x11] = 1; // channel
        img[0x12] = 0xc4; // submode: EOF + realtime + audio
        img[0x13] = 0x00; // coding: mono 37800
        cd.insert_disc(Disc::new(img).unwrap());
        cd.write8(2, 0xc0, 0);
        cd.write8(1, 0x0e, 0); // Setmode XA
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_DELAY + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 0); // ReadN LBA 0
        cd.write8(0, 1, 0);
        let t = ACK_DELAY + 2;
        acked(&mut cd, &mut irq, t);
        let t = t + cd.seek_cycles(0) + CPU_HZ / 150 + ACK_DELAY;
        cd.tick(t, &mut irq); // XA sector consumed silently
        cd.write8(0, 0, 0);
        cd.write8(1, 0x10, t); // GetlocL
        cd.write8(0, 1, 0);
        let (int, resp) = acked(&mut cd, &mut irq, t + ACK_DELAY + 1);
        assert_eq!(int, 3);
        assert_eq!(resp, vec![0x00, 0x02, 0x00, 0x00, 2, 1, 0xc4, 0x00]);
    }

    #[test]
    fn contiguous_xa_is_throttled_to_playback_rate() {
        // 20 back-to-back mono audio sectors (no interleave, like voice
        // files): the drive must gate on the decoder instead of dropping.
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        let mut img = vec![0u8; RAW_SECTOR * 20];
        for s in 0..20 {
            img[s * RAW_SECTOR + 0x12] = 0x44; // realtime + audio
            img[s * RAW_SECTOR + 0x13] = 0x00; // mono 37800Hz 4bit
        }
        cd.insert_disc(Disc::new(img).unwrap());
        cd.write8(2, 0xc0, 0); // double speed + XA
        cd.write8(1, 0x0e, 0);
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_DELAY + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 0); // ReadN from LBA 0
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_DELAY + 1);

        // Drain like the SPU: 1 frame per 768 cycles
        let mut now = ACK_DELAY + 2;
        let mut consumed = 0u64;
        // Mono 37800 sector -> 4704 output frames; 20 sectors ~ 2.1s audio
        for _ in 0..150_000_000u64 / 768 {
            now += 768;
            cd.tick(now, &mut irq);
            if cd.xa_out.len() >= 2 {
                cd.xa_out.pop_front();
                cd.xa_out.pop_front();
                consumed += 1;
            }
        }
        assert_eq!(cd.xa_dropped, 0, "back-pressure must prevent drops");
        assert_eq!(cd.xa_sectors, 20);
        // All frames delivered at the consumption rate
        assert_eq!(consumed, cd.xa_frames);
    }

    #[test]
    fn realtime_xa_sector_is_decoded_not_announced() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        let mut img = vec![0u8; RAW_SECTOR];
        img[0x12] = 0x44; // submode: realtime + audio
        img[0x13] = 0x01; // coding: stereo, 37800 Hz, 4-bit
        for i in 0..2304 {
            img[0x18 + i] = 0x11; // arbitrary non-zero nibbles
        }
        cd.insert_disc(Disc::new(img).unwrap());
        cd.write8(2, 0xc0, 0); // Setmode: double speed + XA enable
        cd.write8(1, 0x0e, 0);
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_DELAY + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 200_000); // ReadN from LBA 0
        cd.write8(0, 1, 0);
        let t = 200_000 + ACK_DELAY + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 3);
        let t = t + cd.seek_cycles(0) + CPU_HZ / 150 + ACK_DELAY + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 0, "XA sector must not raise INT1");
        // 18 groups * 8 units * 28 samples = 2016 stereo frames at 37800 Hz
        // -> ~2352 frames after resampling to 44100
        assert!(cd.xa_out.len() / 2 > 2000, "got {} frames", cd.xa_out.len() / 2);
    }
}
