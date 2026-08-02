//! Root counters (timers 0..2).
//!
//! Lazy catch-up model: counters advance only when their registers are
//! accessed or when the system forces a sync (once per vblank), computing
//! elapsed ticks from the CPU cycle count. Dotclock and hblank sources are
//! approximated with fixed ratios for now; sync modes (mode bit 0) are not
//! implemented yet.

use crate::bus::Irq;
use tracing::debug;

/// ~NTSC scanline length in CPU cycles (3413.6 video clocks / 1.5853).
pub const CYCLES_PER_LINE: u64 = 2153;
/// Dotclock divider approximation for the common 320-pixel mode.
const DOTCLOCK_DIV: u64 = 5;

#[derive(Default, Clone, Copy)]
struct Timer {
    counter: u32,
    mode: u32,
    target: u32,
    /// CPU cycle of the last catch-up.
    last_sync: u64,
    /// Sub-tick remainder in CPU cycles for divided clock sources.
    frac: u64,
}

pub struct Timers {
    t: [Timer; 3],
}

impl Timers {
    pub fn new() -> Self {
        Self {
            t: [Timer::default(); 3],
        }
    }

    pub fn read(&mut self, p: u32, now: u64, irq: &mut Irq) -> u32 {
        let idx = ((p - 0x1f80_1100) >> 4) as usize;
        self.catch_up(idx, now, irq);
        match p & 0xf {
            0x0 => self.t[idx].counter,
            0x4 => {
                // Bits 11/12 (reached target/overflow) clear on read
                let v = self.t[idx].mode;
                self.t[idx].mode &= !(0x1800);
                v
            }
            0x8 => self.t[idx].target,
            _ => 0,
        }
    }

    pub fn write(&mut self, p: u32, val: u32, now: u64, irq: &mut Irq) {
        let idx = ((p - 0x1f80_1100) >> 4) as usize;
        self.catch_up(idx, now, irq);
        match p & 0xf {
            0x0 => self.t[idx].counter = val & 0xffff,
            0x4 => {
                if val & 1 != 0 {
                    debug!(target: "psx_core::timers", "timer{idx} sync mode not implemented");
                }
                // Writing mode resets the counter and re-arms the IRQ (bit 10
                // reads back 1 = not requested)
                self.t[idx].mode = (val & 0x3ff) | (1 << 10);
                self.t[idx].counter = 0;
                self.t[idx].frac = 0;
            }
            0x8 => self.t[idx].target = val & 0xffff,
            _ => {}
        }
    }

    /// Advance all timers (called once per vblank so IRQs cannot lag by
    /// more than a frame even without register accesses).
    pub fn sync_all(&mut self, now: u64, irq: &mut Irq) {
        for idx in 0..3 {
            self.catch_up(idx, now, irq);
        }
    }

    fn catch_up(&mut self, idx: usize, now: u64, irq: &mut Irq) {
        let t = &mut self.t[idx];
        let elapsed = now.saturating_sub(t.last_sync) + t.frac;
        t.last_sync = now;

        // CPU cycles per timer tick for the selected clock source
        let source = (t.mode >> 8) & 3;
        let div = match (idx, source) {
            (0, 1 | 3) => DOTCLOCK_DIV,
            (1, 1 | 3) => CYCLES_PER_LINE,
            (2, 2 | 3) => 8,
            _ => 1,
        };
        let ticks = elapsed / div;
        t.frac = elapsed % div;
        if ticks == 0 {
            return;
        }

        let target = t.target & 0xffff;
        let mut counter = t.counter as u64 + ticks;
        let mut fire = false;

        if counter > target as u64 && t.counter <= target {
            t.mode |= 1 << 11; // reached target
            if t.mode & (1 << 4) != 0 {
                fire = true;
            }
            if t.mode & (1 << 3) != 0 {
                // Reset-on-target: wrap within [0, target]
                counter %= target as u64 + 1;
            }
        }
        if counter > 0xffff {
            t.mode |= 1 << 12; // overflow
            if t.mode & (1 << 5) != 0 {
                fire = true;
            }
            counter &= 0xffff;
        }
        t.counter = counter as u32 & 0xffff;

        if fire {
            t.mode &= !(1 << 10); // IRQ requested (active low)
            irq.raise(4 + idx as u32);
        }
    }
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}
