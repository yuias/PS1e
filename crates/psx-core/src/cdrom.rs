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
//!
//! The drive lid is modeled too ([`Cdrom::open_shell`] /
//! [`Cdrom::close_shell`]), which is what makes a disc swap observable: the
//! disc itself never changes under a running game, the lid does.

use crate::bus::Irq;
use std::collections::VecDeque;
use tracing::{debug, trace, warn};

/// Raw sector size of BIN images.
pub const RAW_SECTOR: usize = 2352;

const CPU_HZ: u64 = 33_868_800;
// First-response latencies, averaged from hardware measurements. The drive
// answers most commands in the same time, and answers quicker while the
// motor is stopped; Init and ReadTOC initialise the drive before replying.
const ACK_RUNNING: u64 = 0x0000_c4e1;
const ACK_STOPPED: u64 = 0x0000_5cf4;
const ACK_INIT: u64 = 0x0001_3cce;

// Second-response latencies, from the same measurements. Pause and Stop
// both depend on the drive's state; Stop takes longer at double speed than
// at single, as the motor has further to spin down.
const GETID_DELAY: u64 = 0x0000_4a00;
const PAUSE_SINGLE: u64 = 0x0021_181c;
const PAUSE_DOUBLE: u64 = 0x0010_bd93;
const PAUSE_PAUSED: u64 = 0x0000_1df2;
const STOP_SINGLE: u64 = 0x00d3_8aca;
const STOP_DOUBLE: u64 = 0x018a_6076;
const STOP_STOPPED: u64 = 0x0000_1d7b;

/// Second response of the commands whose duration has not been measured.
const COMPLETE_DELAY: u64 = 120_000;

const ADPCM_POS: [i32; 5] = [0, 60, 115, 98, 122];
const ADPCM_NEG: [i32; 5] = [0, 0, -52, -55, -60];

/// One TOC entry.
#[derive(Clone, Copy, Debug)]
pub struct Track {
    pub number: u8,
    pub audio: bool,
    /// Absolute LBA of the track's INDEX 01.
    pub start: u32,
}

pub struct Disc {
    /// Raw image, 2352-byte sectors, starting at LBA 0 (= MSF 00:02:00).
    data: Vec<u8>,
    /// TOC, ascending by start LBA. Never empty.
    tracks: Vec<Track>,
}

impl Disc {
    /// Single data track covering the whole image (plain .bin).
    pub fn new(data: Vec<u8>) -> Result<Self, String> {
        let track = Track {
            number: 1,
            audio: false,
            start: 0,
        };
        Self::with_tracks(data, vec![track])
    }

    /// Image with an explicit TOC (from a cue sheet).
    pub fn with_tracks(data: Vec<u8>, tracks: Vec<Track>) -> Result<Self, String> {
        if data.is_empty() || !data.len().is_multiple_of(RAW_SECTOR) {
            return Err(format!(
                "disc image size {} is not a multiple of {RAW_SECTOR} bytes",
                data.len()
            ));
        }
        let count = (data.len() / RAW_SECTOR) as u32;
        if tracks.is_empty() {
            return Err("disc has no tracks".into());
        }
        if !tracks.windows(2).all(|w| w[0].start <= w[1].start) {
            return Err("track starts are not ascending".into());
        }
        if tracks.iter().any(|t| t.start >= count) {
            return Err("track start beyond end of image".into());
        }
        Ok(Self { data, tracks })
    }

    /// The TOC, ascending by start LBA.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The 2048-byte user data of a data sector, for readers that want the
    /// filesystem rather than the drive (the ISO9660 volume descriptors, for
    /// one). Mode 1 keeps its data right after the header, mode 2 form 1
    /// after the subheader; audio sectors have no such field.
    pub fn user_data(&self, lba: u32) -> Option<&[u8]> {
        let raw = self.sector(lba)?;
        if self.track_at(lba).audio {
            return None;
        }
        let ofs = if raw[0x0f] == 1 { 0x10 } else { 0x18 };
        raw.get(ofs..ofs + 0x800)
    }

    fn sector(&self, lba: u32) -> Option<&[u8]> {
        let ofs = lba as usize * RAW_SECTOR;
        self.data.get(ofs..ofs + RAW_SECTOR)
    }

    fn sector_count(&self) -> u32 {
        (self.data.len() / RAW_SECTOR) as u32
    }

    /// The track containing `lba` (the last one starting at or before it).
    fn track_at(&self, lba: u32) -> Track {
        *self
            .tracks
            .iter()
            .rev()
            .find(|t| t.start <= lba)
            .unwrap_or(&self.tracks[0])
    }

    fn track_start(&self, number: u8) -> Option<u32> {
        self.tracks
            .iter()
            .find(|t| t.number == number)
            .map(|t| t.start)
    }

    fn last_track(&self) -> u8 {
        self.tracks.last().map_or(1, |t| t.number)
    }
}

/// Drive status byte bits.
mod stat {
    pub const ERROR: u8 = 1 << 0;
    pub const MOTOR_ON: u8 = 1 << 1;
    pub const SHELL_OPEN: u8 = 1 << 4;
    pub const READING: u8 = 1 << 5;
    pub const PLAYING: u8 = 1 << 7;
}

/// Error byte of the unsolicited INT5 raised when the lid opens.
const ERR_DOOR_OPENED: u8 = 0x08;
/// Error byte for commands the drive cannot service without a disc it
/// can reach: the lid is open, or the drive is empty.
const ERR_NOT_READY: u8 = 0x80;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Cdrom {
    /// Not part of save states; the frontend re-injects it on load.
    #[serde(skip)]
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
    /// With the XA filter bit off, the drive latches the file/channel of
    /// the first ADPCM sector after a read starts and plays only that
    /// stream — this is how games play one voice out of a multiplexed
    /// bank without ever calling Setfilter.
    xa_latch: Option<(u8, u8)>,
    motor_on: bool,
    /// The lid is physically open right now.
    shell_open: bool,
    /// Sticky "is/was open" half of stat bit 4. Only Getstat clears it, and
    /// only once the lid is shut again — that latch is how a game polling
    /// Getstat learns a swap happened instead of missing the event entirely.
    shell_latched: bool,
    reading: bool,
    /// CD-DA playback (Play command) is active.
    playing: bool,
    /// Track being played, for auto-pause at track boundaries.
    play_track: u8,
    /// Sectors until the next Report interrupt (mode bit 2).
    report_in: u32,
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
            xa_latch: None,
            motor_on: true,
            shell_open: false,
            shell_latched: false,
            reading: false,
            playing: false,
            play_track: 0,
            report_in: 0,
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

    /// Remove the disc (save-state plumbing: the image is carried over
    /// outside the serialized state).
    pub fn take_disc(&mut self) -> Option<Disc> {
        self.disc.take()
    }

    /// Counterpart of [`Cdrom::take_disc`]; `None` leaves the drive empty.
    pub fn set_disc(&mut self, disc: Option<Disc>) {
        self.disc = disc;
    }

    pub fn insert_disc(&mut self, disc: Disc) {
        debug!(target: "psx_core::cdrom", "disc inserted: {} sectors, {} tracks",
               disc.sector_count(), disc.tracks().len());
        self.disc = Some(disc);
    }

    /// Open the drive lid. The disc stays in the drive — what a game
    /// observes is the lid, not the platter: the spindle stops, an
    /// unsolicited INT5 fires and stat bit 4 goes up. Queued command
    /// responses are dropped; on hardware the error pre-empts them.
    pub fn open_shell(&mut self, now: u64) {
        if self.shell_open {
            return;
        }
        debug!(target: "psx_core::cdrom", "shell opened");
        self.shell_open = true;
        self.shell_latched = true;
        self.motor_on = false;
        self.reading = false;
        self.playing = false;
        self.xa_out.clear();
        self.pending.clear();
        let st = self.stat_byte() | stat::ERROR;
        self.push_int(now, self.ack_delay(), 5, vec![st, ERR_DOOR_OPENED]);
    }

    /// Close the lid, optionally over a different disc (`None` puts the
    /// current one back, which is what a cancelled swap does). The motor
    /// spins back up; stat bit 4 stays set until the game's next Getstat,
    /// which is the edge it swaps on.
    pub fn close_shell(&mut self, disc: Option<Disc>) {
        if let Some(disc) = disc {
            self.insert_disc(disc);
        }
        debug!(target: "psx_core::cdrom", "shell closed");
        self.shell_open = false;
        self.motor_on = true;
        // The sled parks at the lead-in while the lid is open, so the first
        // seek after a swap pays full travel time.
        self.head_lba = 0;
    }

    fn stat_byte(&self) -> u8 {
        let mut s = 0;
        if self.motor_on {
            s |= stat::MOTOR_ON;
        }
        if self.shell_open || self.shell_latched {
            s |= stat::SHELL_OPEN;
        }
        if self.reading {
            s |= stat::READING;
        }
        if self.playing {
            s |= stat::PLAYING;
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

    /// Latency before the first response. The drive's mainloop is quicker
    /// to answer while the motor is stopped, since it has less maintenance
    /// work to interleave.
    fn ack_delay(&self) -> u64 {
        if self.motor_on {
            ACK_RUNNING
        } else {
            ACK_STOPPED
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
        if self.int_flag & 7 == 0
            && let Some((deadline, _, _)) = self.pending.front()
            && *deadline <= now
        {
            let (_, int, resp) = self.pending.pop_front().unwrap();
            self.deliver(int, &resp, irq);
            return;
        }

        // Sector streaming
        if self.reading && now >= self.next_sector_at {
            self.process_sector(now, irq);
        } else if self.playing && now >= self.next_sector_at {
            self.process_cdda_sector(now, irq);
        }
    }

    /// Handle one CD-DA sector: 2352 bytes of raw 44.1kHz stereo PCM,
    /// streamed into the SPU's CD input like decoded XA. Auto-pause (mode
    /// bit 1) stops with INT4 at track boundaries; Report (mode bit 2)
    /// emits INT1 position reports along the way.
    fn process_cdda_sector(&mut self, now: u64, irq: &mut Irq) {
        let ended = match self.disc.as_ref().map(|d| d.sector_count()) {
            None => true,
            Some(count) => self.read_lba >= count,
        };
        let crossed = !ended
            && self
                .disc
                .as_ref()
                .is_some_and(|d| d.track_at(self.read_lba).number != self.play_track);
        if ended || (crossed && self.mode & 0x02 != 0) {
            // End of disc always pauses; a track boundary only with the
            // auto-pause mode bit set. INT4 needs the previous INT acked.
            if self.int_flag & 7 != 0 || !self.pending.is_empty() {
                return;
            }
            self.playing = false;
            let st = self.stat_byte();
            self.deliver(4, &[st], irq);
            debug!(target: "psx_core::cdrom",
                   "CD-DA auto-pause at LBA {} ({})", self.read_lba,
                   if ended { "end of disc" } else { "track boundary" });
            return;
        }
        if crossed {
            self.play_track = self.disc.as_ref().unwrap().track_at(self.read_lba).number;
        }

        if self.mode & 0x04 != 0 && self.report_in == 0 {
            // Position report; skipped (not stalled) while an INT is pending
            if self.int_flag & 7 == 0 && self.pending.is_empty() {
                let track = self
                    .disc
                    .as_ref()
                    .map_or(1, |d| d.track_at(self.read_lba).number);
                let (amm, ass, aff) = lba_to_bcd_msf(self.read_lba);
                let st = self.stat_byte();
                self.deliver(
                    1,
                    &[st, to_bcd(track as u32), 0x01, amm, ass, aff, 0, 0],
                    irq,
                );
                self.report_in = 75;
            }
        } else if self.report_in > 0 {
            self.report_in -= 1;
        }

        if self.muted {
            self.advance_sector(now);
            return;
        }
        // Back-pressure, as for XA: hold the sector until the SPU-side
        // backlog drains below ~2 sectors' worth of frames
        if self.xa_out.len() / 2 > 9_408 {
            return;
        }
        let raw = self.disc.as_ref().and_then(|d| d.sector(self.read_lba));
        for frame in raw.unwrap().as_chunks::<4>().0 {
            self.xa_out
                .push_back(i16::from_le_bytes([frame[0], frame[1]]));
            self.xa_out
                .push_back(i16::from_le_bytes([frame[2], frame[3]]));
        }
        self.advance_sector(now);
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
                    let pass = if self.mode & 0x08 != 0 {
                        file == self.filter_file && channel == self.filter_channel
                    } else {
                        // Filter off: first stream wins (see xa_latch)
                        match self.xa_latch {
                            None => {
                                self.xa_latch = Some((file, channel));
                                true
                            }
                            Some(latch) => latch == (file, channel),
                        }
                    };
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
                // Running into the lead-out ends the read the way a CD-DA
                // stream ends at the end of the disc: INT4, once the previous
                // interrupt is acknowledged.
                if self.int_flag & 7 != 0 || !self.pending.is_empty() {
                    return;
                }
                warn!(target: "psx_core::cdrom",
                      "read past end of disc at LBA {}", self.read_lba);
                self.reading = false;
                let st = self.stat_byte();
                self.deliver(4, &[st], irq);
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
        let rate = if coding & 0x0c == 0x04 {
            18_900
        } else {
            37_800
        };
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
                    for (&l, &r) in unit_buf[0].iter().zip(&unit_buf[1]) {
                        let r = if stereo { r } else { l };
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
            if self.xa_dropped == 1 || self.xa_dropped.is_multiple_of(44_100) {
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
            let lerp = |a: i16, b: i16| (a as i32 + (b as i32 - a as i32) * t / 44_100) as i16;
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
        // Without a disc the drive can reach — lid open, or nothing loaded
        // — there is no TOC to lock onto, so everything needing one fails
        // (psx-spx: error 80h on 02h..09h, 0Bh..0Dh, 10h..16h, 1Ah, 1Bh, 1Dh).
        // The command set that still answers is what a game polls to find out
        // when the drive becomes usable. GetID is the exception once the lid
        // is shut: an empty drive reports "no disc" instead of failing.
        let needs_disc = matches!(cmd, 0x02..=0x09 | 0x0b..=0x0d | 0x10..=0x16 | 0x1b | 0x1d)
            || (cmd == 0x1a && self.shell_open);
        if needs_disc && (self.shell_open || self.disc.is_none()) {
            self.push_int(
                now,
                self.ack_delay(),
                5,
                vec![st | stat::ERROR, ERR_NOT_READY],
            );
            return;
        }
        match cmd {
            0x01 => {
                // Getstat, alias Nop. Uniquely among the commands it clears
                // the sticky shell-open flag, once the lid is shut.
                self.shell_latched &= self.shell_open;
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x02 => {
                // Setloc(mm, ss, ff) in BCD
                if params.len() >= 3 {
                    let (mm, ss, ff) = (
                        from_bcd(params[0]),
                        from_bcd(params[1]),
                        from_bcd(params[2]),
                    );
                    self.seek_target = (mm * 60 + ss) * 75 + ff - 150;
                }
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x03 => {
                // Play(track?): CD-DA from the given track's start, or from
                // the Setloc target (games always Setloc before a bare Play)
                let track = params.first().copied().map(from_bcd).unwrap_or(0) as u8;
                let target = if track > 0 {
                    self.disc
                        .as_ref()
                        .and_then(|d| d.track_start(track))
                        .unwrap_or(self.seek_target)
                } else {
                    self.seek_target
                };
                let seek = self.seek_cycles(target);
                self.read_lba = target;
                self.head_lba = target;
                self.reading = false;
                self.playing = true;
                self.motor_on = true;
                self.play_track = self.disc.as_ref().map_or(1, |d| d.track_at(target).number);
                self.report_in = 0;
                self.xa_out.clear();
                self.next_sector_at = now + self.ack_delay() + seek + self.sector_period();
                debug!(target: "psx_core::cdrom",
                       "Play track {} from LBA {target}", self.play_track);
                self.push_int(now, self.ack_delay(), 3, vec![self.stat_byte()]);
            }
            0x07 => {
                // MotorOn
                self.motor_on = true;
                self.push_int(now, self.ack_delay(), 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![self.stat_byte()]);
            }
            0x06 | 0x1b => {
                // ReadN / ReadS: implicit seek to the Setloc target.
                // Undelivered XA from a previous stream is flushed so clips
                // don't bleed into each other.
                let seek = self.seek_cycles(self.seek_target);
                self.read_lba = self.seek_target;
                self.reading = true;
                self.playing = false;
                self.motor_on = true;
                self.xa_out.clear();
                self.xa_hist = [(0, 0); 2];
                self.xa_prev = (0, 0);
                self.xa_phase = 0;
                self.xa_latch = None;
                self.next_sector_at = now + self.ack_delay() + seek + self.sector_period();
                let st = self.stat_byte();
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x08 => {
                // Stop
                self.reading = false;
                self.playing = false;
                self.push_int(now, self.ack_delay(), 3, vec![self.stat_byte()]);
                let spin_down = if !self.motor_on {
                    STOP_STOPPED
                } else if self.mode & 0x80 != 0 {
                    STOP_DOUBLE
                } else {
                    STOP_SINGLE
                };
                self.motor_on = false;
                self.push_int(now, spin_down, 2, vec![self.stat_byte()]);
            }
            0x09 => {
                // Pause: about five sectors' worth of time, unless the
                // drive had already stopped delivering them
                self.push_int(now, self.ack_delay(), 3, vec![self.stat_byte()]);
                let pause = if !self.reading && !self.playing {
                    PAUSE_PAUSED
                } else if self.mode & 0x80 != 0 {
                    PAUSE_DOUBLE
                } else {
                    PAUSE_SINGLE
                };
                self.reading = false;
                self.playing = false;
                self.push_int(now, pause, 2, vec![self.stat_byte()]);
            }
            0x0a => {
                // Init: reset mode, stop reading
                self.mode = 0;
                self.reading = false;
                self.playing = false;
                self.motor_on = true;
                let st = self.stat_byte();
                self.push_int(now, ACK_INIT, 3, vec![st]);
                self.push_int(now, COMPLETE_DELAY, 2, vec![st]);
            }
            0x0b => {
                // Mute: silence CD audio immediately, including anything
                // already decoded but not yet mixed
                self.muted = true;
                self.xa_out.clear();
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x0c => {
                self.muted = false;
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x0d => {
                // Setfilter(file, channel); also re-arms the implicit latch
                if params.len() >= 2 {
                    self.filter_file = params[0];
                    self.filter_channel = params[1];
                }
                self.xa_latch = None;
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x0e => {
                // Setmode
                if let Some(&m) = params.first() {
                    self.mode = m;
                }
                self.push_int(now, self.ack_delay(), 3, vec![st]);
            }
            0x10 => {
                // GetlocL: real header + subheader of the last read sector.
                // The subheader matters: XA voice clips end with an
                // EOF-flagged sector that games poll for here.
                self.push_int(now, self.ack_delay(), 3, self.last_header.to_vec());
            }
            0x11 => {
                // GetlocP: track, index, track-relative and absolute MSF
                let (track, rel) = match self.disc.as_ref() {
                    Some(d) => {
                        let t = d.track_at(self.read_lba);
                        (t.number, self.read_lba.saturating_sub(t.start))
                    }
                    None => (1, self.read_lba),
                };
                let (mm, ss, ff) = frames_to_bcd_msf(rel);
                let (amm, ass, aff) = lba_to_bcd_msf(self.read_lba);
                self.push_int(
                    now,
                    self.ack_delay(),
                    3,
                    vec![to_bcd(track as u32), 0x01, mm, ss, ff, amm, ass, aff],
                );
            }
            0x13 => {
                // GetTN: first and last track number
                let last = self.disc.as_ref().map_or(1, Disc::last_track);
                self.push_int(
                    now,
                    self.ack_delay(),
                    3,
                    vec![st, 0x01, to_bcd(last as u32)],
                );
            }
            0x14 => {
                // GetTD: track start (0 = end-of-disc)
                let lba = match params.first().copied().map(from_bcd) {
                    Some(0) => self.disc.as_ref().map_or(0, Disc::sector_count),
                    Some(n) => self
                        .disc
                        .as_ref()
                        .and_then(|d| d.track_start(n as u8))
                        .unwrap_or(0),
                    None => 0,
                };
                let (mm, ss, _) = lba_to_bcd_msf(lba);
                self.push_int(now, self.ack_delay(), 3, vec![st, mm, ss]);
            }
            0x15 | 0x16 => {
                // SeekL / SeekP with distance-based latency
                let seek = self.seek_cycles(self.seek_target);
                self.read_lba = self.seek_target;
                self.head_lba = self.seek_target;
                self.reading = false;
                self.push_int(now, self.ack_delay(), 3, vec![st]);
                self.push_int(now, seek, 2, vec![self.stat_byte()]);
            }
            0x19 => {
                // Test: only the BIOS-version sub-command is meaningful here
                match params.first() {
                    Some(0x20) => {
                        self.push_int(now, self.ack_delay(), 3, vec![0x94, 0x09, 0x19, 0xc0])
                    }
                    sub => {
                        warn!(target: "psx_core::cdrom", "Test sub-command {sub:02x?} stubbed");
                        self.push_int(now, self.ack_delay(), 3, vec![st]);
                    }
                }
            }
            0x1a => {
                // GetID
                self.push_int(now, self.ack_delay(), 3, vec![st]);
                if self.disc.is_some() {
                    // Licensed NTSC-J disc
                    self.push_int(
                        now,
                        GETID_DELAY,
                        2,
                        vec![0x02, 0x00, 0x20, 0x00, b'S', b'C', b'E', b'I'],
                    );
                } else {
                    // Door closed, no disc
                    self.push_int(now, GETID_DELAY, 5, vec![0x08, 0x40, 0, 0, 0, 0, 0, 0]);
                }
            }
            0x1e => {
                // ReadTOC: initialises like Init, then scans the whole TOC
                self.push_int(now, ACK_INIT, 3, vec![st]);
                self.push_int(now, CPU_HZ, 2, vec![self.stat_byte()]);
            }
            _ => {
                warn!(target: "psx_core::cdrom", "unknown command {cmd:#04x}");
                self.push_int(now, self.ack_delay(), 5, vec![0x11, 0x40]);
            }
        }
    }
}

impl Default for Cdrom {
    fn default() -> Self {
        Self::new()
    }
}

fn to_bcd(v: u32) -> u8 {
    (((v / 10) << 4) | (v % 10)) as u8
}

fn from_bcd(v: u8) -> u32 {
    ((v >> 4) * 10 + (v & 0xf)) as u32
}

/// Frame count -> BCD (mm, ss, ff), with no lead-in offset (track-relative).
fn frames_to_bcd_msf(frames: u32) -> (u8, u8, u8) {
    (
        to_bcd(frames / (60 * 75)),
        to_bcd(frames / 75 % 60),
        to_bcd(frames % 75),
    )
}

/// LBA -> BCD (mm, ss, ff), including the 2-second lead-in offset.
fn lba_to_bcd_msf(lba: u32) -> (u8, u8, u8) {
    frames_to_bcd_msf(lba + 150)
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

    /// Issue `cmd` with no parameters at `now`.
    fn command(cd: &mut Cdrom, cmd: u8, now: u64) {
        cd.write8(0, 0, now);
        cd.write8(1, cmd, now);
        cd.write8(0, 1, now);
    }

    /// Pause and Stop both answer far quicker when the drive has nothing to
    /// wind down, which is the difference the flat latency used to hide.
    /// Responses chain, so a second response lands one ack later.
    #[test]
    fn second_response_follows_the_drive_state() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(Disc::new(vec![0; RAW_SECTOR * 4]).unwrap());

        // Pausing a running read waits about five sectors...
        cd.reading = true;
        command(&mut cd, 0x09, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        let settled = ACK_RUNNING + PAUSE_SINGLE;
        assert_eq!(acked(&mut cd, &mut irq, settled - 1).0, 0, "not yet");
        assert_eq!(acked(&mut cd, &mut irq, settled).0, 2, "pause completed");

        // ...whereas pausing an idle drive returns almost at once.
        let t0 = settled + 1;
        command(&mut cd, 0x09, t0);
        acked(&mut cd, &mut irq, t0 + ACK_RUNNING);
        let quick = t0 + ACK_RUNNING + PAUSE_PAUSED;
        assert_eq!(acked(&mut cd, &mut irq, quick).0, 2, "already paused");
    }

    /// The first response is quicker while the motor is stopped.
    #[test]
    fn first_response_follows_the_motor() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(Disc::new(vec![0; RAW_SECTOR]).unwrap());
        assert_eq!(cd.ack_delay(), ACK_RUNNING);

        command(&mut cd, 0x08, 0); // Stop
        acked(&mut cd, &mut irq, ACK_RUNNING);
        assert_eq!(cd.ack_delay(), ACK_STOPPED);
    }

    /// A swap is observed entirely through stat bit 4: it goes up when the
    /// lid opens, survives the lid closing, and only a Getstat clears it.
    #[test]
    fn shell_open_latches_until_getstat_with_the_lid_shut() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;

        cd.write8(0, 1, 0); // index 1: the flag register `acked` writes
        cd.open_shell(0);
        // Unsolicited INT5: stat with shell-open + error, "door became opened"
        let (int, resp) = acked(&mut cd, &mut irq, ACK_RUNNING);
        assert_eq!(int, 5);
        assert_eq!(resp, vec![stat::SHELL_OPEN | stat::ERROR, ERR_DOOR_OPENED]);

        cd.close_shell(None);
        assert!(
            cd.stat_byte() & stat::SHELL_OPEN != 0,
            "latch survives close"
        );

        // The Getstat that clears the latch still reports it, so the game
        // sees the swap exactly once.
        let now = ACK_RUNNING * 2;
        cd.write8(0, 0, now);
        cd.write8(1, 0x01, now);
        cd.write8(0, 1, now);
        let (int, resp) = acked(&mut cd, &mut irq, now + ACK_RUNNING);
        assert_eq!(int, 3);
        assert_eq!(resp, vec![stat::MOTOR_ON | stat::SHELL_OPEN]);
        assert_eq!(cd.stat_byte(), stat::MOTOR_ON);
    }

    #[test]
    fn getid_reports_the_door_open_error() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(Disc::new(vec![0; RAW_SECTOR]).unwrap());
        cd.write8(0, 1, 0); // index 1: the flag register `acked` writes
        cd.open_shell(0);
        acked(&mut cd, &mut irq, ACK_RUNNING); // the lid-open INT5

        let now = ACK_RUNNING * 2;
        cd.write8(0, 0, now);
        cd.write8(1, 0x1a, now);
        cd.write8(0, 1, now);
        let (int, resp) = acked(&mut cd, &mut irq, now + ACK_RUNNING);
        assert_eq!(int, 5);
        assert_eq!(resp, vec![0x11, ERR_NOT_READY]);
    }

    /// A boot path that probes the drive by reading, rather than by GetID,
    /// gets an error instead of silence — without one it waits forever.
    #[test]
    fn empty_drive_fails_the_commands_that_need_a_disc() {
        // ReadN, SeekL, Setloc: one from each range of the psx-spx list
        for cmd in [0x06u8, 0x15, 0x02] {
            let mut cd = Cdrom::new();
            let mut irq = Irq::default();
            cd.int_enable = 0x1f;
            cd.write8(1, cmd, 0);
            cd.write8(0, 1, 0);
            let (int, resp) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
            assert_eq!(int, 5, "cmd {cmd:#04x}");
            assert_eq!(
                resp,
                vec![stat::MOTOR_ON | stat::ERROR, ERR_NOT_READY],
                "cmd {cmd:#04x}"
            );
            assert!(!cd.reading, "cmd {cmd:#04x} started the sector pump");
        }
    }

    /// A read that runs off the end of the disc ends the way a CD-DA stream
    /// does at the same place: the drive says so instead of falling silent.
    #[test]
    fn reading_past_the_end_of_the_disc_reports_data_end() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(Disc::new(vec![0u8; RAW_SECTOR]).unwrap());
        // Setloc 00:02:00 (LBA 0), then ReadN over the one-sector disc
        cd.write8(2, 0x00, 0);
        cd.write8(2, 0x02, 0);
        cd.write8(2, 0x00, 0);
        cd.write8(1, 0x02, 0);
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 100_000);
        cd.write8(0, 1, 0);
        let mut now = 100_000 + ACK_RUNNING + 1;
        assert_eq!(acked(&mut cd, &mut irq, now).0, 3);

        let mut ints = Vec::new();
        for _ in 0..2 {
            now += cd.seek_cycles(0) + CPU_HZ / 75 + 1;
            ints.push(acked(&mut cd, &mut irq, now).0);
        }
        assert_eq!(ints, vec![1, 4]); // the only sector, then end of disc
        assert!(!cd.reading);
    }

    /// Opening the lid stops the drive but leaves the disc in it, so a
    /// cancelled swap resumes on the same image.
    #[test]
    fn closing_without_a_disc_keeps_the_old_one() {
        let mut cd = Cdrom::new();
        cd.insert_disc(Disc::new(vec![0; RAW_SECTOR * 4]).unwrap());
        cd.open_shell(0);
        assert!(!cd.motor_on && cd.disc.is_some());
        cd.close_shell(None);
        assert!(cd.motor_on);
        assert_eq!(cd.disc.as_ref().unwrap().sector_count(), 4);
    }

    #[test]
    fn getstat_yields_int3() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.write8(1, 0x01, 0); // Getstat (index 0)
        cd.write8(0, 1, 0); // switch to index 1 for the flag register
        let (int, resp) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        assert_eq!(int, 3);
        assert_eq!(resp, vec![stat::MOTOR_ON]);
        assert!(irq.stat & (1 << 2) != 0);
    }

    /// GetID is the one disc-requiring command an empty drive answers with
    /// the lid shut: a shell tells "no disc" from "drive unusable" by it, so
    /// the empty-drive gate must not swallow it.
    #[test]
    fn getid_without_disc_reports_int5() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.write8(1, 0x1a, 0);
        cd.write8(0, 1, 0);
        let (int, _) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        assert_eq!(int, 3);
        let (int, resp) = acked(&mut cd, &mut irq, ACK_RUNNING + COMPLETE_DELAY + 2);
        assert_eq!(int, 5);
        assert_eq!(resp[..2], [0x08, 0x40]);
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
        let (int, _) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        assert_eq!(int, 3);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 100_000); // ReadN
        cd.write8(0, 1, 0);
        let t = 100_000 + ACK_RUNNING + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 3);
        // Includes the implicit-seek latency before the first sector
        let t = t + cd.seek_cycles(0) + CPU_HZ / 75 + ACK_RUNNING + 1;
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
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 0); // ReadN LBA 0
        cd.write8(0, 1, 0);
        let t = ACK_RUNNING + 2;
        acked(&mut cd, &mut irq, t);
        let t = t + cd.seek_cycles(0) + CPU_HZ / 150 + ACK_RUNNING;
        cd.tick(t, &mut irq); // XA sector consumed silently
        cd.write8(0, 0, 0);
        cd.write8(1, 0x10, t); // GetlocL
        cd.write8(0, 1, 0);
        let (int, resp) = acked(&mut cd, &mut irq, t + ACK_RUNNING + 1);
        assert_eq!(int, 3);
        assert_eq!(resp, vec![0x00, 0x02, 0x00, 0x00, 2, 1, 0xc4, 0x00]);
    }

    #[test]
    fn xa_latch_plays_only_the_first_stream_of_a_multiplexed_bank() {
        // Bank of sectors with file numbers cycling 1,2,1,2..: without
        // Setfilter, only the stream of the first sector may decode.
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        let mut img = vec![0u8; RAW_SECTOR * 8];
        for s in 0..8 {
            img[s * RAW_SECTOR + 0x10] = 1 + (s as u8 & 1); // file 1/2
            img[s * RAW_SECTOR + 0x11] = 1;
            img[s * RAW_SECTOR + 0x12] = 0x64; // realtime + form2 + audio
            img[s * RAW_SECTOR + 0x13] = 0x00;
        }
        cd.insert_disc(Disc::new(img).unwrap());
        cd.write8(2, 0xe0, 0); // double speed + XA + whole sector, filter OFF
        cd.write8(1, 0x0e, 0);
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x1b, 0); // ReadS from LBA 0 (first sector: file 1)
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        let mut now = ACK_RUNNING + 2;
        // Drain aggressively so back-pressure never holds
        for _ in 0..2_000_000 {
            now += 768;
            cd.tick(now, &mut irq);
            cd.xa_out.clear();
            if cd.xa_sectors + 4 >= 8 {
                break;
            }
        }
        // Only the 4 file-1 sectors decode; file-2 sectors pass by
        assert_eq!(cd.xa_sectors, 4);
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
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 0); // ReadN from LBA 0
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);

        // Drain like the SPU: 1 frame per 768 cycles
        let mut now = ACK_RUNNING + 2;
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

    /// 3-track disc: 75 data sectors, then two 75-sector audio tracks.
    /// Audio sector bytes are filled with the sector's LBA (low byte).
    fn multitrack_disc() -> Disc {
        let mut img = vec![0u8; RAW_SECTOR * 225];
        for lba in 75..225 {
            img[lba * RAW_SECTOR..(lba + 1) * RAW_SECTOR].fill(lba as u8);
        }
        let track = |number, audio, start| Track {
            number,
            audio,
            start,
        };
        Disc::with_tracks(
            img,
            vec![track(1, false, 0), track(2, true, 75), track(3, true, 150)],
        )
        .unwrap()
    }

    #[test]
    fn play_streams_cdda_pcm_from_the_requested_track() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(multitrack_disc());
        cd.write8(2, 0x02, 0); // Play(track 2, BCD)
        cd.write8(1, 0x03, 0);
        cd.write8(0, 1, 0);
        let (int, _) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        assert_eq!(int, 3);
        assert!(cd.stat_byte() & stat::PLAYING != 0);

        let mut now = ACK_RUNNING + 2;
        let mut first = None;
        for _ in 0..10_000_000u64 / 768 {
            now += 768;
            cd.tick(now, &mut irq);
            if first.is_none() && !cd.xa_out.is_empty() {
                first = cd.xa_out.front().copied();
                break;
            }
        }
        // Track 2 starts at LBA 75; its PCM bytes are all 75 (0x4b)
        assert_eq!(first, Some(i16::from_le_bytes([75, 75])));
    }

    #[test]
    fn cdda_autopauses_with_int4_at_the_track_boundary() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(multitrack_disc());
        cd.write8(2, 0x02, 0); // Setmode: auto-pause
        cd.write8(1, 0x0e, 0);
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(2, 0x02, 0);
        cd.write8(1, 0x03, 0); // Play(track 2)
        cd.write8(0, 1, 0);
        acked(&mut cd, &mut irq, 2 * ACK_RUNNING + 2);

        let mut now = 2 * ACK_RUNNING + 3;
        let mut frames = 0u64;
        let mut int4 = false;
        for _ in 0..200_000_000u64 / 768 {
            now += 768;
            cd.tick(now, &mut irq);
            frames += cd.xa_out.len() as u64 / 2;
            cd.xa_out.clear();
            if cd.int_flag & 7 == 4 {
                int4 = true;
                break;
            }
        }
        assert!(int4, "expected INT4 at the boundary to track 3");
        assert!(cd.stat_byte() & stat::PLAYING == 0);
        // Exactly track 2's 75 sectors were played (588 frames each)
        assert_eq!(frames, 75 * 588);
    }

    #[test]
    fn toc_commands_reflect_the_cue_tracks() {
        let mut cd = Cdrom::new();
        let mut irq = Irq::default();
        cd.int_enable = 0x1f;
        cd.insert_disc(multitrack_disc());
        cd.write8(1, 0x13, 0); // GetTN
        cd.write8(0, 1, 0);
        let (int, resp) = acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        assert_eq!(int, 3);
        assert_eq!(&resp[1..], &[0x01, 0x03]);
        cd.write8(0, 0, 0);
        cd.write8(2, 0x02, 0); // GetTD(2): LBA 75 -> absolute 00:03
        cd.write8(1, 0x14, 0);
        cd.write8(0, 1, 0);
        let (_, resp) = acked(&mut cd, &mut irq, 2 * ACK_RUNNING + 2);
        assert_eq!(&resp[1..], &[0x00, 0x03]);
        cd.write8(0, 0, 0);
        cd.write8(2, 0x00, 0); // GetTD(0): end of disc, 225 -> 00:05
        cd.write8(1, 0x14, 0);
        cd.write8(0, 1, 0);
        let (_, resp) = acked(&mut cd, &mut irq, 3 * ACK_RUNNING + 3);
        assert_eq!(&resp[1..], &[0x00, 0x05]);
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
        acked(&mut cd, &mut irq, ACK_RUNNING + 1);
        cd.write8(0, 0, 0);
        cd.write8(1, 0x06, 200_000); // ReadN from LBA 0
        cd.write8(0, 1, 0);
        let t = 200_000 + ACK_RUNNING + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 3);
        let t = t + cd.seek_cycles(0) + CPU_HZ / 150 + ACK_RUNNING + 1;
        let (int, _) = acked(&mut cd, &mut irq, t);
        assert_eq!(int, 0, "XA sector must not raise INT1");
        // 18 groups * 8 units * 28 samples = 2016 stereo frames at 37800 Hz
        // -> ~2352 frames after resampling to 44100
        assert!(
            cd.xa_out.len() / 2 > 2000,
            "got {} frames",
            cd.xa_out.len() / 2
        );
    }
}
