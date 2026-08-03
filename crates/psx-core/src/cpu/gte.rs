//! Geometry Transformation Engine (COP2).
//!
//! Implements the fixed-point vector/matrix pipeline used for vertex
//! projection and lighting, per nocash PSX-SPX
//! (<https://psx-spx.consoledev.net/geometrytransformationenginegte/>).
//! Accuracy of saturation and FLAG bits matters: games rely on exact GTE
//! results for polygon coordinates, so any deviation shows up as visibly
//! broken geometry.

// Index loops are kept where `i` mirrors the hardware register number
// (MAC1..3 / IR1..3): iterator/zip forms would obscure that correspondence.
#![allow(clippy::needless_range_loop)]

/// `unr_table[i] = max(0, (0x40000 / (i + 0x100) + 1) / 2 - 0x101)`, per
/// PSX-SPX "GTE Division Inaccuracy". Generated instead of hand-transcribed
/// to avoid a 257-entry copy/paste error; verified against the published
/// table for several indices while implementing this module.
const UNR_TABLE: [u8; 257] = build_unr_table();

const fn build_unr_table() -> [u8; 257] {
    let mut table = [0u8; 257];
    let mut i = 0;
    while i < 257 {
        let v = (0x40000 / (i as u32 + 0x100)).div_ceil(2);
        table[i] = if v >= 0x101 { (v - 0x101) as u8 } else { 0 };
        i += 1;
    }
    table
}

/// 44-bit accumulator bounds for MAC1..MAC3 overflow checks (pre-shift sum).
const MAC123_MAX: i64 = (1i64 << 43) - 1;
const MAC123_MIN: i64 = -(1i64 << 43);

#[derive(Debug, Clone)]
pub struct Gte {
    // --- Data registers ---
    /// V0..V2, each [x, y, z].
    v: [[i16; 3]; 3],
    /// RGBC packed as R | G<<8 | B<<16 | CODE<<24.
    rgbc: u32,
    otz: u16,
    /// IR0..IR3.
    ir: [i16; 4],
    /// SXY0..SXY2 fifo, (x, y) pairs.
    sxy_fifo: [(i16, i16); 3],
    /// SZ0..SZ3 fifo.
    sz_fifo: [u16; 4],
    /// RGB0..RGB2 fifo, packed like `rgbc`.
    rgb_fifo: [u32; 3],
    /// Unused/prohibited register, kept as plain read/write storage.
    res1: u32,
    /// MAC0..MAC3.
    mac: [i32; 4],
    lzcs: i32,

    // --- Control registers ---
    /// Rotation matrix, `rt[row][col]`.
    rt: [[i16; 3]; 3],
    tr: [i32; 3],
    /// Light matrix.
    l: [[i16; 3]; 3],
    bk: [i32; 3],
    /// Light color matrix.
    lc: [[i16; 3]; 3],
    fc: [i32; 3],
    ofx: i32,
    ofy: i32,
    /// Projection plane distance; unsigned in the calculations, but reading
    /// the register back sign-extends it (a documented hardware bug).
    h: u16,
    dqa: i16,
    dqb: i32,
    zsf3: i16,
    zsf4: i16,
    /// FLAG bits 12..30 as last set by a command or CTC2; bit31 is derived
    /// on read (it is not itself storable).
    flag: u32,
}

impl Gte {
    pub fn new() -> Self {
        Self {
            v: [[0; 3]; 3],
            rgbc: 0,
            otz: 0,
            ir: [0; 4],
            sxy_fifo: [(0, 0); 3],
            sz_fifo: [0; 4],
            rgb_fifo: [0; 3],
            res1: 0,
            mac: [0; 4],
            lzcs: 0,
            rt: [[0; 3]; 3],
            tr: [0; 3],
            l: [[0; 3]; 3],
            bk: [0; 3],
            lc: [[0; 3]; 3],
            fc: [0; 3],
            ofx: 0,
            ofy: 0,
            h: 0,
            dqa: 0,
            dqb: 0,
            zsf3: 0,
            zsf4: 0,
            flag: 0,
        }
    }

    // ---------------------------------------------------------------
    // Data register file
    // ---------------------------------------------------------------

    pub fn read_data(&mut self, r: u32) -> u32 {
        match r {
            0 => pack16(self.v[0][0], self.v[0][1]),
            1 => self.v[0][2] as i32 as u32,
            2 => pack16(self.v[1][0], self.v[1][1]),
            3 => self.v[1][2] as i32 as u32,
            4 => pack16(self.v[2][0], self.v[2][1]),
            5 => self.v[2][2] as i32 as u32,
            6 => self.rgbc,
            7 => self.otz as u32,
            8 => self.ir[0] as i32 as u32,
            9 => self.ir[1] as i32 as u32,
            10 => self.ir[2] as i32 as u32,
            11 => self.ir[3] as i32 as u32,
            12 => pack16(self.sxy_fifo[0].0, self.sxy_fifo[0].1),
            13 => pack16(self.sxy_fifo[1].0, self.sxy_fifo[1].1),
            14 | 15 => pack16(self.sxy_fifo[2].0, self.sxy_fifo[2].1), // SXY2 / SXYP mirror
            16 => self.sz_fifo[0] as u32,
            17 => self.sz_fifo[1] as u32,
            18 => self.sz_fifo[2] as u32,
            19 => self.sz_fifo[3] as u32,
            20 => self.rgb_fifo[0],
            21 => self.rgb_fifo[1],
            22 => self.rgb_fifo[2],
            23 => self.res1,
            24 => self.mac[0] as u32,
            25 => self.mac[1] as u32,
            26 => self.mac[2] as u32,
            27 => self.mac[3] as u32,
            28 | 29 => self.irgb(),
            30 => self.lzcs as u32,
            31 => self.compute_lzcr(),
            _ => {
                tracing::warn!(target: "psx_core::gte", r, "read_data: register out of range");
                0
            }
        }
    }

    pub fn write_data(&mut self, r: u32, v: u32) {
        match r {
            0 => {
                self.v[0][0] = v as i16;
                self.v[0][1] = (v >> 16) as i16;
            }
            1 => self.v[0][2] = v as i16,
            2 => {
                self.v[1][0] = v as i16;
                self.v[1][1] = (v >> 16) as i16;
            }
            3 => self.v[1][2] = v as i16,
            4 => {
                self.v[2][0] = v as i16;
                self.v[2][1] = (v >> 16) as i16;
            }
            5 => self.v[2][2] = v as i16,
            6 => self.rgbc = v,
            7 => self.otz = v as u16,
            8 => self.ir[0] = v as i16,
            9 => self.ir[1] = v as i16,
            10 => self.ir[2] = v as i16,
            11 => self.ir[3] = v as i16,
            12 => self.sxy_fifo[0] = (v as i16, (v >> 16) as i16),
            13 => self.sxy_fifo[1] = (v as i16, (v >> 16) as i16),
            14 => self.sxy_fifo[2] = (v as i16, (v >> 16) as i16),
            15 => {
                // SXYP: mirror of SXY2 with move-on-write (pushes the fifo).
                self.sxy_fifo[0] = self.sxy_fifo[1];
                self.sxy_fifo[1] = self.sxy_fifo[2];
                self.sxy_fifo[2] = (v as i16, (v >> 16) as i16);
            }
            16 => self.sz_fifo[0] = v as u16,
            17 => self.sz_fifo[1] = v as u16,
            18 => self.sz_fifo[2] = v as u16,
            19 => self.sz_fifo[3] = v as u16,
            20 => self.rgb_fifo[0] = v,
            21 => self.rgb_fifo[1] = v,
            22 => self.rgb_fifo[2] = v,
            23 => self.res1 = v,
            24 => self.mac[0] = v as i32,
            25 => self.mac[1] = v as i32,
            26 => self.mac[2] = v as i32,
            27 => self.mac[3] = v as i32,
            28 => {
                // IRGB: expand 5:5:5 to IR1..3 (each *0x80).
                let r5 = v & 0x1f;
                let g5 = (v >> 5) & 0x1f;
                let b5 = (v >> 10) & 0x1f;
                self.ir[1] = (r5 * 0x80) as i16;
                self.ir[2] = (g5 * 0x80) as i16;
                self.ir[3] = (b5 * 0x80) as i16;
            }
            29 => {} // ORGB is a read-only mirror of IRGB; writes have no effect.
            30 => self.lzcs = v as i32,
            31 => {} // LZCR is read-only.
            _ => tracing::warn!(target: "psx_core::gte", r, v, "write_data: register out of range"),
        }
    }

    /// IR1..3 collapsed to 5:5:5 RGB (shared by IRGB and ORGB reads).
    fn irgb(&self) -> u32 {
        let conv = |ir: i16| ((ir as i32) >> 7).clamp(0, 0x1f) as u32;
        conv(self.ir[1]) | (conv(self.ir[2]) << 5) | (conv(self.ir[3]) << 10)
    }

    fn compute_lzcr(&self) -> u32 {
        if self.lzcs >= 0 {
            self.lzcs.leading_zeros()
        } else {
            (!self.lzcs).leading_zeros()
        }
    }

    // ---------------------------------------------------------------
    // Control register file
    // ---------------------------------------------------------------

    pub fn read_control(&self, r: u32) -> u32 {
        match r {
            0 => pack16(self.rt[0][0], self.rt[0][1]),
            1 => pack16(self.rt[0][2], self.rt[1][0]),
            2 => pack16(self.rt[1][1], self.rt[1][2]),
            3 => pack16(self.rt[2][0], self.rt[2][1]),
            4 => self.rt[2][2] as i32 as u32,
            5 => self.tr[0] as u32,
            6 => self.tr[1] as u32,
            7 => self.tr[2] as u32,
            8 => pack16(self.l[0][0], self.l[0][1]),
            9 => pack16(self.l[0][2], self.l[1][0]),
            10 => pack16(self.l[1][1], self.l[1][2]),
            11 => pack16(self.l[2][0], self.l[2][1]),
            12 => self.l[2][2] as i32 as u32,
            13 => self.bk[0] as u32,
            14 => self.bk[1] as u32,
            15 => self.bk[2] as u32,
            16 => pack16(self.lc[0][0], self.lc[0][1]),
            17 => pack16(self.lc[0][2], self.lc[1][0]),
            18 => pack16(self.lc[1][1], self.lc[1][2]),
            19 => pack16(self.lc[2][0], self.lc[2][1]),
            20 => self.lc[2][2] as i32 as u32,
            21 => self.fc[0] as u32,
            22 => self.fc[1] as u32,
            23 => self.fc[2] as u32,
            24 => self.ofx as u32,
            25 => self.ofy as u32,
            26 => self.h as i16 as i32 as u32, // documented sign-extend bug
            27 => self.dqa as i32 as u32,
            28 => self.dqb as u32,
            29 => self.zsf3 as i32 as u32,
            30 => self.zsf4 as i32 as u32,
            31 => self.flag_value(),
            _ => {
                tracing::warn!(target: "psx_core::gte", r, "read_control: register out of range");
                0
            }
        }
    }

    pub fn write_control(&mut self, r: u32, v: u32) {
        match r {
            0 => {
                self.rt[0][0] = v as i16;
                self.rt[0][1] = (v >> 16) as i16;
            }
            1 => {
                self.rt[0][2] = v as i16;
                self.rt[1][0] = (v >> 16) as i16;
            }
            2 => {
                self.rt[1][1] = v as i16;
                self.rt[1][2] = (v >> 16) as i16;
            }
            3 => {
                self.rt[2][0] = v as i16;
                self.rt[2][1] = (v >> 16) as i16;
            }
            4 => self.rt[2][2] = v as i16,
            5 => self.tr[0] = v as i32,
            6 => self.tr[1] = v as i32,
            7 => self.tr[2] = v as i32,
            8 => {
                self.l[0][0] = v as i16;
                self.l[0][1] = (v >> 16) as i16;
            }
            9 => {
                self.l[0][2] = v as i16;
                self.l[1][0] = (v >> 16) as i16;
            }
            10 => {
                self.l[1][1] = v as i16;
                self.l[1][2] = (v >> 16) as i16;
            }
            11 => {
                self.l[2][0] = v as i16;
                self.l[2][1] = (v >> 16) as i16;
            }
            12 => self.l[2][2] = v as i16,
            13 => self.bk[0] = v as i32,
            14 => self.bk[1] = v as i32,
            15 => self.bk[2] = v as i32,
            16 => {
                self.lc[0][0] = v as i16;
                self.lc[0][1] = (v >> 16) as i16;
            }
            17 => {
                self.lc[0][2] = v as i16;
                self.lc[1][0] = (v >> 16) as i16;
            }
            18 => {
                self.lc[1][1] = v as i16;
                self.lc[1][2] = (v >> 16) as i16;
            }
            19 => {
                self.lc[2][0] = v as i16;
                self.lc[2][1] = (v >> 16) as i16;
            }
            20 => self.lc[2][2] = v as i16,
            21 => self.fc[0] = v as i32,
            22 => self.fc[1] = v as i32,
            23 => self.fc[2] = v as i32,
            24 => self.ofx = v as i32,
            25 => self.ofy = v as i32,
            26 => self.h = v as u16,
            27 => self.dqa = v as i16,
            28 => self.dqb = v as i32,
            29 => self.zsf3 = v as i16,
            30 => self.zsf4 = v as i16,
            // Bits 12..30 are documented as read/write-able by software; bit31
            // and bits 0..11 are always derived/zero and not stored.
            31 => self.flag = v & 0x7fff_f000,
            _ => {
                tracing::warn!(target: "psx_core::gte", r, v, "write_control: register out of range")
            }
        }
    }

    /// FLAG bit31 = OR of bits 30..23 and 18..13, computed on read (not a
    /// stored bit).
    fn flag_value(&self) -> u32 {
        let hi_bits = self.flag & 0x7f80_0000; // bits 23..30
        let lo_bits = self.flag & 0x0007_e000; // bits 13..18
        let error = (hi_bits | lo_bits) != 0;
        self.flag | if error { 1 << 31 } else { 0 }
    }

    // ---------------------------------------------------------------
    // Saturation / accumulator helpers
    // ---------------------------------------------------------------

    fn check_mac_overflow(&mut self, idx: usize, value: i64) {
        let (pos_bit, neg_bit) = match idx {
            1 => (30, 27),
            2 => (29, 26),
            3 => (28, 25),
            _ => unreachable!(),
        };
        if value > MAC123_MAX {
            self.flag |= 1 << pos_bit;
        }
        if value < MAC123_MIN {
            self.flag |= 1 << neg_bit;
        }
    }

    /// Stores MAC1..MAC3 (idx 1..=3): checks the 44-bit accumulator overflow
    /// on the pre-shift value, then applies `SAR shift` and truncates to the
    /// 32-bit register (matching real hardware, MAC1-3 are not saturated,
    /// only checked/flagged).
    fn store_mac(&mut self, idx: usize, raw: i64, shift: u32) -> i32 {
        self.check_mac_overflow(idx, raw);
        let v = (raw >> shift) as i32;
        self.mac[idx] = v;
        v
    }

    fn set_mac0(&mut self, value: i64) -> i32 {
        if value > i32::MAX as i64 {
            self.flag |= 1 << 16;
        }
        if value < i32::MIN as i64 {
            self.flag |= 1 << 15;
        }
        let v = value as i32;
        self.mac[0] = v;
        v
    }

    fn ir_flag_bit(idx: usize) -> u32 {
        match idx {
            1 => 24,
            2 => 23,
            3 => 22,
            _ => unreachable!(),
        }
    }

    fn set_ir123(&mut self, idx: usize, value: i32, lm: bool) {
        let min = if lm { 0 } else { -0x8000 };
        let clamped = value.clamp(min, 0x7fff);
        if clamped != value {
            self.flag |= 1 << Self::ir_flag_bit(idx);
        }
        self.ir[idx] = clamped as i16;
    }

    fn set_ir0(&mut self, value: i32) {
        let clamped = value.clamp(0, 0x1000);
        if clamped != value {
            self.flag |= 1 << 12;
        }
        self.ir[0] = clamped as i16;
    }

    fn push_sxy(&mut self, sx_raw: i32, sy_raw: i32) {
        let sx = sx_raw.clamp(-0x400, 0x3ff);
        if sx != sx_raw {
            self.flag |= 1 << 14;
        }
        let sy = sy_raw.clamp(-0x400, 0x3ff);
        if sy != sy_raw {
            self.flag |= 1 << 13;
        }
        self.sxy_fifo[0] = self.sxy_fifo[1];
        self.sxy_fifo[1] = self.sxy_fifo[2];
        self.sxy_fifo[2] = (sx as i16, sy as i16);
    }

    fn push_sz(&mut self, sz_raw: i32) {
        let sz = sz_raw.clamp(0, 0xffff);
        if sz != sz_raw {
            self.flag |= 1 << 18;
        }
        self.sz_fifo[0] = self.sz_fifo[1];
        self.sz_fifo[1] = self.sz_fifo[2];
        self.sz_fifo[2] = self.sz_fifo[3];
        self.sz_fifo[3] = sz as u16;
    }

    fn set_otz(&mut self, value: i32) {
        let clamped = value.clamp(0, 0xffff);
        if clamped != value {
            self.flag |= 1 << 18; // shared with SZ3 saturation, per FLAG bit 18
        }
        self.otz = clamped as u16;
    }

    fn push_color(&mut self, r_raw: i32, g_raw: i32, b_raw: i32, code: u8) {
        let r = r_raw.clamp(0, 0xff);
        if r != r_raw {
            self.flag |= 1 << 21;
        }
        let g = g_raw.clamp(0, 0xff);
        if g != g_raw {
            self.flag |= 1 << 20;
        }
        let b = b_raw.clamp(0, 0xff);
        if b != b_raw {
            self.flag |= 1 << 19;
        }
        self.rgb_fifo[0] = self.rgb_fifo[1];
        self.rgb_fifo[1] = self.rgb_fifo[2];
        self.rgb_fifo[2] =
            (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((code as u32) << 24);
    }

    fn push_color_from_mac(&mut self, code: u8) {
        self.push_color(self.mac[1] >> 4, self.mac[2] >> 4, self.mac[3] >> 4, code);
    }

    fn vector(&self, i: usize) -> [i32; 3] {
        [
            self.v[i][0] as i32,
            self.v[i][1] as i32,
            self.v[i][2] as i32,
        ]
    }

    fn ir_vector(&self) -> [i32; 3] {
        [self.ir[1] as i32, self.ir[2] as i32, self.ir[3] as i32]
    }

    /// `MAC1..3 = (t*1000h? + row . v) SAR (sf*12)`, `IR1..3 = MAC1..3`.
    /// Shared by the light/color matrix steps of the NC* and CC/CDP family.
    fn mac_dot(
        &mut self,
        m: [[i16; 3]; 3],
        v: [i32; 3],
        t: Option<[i32; 3]>,
        sf: bool,
    ) -> [i32; 3] {
        let shift = if sf { 12 } else { 0 };
        let mut out = [0i32; 3];
        for row in 0..3 {
            let mut raw: i64 = (m[row][0] as i64) * (v[0] as i64)
                + (m[row][1] as i64) * (v[1] as i64)
                + (m[row][2] as i64) * (v[2] as i64);
            if let Some(t) = t {
                raw += (t[row] as i64) * 0x1000;
            }
            out[row] = self.store_mac(row + 1, raw, shift);
        }
        out
    }

    /// UNR (Newton-Raphson) reciprocal used by RTPS/RTPT, per PSX-SPX "GTE
    /// Division Inaccuracy". Deliberately reproduces the hardware's reduced
    /// precision (not a plain division) since games' near-plane behavior
    /// depends on it.
    fn unr_divide(&mut self, h: u16, sz3: u16) -> u32 {
        if (h as u32) < (sz3 as u32) * 2 {
            let z = sz3.leading_zeros(); // 0..=15, sz3 != 0 here
            let n0 = (h as u32) << z;
            let d0 = (sz3 as u32) << z;
            let idx = ((d0 - 0x7fc0) >> 7) as usize;
            let u = UNR_TABLE[idx] as u32 + 0x101;
            let d1 = (0x2000080u32.wrapping_sub(d0.wrapping_mul(u))) >> 8;
            let d2 = (0x80u32.wrapping_add(d1.wrapping_mul(u))) >> 8;
            let result = ((n0 as u64 * d2 as u64 + 0x8000) >> 16) as u32;
            result.min(0x1ffff)
        } else {
            self.flag |= 1 << 17;
            0x1ffff
        }
    }

    // ---------------------------------------------------------------
    // Commands
    // ---------------------------------------------------------------

    pub fn execute(&mut self, cmd: u32) {
        self.flag = 0; // every command starts with a clean FLAG

        let sf = (cmd >> 19) & 1 != 0;
        let mx = (cmd >> 17) & 3;
        let vsel = (cmd >> 15) & 3;
        let cv = (cmd >> 13) & 3;
        let lm = (cmd >> 10) & 1 != 0;
        let op = cmd & 0x3f;

        match op {
            0x01 => {
                let v0 = self.vector(0);
                self.rtp_transform(v0, sf, lm, true);
            }
            0x06 => self.cmd_nclip(),
            0x0c => self.cmd_op(sf, lm),
            0x10 => self.cmd_dpcs(sf, lm),
            0x11 => self.cmd_intpl(sf, lm),
            0x12 => self.cmd_mvmva(sf, lm, mx, vsel, cv),
            0x13 => {
                let v0 = self.vector(0);
                self.cmd_ncds_single(v0, sf, lm);
            }
            0x14 => self.cmd_cdp(sf, lm),
            0x16 => {
                for i in 0..3 {
                    let v = self.vector(i);
                    self.cmd_ncds_single(v, sf, lm);
                }
            }
            0x1b => {
                let v0 = self.vector(0);
                self.cmd_nccs_single(v0, sf, lm);
            }
            0x1c => self.cmd_cc(sf, lm),
            0x1e => {
                let v0 = self.vector(0);
                self.cmd_ncs_single(v0, sf, lm);
            }
            0x20 => {
                for i in 0..3 {
                    let v = self.vector(i);
                    self.cmd_ncs_single(v, sf, lm);
                }
            }
            0x28 => self.cmd_sqr(sf, lm),
            0x29 => self.cmd_dcpl(sf, lm),
            0x2a => self.cmd_dpct(sf, lm),
            0x2d => self.cmd_avsz3(),
            0x2e => self.cmd_avsz4(),
            0x30 => {
                for i in 0..3 {
                    let v = self.vector(i);
                    self.rtp_transform(v, sf, lm, i == 2);
                }
            }
            0x3d => self.cmd_gpf_gpl(sf, lm, false),
            0x3e => self.cmd_gpf_gpl(sf, lm, true),
            0x3f => {
                for i in 0..3 {
                    let v = self.vector(i);
                    self.cmd_nccs_single(v, sf, lm);
                }
            }
            _ => tracing::warn!(target: "psx_core::gte", cmd, op, "unknown GTE command"),
        }
    }

    /// RTPS core, also used 3x by RTPT. `depth_cue` gates the IR0/MAC0
    /// depth-cueing step, which RTPT only computes from the last vertex.
    fn rtp_transform(&mut self, vertex: [i32; 3], sf: bool, lm: bool, depth_cue: bool) {
        let shift = if sf { 12 } else { 0 };
        let mut mac123 = [0i32; 3];
        for row in 0..3 {
            let raw = (self.tr[row] as i64) * 0x1000
                + (self.rt[row][0] as i64) * (vertex[0] as i64)
                + (self.rt[row][1] as i64) * (vertex[1] as i64)
                + (self.rt[row][2] as i64) * (vertex[2] as i64);
            mac123[row] = self.store_mac(row + 1, raw, shift);
        }
        self.set_ir123(1, mac123[0], lm);
        self.set_ir123(2, mac123[1], lm);

        // Hardware quirk: IR3's saturation FLAG is always checked as if
        // sf=1 (i.e. against MAC3 SAR 12), even when sf=0; but the stored
        // IR3 itself uses the actual sf/lm-selected range.
        let ir3_check = if sf { mac123[2] } else { mac123[2] >> 12 };
        if !(-0x8000..=0x7fff).contains(&ir3_check) {
            self.flag |= 1 << 22;
        }
        let ir3_min = if lm { 0 } else { -0x8000 };
        self.ir[3] = mac123[2].clamp(ir3_min, 0x7fff) as i16;

        let sz3_shift = if sf { 0 } else { 12 };
        self.push_sz(mac123[2] >> sz3_shift);
        let sz3 = self.sz_fifo[3];

        let n = self.unr_divide(self.h, sz3);

        let sx = self.set_mac0((n as i64) * (self.ir[1] as i64) + (self.ofx as i64)) >> 16;
        let sy = self.set_mac0((n as i64) * (self.ir[2] as i64) + (self.ofy as i64)) >> 16;
        self.push_sxy(sx, sy);

        if depth_cue {
            let mac0 = self.set_mac0((n as i64) * (self.dqa as i64) + (self.dqb as i64));
            self.set_ir0(mac0 >> 12);
        }
    }

    fn cmd_nclip(&mut self) {
        let (sx0, sy0) = (self.sxy_fifo[0].0 as i64, self.sxy_fifo[0].1 as i64);
        let (sx1, sy1) = (self.sxy_fifo[1].0 as i64, self.sxy_fifo[1].1 as i64);
        let (sx2, sy2) = (self.sxy_fifo[2].0 as i64, self.sxy_fifo[2].1 as i64);
        let sum = sx0 * sy1 + sx1 * sy2 + sx2 * sy0 - sx0 * sy2 - sx1 * sy0 - sx2 * sy1;
        self.set_mac0(sum);
    }

    fn cmd_avsz3(&mut self) {
        let sum = (self.zsf3 as i64)
            * ((self.sz_fifo[1] as i64) + (self.sz_fifo[2] as i64) + (self.sz_fifo[3] as i64));
        let mac0 = self.set_mac0(sum);
        self.set_otz(mac0 >> 12);
    }

    fn cmd_avsz4(&mut self) {
        let sum = (self.zsf4 as i64)
            * ((self.sz_fifo[0] as i64)
                + (self.sz_fifo[1] as i64)
                + (self.sz_fifo[2] as i64)
                + (self.sz_fifo[3] as i64));
        let mac0 = self.set_mac0(sum);
        self.set_otz(mac0 >> 12);
    }

    fn cmd_op(&mut self, sf: bool, lm: bool) {
        let shift = if sf { 12 } else { 0 };
        // D1,D2,D3 are the RT diagonal, "misused" as a vector (per PSX-SPX).
        let d1 = self.rt[0][0] as i64;
        let d2 = self.rt[1][1] as i64;
        let d3 = self.rt[2][2] as i64;
        let [ir1, ir2, ir3] = self.ir_vector().map(|x| x as i64);
        let raws = [
            ir3 * d2 - ir2 * d3,
            ir1 * d3 - ir3 * d1,
            ir2 * d1 - ir1 * d2,
        ];
        let mut mac = [0i32; 3];
        for i in 0..3 {
            mac[i] = self.store_mac(i + 1, raws[i], shift);
        }
        for i in 0..3 {
            self.set_ir123(i + 1, mac[i], lm);
        }
    }

    fn cmd_sqr(&mut self, sf: bool, lm: bool) {
        let shift = if sf { 12 } else { 0 };
        let mut mac = [0i32; 3];
        for i in 0..3 {
            let x = self.ir[i + 1] as i64;
            mac[i] = self.store_mac(i + 1, x * x, shift);
        }
        for i in 0..3 {
            self.set_ir123(i + 1, mac[i], lm);
        }
    }

    fn garbage_matrix(&self) -> [[i16; 3]; 3] {
        // MVMVA mx=3 selects an undocumented "garbage" matrix on real
        // hardware; reproduced here since some homebrew/test ROMs probe it.
        let r = (self.rgbc & 0xff) as i32;
        [
            [(-(r * 0x10)) as i16, (r * 0x10) as i16, self.ir[0]],
            [self.rt[0][2], self.rt[0][2], self.rt[0][2]],
            [self.rt[1][1], self.rt[1][1], self.rt[1][1]],
        ]
    }

    fn cmd_mvmva(&mut self, sf: bool, lm: bool, mx: u32, vsel: u32, cv: u32) {
        let m = match mx {
            0 => self.rt,
            1 => self.l,
            2 => self.lc,
            _ => self.garbage_matrix(),
        };
        let v = match vsel {
            0 => self.vector(0),
            1 => self.vector(1),
            2 => self.vector(2),
            _ => self.ir_vector(),
        };
        // cv=2 (FC) is bugged on real hardware: the translation term AND the
        // first matrix column term are dropped, only the last two remain.
        let (t, skip_col0): (Option<[i32; 3]>, bool) = match cv {
            0 => (Some(self.tr), false),
            1 => (Some(self.bk), false),
            2 => (None, true),
            _ => (None, false),
        };
        let shift = if sf { 12 } else { 0 };
        let mut mac = [0i32; 3];
        for row in 0..3 {
            let mut raw: i64 = if skip_col0 {
                (m[row][1] as i64) * (v[1] as i64) + (m[row][2] as i64) * (v[2] as i64)
            } else {
                (m[row][0] as i64) * (v[0] as i64)
                    + (m[row][1] as i64) * (v[1] as i64)
                    + (m[row][2] as i64) * (v[2] as i64)
            };
            if let Some(t) = t {
                raw += (t[row] as i64) * 0x1000;
            }
            mac[row] = self.store_mac(row + 1, raw, shift);
        }
        for row in 0..3 {
            self.set_ir123(row + 1, mac[row], lm);
        }
    }

    /// `[IR1,IR2,IR3] = ((FC<<12) - MAC) SAR sf*12` (saturated as if lm=0),
    /// `[MAC1,MAC2,MAC3] = IR*IR0 + MAC` (un-shifted; step_final applies the
    /// final SAR). Shared by NCDx/CDP/DCPL/DPCS/DPCT/INTPL.
    fn apply_depth_cue(&mut self, prev_mac: [i32; 3], sf: bool) -> [i32; 3] {
        let shift = if sf { 12 } else { 0 };
        let mut ir_temp = [0i32; 3];
        for row in 0..3 {
            let raw = (self.fc[row] as i64) * 0x1000 - (prev_mac[row] as i64);
            let shifted = (raw >> shift) as i32;
            let clamped = shifted.clamp(-0x8000, 0x7fff); // always lm=0 boundary here
            if clamped != shifted {
                self.flag |= 1 << Self::ir_flag_bit(row + 1);
            }
            self.ir[row + 1] = clamped as i16;
            ir_temp[row] = clamped;
        }
        let mut out = [0i32; 3];
        for row in 0..3 {
            let raw = (ir_temp[row] as i64) * (self.ir[0] as i64) + (prev_mac[row] as i64);
            out[row] = self.store_mac(row + 1, raw, 0);
        }
        out
    }

    fn step_final(&mut self, prev: [i32; 3], sf: bool, lm: bool) -> [i32; 3] {
        let shift = if sf { 12 } else { 0 };
        let mut mac = [0i32; 3];
        for i in 0..3 {
            mac[i] = self.store_mac(i + 1, prev[i] as i64, shift);
        }
        for i in 0..3 {
            self.set_ir123(i + 1, mac[i], lm);
        }
        mac
    }

    fn step_color_mult(&mut self) -> [i32; 3] {
        // [R*IR1, G*IR2, B*IR3] SHL 4, no shift applied yet (step_final does).
        let r = (self.rgbc & 0xff) as i64;
        let g = ((self.rgbc >> 8) & 0xff) as i64;
        let b = ((self.rgbc >> 16) & 0xff) as i64;
        let [ir1, ir2, ir3] = self.ir_vector().map(|x| x as i64);
        let vals = [r * ir1, g * ir2, b * ir3];
        let mut out = [0i32; 3];
        for i in 0..3 {
            out[i] = self.store_mac(i + 1, vals[i] << 4, 0);
        }
        out
    }

    fn cmd_ncs_single(&mut self, normal: [i32; 3], sf: bool, lm: bool) {
        let m = self.l;
        let step1 = self.mac_dot(m, normal, None, sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step1[i], lm);
        }
        let ir = self.ir_vector();
        let lc = self.lc;
        let bk = self.bk;
        let step2 = self.mac_dot(lc, ir, Some(bk), sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step2[i], lm);
        }
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_nccs_single(&mut self, normal: [i32; 3], sf: bool, lm: bool) {
        let m = self.l;
        let step1 = self.mac_dot(m, normal, None, sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step1[i], lm);
        }
        let ir = self.ir_vector();
        let lc = self.lc;
        let bk = self.bk;
        let step2 = self.mac_dot(lc, ir, Some(bk), sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step2[i], lm);
        }
        let mult = self.step_color_mult();
        self.step_final(mult, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_ncds_single(&mut self, normal: [i32; 3], sf: bool, lm: bool) {
        let m = self.l;
        let step1 = self.mac_dot(m, normal, None, sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step1[i], lm);
        }
        let ir = self.ir_vector();
        let lc = self.lc;
        let bk = self.bk;
        let step2 = self.mac_dot(lc, ir, Some(bk), sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step2[i], lm);
        }
        let mult = self.step_color_mult();
        let dq = self.apply_depth_cue(mult, sf);
        self.step_final(dq, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_cc(&mut self, sf: bool, lm: bool) {
        let ir = self.ir_vector();
        let lc = self.lc;
        let bk = self.bk;
        let step2 = self.mac_dot(lc, ir, Some(bk), sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step2[i], lm);
        }
        let mult = self.step_color_mult();
        self.step_final(mult, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_cdp(&mut self, sf: bool, lm: bool) {
        let ir = self.ir_vector();
        let lc = self.lc;
        let bk = self.bk;
        let step2 = self.mac_dot(lc, ir, Some(bk), sf);
        for i in 0..3 {
            self.set_ir123(i + 1, step2[i], lm);
        }
        let mult = self.step_color_mult();
        let dq = self.apply_depth_cue(mult, sf);
        self.step_final(dq, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_dcpl(&mut self, sf: bool, lm: bool) {
        let mult = self.step_color_mult();
        let dq = self.apply_depth_cue(mult, sf);
        self.step_final(dq, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    fn cmd_intpl(&mut self, sf: bool, lm: bool) {
        let [ir1, ir2, ir3] = self.ir_vector().map(|x| x as i64);
        let vals = [ir1 << 12, ir2 << 12, ir3 << 12];
        let mut mult = [0i32; 3];
        for i in 0..3 {
            mult[i] = self.store_mac(i + 1, vals[i], 0);
        }
        let dq = self.apply_depth_cue(mult, sf);
        self.step_final(dq, sf, lm);
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }

    /// Shared by DPCS (from RGBC) and each DPCT iteration (from RGB0).
    fn cmd_dpcs_core(&mut self, r: u8, g: u8, b: u8, sf: bool, lm: bool, code: u8) {
        let vals = [(r as i64) << 16, (g as i64) << 16, (b as i64) << 16];
        let mut mult = [0i32; 3];
        for i in 0..3 {
            mult[i] = self.store_mac(i + 1, vals[i], 0);
        }
        let dq = self.apply_depth_cue(mult, sf);
        self.step_final(dq, sf, lm);
        self.push_color_from_mac(code);
    }

    fn cmd_dpcs(&mut self, sf: bool, lm: bool) {
        let r = (self.rgbc & 0xff) as u8;
        let g = ((self.rgbc >> 8) & 0xff) as u8;
        let b = ((self.rgbc >> 16) & 0xff) as u8;
        let code = (self.rgbc >> 24) as u8;
        self.cmd_dpcs_core(r, g, b, sf, lm, code);
    }

    fn cmd_dpct(&mut self, sf: bool, lm: bool) {
        let code = (self.rgbc >> 24) as u8;
        for _ in 0..3 {
            // Reads from the bottom of the Color FIFO, not RGBC (per PSX-SPX).
            let rgb0 = self.rgb_fifo[0];
            let r = (rgb0 & 0xff) as u8;
            let g = ((rgb0 >> 8) & 0xff) as u8;
            let b = ((rgb0 >> 16) & 0xff) as u8;
            self.cmd_dpcs_core(r, g, b, sf, lm, code);
        }
    }

    fn cmd_gpf_gpl(&mut self, sf: bool, lm: bool, use_base: bool) {
        let shift = if sf { 12 } else { 0 };
        let base = if use_base {
            [self.mac[1] as i64, self.mac[2] as i64, self.mac[3] as i64]
        } else {
            [0i64; 3]
        };
        let [ir1, ir2, ir3] = self.ir_vector().map(|x| x as i64);
        let ir = [ir1, ir2, ir3];
        let mut mac = [0i32; 3];
        for i in 0..3 {
            let raw = (base[i] << shift) + ir[i] * (self.ir[0] as i64);
            mac[i] = self.store_mac(i + 1, raw, shift);
        }
        for i in 0..3 {
            self.set_ir123(i + 1, mac[i], lm);
        }
        let code = (self.rgbc >> 24) as u8;
        self.push_color_from_mac(code);
    }
}

impl Default for Gte {
    fn default() -> Self {
        Self::new()
    }
}

fn pack16(lo: i16, hi: i16) -> u32 {
    (lo as u16 as u32) | ((hi as u16 as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity rotation (RT = 1.0 on the diagonal, in 1,3,12 fixed point),
    /// zero translation/offset, sf=1. Expected values hand-computed by
    /// following the UNR division algorithm exactly (see module comment for
    /// the worked example this mirrors).
    #[test]
    fn rtps_identity_rotation() {
        let mut gte = Gte::new();
        gte.write_control(0, pack16(0x1000, 0)); // RT11=1.0, RT12=0
        gte.write_control(1, pack16(0, 0)); // RT13=0, RT21=0
        gte.write_control(2, pack16(0x1000, 0)); // RT22=1.0, RT23=0
        gte.write_control(3, pack16(0, 0)); // RT31=0, RT32=0
        gte.write_control(4, 0x1000); // RT33=1.0
        gte.write_control(24, 0); // OFX
        gte.write_control(25, 0); // OFY
        gte.write_control(26, 0x400); // H
        gte.write_control(27, 0); // DQA
        gte.write_control(28, 0); // DQB

        gte.write_data(0, pack16(100, 50)); // VX0=100, VY0=50
        gte.write_data(1, 1000); // VZ0=1000

        gte.execute(0x0018_0001); // RTPS, sf=1 (bit19)

        assert_eq!(gte.ir[1], 100);
        assert_eq!(gte.ir[2], 50);
        assert_eq!(gte.ir[3], 1000);
        assert_eq!(gte.mac[1], 100);
        assert_eq!(gte.mac[2], 50);
        assert_eq!(gte.mac[3], 1000);
        assert_eq!(gte.sz_fifo[3], 1000);
        assert_eq!(gte.sxy_fifo[2], (102, 51));
        assert_eq!(gte.ir[0], 0);
        assert_eq!(gte.flag_value(), 0);
    }

    #[test]
    fn nclip_winding_sign() {
        let mut gte = Gte::new();
        // (0,0) -> (10,0) -> (0,10): counter-clockwise, area*2 = +100.
        gte.write_data(12, pack16(0, 0));
        gte.write_data(13, pack16(10, 0));
        gte.write_data(14, pack16(0, 10));
        gte.execute(0x0140_0006); // NCLIP
        assert_eq!(gte.mac[0], 100);
        assert_eq!(gte.read_data(24), 100);

        // Reversed winding flips the sign.
        gte.write_data(12, pack16(0, 10));
        gte.write_data(13, pack16(10, 0));
        gte.write_data(14, pack16(0, 0));
        gte.execute(0x1400_0006);
        assert_eq!(gte.mac[0], -100);
    }

    #[test]
    fn avsz3_scales_and_averages() {
        let mut gte = Gte::new();
        gte.write_data(17, 100); // SZ1
        gte.write_data(18, 200); // SZ2
        gte.write_data(19, 300); // SZ3
        gte.write_control(29, 0x1000); // ZSF3 = 1.0
        gte.execute(0x0158_002d); // AVSZ3
        // MAC0 = 0x1000 * (100+200+300) = 4096*600 = 2457600; OTZ = MAC0>>12 = 600.
        assert_eq!(gte.mac[0], 4096 * 600);
        assert_eq!(gte.otz, 600);
    }

    #[test]
    fn lzcr_counts_leading_sign_bits() {
        let mut gte = Gte::new();
        gte.write_data(30, 0x0000_00ff); // positive: 24 leading zero bits
        assert_eq!(gte.read_data(31), 24);

        gte.write_data(30, 0xffff_ff00u32 as i32 as u32); // -256: 24 leading one bits
        assert_eq!(gte.read_data(31), 24);

        gte.write_data(30, 0); // all zero -> 32 leading zeros
        assert_eq!(gte.read_data(31), 32);

        gte.write_data(30, 0xffff_ffffu32); // all one -> 32 leading ones
        assert_eq!(gte.read_data(31), 32);
    }

    #[test]
    fn irgb_orgb_roundtrip() {
        let mut gte = Gte::new();
        // 5-bit values that survive *0x80 then >>7 without rounding loss.
        let packed = 31 | (15 << 5) | (7 << 10);
        gte.write_data(28, packed);
        assert_eq!(gte.ir[1], 31 * 0x80);
        assert_eq!(gte.ir[2], 15 * 0x80);
        assert_eq!(gte.ir[3], 7 * 0x80);
        assert_eq!(gte.read_data(28), packed);
        assert_eq!(gte.read_data(29), packed); // ORGB mirrors IRGB
    }
}
