//! Software rasterizer: triangles, rectangles, lines.
//!
//! Integer edge-function rasterization with a top-left fill rule (PS1 omits
//! right/bottom edge pixels). Dithering follows the hardware rule: applied
//! to gouraud shading and texture modulation on polygons and lines, never
//! to rectangles, fills or raw textures.

use super::{Gpu, TexDepth, VRAM_WIDTH};

#[derive(Clone, Copy, Default)]
struct Vertex {
    x: i32,
    y: i32,
    r: i32,
    g: i32,
    b: i32,
    u: i32,
    v: i32,
}

#[derive(Clone, Copy)]
struct TexConfig {
    page_x: i32, // in 64-halfword units
    page_y: i32, // in 256-line units
    depth: TexDepth,
    clut_x: i32, // in 16-halfword units
    clut_y: i32,
}

/// Sign-extend the 11-bit vertex coordinates.
fn vertex_xy(word: u32) -> (i32, i32) {
    let x = ((word & 0x7ff) << 21) as i32 >> 21;
    let y = (((word >> 16) & 0x7ff) << 21) as i32 >> 21;
    (x, y)
}

fn color_rgb(word: u32) -> (i32, i32, i32) {
    (
        (word & 0xff) as i32,
        ((word >> 8) & 0xff) as i32,
        ((word >> 16) & 0xff) as i32,
    )
}

fn tex_config(clut: u32, page: u32) -> TexConfig {
    TexConfig {
        page_x: (page & 0xf) as i32,
        page_y: ((page >> 4) & 1) as i32,
        depth: match (page >> 7) & 3 {
            0 => TexDepth::T4Bit,
            1 => TexDepth::T8Bit,
            _ => TexDepth::T15Bit,
        },
        clut_x: (clut & 0x3f) as i32,
        clut_y: ((clut >> 6) & 0x1ff) as i32,
    }
}

/// The hardware's 4x4 ordered-dither offsets, added to 8-bit channels
/// before truncation to 5 bits.
const DITHER: [[i32; 4]; 4] = [
    [-4, 0, -3, 1],
    [2, -2, 3, -1],
    [-3, 1, -4, 0],
    [3, -1, 2, -2],
];

fn orient2d(a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> i64 {
    (b.0 - a.0) as i64 * (c.1 - a.1) as i64 - (b.1 - a.1) as i64 * (c.0 - a.0) as i64
}

/// With our positive-area (clockwise on screen, y-down) winding, edges that
/// are horizontal-going-right or going-up bound the interior from the
/// top/left and get their boundary pixels drawn.
fn is_top_left(a: (i32, i32), b: (i32, i32)) -> bool {
    (a.1 == b.1 && b.0 > a.0) || b.1 < a.1
}

impl Gpu {
    pub(super) fn draw_polygon_command(&mut self, op: u8, cmd: &[u32]) {
        let gouraud = op & 0x10 != 0;
        let quad = op & 0x08 != 0;
        let textured = op & 0x04 != 0;
        let semi = op & 0x02 != 0;
        let raw = textured && op & 0x01 != 0;
        let n = if quad { 4 } else { 3 };

        let mut vs = [Vertex::default(); 4];
        let mut clut = 0u32;
        let mut page = 0u32;
        let mut w = 1usize;
        let (mut r, mut g, mut b) = color_rgb(cmd[0]);
        for (k, vert) in vs.iter_mut().take(n).enumerate() {
            if k > 0 && gouraud {
                (r, g, b) = color_rgb(cmd[w]);
                w += 1;
            }
            let (x, y) = vertex_xy(cmd[w]);
            w += 1;
            let (u, v) = if textured {
                let uvw = cmd[w];
                w += 1;
                match k {
                    0 => clut = uvw >> 16,
                    1 => page = uvw >> 16,
                    _ => {}
                }
                ((uvw & 0xff) as i32, ((uvw >> 8) & 0xff) as i32)
            } else {
                (0, 0)
            };
            *vert = Vertex {
                x: x + self.draw_offset.0,
                y: y + self.draw_offset.1,
                r,
                g,
                b,
                u,
                v,
            };
        }

        let tex = textured.then(|| tex_config(clut, page));
        // Textured polygons carry their own semi-transparency mode
        let semi_mode = if textured {
            ((page >> 5) & 3) as u8
        } else {
            self.semi_mode
        };

        self.triangle(&[vs[0], vs[1], vs[2]], tex, gouraud, semi, semi_mode, raw);
        if quad {
            self.triangle(&[vs[1], vs[2], vs[3]], tex, gouraud, semi, semi_mode, raw);
        }
    }

    pub(super) fn draw_rect_command(&mut self, op: u8, cmd: &[u32]) {
        let textured = op & 0x04 != 0;
        let semi = op & 0x02 != 0;
        let raw = textured && op & 0x01 != 0;
        let (r, g, b) = color_rgb(cmd[0]);

        let mut w = 1usize;
        let (vx, vy) = vertex_xy(cmd[w]);
        w += 1;
        let (mut clut, mut u0, mut v0) = (0u32, 0i32, 0i32);
        if textured {
            let uvw = cmd[w];
            w += 1;
            clut = uvw >> 16;
            u0 = (uvw & 0xff) as i32;
            v0 = ((uvw >> 8) & 0xff) as i32;
        }
        let (rw, rh) = match (op >> 3) & 3 {
            0 => {
                let s = cmd[w];
                ((s & 0x3ff) as i32, ((s >> 16) & 0x1ff) as i32)
            }
            1 => (1, 1),
            2 => (8, 8),
            _ => (16, 16),
        };

        // Rectangles always use the current E1 texture page
        let page = (self.tex_page_x as u32)
            | (self.tex_page_y as u32) << 4
            | (self.tex_depth as u32) << 7;
        let tex = textured.then(|| tex_config(clut, page));

        let x0 = vx + self.draw_offset.0;
        let y0 = vy + self.draw_offset.1;
        for dy in 0..rh {
            let y = y0 + dy;
            if y < self.draw_min.1 || y > self.draw_max.1 {
                continue;
            }
            for dx in 0..rw {
                let x = x0 + dx;
                if x < self.draw_min.0 || x > self.draw_max.0 {
                    continue;
                }
                let px = match tex {
                    Some(tc) => {
                        // E1 bits 12/13 mirror rectangle texture fetches
                        let (fx, fy) = self.rect_tex_flip;
                        let tu = if fx { u0 - dx } else { u0 + dx } & 0xff;
                        let tv = if fy { v0 - dy } else { v0 + dy } & 0xff;
                        let texel = self.sample(tu, tv, &tc);
                        if texel == 0 {
                            continue; // fully transparent texel
                        }
                        // Rectangles are never dithered
                        self.shade_texel(texel, r, g, b, raw, x, y, false)
                    }
                    None => rgb_to_555(r, g, b),
                };
                let apply_semi = semi && (tex.is_none() || px & 0x8000 != 0);
                self.put_pixel(x, y, px, apply_semi, self.semi_mode);
            }
        }
    }

    pub(super) fn draw_line_command(&mut self, cmd: &[u32]) {
        let op = (cmd[0] >> 24) as u8;
        let gouraud = op & 0x10 != 0;
        let semi = op & 0x02 != 0;
        let (r0, g0, b0) = color_rgb(cmd[0]);
        let (v1w, c2w, v2w) = if gouraud {
            (cmd[1], cmd[2], cmd[3])
        } else {
            (cmd[1], cmd[0], cmd[2])
        };
        let (r1, g1, b1) = color_rgb(c2w);
        let (x0, y0) = vertex_xy(v1w);
        let (x1, y1) = vertex_xy(v2w);
        let (x0, y0) = (x0 + self.draw_offset.0, y0 + self.draw_offset.1);
        let (x1, y1) = (x1 + self.draw_offset.0, y1 + self.draw_offset.1);

        let dx = x1 - x0;
        let dy = y1 - y0;
        if dx.abs() >= 1024 || dy.abs() >= 512 {
            return;
        }
        let steps = dx.abs().max(dy.abs()).max(1);
        // 16.16 fixed-point DDA
        let sx = ((dx as i64) << 16) / steps as i64;
        let sy = ((dy as i64) << 16) / steps as i64;
        let mut fx = (x0 as i64) << 16;
        let mut fy = (y0 as i64) << 16;
        for i in 0..=steps {
            let x = (fx >> 16) as i32;
            let y = (fy >> 16) as i32;
            if x >= self.draw_min.0 && x <= self.draw_max.0 && y >= self.draw_min.1 && y <= self.draw_max.1
            {
                let t = i as i64;
                let lerp = |a: i32, b: i32| (a as i64 + (b - a) as i64 * t / steps as i64) as i32;
                let px = rgb_to_555_dithered(
                    lerp(r0, r1),
                    lerp(g0, g1),
                    lerp(b0, b1),
                    x,
                    y,
                    self.dither && gouraud,
                );
                self.put_pixel(x, y, px, semi, self.semi_mode);
            }
            fx += sx;
            fy += sy;
        }
    }

    fn triangle(
        &mut self,
        v: &[Vertex; 3],
        tex: Option<TexConfig>,
        gouraud: bool,
        semi: bool,
        semi_mode: u8,
        raw: bool,
    ) {
        let mut v0 = v[0];
        let mut v1 = v[1];
        let v2 = v[2];
        let mut area = orient2d((v0.x, v0.y), (v1.x, v1.y), (v2.x, v2.y));
        if area == 0 {
            return;
        }
        if area < 0 {
            std::mem::swap(&mut v0, &mut v1);
            area = -area;
        }

        let min_x = v0.x.min(v1.x).min(v2.x);
        let max_x = v0.x.max(v1.x).max(v2.x);
        let min_y = v0.y.min(v1.y).min(v2.y);
        let max_y = v0.y.max(v1.y).max(v2.y);
        // Oversized primitives are rejected by the hardware
        if max_x - min_x >= 1024 || max_y - min_y >= 512 {
            return;
        }

        let x0 = min_x.max(self.draw_min.0);
        let x1 = max_x.min(self.draw_max.0);
        let y0 = min_y.max(self.draw_min.1);
        let y1 = max_y.min(self.draw_max.1);
        if x0 > x1 || y0 > y1 {
            return;
        }

        // Dither gouraud shading and texture modulation (never raw texture)
        let dither = self.dither && (gouraud || (tex.is_some() && !raw));

        let a = (v0.x, v0.y);
        let b = (v1.x, v1.y);
        let c = (v2.x, v2.y);
        let bias0 = if is_top_left(b, c) { 0 } else { -1 };
        let bias1 = if is_top_left(c, a) { 0 } else { -1 };
        let bias2 = if is_top_left(a, b) { 0 } else { -1 };

        // Edge values at the top-left corner of the bbox, then step
        let mut w0_row = orient2d(b, c, (x0, y0));
        let mut w1_row = orient2d(c, a, (x0, y0));
        let mut w2_row = orient2d(a, b, (x0, y0));
        let (a01, b01) = (-(b.1 - a.1) as i64, (b.0 - a.0) as i64);
        let (a12, b12) = (-(c.1 - b.1) as i64, (c.0 - b.0) as i64);
        let (a20, b20) = (-(a.1 - c.1) as i64, (a.0 - c.0) as i64);

        for y in y0..=y1 {
            let mut w0 = w0_row;
            let mut w1 = w1_row;
            let mut w2 = w2_row;
            for x in x0..=x1 {
                if w0 + bias0 >= 0 && w1 + bias1 >= 0 && w2 + bias2 >= 0 {
                    let interp = |p0: i32, p1: i32, p2: i32| {
                        ((w0 * p0 as i64 + w1 * p1 as i64 + w2 * p2 as i64) / area) as i32
                    };
                    let (r, g, b_) = if gouraud {
                        (
                            interp(v0.r, v1.r, v2.r),
                            interp(v0.g, v1.g, v2.g),
                            interp(v0.b, v1.b, v2.b),
                        )
                    } else {
                        (v0.r, v0.g, v0.b)
                    };
                    let px = match tex {
                        Some(tc) => {
                            let u = interp(v0.u, v1.u, v2.u) & 0xff;
                            let vv = interp(v0.v, v1.v, v2.v) & 0xff;
                            let texel = self.sample(u, vv, &tc);
                            if texel == 0 {
                                // fully transparent texel: skip pixel
                                w0 += a12;
                                w1 += a20;
                                w2 += a01;
                                continue;
                            }
                            self.shade_texel(texel, r, g, b_, raw, x, y, dither)
                        }
                        None => rgb_to_555_dithered(r, g, b_, x, y, dither),
                    };
                    let apply_semi = semi && (tex.is_none() || px & 0x8000 != 0);
                    self.put_pixel(x, y, px, apply_semi, semi_mode);
                }
                w0 += a12;
                w1 += a20;
                w2 += a01;
            }
            w0_row += b12;
            w1_row += b20;
            w2_row += b01;
        }
    }

    /// Fetch a texel (after the texture window) as a raw 1555 value.
    /// Returns 0 for the fully-transparent color.
    fn sample(&self, u: i32, v: i32, tc: &TexConfig) -> u16 {
        let (mx, my) = self.tex_win_mask;
        let (ox, oy) = self.tex_win_off;
        let u = (u & !((mx as i32) * 8)) | (((ox & mx) as i32) * 8);
        let v = (v & !((my as i32) * 8)) | (((oy & my) as i32) * 8);
        let py = ((tc.page_y * 256 + v) & 511) as usize;
        let fetch = |x: i32| self.vram[py * VRAM_WIDTH + ((x & 1023) as usize)];
        match tc.depth {
            TexDepth::T4Bit => {
                let t = fetch(tc.page_x * 64 + u / 4);
                let idx = (t >> ((u & 3) * 4)) & 0xf;
                self.clut_lookup(tc, idx as i32)
            }
            TexDepth::T8Bit => {
                let t = fetch(tc.page_x * 64 + u / 2);
                let idx = (t >> ((u & 1) * 8)) & 0xff;
                self.clut_lookup(tc, idx as i32)
            }
            TexDepth::T15Bit => fetch(tc.page_x * 64 + u),
        }
    }

    fn clut_lookup(&self, tc: &TexConfig, idx: i32) -> u16 {
        let x = ((tc.clut_x * 16 + idx) & 1023) as usize;
        self.vram[(tc.clut_y & 511) as usize * VRAM_WIDTH + x]
    }

    /// Modulate a texel with the vertex color ((tex * color) / 128 per
    /// channel), preserving the STP bit. `raw` skips modulation; modulated
    /// results are dithered in the 8-bit domain like hardware.
    fn shade_texel(
        &self,
        texel: u16,
        r: i32,
        g: i32,
        b: i32,
        raw: bool,
        x: i32,
        y: i32,
        dither: bool,
    ) -> u16 {
        if raw {
            return texel;
        }
        let expand = |c: u16| (((c & 0x1f) << 3) | ((c & 0x1f) >> 2)) as i32;
        let mr = (expand(texel) * r >> 7).min(255);
        let mg = (expand(texel >> 5) * g >> 7).min(255);
        let mb = (expand(texel >> 10) * b >> 7).min(255);
        rgb_to_555_dithered(mr, mg, mb, x, y, dither) | (texel & 0x8000)
    }

    /// Final pixel write: clip is already done; handles semi-transparency
    /// blending and the mask-bit rules.
    fn put_pixel(&mut self, x: i32, y: i32, px: u16, semi: bool, semi_mode: u8) {
        if !(0..1024).contains(&x) || !(0..512).contains(&y) {
            return;
        }
        let i = y as usize * VRAM_WIDTH + x as usize;
        if self.check_mask && self.vram[i] & 0x8000 != 0 {
            return;
        }
        let out = if semi {
            let back = self.vram[i];
            blend(back, px, semi_mode) | (px & 0x8000)
        } else {
            px
        };
        self.vram[i] = out | if self.force_mask { 0x8000 } else { 0 };
    }
}

fn rgb_to_555(r: i32, g: i32, b: i32) -> u16 {
    (((r >> 3) & 0x1f) | ((g >> 3) & 0x1f) << 5 | ((b >> 3) & 0x1f) << 10) as u16
}

/// 8-bit channels to 15-bit, with the ordered dither applied first.
fn rgb_to_555_dithered(r: i32, g: i32, b: i32, x: i32, y: i32, dither: bool) -> u16 {
    if !dither {
        return rgb_to_555(r.clamp(0, 255), g.clamp(0, 255), b.clamp(0, 255));
    }
    let d = DITHER[(y & 3) as usize][(x & 3) as usize];
    rgb_to_555(
        (r + d).clamp(0, 255),
        (g + d).clamp(0, 255),
        (b + d).clamp(0, 255),
    )
}

/// Semi-transparency: per-channel blend of back(B) and front(F) 5-bit values.
fn blend(back: u16, front: u16, mode: u8) -> u16 {
    let mut out = 0u16;
    for shift in [0, 5, 10] {
        let b = ((back >> shift) & 0x1f) as i32;
        let f = ((front >> shift) & 0x1f) as i32;
        let v = match mode {
            0 => (b + f) / 2,
            1 => b + f,
            2 => b - f,
            _ => b + f / 4,
        }
        .clamp(0, 31) as u16;
        out |= v << shift;
    }
    out
}
