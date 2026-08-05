//! MDEC: macroblock decoder (FMV / compressed image decompression).
//!
//! Implements the full decode pipeline per PSX-SPX: run-length coefficient
//! expansion, dequantization with the uploaded quant tables, the two-pass
//! IDCT with the uploaded scale table, and YUV->RGB conversion for the
//! 15/24-bit modes (4/8-bit modes output luma only). Input is consumed
//! instantly; decoded pixels sit in an output FIFO drained by reads or DMA1.

use std::collections::VecDeque;
use tracing::trace;

const EOB: u16 = 0xfe00;
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, //
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, //
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, //
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// serde for `Vec<[i16; 64]>` (serde's array support stops at 32): the
/// blocks are flattened into one `Vec<i16>` and re-chunked on load.
mod flat_blocks {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[[i16; 64]], s: S) -> Result<S::Ok, S::Error> {
        let flat: Vec<i16> = v.iter().flatten().copied().collect();
        flat.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[i16; 64]>, D::Error> {
        let flat: Vec<i16> = Vec::deserialize(d)?;
        if !flat.len().is_multiple_of(64) {
            return Err(D::Error::custom("block data not a multiple of 64"));
        }
        Ok(flat
            .chunks_exact(64)
            .map(|c| c.try_into().unwrap())
            .collect())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Command {
    Idle,
    Decode,
    SetQuant { color: bool },
    SetScale,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Mdec {
    #[serde(with = "serde_big_array::BigArray")]
    luma_quant: [u8; 64],
    #[serde(with = "serde_big_array::BigArray")]
    chroma_quant: [u8; 64],
    #[serde(with = "serde_big_array::BigArray")]
    scale: [i16; 64],

    command: Command,
    /// Remaining parameter halfwords for the current command.
    remaining: u32,
    /// Halfword scratch for table uploads / coefficient stream.
    param_idx: usize,

    // Decode state
    depth: u8, // 0=4bit 1=8bit 2=24bit 3=15bit
    signed: bool,
    bit15: bool,
    /// Coefficient accumulator for the block being parsed.
    #[serde(with = "serde_big_array::BigArray")]
    coefs: [i16; 64],
    coef_idx: usize,
    in_block: bool,
    qscale: i32,
    /// Decoded 8x8 blocks pending macroblock assembly (Cr, Cb, Y1..Y4).
    #[serde(with = "flat_blocks")]
    blocks: Vec<[i16; 64]>,

    out: VecDeque<u32>,
    dma_in_enable: bool,
    dma_out_enable: bool,
}

impl Mdec {
    pub fn new() -> Self {
        Self {
            luma_quant: [1; 64],
            chroma_quant: [1; 64],
            scale: [0; 64],
            command: Command::Idle,
            remaining: 0,
            param_idx: 0,
            depth: 0,
            signed: false,
            bit15: false,
            coefs: [0; 64],
            coef_idx: 0,
            in_block: false,
            qscale: 0,
            blocks: Vec::with_capacity(6),
            out: VecDeque::new(),
            dma_in_enable: false,
            dma_out_enable: false,
        }
    }

    pub fn status(&self) -> u32 {
        let mut s = 0u32;
        if self.out.is_empty() {
            s |= 1 << 31;
        }
        // Data-in FIFO never fills (input is consumed instantly)
        if self.command != Command::Idle {
            s |= 1 << 29;
        }
        if self.dma_in_enable && self.command != Command::Idle {
            s |= 1 << 28;
        }
        if self.dma_out_enable && !self.out.is_empty() {
            s |= 1 << 27;
        }
        s |= (self.depth as u32) << 25;
        s |= (self.signed as u32) << 24;
        s |= (self.bit15 as u32) << 23;
        s |= ((self.blocks.len() as u32) & 7) << 16;
        s |= (self.remaining / 2).wrapping_sub(1) & 0xffff;
        s
    }

    pub fn write_control(&mut self, val: u32) {
        if val & (1 << 31) != 0 {
            // Reset: abort command, drain FIFOs
            self.command = Command::Idle;
            self.remaining = 0;
            self.in_block = false;
            self.blocks.clear();
            self.out.clear();
        }
        self.dma_in_enable = val & (1 << 30) != 0;
        self.dma_out_enable = val & (1 << 29) != 0;
    }

    pub fn read_data(&mut self) -> u32 {
        self.out.pop_front().unwrap_or(0)
    }

    /// Command/parameter port (also fed by DMA channel 0).
    pub fn write_data(&mut self, word: u32) {
        if self.command == Command::Idle {
            let op = word >> 29;
            match op {
                1 => {
                    self.command = Command::Decode;
                    self.depth = ((word >> 27) & 3) as u8;
                    self.signed = word & (1 << 26) != 0;
                    self.bit15 = word & (1 << 25) != 0;
                    self.remaining = (word & 0xffff) * 2;
                    self.in_block = false;
                    self.blocks.clear();
                    trace!(target: "psx_core::mdec",
                           "decode depth={} words={}", self.depth, word & 0xffff);
                }
                2 => {
                    self.command = Command::SetQuant {
                        color: word & 1 != 0,
                    };
                    let bytes = if word & 1 != 0 { 128 } else { 64 };
                    self.remaining = bytes / 2;
                    self.param_idx = 0;
                }
                3 => {
                    self.command = Command::SetScale;
                    self.remaining = 64;
                    self.param_idx = 0;
                }
                _ => {
                    // No-op commands just set status bits on real hardware
                    trace!(target: "psx_core::mdec", "command {op} ignored");
                }
            }
            return;
        }

        for half in [word as u16, (word >> 16) as u16] {
            if self.remaining == 0 {
                break;
            }
            self.remaining -= 1;
            self.feed_halfword(half);
        }
        if self.remaining == 0 {
            if self.command == Command::Decode && self.in_block {
                // Stream ended mid-block: flush what we have
                self.end_block();
            }
            self.command = Command::Idle;
        }
    }

    fn feed_halfword(&mut self, half: u16) {
        match self.command {
            Command::SetQuant { color } => {
                let b = half.to_le_bytes();
                for (k, byte) in b.iter().enumerate() {
                    let i = self.param_idx + k;
                    if i < 64 {
                        self.luma_quant[i] = *byte;
                    } else if color {
                        self.chroma_quant[i - 64] = *byte;
                    }
                }
                self.param_idx += 2;
            }
            Command::SetScale => {
                if self.param_idx < 64 {
                    self.scale[self.param_idx] = half as i16;
                }
                self.param_idx += 1;
            }
            Command::Decode => self.feed_coefficient(half),
            Command::Idle => unreachable!(),
        }
    }

    fn feed_coefficient(&mut self, half: u16) {
        if !self.in_block {
            if half == EOB {
                return; // padding between blocks
            }
            // DC halfword: quant scale in bits 10..16, DC value in 0..10
            self.in_block = true;
            self.coefs = [0; 64];
            self.coef_idx = 1;
            self.qscale = ((half >> 10) & 0x3f) as i32;
            let dc = sign10(half);
            let quant = self.quant_table()[0] as i32;
            self.coefs[0] = if self.qscale == 0 {
                (dc * 2).clamp(-0x400, 0x3ff) as i16
            } else {
                (dc * quant).clamp(-0x400, 0x3ff) as i16
            };
            return;
        }

        if half == EOB {
            self.end_block();
            return;
        }
        let run = (half >> 10) as usize;
        let level = sign10(half);
        self.coef_idx += run;
        if self.coef_idx >= 64 {
            // Overrun: treat as end of block (hardware wraps, games don't rely on it)
            self.end_block();
            return;
        }
        let quant = self.quant_table()[self.coef_idx] as i32;
        let v = if self.qscale == 0 {
            level * 2
        } else {
            (level * quant * self.qscale + 4) / 8
        };
        self.coefs[ZIGZAG[self.coef_idx]] = v.clamp(-0x400, 0x3ff) as i16;
        self.coef_idx += 1;
        if self.coef_idx >= 64 {
            self.end_block();
        }
    }

    fn quant_table(&self) -> &[u8; 64] {
        // Cr/Cb (the first two blocks of a color macroblock) use the chroma table
        if self.depth >= 2 && self.blocks.len() < 2 {
            &self.chroma_quant
        } else {
            &self.luma_quant
        }
    }

    fn end_block(&mut self) {
        self.in_block = false;
        let block = idct(&self.coefs, &self.scale);
        self.blocks.push(block);
        let needed = if self.depth >= 2 { 6 } else { 1 };
        if self.blocks.len() >= needed {
            self.emit_macroblock();
            self.blocks.clear();
        }
    }

    fn emit_macroblock(&mut self) {
        match self.depth {
            0 | 1 => {
                // Monochrome 8x8: y is a signed sample, bias to unsigned
                let y = &self.blocks[0];
                if self.depth == 1 {
                    for chunk in y.chunks(4) {
                        let mut w = 0u32;
                        for (i, s) in chunk.iter().enumerate() {
                            let v = mono_pixel(*s, self.signed);
                            w |= (v as u32) << (8 * i);
                        }
                        self.out.push_back(w);
                    }
                } else {
                    for chunk in y.chunks(8) {
                        let mut w = 0u32;
                        for (i, s) in chunk.iter().enumerate() {
                            let v = mono_pixel(*s, self.signed) >> 4;
                            w |= (v as u32) << (4 * i);
                        }
                        self.out.push_back(w);
                    }
                }
            }
            _ => self.emit_color_macroblock(),
        }
    }

    /// Assemble a 16x16 RGB macroblock from Cr, Cb, Y1..Y4.
    fn emit_color_macroblock(&mut self) {
        let (cr, cb) = (&self.blocks[0], &self.blocks[1]);
        let mut rgb = [[0u8; 3]; 256];
        for (quad, yblk) in self.blocks[2..6].iter().enumerate() {
            let (bx, by) = ((quad & 1) * 8, (quad >> 1) * 8);
            for y in 0..8 {
                for x in 0..8 {
                    let (px, py) = (bx + x, by + y);
                    let c = (px / 2) + (py / 2) * 8;
                    let r0 = cr[c] as f32;
                    let b0 = cb[c] as f32;
                    let rr = 1.402 * r0;
                    let bb = 1.772 * b0;
                    let gg = -0.3437 * b0 - 0.7143 * r0;
                    let yy = yblk[x + y * 8] as f32;
                    let bias = if self.signed { 0.0 } else { 128.0 };
                    let clamp = |v: f32| (v.clamp(-128.0, 127.0) + 128.0) as u8;
                    rgb[px + py * 16] = [
                        clamp(yy + rr + bias - 128.0),
                        clamp(yy + gg + bias - 128.0),
                        clamp(yy + bb + bias - 128.0),
                    ];
                }
            }
        }
        match self.depth {
            3 => {
                let stp = (self.bit15 as u16) << 15;
                for pair in rgb.chunks(2) {
                    let p = |c: [u8; 3]| -> u16 {
                        ((c[0] >> 3) as u16)
                            | ((c[1] >> 3) as u16) << 5
                            | ((c[2] >> 3) as u16) << 10
                            | stp
                    };
                    self.out
                        .push_back(p(pair[0]) as u32 | (p(pair[1]) as u32) << 16);
                }
            }
            _ => {
                // 24-bit: packed RGB bytes
                let mut bytes = Vec::with_capacity(768);
                for c in rgb {
                    bytes.extend_from_slice(&c);
                }
                for chunk in bytes.chunks(4) {
                    let mut w = 0u32;
                    for (i, b) in chunk.iter().enumerate() {
                        w |= (*b as u32) << (8 * i);
                    }
                    self.out.push_back(w);
                }
            }
        }
    }
}

impl Default for Mdec {
    fn default() -> Self {
        Self::new()
    }
}

/// Sign-extend the 10-bit DC/level field.
fn sign10(half: u16) -> i32 {
    ((half as i32) << 22) >> 22
}

fn mono_pixel(s: i16, signed: bool) -> u8 {
    if signed {
        (s.clamp(-128, 127) + 128) as u8
    } else {
        s.clamp(0, 255) as u8
    }
}

/// Two-pass IDCT with the uploaded scale table (PSX-SPX reference form).
fn idct(coefs: &[i16; 64], scale: &[i16; 64]) -> [i16; 64] {
    let mut src = [0i64; 64];
    let mut dst = [0i64; 64];
    for i in 0..64 {
        src[i] = coefs[i] as i64;
    }
    for _pass in 0..2 {
        for x in 0..8 {
            for y in 0..8 {
                let mut sum = 0i64;
                for z in 0..8 {
                    sum += src[y + z * 8] * (scale[x + z * 8] as i64 / 8);
                }
                dst[x + y * 8] = (sum + 0xfff) >> 13;
            }
        }
        std::mem::swap(&mut src, &mut dst);
    }
    let mut out = [0i16; 64];
    for i in 0..64 {
        out[i] = src[i].clamp(-128, 127) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_scale() -> [i16; 64] {
        // Standard JPEG-style cosine table in 1.13 fixed point, as the BIOS
        // uploads: scale[x + z*8] = round(cos((2x+1)*z*pi/16) / 2 * 0x4000)
        // with the z=0 row scaled by 1/sqrt(2).
        let mut t = [0i16; 64];
        for z in 0..8 {
            for x in 0..8 {
                let c = ((2 * x + 1) as f64 * z as f64 * std::f64::consts::PI / 16.0).cos();
                let s = if z == 0 { c / (2.0f64).sqrt() } else { c };
                t[x + z * 8] = (s / 2.0 * 16384.0).round() as i16;
            }
        }
        t
    }

    #[test]
    fn flat_dc_block_decodes_to_uniform_luma() {
        let mut m = Mdec::new();
        m.write_data(3 << 29); // set scale table
        let scale = default_scale();
        for pair in scale.chunks(2) {
            m.write_data((pair[0] as u16 as u32) | (pair[1] as u16 as u32) << 16);
        }
        m.write_data(2 << 29); // set luma quant table
        for chunk in [1u8; 64].chunks(4) {
            m.write_data(u32::from_le_bytes(chunk.try_into().unwrap()));
        }
        // Decode one 8-bit mono block: qscale=1, DC=64, then EOB. 17 words.
        m.write_data((1 << 29) | (1 << 27) | 17);
        m.write_data(((1 << 10) | 64) as u32 | (EOB as u32) << 16);
        for _ in 0..16 {
            m.write_data((EOB as u32) | (EOB as u32) << 16);
        }
        // A pure-DC block is uniform; 16 words of 4 pixels for 8x8 @ 8bpp
        let first = m.read_data();
        assert_ne!(first, 0);
        let px = first & 0xff;
        assert_eq!(first, px * 0x0101_0101);
    }
}
