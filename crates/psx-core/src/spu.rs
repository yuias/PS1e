//! SPU: 24-voice ADPCM synthesizer.
//!
//! Implements voice playback (ADPCM block decoding, pitch stepping, ADSR
//! envelopes per the PSX-SPX rate algorithm), key on/off/ENDX, main volume,
//! manual/DMA RAM transfers, and IRQ9 both from transfer writes and from a
//! voice fetching the IRQ address (sound drivers use a looping voice over
//! the IRQ address as their tick).
//!
//! Also modeled: reverb, the noise generator, pitch modulation (PMON) and
//! 4-point interpolation (Catmull-Rom; audibly equivalent to the hardware
//! gaussian table — swap the table in for bit-accuracy later). Volume
//! sweeps are still treated as fixed maximum.

use crate::bus::Irq;
use std::collections::VecDeque;
use tracing::{debug, trace};

pub const SPU_RAM_SIZE: usize = 512 * 1024;
/// One output sample every 768 CPU cycles = 44100 Hz.
pub const CYCLES_PER_SAMPLE: u64 = 768;

const REG_BASE: u32 = 0x1f80_1c00;
/// Keep at most ~1.5s of audio buffered so headless runs stay bounded.
const OUTPUT_CAP: usize = 65536 * 2;

const ADPCM_POS: [i32; 5] = [0, 60, 115, 98, 122];
const ADPCM_NEG: [i32; 5] = [0, 0, -52, -55, -60];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone)]
struct Voice {
    /// Current ADPCM block address (byte address in SPU RAM).
    cur_addr: u32,
    /// Loop return address captured from the loop-start flag (byte address).
    repeat_addr: u32,
    /// 4.12 fixed-point position within the decoded block.
    pitch_counter: u32,
    decoded: [i16; 28],
    /// Tail of the previous block, for interpolation across boundaries.
    carry: [i16; 3],
    hist: (i32, i32),
    phase: Phase,
    /// 15-bit envelope level.
    adsr_vol: i32,
    /// Samples until the next envelope step.
    env_wait: u32,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            cur_addr: 0,
            repeat_addr: 0,
            pitch_counter: 0,
            decoded: [0; 28],
            carry: [0; 3],
            hist: (0, 0),
            phase: Phase::Off,
            adsr_vol: 0,
            env_wait: 0,
        }
    }
}

pub struct Spu {
    /// Raw 16-bit register file for 0x1f801c00..0x1f801e00.
    regs: [u16; 0x100],
    pub ram: Box<[u8]>,
    voices: Vec<Voice>,
    endx: u32,
    xfer_addr: u32,
    irq_flag: bool,
    /// Interleaved stereo output, drained by the frontend.
    output: VecDeque<i16>,
    /// CD/XA audio input frames, mixed in at 44.1kHz.
    cd_in: VecDeque<(i16, i16)>,
    /// Frames lost to the input cap (drift diagnostics).
    pub cd_dropped: u64,
    /// Reverb work-area cursor (bytes, relative to mBASE) and held output;
    /// the reverb core runs at 22050 Hz (every other sample).
    rev_cur: usize,
    rev_phase: bool,
    rev_out: (i32, i32),
    /// Noise generator (LFSR clocked by SPUCNT bits 8-13).
    noise_lfsr: u16,
    noise_timer: i32,
    /// Per-voice output of the current sample, for pitch modulation.
    last_out: [i32; 24],
}

impl Spu {
    pub fn new() -> Self {
        Self {
            regs: [0; 0x100],
            ram: vec![0; SPU_RAM_SIZE].into_boxed_slice(),
            voices: vec![Voice::default(); 24],
            endx: 0,
            xfer_addr: 0,
            irq_flag: false,
            output: VecDeque::new(),
            cd_in: VecDeque::new(),
            cd_dropped: 0,
            rev_cur: 0,
            rev_phase: false,
            rev_out: (0, 0),
            noise_lfsr: 1,
            noise_timer: 0,
            last_out: [0; 24],
        }
    }

    /// Buffered CD-input frames (for the drive's back-pressure check).
    pub fn cd_in_level(&self) -> usize {
        self.cd_in.len()
    }

    /// Feed one 44.1kHz stereo frame of CD/XA audio.
    pub fn push_cd_audio(&mut self, l: i16, r: i16) {
        // Bound to ~1s in case the SPU is disabled while the drive streams
        if self.cd_in.len() >= 44_100 {
            self.cd_in.pop_front();
            self.cd_dropped += 1;
            if self.cd_dropped == 1 || self.cd_dropped.is_multiple_of(44_100) {
                tracing::warn!(target: "psx_core::spu",
                    "CD input overflowing ({} frames dropped)", self.cd_dropped);
            }
        }
        self.cd_in.push_back((l, r));
    }

    /// Move buffered samples out (interleaved stereo i16).
    pub fn drain_output(&mut self, out: &mut Vec<i16>) {
        out.extend(self.output.drain(..));
    }

    pub fn buffered_samples(&self) -> usize {
        self.output.len() / 2
    }

    // --- Register helpers ---------------------------------------------

    fn reg(&self, ofs: usize) -> u16 {
        self.regs[ofs / 2]
    }

    fn voice_reg(&self, v: usize, ofs: usize) -> u16 {
        self.reg(v * 0x10 + ofs)
    }

    fn spucnt(&self) -> u16 {
        self.reg(0x1aa)
    }

    fn irq_enabled(&self) -> bool {
        let cnt = self.spucnt();
        cnt & (1 << 15) != 0 && cnt & (1 << 6) != 0
    }

    fn irq_addr(&self) -> u32 {
        self.reg(0x1a4) as u32 * 8
    }

    fn raise_irq(&mut self, irq: &mut Irq) {
        if !self.irq_flag {
            trace!(target: "psx_core::spu", "SPU IRQ at {:#x}", self.irq_addr());
        }
        self.irq_flag = true;
        irq.raise(9);
    }

    /// Volume register: sweep mode (bit 15) is approximated as full volume.
    fn volume(reg: u16) -> i32 {
        if reg & 0x8000 != 0 {
            0x7fff
        } else {
            ((reg as i16) << 1) as i32
        }
    }

    // --- MMIO ----------------------------------------------------------

    pub fn read16(&mut self, p: u32) -> u16 {
        let ofs = (p - REG_BASE) as usize;
        match ofs {
            // Voice current ADSR volume
            _ if ofs < 0x180 && ofs & 0xf == 0xc => self.voices[ofs >> 4].adsr_vol as u16,
            // Voice repeat address (live: updated by loop-start flags)
            _ if ofs < 0x180 && ofs & 0xf == 0xe => (self.voices[ofs >> 4].repeat_addr / 8) as u16,
            0x19c => self.endx as u16,
            0x19e => (self.endx >> 16) as u16,
            // SPUSTAT: low 6 bits mirror SPUCNT; bit 6 is the IRQ flag.
            0x1ae => (self.spucnt() & 0x3f) | (self.irq_flag as u16) << 6,
            _ => self.regs[ofs / 2],
        }
    }

    pub fn write16(&mut self, p: u32, val: u16, irq: &mut Irq) {
        let ofs = (p - REG_BASE) as usize;
        match ofs {
            0x1a6 => {
                self.xfer_addr = val as u32 * 8;
                self.regs[ofs / 2] = val;
            }
            0x1a8 => {
                // Data port: manual transfer into SPU RAM
                let a = (self.xfer_addr as usize) & (SPU_RAM_SIZE - 1);
                self.ram[a..a + 2].copy_from_slice(&val.to_le_bytes());
                if self.irq_enabled() {
                    let ia = self.irq_addr();
                    if self.xfer_addr <= ia + 1 && ia < self.xfer_addr + 2 {
                        self.raise_irq(irq);
                    }
                }
                self.xfer_addr = (self.xfer_addr + 2) & (SPU_RAM_SIZE as u32 - 1);
            }
            0x1aa => {
                // SPUCNT: clearing the IRQ-enable bit acknowledges the IRQ
                if val & (1 << 6) == 0 {
                    self.irq_flag = false;
                }
                self.regs[ofs / 2] = val;
                debug!(target: "psx_core::spu", "SPUCNT = {val:#06x}");
            }
            0x188 => self.key_on((val as u32) & 0xffff),
            0x18a => self.key_on(((val as u32) << 16) & 0x00ff_0000),
            0x18c => self.key_off((val as u32) & 0xffff),
            0x18e => self.key_off(((val as u32) << 16) & 0x00ff_0000),
            // Writing repeat address overrides the captured loop point
            _ if ofs < 0x180 && ofs & 0xf == 0xe => {
                self.voices[ofs >> 4].repeat_addr = val as u32 * 8;
                self.regs[ofs / 2] = val;
            }
            _ => self.regs[ofs / 2] = val,
        }
    }

    fn key_on(&mut self, mask: u32) {
        for v in 0..24 {
            if mask & (1 << v) == 0 {
                continue;
            }
            let start = self.voice_reg(v, 0x6) as u32 * 8;
            let voice = &mut self.voices[v];
            voice.cur_addr = start;
            voice.repeat_addr = start;
            voice.pitch_counter = 0x1c << 12; // force first block fetch
            voice.hist = (0, 0);
            voice.decoded = [0; 28];
            voice.carry = [0; 3];
            voice.phase = Phase::Attack;
            voice.adsr_vol = 0;
            voice.env_wait = 0;
            self.endx &= !(1 << v);
        }
        if mask != 0 {
            trace!(target: "psx_core::spu", "key on {mask:#08x}");
        }
    }

    fn key_off(&mut self, mask: u32) {
        for v in 0..24 {
            if mask & (1 << v) != 0 && self.voices[v].phase != Phase::Off {
                self.voices[v].phase = Phase::Release;
                self.voices[v].env_wait = 0;
            }
        }
    }

    // --- DMA -----------------------------------------------------------

    pub fn dma_write_word(&mut self, w: u32, irq: &mut Irq) {
        self.write16(REG_BASE + 0x1a8, w as u16, irq);
        self.write16(REG_BASE + 0x1a8, (w >> 16) as u16, irq);
    }

    pub fn dma_read_word(&mut self) -> u32 {
        let a = (self.xfer_addr as usize) & (SPU_RAM_SIZE - 1) & !3;
        let w = u32::from_le_bytes(self.ram[a..a + 4].try_into().unwrap());
        self.xfer_addr = (self.xfer_addr + 4) & (SPU_RAM_SIZE as u32 - 1);
        w
    }

    // --- Mixing --------------------------------------------------------

    /// Produce one stereo output sample (called every 768 CPU cycles).
    pub fn generate_sample(&mut self, irq: &mut Irq) {
        let cnt = self.spucnt();
        let enabled = cnt & (1 << 15) != 0;
        let muted = cnt & (1 << 14) == 0;
        let eon = (self.reg(0x198) as u32) | (self.reg(0x19a) as u32) << 16;

        let mut mix_l = 0i32;
        let mut mix_r = 0i32;
        let mut rev_l = 0i32;
        let mut rev_r = 0i32;

        self.step_noise();
        let noise_on = (self.reg(0x194) as u32) | (self.reg(0x196) as u32) << 16;
        let pmon = (self.reg(0x190) as u32) | (self.reg(0x192) as u32) << 16;

        for v in 0..24 {
            if self.voices[v].phase == Phase::Off {
                self.last_out[v] = 0;
                continue;
            }
            let sample = self.step_voice(v, irq, noise_on, pmon);
            self.last_out[v] = sample;
            let vol_l = Self::volume(self.voice_reg(v, 0x0));
            let vol_r = Self::volume(self.voice_reg(v, 0x2));
            let (sl, sr) = ((sample * vol_l) >> 15, (sample * vol_r) >> 15);
            mix_l += sl;
            mix_r += sr;
            if eon & (1 << v) != 0 {
                rev_l += sl;
                rev_r += sr;
            }
        }

        // CD / XA audio input
        let (cd_l, cd_r) = self.cd_in.pop_front().unwrap_or((0, 0));
        if cnt & 1 != 0 {
            let cvl = self.reg(0x1b0) as i16 as i32;
            let cvr = self.reg(0x1b2) as i16 as i32;
            let (cl, cr) = ((cd_l as i32 * cvl) >> 15, (cd_r as i32 * cvr) >> 15);
            mix_l += cl;
            mix_r += cr;
            if cnt & (1 << 2) != 0 {
                rev_l += cl;
                rev_r += cr;
            }
        }

        // Reverb core runs at 22050 Hz; output held between steps
        self.rev_phase = !self.rev_phase;
        if self.rev_phase {
            self.rev_out =
                self.reverb_step(rev_l.clamp(-0x8000, 0x7fff), rev_r.clamp(-0x8000, 0x7fff));
        }

        let main_l = Self::volume(self.reg(0x180));
        let main_r = Self::volume(self.reg(0x182));
        let (mut l, mut r) = if enabled && !muted {
            (
                ((mix_l * main_l) >> 15) + self.rev_out.0,
                ((mix_r * main_r) >> 15) + self.rev_out.1,
            )
        } else {
            (0, 0)
        };
        l = l.clamp(-0x8000, 0x7fff);
        r = r.clamp(-0x8000, 0x7fff);

        if self.output.len() >= OUTPUT_CAP {
            self.output.pop_front();
            self.output.pop_front();
        }
        self.output.push_back(l as i16);
        self.output.push_back(r as i16);
    }

    /// One 22050 Hz reverb step (PSX-SPX reverb formula, applied verbatim).
    fn reverb_step(&mut self, in_l: i32, in_r: i32) -> (i32, i32) {
        // Reverb register block at 0x1dc0: volumes are signed, addresses
        // are in 8-byte units within the work area [mBASE*8 .. 512K).
        let rv = |n: usize| self.reg(0x1c0 + n * 2) as i16 as i32;
        let ra = |n: usize| self.reg(0x1c0 + n * 2) as i64 * 8;
        let (d_apf1, d_apf2) = (ra(0), ra(1));
        let (v_iir, v_wall) = (rv(2), rv(7));
        let combs = [rv(3), rv(4), rv(5), rv(6)];
        let (v_apf1, v_apf2) = (rv(8), rv(9));
        let (m_lsame, m_rsame) = (ra(10), ra(11));
        let m_comb_l = [ra(12), ra(14), ra(20), ra(22)];
        let m_comb_r = [ra(13), ra(15), ra(21), ra(23)];
        let (d_lsame, d_rsame) = (ra(16), ra(17));
        let (m_ldiff, m_rdiff) = (ra(18), ra(19));
        let (d_ldiff, d_rdiff) = (ra(24), ra(25));
        let (m_lapf1, m_rapf1) = (ra(26), ra(27));
        let (m_lapf2, m_rapf2) = (ra(28), ra(29));
        let (v_lin, v_rin) = (rv(30), rv(31));

        let base = ((self.reg(0x1a2) as usize) * 8).min(SPU_RAM_SIZE - 2);
        let len = (SPU_RAM_SIZE - base) as i64;
        let cur = self.rev_cur as i64;
        let ptr = |off: i64| base + ((cur + off).rem_euclid(len) as usize & !1);
        let rd = |ram: &[u8], off: i64| {
            let p = ptr(off);
            i16::from_le_bytes([ram[p], ram[p + 1]]) as i32
        };
        let write_enable = self.spucnt() & (1 << 7) != 0;

        let sat = |v: i32| v.clamp(-0x8000, 0x7fff);
        let mul = |a: i32, v: i32| (a * v) >> 15;

        let lin = mul(in_l, v_lin);
        let rin = mul(in_r, v_rin);

        // Same-side and cross-side wall reflections (one-pole IIR each)
        let mut wr_list: [(i64, i32); 2 + 4] = [(0, 0); 6];
        let refl = |input: i32, d_src: i64, m_dst: i64, ram: &[u8]| {
            let prev = rd(ram, m_dst - 2);
            sat(mul(input + mul(rd(ram, d_src), v_wall) - prev, v_iir) + prev)
        };
        wr_list[0] = (m_lsame, refl(lin, d_lsame, m_lsame, &self.ram));
        wr_list[1] = (m_rsame, refl(rin, d_rsame, m_rsame, &self.ram));
        wr_list[2] = (m_ldiff, refl(lin, d_rdiff, m_ldiff, &self.ram));
        wr_list[3] = (m_rdiff, refl(rin, d_ldiff, m_rdiff, &self.ram));

        // Comb filters
        let mut out_l = 0i32;
        let mut out_r = 0i32;
        for k in 0..4 {
            out_l += mul(rd(&self.ram, m_comb_l[k]), combs[k]);
            out_r += mul(rd(&self.ram, m_comb_r[k]), combs[k]);
        }
        out_l = sat(out_l);
        out_r = sat(out_r);

        // Two all-pass stages
        let apf = |out: i32, m: i64, d: i64, v: i32, ram: &[u8]| {
            let tap = rd(ram, m - d);
            let w = sat(out - mul(tap, v));
            (w, sat(mul(w, v) + tap))
        };
        let (w, o) = apf(out_l, m_lapf1, d_apf1, v_apf1, &self.ram);
        wr_list[4] = (m_lapf1, w);
        out_l = o;
        let (w, o) = apf(out_r, m_rapf1, d_apf1, v_apf1, &self.ram);
        wr_list[5] = (m_rapf1, w);
        out_r = o;
        // Second all-pass writes go through the same gate; apply inline
        let (w2l, o) = apf(out_l, m_lapf2, d_apf2, v_apf2, &self.ram);
        out_l = o;
        let (w2r, o) = apf(out_r, m_rapf2, d_apf2, v_apf2, &self.ram);
        out_r = o;

        if write_enable {
            for (m, v) in wr_list
                .iter()
                .chain([(m_lapf2, w2l), (m_rapf2, w2r)].iter())
            {
                let p = ptr(*m);
                self.ram[p..p + 2].copy_from_slice(&(*v as i16).to_le_bytes());
            }
        }

        self.rev_cur = ((cur + 2).rem_euclid(len)) as usize;

        let v_lout = self.reg(0x184) as i16 as i32;
        let v_rout = self.reg(0x186) as i16 as i32;
        (mul(out_l, v_lout), mul(out_r, v_rout))
    }

    /// Clock the noise LFSR (rate from SPUCNT bits 8-13).
    fn step_noise(&mut self) {
        let cnt = self.spucnt() as u32;
        let shift = (cnt >> 10) & 0xf;
        let step = 4 + ((cnt >> 8) & 3) as i32;
        self.noise_timer -= step;
        while self.noise_timer < 0 {
            self.noise_timer += 0x20000 >> shift;
            let l = self.noise_lfsr;
            let parity = ((l >> 15) ^ (l >> 12) ^ (l >> 11) ^ (l >> 10) ^ 1) & 1;
            self.noise_lfsr = (l << 1) | parity;
        }
    }

    /// Advance one voice by one sample: pitch step, block decode, envelope.
    fn step_voice(&mut self, v: usize, irq: &mut Irq, noise_on: u32, pmon: u32) -> i32 {
        // Pitch step (0x1000 = 44100 Hz), capped at 4x. PMON scales the
        // step by the previous voice's current output.
        let mut step = (self.voice_reg(v, 0x4) as u32).min(0x4000) as i32;
        if v > 0 && pmon & (1 << v) != 0 {
            let factor = self.last_out[v - 1].clamp(-0x8000, 0x7fff);
            step = (step * (0x8000 + factor)) >> 15;
        }
        self.voices[v].pitch_counter += step.clamp(0, 0x4000) as u32;
        while self.voices[v].pitch_counter >= 28 << 12 {
            self.voices[v].pitch_counter -= 28 << 12;
            self.fetch_block(v, irq);
        }

        let raw = if noise_on & (1 << v) != 0 {
            self.noise_lfsr as i16 as i32
        } else {
            // 4-point interpolation around the current position; the two
            // middle points span the fractional interval
            let voice = &self.voices[v];
            let idx = (voice.pitch_counter >> 12) as i32;
            let t = (voice.pitch_counter & 0xfff) as i32;
            let at = |k: i32| -> i32 {
                if k < 0 {
                    voice.carry[(3 + k) as usize] as i32
                } else {
                    voice.decoded[k.min(27) as usize] as i32
                }
            };
            catmull_rom(at(idx - 3), at(idx - 2), at(idx - 1), at(idx), t)
        };

        self.tick_envelope(v);
        (raw * self.voices[v].adsr_vol) >> 15
    }

    /// Decode the ADPCM block at the voice's current address, then apply
    /// its loop flags and advance.
    fn fetch_block(&mut self, v: usize, irq: &mut Irq) {
        let addr = self.voices[v].cur_addr as usize & (SPU_RAM_SIZE - 1) & !0xf;
        if self.irq_enabled() {
            let ia = self.irq_addr() as usize;
            if (addr..addr + 16).contains(&ia) {
                self.raise_irq(irq);
            }
        }

        let header = self.ram[addr];
        let flags = self.ram[addr + 1];
        let shift = (header & 0xf).min(12) as i32;
        let filter = ((header >> 4) & 7).min(4) as usize;
        // Preserve the block tail for interpolation continuity
        let tail = &self.voices[v].decoded[25..28];
        self.voices[v].carry = [tail[0], tail[1], tail[2]];
        let (mut h0, mut h1) = self.voices[v].hist;
        for i in 0..28 {
            let byte = self.ram[addr + 2 + i / 2];
            let nibble = (byte >> ((i & 1) * 4)) & 0xf;
            let s = (((nibble as i32) << 28) >> 28 << 12) >> shift;
            let sample = (s + (h0 * ADPCM_POS[filter] + h1 * ADPCM_NEG[filter] + 32) / 64)
                .clamp(-0x8000, 0x7fff);
            self.voices[v].decoded[i] = sample as i16;
            h1 = h0;
            h0 = sample;
        }
        self.voices[v].hist = (h0, h1);

        if flags & 0x4 != 0 {
            // Loop start: capture the return point
            self.voices[v].repeat_addr = addr as u32;
        }
        if flags & 0x1 != 0 {
            // Loop end: jump to the loop point; without the repeat flag the
            // voice is muted (envelope forced to release at zero)
            self.endx |= 1 << v;
            self.voices[v].cur_addr = self.voices[v].repeat_addr;
            if flags & 0x2 == 0 {
                self.voices[v].phase = Phase::Release;
                self.voices[v].adsr_vol = 0;
            }
        } else {
            self.voices[v].cur_addr = (addr as u32 + 16) & (SPU_RAM_SIZE as u32 - 1);
        }
    }

    fn tick_envelope(&mut self, v: usize) {
        let adsr = (self.voice_reg(v, 0x8) as u32) | (self.voice_reg(v, 0xa) as u32) << 16;
        let sustain_level = (((adsr & 0xf) + 1) * 0x800).min(0x7fff) as i32;

        let voice = &mut self.voices[v];
        if voice.env_wait > 0 {
            voice.env_wait -= 1;
            return;
        }

        // (rate, decreasing, exponential) for the current phase
        let (rate, dec, exp) = match voice.phase {
            Phase::Attack => ((adsr >> 8) & 0x7f, false, adsr & (1 << 15) != 0),
            Phase::Decay => (((adsr >> 4) & 0xf) * 4, true, true),
            Phase::Sustain => (
                (adsr >> 22) & 0x7f,
                adsr & (1 << 30) != 0,
                adsr & (1 << 31) != 0,
            ),
            Phase::Release => (((adsr >> 16) & 0x1f) * 4, true, adsr & (1 << 21) != 0),
            Phase::Off => return,
        };

        // PSX-SPX envelope rate algorithm
        let shift = (rate >> 2) as i32;
        let base = if dec {
            -8 + (rate & 3) as i32
        } else {
            7 - (rate & 3) as i32
        };
        let mut wait = 1u64 << (shift - 11).max(0);
        let mut step = base << (11 - shift).max(0);
        if exp {
            if !dec && voice.adsr_vol > 0x6000 {
                wait *= 4;
            }
            if dec {
                step = step * voice.adsr_vol / 0x8000;
                if step == 0 {
                    step = -1; // keep decaying even at low levels
                }
            }
        }
        voice.env_wait = wait.min(u32::MAX as u64) as u32 - 1;
        voice.adsr_vol = (voice.adsr_vol + step).clamp(0, 0x7fff);

        match voice.phase {
            Phase::Attack if voice.adsr_vol >= 0x7fff => voice.phase = Phase::Decay,
            Phase::Decay if voice.adsr_vol <= sustain_level => voice.phase = Phase::Sustain,
            Phase::Release if voice.adsr_vol == 0 => voice.phase = Phase::Off,
            _ => {}
        }
    }
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

/// Catmull-Rom interpolation between p1 and p2 (t is a 12-bit fraction).
fn catmull_rom(p0: i32, p1: i32, p2: i32, p3: i32, t: i32) -> i32 {
    // Evaluate in 20.12 fixed point: 0.5*(2p1 + (p2-p0)t + (2p0-5p1+4p2-p3)t^2
    // + (3p1-3p2+p3-p0)t^3)
    let t = t as i64;
    let (p0, p1, p2, p3) = (p0 as i64, p1 as i64, p2 as i64, p3 as i64);
    let a = 3 * (p1 - p2) + p3 - p0;
    let b = 2 * p0 - 5 * p1 + 4 * p2 - p3;
    let c = p2 - p0;
    let v = ((((a * t) >> 12) + b) * t) >> 12;
    let v = ((v + c) * t) >> 12;
    (((v + 2 * p1) / 2) as i32).clamp(-0x8000, 0x7fff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_hitting_irq_address_raises_irq9() {
        let mut spu = Spu::new();
        let mut irq = Irq::default();
        spu.write16(REG_BASE + 0x1aa, 0x8040, &mut irq); // enable + IRQ enable
        spu.write16(REG_BASE + 0x1a4, 0x0002, &mut irq); // IRQ addr = 0x10
        spu.write16(REG_BASE + 0x1a6, 0x0000, &mut irq); // transfer addr = 0
        for i in 0..16 {
            spu.write16(REG_BASE + 0x1a8, i, &mut irq);
        }
        assert!(irq.stat & (1 << 9) != 0);
        assert!(spu.read16(REG_BASE + 0x1ae) & (1 << 6) != 0);
        spu.write16(REG_BASE + 0x1aa, 0x8000, &mut irq);
        assert!(spu.read16(REG_BASE + 0x1ae) & (1 << 6) == 0);
    }

    /// A looping ADPCM block of maximal nibbles should produce non-silent
    /// output once keyed on with instant attack.
    #[test]
    fn keyed_voice_produces_audio() {
        let mut spu = Spu::new();
        let mut irq = Irq::default();
        // Block at 0x1000: shift 0, filter 0, loop end+repeat, nibble 0x7
        let base = 0x1000usize;
        spu.ram[base] = 0x00;
        spu.ram[base + 1] = 0x03; // end + repeat
        for i in 0..14 {
            spu.ram[base + 2 + i] = 0x77;
        }
        spu.write16(REG_BASE + 0x1aa, 0xc000, &mut irq); // enable + unmute
        spu.write16(REG_BASE + 0x180, 0x3fff, &mut irq); // main vol L
        spu.write16(REG_BASE + 0x182, 0x3fff, &mut irq);
        spu.write16(REG_BASE, 0x3fff, &mut irq); // voice 0 vol L
        spu.write16(REG_BASE + 0x2, 0x3fff, &mut irq);
        spu.write16(REG_BASE + 0x4, 0x1000, &mut irq); // pitch = 44100
        spu.write16(REG_BASE + 0x6, (base / 8) as u16, &mut irq); // start
        spu.write16(REG_BASE + 0x8, 0x000f, &mut irq); // instant attack, max sustain
        spu.write16(REG_BASE + 0xa, 0x0000, &mut irq);
        spu.write16(REG_BASE + 0x188, 1, &mut irq); // key on voice 0

        let mut peak = 0i32;
        for _ in 0..2000 {
            spu.generate_sample(&mut irq);
        }
        let mut buf = Vec::new();
        spu.drain_output(&mut buf);
        for s in buf {
            peak = peak.max(s.unsigned_abs() as i32);
        }
        assert!(peak > 0x100, "expected audible output, peak={peak}");
        // ENDX latched by the loop-end flag
        assert!(spu.read16(REG_BASE + 0x19c) & 1 != 0);
    }
}
