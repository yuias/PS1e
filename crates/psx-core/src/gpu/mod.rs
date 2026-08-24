//! GPU: GP0/GP1 command decoding, VRAM, display state.
//!
//! Rendering is a software rasterizer (see `rasterizer.rs`) writing into a
//! 1024x512 16-bit VRAM buffer; the frontend presents it. Commands execute
//! instantly — GPU busy timing is a later accuracy milestone.

mod rasterizer;

use std::collections::VecDeque;
use tracing::{debug, trace, warn};

pub const VRAM_WIDTH: usize = 1024;
pub const VRAM_HEIGHT: usize = 512;

/// What GP0 is currently receiving.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
enum Gp0Mode {
    Command,
    /// CPU->VRAM image transfer: destination rect and write cursor.
    ImageLoad {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        cur: u32,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum TexDepth {
    T4Bit = 0,
    T8Bit = 1,
    T15Bit = 2,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Gpu {
    pub vram: Box<[u16]>,

    // GP0 assembly buffer
    fifo: Vec<u32>,
    words_needed: usize,
    mode: Gp0Mode,

    // Active polyline: command word plus the tail vertex to chain from
    in_polyline: bool,
    poly_cmd: u32,
    poly_last_color: u32,
    poly_last_vertex: u32,

    // Drawing state (GP0 E1..E6)
    pub tex_page_x: u16, // in 64-halfword units
    pub tex_page_y: u16, // in 256-line units
    pub semi_mode: u8,   // global semi-transparency mode
    pub tex_depth: TexDepth,
    dither: bool,
    draw_to_display: bool,
    tex_disable: bool,
    tex_win_mask: (u8, u8),
    tex_win_off: (u8, u8),
    rect_tex_flip: (bool, bool),
    draw_min: (i32, i32),
    draw_max: (i32, i32),
    draw_offset: (i32, i32),
    force_mask: bool,
    check_mask: bool,

    // Display state (GP1)
    display_disabled: bool,
    display_vram_start: (u16, u16),
    display_h_range: (u16, u16),
    display_v_range: (u16, u16),
    hres: u16,
    vres: u16,
    pal_mode: bool,
    color_24bit: bool,
    interlaced: bool,
    dma_direction: u8,
    irq_pending: bool,

    /// Toggles every frame (GPUSTAT bit 31).
    pub odd_frame: bool,

    /// When set, every executed GP0/GP1 command is decoded to the
    /// `psx_core::gpu::cmd` tracing target at debug level.
    pub log_commands: bool,
    /// Vblank count since reset; tags command-log entries with the frame.
    pub frame_count: u64,

    /// Buffered VRAM->CPU transfer data, popped via GPUREAD.
    read_queue: VecDeque<u32>,

    /// Display area captured at the last vblank — what the TV shows.
    /// Presenting this instead of live VRAM avoids mid-frame flicker.
    pub frame: Frame,
}

/// A vblank snapshot of the display area, as raw VRAM halfwords per row
/// (`stride` halfwords each; 24-bit rows pack 2 pixels into 3 halfwords).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    pub pixels: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub is_24bit: bool,
    pub enabled: bool,
}

/// Nominal video clocks, per psx-spx. A console always runs its own
/// region's clock; keying off the selected display mode instead is what
/// psx-spx recommends for emulation.
const NTSC_VIDEO_CLOCK_HZ: u64 = 53_693_175;
const PAL_VIDEO_CLOCK_HZ: u64 = 53_203_425;
const NTSC_VCLK_PER_LINE: u64 = 3413;
const PAL_VCLK_PER_LINE: u64 = 3406;

/// Video timing of the current display mode: what the root counters gate
/// on, and how long a field lasts.
///
/// Derived from the nominal video clocks against the 33.8688 MHz CPU
/// clock. Interlaced fields are rounded up to whole scanlines rather than
/// the hardware's 262.5/312.5, which costs half a line of drift per field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTiming {
    /// CPU cycles per scanline.
    pub cycles_per_line: u64,
    /// Scanlines per field, blanking included.
    pub lines_per_frame: u64,
    /// Scanlines carrying picture; the remainder is vertical blanking.
    pub visible_lines: u64,
    /// Horizontal blanking per scanline, in CPU cycles.
    pub hblank_cycles: u64,
    /// CPU cycles per dotclock tick, as `(cycles, ticks)`. Kept as a ratio
    /// because it is fractional in every mode: 320-pixel NTSC, for one,
    /// runs 5.05 cycles per dot. The hardware additionally truncates the
    /// fractional dots at the end of each scanline; that is not modelled.
    pub dotclock: (u64, u64),
}

impl VideoTiming {
    /// 3413 video clocks per line, 263 lines per field.
    pub const NTSC: Self = Self {
        cycles_per_line: 2153,
        lines_per_frame: 263,
        visible_lines: 240,
        hblank_cycles: 538,
        dotclock: (8 * crate::CPU_CLOCK_HZ, NTSC_VIDEO_CLOCK_HZ),
    };
    /// 3406 video clocks per line, 314 lines per field.
    pub const PAL: Self = Self {
        cycles_per_line: 2168,
        lines_per_frame: 314,
        visible_lines: 288,
        hblank_cycles: 538,
        dotclock: (8 * crate::CPU_CLOCK_HZ, PAL_VIDEO_CLOCK_HZ),
    };

    /// CPU cycles per field.
    pub const fn cycles_per_frame(&self) -> u64 {
        self.cycles_per_line * self.lines_per_frame
    }

    /// Vertical blanking, in CPU cycles.
    pub const fn vblank_cycles(&self) -> u64 {
        self.lines_per_frame.saturating_sub(self.visible_lines) * self.cycles_per_line
    }
}

impl Gpu {
    pub fn new() -> Self {
        Self {
            vram: vec![0u16; VRAM_WIDTH * VRAM_HEIGHT].into_boxed_slice(),
            fifo: Vec::with_capacity(16),
            words_needed: 0,
            mode: Gp0Mode::Command,
            in_polyline: false,
            poly_cmd: 0,
            poly_last_color: 0,
            poly_last_vertex: 0,
            tex_page_x: 0,
            tex_page_y: 0,
            semi_mode: 0,
            tex_depth: TexDepth::T4Bit,
            dither: false,
            draw_to_display: false,
            tex_disable: false,
            tex_win_mask: (0, 0),
            tex_win_off: (0, 0),
            rect_tex_flip: (false, false),
            draw_min: (0, 0),
            draw_max: (0, 0),
            draw_offset: (0, 0),
            force_mask: false,
            check_mask: false,
            display_disabled: true,
            display_vram_start: (0, 0),
            display_h_range: (0x200, 0x200 + 256 * 10),
            display_v_range: (0x10, 0x10 + 240),
            hres: 320,
            vres: 240,
            pal_mode: false,
            color_24bit: false,
            interlaced: false,
            dma_direction: 0,
            irq_pending: false,
            odd_frame: false,
            log_commands: false,
            frame_count: 0,
            read_queue: VecDeque::new(),
            frame: Frame::default(),
        }
    }

    // --- Frontend accessors -------------------------------------------

    pub fn display_enabled(&self) -> bool {
        !self.display_disabled
    }

    /// Video timing for the current display mode: region-dependent field
    /// geometry, refined by the configured display window and horizontal
    /// resolution. A degenerate window (some titles leave one zeroed)
    /// falls back to the region's nominal blanking.
    pub fn video_timing(&self) -> VideoTiming {
        let (base, vclk_per_line, video_clock) = if self.pal_mode {
            (VideoTiming::PAL, PAL_VCLK_PER_LINE, PAL_VIDEO_CLOCK_HZ)
        } else {
            (VideoTiming::NTSC, NTSC_VCLK_PER_LINE, NTSC_VIDEO_CLOCK_HZ)
        };

        // GP1(06): active picture per scanline, in video clocks
        let active = self
            .display_h_range
            .1
            .saturating_sub(self.display_h_range.0) as u64;
        let hblank_cycles = if active == 0 || active > vclk_per_line {
            base.hblank_cycles
        } else {
            (vclk_per_line - active) * base.cycles_per_line / vclk_per_line
        };

        // GP1(07): first and last displayed scanline
        let lines = self
            .display_v_range
            .1
            .saturating_sub(self.display_v_range.0) as u64;
        let visible_lines = if lines == 0 || lines > base.lines_per_frame {
            base.visible_lines
        } else {
            lines
        };

        VideoTiming {
            visible_lines,
            hblank_cycles,
            dotclock: (self.dotclock_divider() * crate::CPU_CLOCK_HZ, video_clock),
            ..base
        }
    }

    /// Video clocks per dot for the current horizontal resolution.
    fn dotclock_divider(&self) -> u64 {
        match self.hres {
            256 => 10,
            320 => 8,
            368 => 7,
            512 => 5,
            _ => 4, // 640
        }
    }

    pub fn display_resolution(&self) -> (u32, u32) {
        // Visible height comes from the GP1(07) vertical range, not a fixed
        // 240/480: BIOSes/games display fewer lines (e.g. 232) and showing
        // more scans out garbage below the framebuffer.
        let lines = self
            .display_v_range
            .1
            .saturating_sub(self.display_v_range.0) as u32;
        let lines = if lines == 0 { 240 } else { lines.min(256) };
        let h = if self.vres == 480 {
            (lines * 2).min(480)
        } else {
            lines
        };
        (self.hres as u32, h)
    }

    pub fn display_vram_start(&self) -> (u32, u32) {
        (
            self.display_vram_start.0 as u32,
            self.display_vram_start.1 as u32,
        )
    }

    pub fn is_24bit(&self) -> bool {
        self.color_24bit
    }

    /// Called by the system once per vblank.
    pub fn vblank(&mut self) {
        self.odd_frame = !self.odd_frame;
        self.frame_count += 1;
        self.capture_frame();
    }

    /// Latch the display area into [`Frame`].
    ///
    /// In 480i the hardware scans out one field per vblank; mirroring that,
    /// only the rows of the current field are refreshed and the other
    /// field's rows are kept from the previous capture (weave). This keeps
    /// games that render per-field from flickering at half brightness.
    fn capture_frame(&mut self) {
        let (w, h) = self.display_resolution();
        let (sx, sy) = self.display_vram_start();
        let stride = if self.color_24bit {
            (w * 3).div_ceil(2)
        } else {
            w
        };
        let total = (stride * h) as usize;
        let full = self.frame.pixels.len() != total
            || self.frame.width != w
            || self.frame.stride != stride
            || self.frame.is_24bit != self.color_24bit
            || !(self.interlaced && self.vres == 480);
        self.frame.width = w;
        self.frame.height = h;
        self.frame.stride = stride;
        self.frame.is_24bit = self.color_24bit;
        self.frame.enabled = !self.display_disabled;
        self.frame.pixels.resize(total, 0);
        let field = self.odd_frame as u32;
        for y in 0..h {
            if !full && y & 1 != field {
                continue;
            }
            let row = (((sy + y) & 0x1ff) as usize) * VRAM_WIDTH;
            let dst = (y * stride) as usize;
            for x in 0..stride {
                self.frame.pixels[dst + x as usize] =
                    self.vram[row + (((sx + x) & 0x3ff) as usize)];
            }
        }
    }

    // --- Register interface -------------------------------------------

    pub fn status(&self) -> u32 {
        let hr = match self.hres {
            256 => 0u32,
            320 => 1,
            512 => 2,
            640 => 3,
            _ => 1, // 368 handled via bit 16
        };
        let mut s = 0u32;
        s |= self.tex_page_x as u32 & 0xf;
        s |= (self.tex_page_y as u32 & 1) << 4;
        s |= (self.semi_mode as u32) << 5;
        s |= (self.tex_depth as u32) << 7;
        s |= (self.dither as u32) << 9;
        s |= (self.draw_to_display as u32) << 10;
        s |= (self.force_mask as u32) << 11;
        s |= (self.check_mask as u32) << 12;
        // Bit 13: interlace field (reads 1 while interlace is off)
        s |= ((!self.interlaced || self.odd_frame) as u32) << 13;
        s |= (self.tex_disable as u32) << 15;
        s |= ((self.hres == 368) as u32) << 16;
        s |= hr << 17;
        s |= ((self.vres == 480) as u32) << 19;
        s |= (self.pal_mode as u32) << 20;
        s |= (self.color_24bit as u32) << 21;
        s |= (self.interlaced as u32) << 22;
        s |= (self.display_disabled as u32) << 23;
        s |= (self.irq_pending as u32) << 24;
        // Ready flags: commands run instantly, so always ready; "send VRAM"
        // only while a read transfer has buffered data.
        s |= 1 << 26;
        s |= ((!self.read_queue.is_empty()) as u32) << 27;
        s |= 1 << 28;
        s |= (self.dma_direction as u32) << 29;
        // DMA request (bit 25) mirrors the selected ready flag
        let dreq = match self.dma_direction {
            0 => 0,
            1 => 1, // FIFO ready (always, we have no real FIFO depth)
            2 => (s >> 28) & 1,
            _ => (s >> 27) & 1,
        };
        s |= dreq << 25;
        // Odd/even flag: toggles per field/frame. Real hardware also clears
        // it during vblank and toggles per scanline in 240p; line-accurate
        // GPU timing will refine this. It must keep toggling in interlaced
        // mode — the shell spins waiting for it to change.
        s |= (self.odd_frame as u32) << 31;
        s
    }

    pub fn gpuread(&mut self) -> u32 {
        self.read_queue.pop_front().unwrap_or(0)
    }

    /// GP0: drawing / VRAM-access command stream (also fed by DMA ch2).
    pub fn gp0(&mut self, word: u32) {
        match self.mode {
            Gp0Mode::ImageLoad { x, y, w, h, cur } => {
                let total = w as u32 * h as u32;
                for i in 0..2 {
                    let ofs = cur + i;
                    if ofs < total {
                        let px = (word >> (16 * i)) as u16;
                        let dx = (x + (ofs % w as u32) as u16) & 0x3ff;
                        let dy = (y + (ofs / w as u32) as u16) & 0x1ff;
                        self.write_vram(dx as i32, dy as i32, px, false);
                    }
                }
                self.mode = if cur + 2 >= total {
                    Gp0Mode::Command
                } else {
                    Gp0Mode::ImageLoad {
                        x,
                        y,
                        w,
                        h,
                        cur: cur + 2,
                    }
                };
                return;
            }
            Gp0Mode::Command => {}
        }

        if self.in_polyline {
            self.gp0_polyline(word);
            return;
        }

        if self.fifo.is_empty() {
            self.words_needed = Self::command_length((word >> 24) as u8);
        }
        self.fifo.push(word);
        if self.fifo.len() < self.words_needed {
            return;
        }

        let op = (self.fifo[0] >> 24) as u8;
        let cmd: Vec<u32> = self.fifo.drain(..).collect();
        self.execute(op, &cmd);
    }

    /// Continuation words of a polyline: one segment per (color,) vertex.
    fn gp0_polyline(&mut self, word: u32) {
        if word & 0xf000_f000 == 0x5000_5000 {
            self.in_polyline = false;
            self.fifo.clear();
            return;
        }
        self.fifo.push(word);
        let gouraud = self.poly_cmd & 0x1000_0000 != 0;
        let need = if gouraud { 2 } else { 1 };
        if self.fifo.len() < need {
            return;
        }
        let (c2, v2) = if gouraud {
            (self.fifo[0], self.fifo[1])
        } else {
            (self.poly_cmd, self.fifo[0])
        };
        self.fifo.clear();
        let head = (self.poly_cmd & 0xff00_0000) | (self.poly_last_color & 0x00ff_ffff);
        let cmd = if gouraud {
            vec![head, self.poly_last_vertex, c2, v2]
        } else {
            vec![head, self.poly_last_vertex, v2]
        };
        if self.log_commands {
            debug!(target: "psx_core::gpu::cmd",
                   "[f{}] GP0(..) polyline seg {:?}->{:?}",
                   self.frame_count, vertex(self.poly_last_vertex), vertex(v2));
        }
        self.draw_line_command(&cmd);
        self.poly_last_color = c2 & 0x00ff_ffff;
        self.poly_last_vertex = v2;
    }

    /// Words (including the command word) each GP0 opcode consumes.
    fn command_length(op: u8) -> usize {
        match op {
            0x02 => 3,
            0x20..=0x27 => {
                if op & 0x04 != 0 { 7 } else { 4 } // textured tri : flat tri
            }
            0x28..=0x2f => {
                if op & 0x04 != 0 {
                    9
                } else {
                    5
                }
            }
            0x30..=0x37 => {
                if op & 0x04 != 0 {
                    9
                } else {
                    6
                }
            }
            0x38..=0x3f => {
                if op & 0x04 != 0 {
                    12
                } else {
                    8
                }
            }
            0x40..=0x5f => {
                let gouraud = op & 0x10 != 0;
                if gouraud { 4 } else { 3 }
            }
            0x60..=0x7f => {
                let textured = op & 0x04 != 0;
                let variable = op & 0x18 == 0;
                1 + 1 + textured as usize + variable as usize
            }
            0x80..=0x9f => 4,
            0xa0..=0xbf => 3,
            0xc0..=0xdf => 3,
            _ => 1,
        }
    }

    fn execute(&mut self, op: u8, cmd: &[u32]) {
        if self.log_commands {
            self.log_command(op, cmd);
        }
        match op {
            0x00 => {} // nop
            0x01 => {} // clear texture cache (no cache yet)
            0x02 => self.fill_rect(cmd),
            0x1f => self.irq_pending = true,
            0x20..=0x3f => self.draw_polygon_command(op, cmd),
            0x40..=0x5f => {
                self.draw_line_command(cmd);
                if op & 0x08 != 0 {
                    // Polyline: subsequent words extend from the tail vertex
                    self.in_polyline = true;
                    self.poly_cmd = cmd[0];
                    let gouraud = op & 0x10 != 0;
                    self.poly_last_color = if gouraud { cmd[2] } else { cmd[0] } & 0x00ff_ffff;
                    self.poly_last_vertex = *cmd.last().unwrap();
                }
            }
            0x60..=0x7f => self.draw_rect_command(op, cmd),
            0x80..=0x9f => self.vram_copy(cmd),
            0xa0..=0xbf => {
                let (x, y) = unpack_coord(cmd[1]);
                let (w, h) = unpack_size(cmd[2]);
                trace!(target: "psx_core::gpu", "CPU->VRAM {w}x{h} at ({x},{y})");
                self.mode = Gp0Mode::ImageLoad { x, y, w, h, cur: 0 };
            }
            0xc0..=0xdf => self.vram_read(cmd),
            0xe1 => {
                self.tex_page_x = (cmd[0] & 0xf) as u16;
                self.tex_page_y = ((cmd[0] >> 4) & 1) as u16;
                self.semi_mode = ((cmd[0] >> 5) & 3) as u8;
                self.tex_depth = match (cmd[0] >> 7) & 3 {
                    0 => TexDepth::T4Bit,
                    1 => TexDepth::T8Bit,
                    _ => TexDepth::T15Bit,
                };
                self.dither = cmd[0] & (1 << 9) != 0;
                self.draw_to_display = cmd[0] & (1 << 10) != 0;
                self.tex_disable = cmd[0] & (1 << 11) != 0;
                self.rect_tex_flip = (cmd[0] & (1 << 12) != 0, cmd[0] & (1 << 13) != 0);
            }
            0xe2 => {
                self.tex_win_mask = ((cmd[0] & 0x1f) as u8, ((cmd[0] >> 5) & 0x1f) as u8);
                self.tex_win_off = (((cmd[0] >> 10) & 0x1f) as u8, ((cmd[0] >> 15) & 0x1f) as u8);
            }
            0xe3 => {
                self.draw_min = ((cmd[0] & 0x3ff) as i32, ((cmd[0] >> 10) & 0x1ff) as i32);
            }
            0xe4 => {
                self.draw_max = ((cmd[0] & 0x3ff) as i32, ((cmd[0] >> 10) & 0x1ff) as i32);
            }
            0xe5 => {
                // Signed 11-bit offsets
                let x = ((cmd[0] & 0x7ff) << 21) as i32 >> 21;
                let y = (((cmd[0] >> 11) & 0x7ff) << 21) as i32 >> 21;
                self.draw_offset = (x, y);
            }
            0xe6 => {
                self.force_mask = cmd[0] & 1 != 0;
                self.check_mask = cmd[0] & 2 != 0;
            }
            _ => warn!(target: "psx_core::gpu", "unhandled GP0 {op:#04x}"),
        }
    }

    /// Decode one executed GP0 command into a human-readable log line.
    /// Nops are skipped to keep the stream readable.
    fn log_command(&self, op: u8, cmd: &[u32]) {
        use std::fmt::Write;
        let desc = match op {
            0x00 => return,
            0x01 => "clear texture cache".to_string(),
            0x02 => {
                let (x, y) = unpack_coord(cmd[1]);
                let (w, h) = ((cmd[2] & 0x3ff) as u16, ((cmd[2] >> 16) & 0x1ff) as u16);
                format!(
                    "fill rect {w}x{h} at ({x},{y}) color={:06x}",
                    cmd[0] & 0xff_ffff
                )
            }
            0x1f => "irq request".to_string(),
            0x20..=0x3f => {
                let quad = op & 0x08 != 0;
                let gouraud = op & 0x10 != 0;
                let textured = op & 0x04 != 0;
                let stride = 1 + textured as usize + gouraud as usize;
                let mut verts = String::new();
                for i in 0..if quad { 4 } else { 3 } {
                    let (x, y) = vertex(cmd[1 + i * stride]);
                    let _ = write!(verts, "({x},{y})");
                }
                format!(
                    "{} {}{}{}{}verts={verts} color={:06x}",
                    if quad { "quad" } else { "tri" },
                    if gouraud { "gouraud " } else { "flat " },
                    if textured { "textured " } else { "" },
                    if op & 0x02 != 0 { "semi " } else { "" },
                    if textured && op & 0x01 != 0 {
                        "raw "
                    } else {
                        ""
                    },
                    cmd[0] & 0xff_ffff,
                )
            }
            0x40..=0x5f => {
                let gouraud = op & 0x10 != 0;
                let v1 = cmd[if gouraud { 3 } else { 2 }];
                format!(
                    "line{}{} {:?}->{:?} color={:06x}",
                    if gouraud { " gouraud" } else { "" },
                    if op & 0x08 != 0 {
                        " (polyline start)"
                    } else {
                        ""
                    },
                    vertex(cmd[1]),
                    vertex(v1),
                    cmd[0] & 0xff_ffff,
                )
            }
            0x60..=0x7f => {
                let textured = op & 0x04 != 0;
                let (w, h) = match (op >> 3) & 3 {
                    0 => {
                        let s = cmd[2 + textured as usize];
                        ((s & 0x3ff) as u16, ((s >> 16) & 0x1ff) as u16)
                    }
                    1 => (1, 1),
                    2 => (8, 8),
                    _ => (16, 16),
                };
                format!(
                    "rect{} {w}x{h} at {:?} color={:06x}",
                    if textured { " textured" } else { "" },
                    vertex(cmd[1]),
                    cmd[0] & 0xff_ffff,
                )
            }
            0x80..=0x9f => {
                let (sx, sy) = unpack_coord(cmd[1]);
                let (dx, dy) = unpack_coord(cmd[2]);
                let (w, h) = unpack_size(cmd[3]);
                format!("vram copy {w}x{h} ({sx},{sy})->({dx},{dy})")
            }
            0xa0..=0xbf => {
                let (x, y) = unpack_coord(cmd[1]);
                let (w, h) = unpack_size(cmd[2]);
                format!("cpu->vram {w}x{h} at ({x},{y})")
            }
            0xc0..=0xdf => {
                let (x, y) = unpack_coord(cmd[1]);
                let (w, h) = unpack_size(cmd[2]);
                format!("vram->cpu {w}x{h} at ({x},{y})")
            }
            0xe1 => format!("texpage {:06x}", cmd[0] & 0xff_ffff),
            0xe2 => format!("tex window {:06x}", cmd[0] & 0xff_ffff),
            0xe3 => format!(
                "draw area min ({},{})",
                cmd[0] & 0x3ff,
                (cmd[0] >> 10) & 0x1ff
            ),
            0xe4 => format!(
                "draw area max ({},{})",
                cmd[0] & 0x3ff,
                (cmd[0] >> 10) & 0x1ff
            ),
            0xe5 => {
                let x = ((cmd[0] & 0x7ff) << 21) as i32 >> 21;
                let y = (((cmd[0] >> 11) & 0x7ff) << 21) as i32 >> 21;
                format!("draw offset ({x},{y})")
            }
            0xe6 => format!("mask force={} check={}", cmd[0] & 1, (cmd[0] >> 1) & 1),
            _ => format!("unknown {:08x}", cmd[0]),
        };
        debug!(target: "psx_core::gpu::cmd", "[f{}] GP0({op:02x}) {desc}", self.frame_count);
    }

    /// GP1: display control.
    pub fn gp1(&mut self, word: u32) {
        let op = word >> 24;
        if self.log_commands {
            let name = match op {
                0x00 => "reset",
                0x01 => "reset command buffer",
                0x02 => "ack irq",
                0x03 => "display enable",
                0x04 => "dma direction",
                0x05 => "display vram start",
                0x06 => "display h-range",
                0x07 => "display v-range",
                0x08 => "display mode",
                0x10..=0x1f => "get info",
                _ => "unknown",
            };
            debug!(target: "psx_core::gpu::cmd",
                   "[f{}] GP1({op:02x}) {name} {:06x}", self.frame_count, word & 0xff_ffff);
        }
        match op {
            0x00 => {
                // Full state reset; VRAM contents, the command-log switch and
                // the frame counter survive a GP1 reset
                let vram = std::mem::take(&mut self.vram);
                let (log, frames) = (self.log_commands, self.frame_count);
                *self = Gpu::new();
                self.vram = vram;
                self.log_commands = log;
                self.frame_count = frames;
            }
            0x01 => {
                self.fifo.clear();
                self.in_polyline = false;
                self.mode = Gp0Mode::Command;
            }
            0x02 => self.irq_pending = false,
            0x03 => self.display_disabled = word & 1 != 0,
            0x04 => self.dma_direction = (word & 3) as u8,
            0x05 => {
                self.display_vram_start = ((word & 0x3fe) as u16, ((word >> 10) & 0x1ff) as u16);
            }
            0x06 => {
                self.display_h_range = ((word & 0xfff) as u16, ((word >> 12) & 0xfff) as u16);
            }
            0x07 => {
                self.display_v_range = ((word & 0x3ff) as u16, ((word >> 10) & 0x3ff) as u16);
            }
            0x08 => {
                self.hres = if word & (1 << 6) != 0 {
                    368
                } else {
                    match word & 3 {
                        0 => 256,
                        1 => 320,
                        2 => 512,
                        _ => 640,
                    }
                };
                self.interlaced = word & (1 << 5) != 0;
                self.vres = if word & 4 != 0 && self.interlaced {
                    480
                } else {
                    240
                };
                self.pal_mode = word & 8 != 0;
                self.color_24bit = word & 0x10 != 0;
                debug!(target: "psx_core::gpu",
                       "display mode {}x{} {} {}", self.hres, self.vres,
                       if self.pal_mode { "PAL" } else { "NTSC" },
                       if self.color_24bit { "24bit" } else { "15bit" });
            }
            0x10..=0x1f => {
                // Get GPU info; only the commonly used sub-ops
                self.read_queue.clear();
                let v = match word & 0xf {
                    2 => {
                        let (mx, my) = self.tex_win_mask;
                        let (ox, oy) = self.tex_win_off;
                        (mx as u32) | (my as u32) << 5 | (ox as u32) << 10 | (oy as u32) << 15
                    }
                    3 => (self.draw_min.0 as u32) | (self.draw_min.1 as u32) << 10,
                    4 => (self.draw_max.0 as u32) | (self.draw_max.1 as u32) << 10,
                    5 => {
                        (self.draw_offset.0 as u32 & 0x7ff)
                            | (self.draw_offset.1 as u32 & 0x7ff) << 11
                    }
                    7 => 2, // GPU version
                    _ => 0,
                };
                self.read_queue.push_back(v);
            }
            _ => warn!(target: "psx_core::gpu", "unhandled GP1 {op:#04x}"),
        }
    }

    // --- VRAM block operations ----------------------------------------

    fn fill_rect(&mut self, cmd: &[u32]) {
        // Fill ignores draw area/offset and mask; coords are pre-masked
        let c = color_to_5551(cmd[0]);
        let x0 = (cmd[1] & 0x3f0) as usize;
        let y0 = ((cmd[1] >> 16) & 0x1ff) as usize;
        let w = (((cmd[2] & 0x3ff) + 0xf) & !0xf) as usize;
        let h = ((cmd[2] >> 16) & 0x1ff) as usize;
        for y in 0..h {
            let row = ((y0 + y) & 0x1ff) * VRAM_WIDTH;
            for x in 0..w {
                self.vram[row + ((x0 + x) & 0x3ff)] = c;
            }
        }
    }

    fn vram_copy(&mut self, cmd: &[u32]) {
        let (sx, sy) = unpack_coord(cmd[1]);
        let (dx, dy) = unpack_coord(cmd[2]);
        let (w, h) = unpack_size(cmd[3]);
        for y in 0..h {
            for x in 0..w {
                let s = (((sy + y) & 0x1ff) as usize) * VRAM_WIDTH + (((sx + x) & 0x3ff) as usize);
                let px = self.vram[s];
                self.write_vram(
                    ((dx + x) & 0x3ff) as i32,
                    ((dy + y) & 0x1ff) as i32,
                    px,
                    false,
                );
            }
        }
    }

    fn vram_read(&mut self, cmd: &[u32]) {
        let (x, y) = unpack_coord(cmd[1]);
        let (w, h) = unpack_size(cmd[2]);
        trace!(target: "psx_core::gpu", "VRAM->CPU {w}x{h} at ({x},{y})");
        let total = w as u32 * h as u32;
        let mut i = 0;
        while i < total {
            let mut word = 0u32;
            for k in 0..2 {
                let ofs = i + k;
                if ofs < total {
                    let px = (x + (ofs % w as u32) as u16) & 0x3ff;
                    let py = (y + (ofs / w as u32) as u16) & 0x1ff;
                    word |= (self.vram[py as usize * VRAM_WIDTH + px as usize] as u32) << (16 * k);
                }
            }
            self.read_queue.push_back(word);
            i += 2;
        }
    }

    /// Store one pixel honoring the mask-bit settings.
    #[inline]
    fn write_vram(&mut self, x: i32, y: i32, px: u16, semi_handled_mask: bool) {
        let i = y as usize * VRAM_WIDTH + x as usize;
        if self.check_mask && self.vram[i] & 0x8000 != 0 {
            return;
        }
        let force = if self.force_mask && !semi_handled_mask {
            0x8000
        } else {
            0
        };
        self.vram[i] = px | force;
    }
}

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

/// Vertex word as the signed 11-bit coordinates the rasterizer sees.
fn vertex(w: u32) -> (i32, i32) {
    let x = ((w & 0x7ff) << 21) as i32 >> 21;
    let y = (((w >> 16) & 0x7ff) << 21) as i32 >> 21;
    (x, y)
}

/// Vertex-style coordinate word: x in bits 0..10, y in bits 16..25.
fn unpack_coord(w: u32) -> (u16, u16) {
    ((w & 0x3ff) as u16, ((w >> 16) & 0x1ff) as u16)
}

/// Size word for transfers: 0 means maximum (1024 / 512).
fn unpack_size(w: u32) -> (u16, u16) {
    let x = ((w.wrapping_sub(1) & 0x3ff) + 1) as u16;
    let y = (((w >> 16).wrapping_sub(1) & 0x1ff) + 1) as u16;
    (x, y)
}

/// 24-bit command color -> 15-bit VRAM pixel.
fn color_to_5551(c: u32) -> u16 {
    let r = (c & 0xff) >> 3;
    let g = ((c >> 8) & 0xff) >> 3;
    let b = ((c >> 16) & 0xff) >> 3;
    (r | g << 5 | b << 10) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_timing_follows_the_display_mode() {
        let mut gpu = Gpu::new();
        let ntsc = gpu.video_timing();
        assert_eq!(ntsc.lines_per_frame, 263);
        assert_eq!(ntsc.visible_lines, 240);
        // Default window is 2560 of the 3413 video clocks on a scanline
        assert_eq!(ntsc.hblank_cycles, 538);
        assert_eq!(
            ntsc.dotclock,
            (8 * crate::CPU_CLOCK_HZ, NTSC_VIDEO_CLOCK_HZ)
        );

        // GP1(08): 640-pixel PAL
        gpu.gp1(0x0800_000b);
        let pal = gpu.video_timing();
        assert_eq!(pal.lines_per_frame, 314);
        assert_eq!(pal.dotclock, (4 * crate::CPU_CLOCK_HZ, PAL_VIDEO_CLOCK_HZ));
        assert!(pal.cycles_per_frame() > ntsc.cycles_per_frame());
    }

    #[test]
    fn video_timing_takes_the_visible_window_from_the_display_range() {
        let mut gpu = Gpu::new();
        // GP1(07): 232 displayed scanlines
        gpu.gp1(0x0700_0000 | 0x10 | ((0x10 + 232) << 10));
        assert_eq!(gpu.video_timing().visible_lines, 232);
        // A degenerate range keeps the nominal window
        gpu.gp1(0x0700_0000);
        assert_eq!(gpu.video_timing().visible_lines, 240);
    }

    /// Paint every VRAM row with a marker value.
    fn paint(gpu: &mut Gpu, base: u16) {
        for y in 0..VRAM_HEIGHT {
            for x in 0..VRAM_WIDTH {
                gpu.vram[y * VRAM_WIDTH + x] = base + y as u16;
            }
        }
    }

    #[test]
    fn interlaced_frame_weaves_fields_across_vblanks() {
        let mut gpu = Gpu::new();
        gpu.gp1(0x0300_0000); // display on
        gpu.gp1(0x0800_0025); // 320 wide, 480i
        assert_eq!(gpu.display_resolution(), (320, 480));

        paint(&mut gpu, 0);
        gpu.vblank(); // first capture after a mode change is full
        assert!(
            gpu.frame
                .pixels
                .iter()
                .enumerate()
                .all(|(i, &p)| { p == (i / gpu.frame.stride as usize) as u16 })
        );

        // Repaint; the next vblank must refresh only one field
        paint(&mut gpu, 1000);
        gpu.vblank();
        let stride = gpu.frame.stride as usize;
        let updated: Vec<bool> = (0..gpu.frame.height as usize)
            .map(|y| gpu.frame.pixels[y * stride] >= 1000)
            .collect();
        assert!(updated.iter().any(|&u| u) && updated.iter().any(|&u| !u));
        assert!(
            updated.windows(2).all(|w| w[0] != w[1]),
            "fields must alternate"
        );

        // The following vblank fills in the other field
        gpu.vblank();
        assert!((0..gpu.frame.height as usize).all(|y| gpu.frame.pixels[y * stride] >= 1000));
    }

    #[test]
    fn progressive_frame_is_fully_recaptured_each_vblank() {
        let mut gpu = Gpu::new();
        gpu.gp1(0x0300_0000);
        gpu.gp1(0x0800_0001); // 320x240 progressive
        paint(&mut gpu, 0);
        gpu.vblank();
        paint(&mut gpu, 1000);
        gpu.vblank();
        let stride = gpu.frame.stride as usize;
        assert!((0..gpu.frame.height as usize).all(|y| gpu.frame.pixels[y * stride] >= 1000));
    }
}
