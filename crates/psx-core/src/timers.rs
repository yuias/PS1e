//! Root counters (timers 0..2).
//!
//! Lazy catch-up model: counters advance only when their registers are
//! accessed or when the system forces a sync (once per vblank), computing
//! elapsed ticks from the CPU cycle count. Dotclock and hblank sources are
//! approximated with fixed ratios, and so are the blanking windows the
//! synchronization modes gate on: both are derived analytically from the
//! absolute cycle count rather than driven by GPU scanout events.

use crate::CYCLES_PER_FRAME;
use crate::bus::Irq;

/// ~NTSC scanline length in CPU cycles (3413.6 video clocks / 1.5853).
pub const CYCLES_PER_LINE: u64 = 2153;
/// NTSC field: 263 scanlines, 240 of them visible.
pub const LINES_PER_FRAME: u64 = 263;
const VISIBLE_LINES: u64 = 240;
/// Dotclock divider approximation for the common 320-pixel mode.
const DOTCLOCK_DIV: u64 = 5;
/// Blanking part of a scanline (~853 of the 3413.6 video clocks).
const HBLANK_CYCLES: u64 = 538;
/// Vertical blanking: the scanlines past the visible area.
const VBLANK_CYCLES: u64 = (LINES_PER_FRAME - VISIBLE_LINES) * CYCLES_PER_LINE;

/// Cycles in `[0, t)` whose phase falls in the leading `blank` cycles of
/// each `period`. Blanking is modelled at the start of the period so that
/// phase 0 of a frame is the vblank edge the system raises IRQ0 on.
fn blanking_before(t: u64, period: u64, blank: u64) -> u64 {
    (t / period) * blank + (t % period).min(blank)
}

/// Cycles in `[from, to)` spent blanking.
fn blanking_within(from: u64, to: u64, period: u64, blank: u64) -> u64 {
    blanking_before(to, period, blank) - blanking_before(from, period, blank)
}

#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Timer {
    counter: u32,
    mode: u32,
    target: u32,
    /// CPU cycle of the last catch-up.
    last_sync: u64,
    /// Sub-tick remainder in CPU cycles for divided clock sources.
    frac: u64,
    /// Sync mode 3 only: the awaited blanking edge has been seen, so the
    /// counter has switched to free run.
    sync_started: bool,
    /// One-shot mode only: an IRQ condition has already been served, so
    /// further ones are suppressed until the mode register is rewritten.
    irq_fired: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
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
                // Writing mode resets the counter and re-arms the IRQ (bit 10
                // reads back 1 = not requested)
                self.t[idx].mode = (val & 0x3ff) | (1 << 10);
                self.t[idx].counter = 0;
                self.t[idx].frac = 0;
                self.t[idx].sync_started = false;
                self.t[idx].irq_fired = false;
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
        let from = std::mem::replace(&mut self.t[idx].last_sync, now);
        if now <= from {
            return;
        }
        let mode = self.t[idx].mode;
        if mode & 1 == 0 {
            self.advance(idx, now - from, irq);
            return;
        }

        let sync = (mode >> 1) & 3;
        if idx == 2 {
            // Counter 2 either stops dead (0/3) or runs free (1/2)
            if sync == 1 || sync == 2 {
                self.advance(idx, now - from, irq);
            }
            return;
        }

        // Counters 0 and 1 gate on hblank and vblank respectively. Walk the
        // interval one blanking edge at a time: the resetting modes have to
        // observe each edge, not just the interval as a whole.
        let (period, blank) = if idx == 0 {
            (CYCLES_PER_LINE, HBLANK_CYCLES)
        } else {
            (CYCLES_PER_FRAME, VBLANK_CYCLES)
        };
        let mut cursor = from;
        while cursor < now {
            let edge = (cursor / period + 1) * period;
            let end = edge.min(now);
            let cycles = match sync {
                // Pause during blanking
                0 => (end - cursor) - blanking_within(cursor, end, period, blank),
                // Reset on the edge, and only count while blanking
                2 => blanking_within(cursor, end, period, blank),
                // Paused until the first edge, free-running after it
                3 if !self.t[idx].sync_started => 0,
                _ => end - cursor,
            };
            self.advance(idx, cycles, irq);
            cursor = end;
            if cursor == edge {
                match sync {
                    1 | 2 => {
                        self.t[idx].counter = 0;
                        self.t[idx].frac = 0;
                    }
                    3 => self.t[idx].sync_started = true,
                    _ => {}
                }
            }
        }
    }

    /// Apply `cycles` of counting time, raising the timer's IRQ when the
    /// target or an overflow is crossed.
    fn advance(&mut self, idx: usize, cycles: u64, irq: &mut Irq) {
        let t = &mut self.t[idx];
        let elapsed = cycles + t.frac;

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
            self.request_irq(idx, irq);
        }
    }

    /// Serve an IRQ condition, honouring the one-shot (bit 6) and
    /// pulse/toggle (bit 7) mode bits.
    fn request_irq(&mut self, idx: usize, irq: &mut Irq) {
        let t = &mut self.t[idx];
        let repeat = t.mode & (1 << 6) != 0;
        if !repeat && t.irq_fired {
            return;
        }
        t.irq_fired = true;

        let raise = if t.mode & (1 << 7) != 0 {
            // Toggle: bit 10 inverts per condition, and the line is driven
            // on its 1 -> 0 edge — so a repeating timer fires every other
            // condition. One-shot leaves the bit low, having toggled once.
            let was_idle = t.mode & (1 << 10) != 0;
            t.mode ^= 1 << 10;
            was_idle
        } else {
            // Pulse: bit 10 dips low for a few clocks, far too briefly to
            // observe through the lazy catch-up, so leave it set.
            true
        };
        if raise {
            irq.raise(4 + idx as u32);
        }
    }
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0_COUNT: u32 = 0x1f80_1100;
    const T0_MODE: u32 = 0x1f80_1104;
    const T1_COUNT: u32 = 0x1f80_1110;
    const T1_MODE: u32 = 0x1f80_1114;
    const T2_COUNT: u32 = 0x1f80_1120;
    const T2_MODE: u32 = 0x1f80_1124;
    const T2_TARGET: u32 = 0x1f80_1128;

    /// Write mode at cycle 0, run to `now`, and read the counter back.
    fn run(mode_addr: u32, count_addr: u32, mode: u32, now: u64) -> u32 {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        timers.write(mode_addr, mode, 0, &mut irq);
        timers.read(count_addr, now, &mut irq)
    }

    #[test]
    fn sync_disabled_counts_every_cycle() {
        assert_eq!(run(T0_MODE, T0_COUNT, 0, 1000), 1000);
    }

    #[test]
    fn counter2_sync_mode_0_and_3_stop() {
        for sync in [0, 3] {
            assert_eq!(run(T2_MODE, T2_COUNT, 1 | (sync << 1), 1000), 0);
        }
    }

    #[test]
    fn counter2_sync_mode_1_and_2_run_free() {
        for sync in [1, 2] {
            assert_eq!(run(T2_MODE, T2_COUNT, 1 | (sync << 1), 1000), 1000);
        }
    }

    #[test]
    fn counter0_pauses_during_hblank() {
        // One full line: only the visible part of it counts
        let visible = (CYCLES_PER_LINE - HBLANK_CYCLES) as u32;
        assert_eq!(run(T0_MODE, T0_COUNT, 1, CYCLES_PER_LINE), visible);
    }

    #[test]
    fn counter0_counts_only_during_hblank_in_mode_2() {
        // Mode 2 also resets on the edge, so only the second line's blanking
        // survives at the two-line mark
        let mode = 1 | (2 << 1);
        assert_eq!(
            run(T0_MODE, T0_COUNT, mode, 2 * CYCLES_PER_LINE - 1),
            HBLANK_CYCLES as u32
        );
    }

    #[test]
    fn counter1_resets_at_the_vblank_edge() {
        let mode = 1 | (1 << 1);
        let past_edge = 100;
        assert_eq!(
            run(T1_MODE, T1_COUNT, mode, CYCLES_PER_FRAME + past_edge),
            past_edge as u32
        );
    }

    #[test]
    fn counter1_mode_3_waits_for_the_first_vblank() {
        let mode = 1 | (3 << 1);
        // Still paused before the edge
        assert_eq!(run(T1_MODE, T1_COUNT, mode, CYCLES_PER_FRAME - 1), 0);
        // Free-running after it, counting only the cycles past the edge
        assert_eq!(run(T1_MODE, T1_COUNT, mode, CYCLES_PER_FRAME + 500), 500);
    }

    #[test]
    fn gated_counting_is_independent_of_how_often_it_is_polled() {
        let mut coarse = Timers::new();
        let mut fine = Timers::new();
        let mut irq = Irq::default();
        coarse.write(T0_MODE, 1, 0, &mut irq);
        fine.write(T0_MODE, 1, 0, &mut irq);
        let end = 3 * CYCLES_PER_LINE;
        for now in (1..=end).step_by(97) {
            fine.read(T0_COUNT, now, &mut irq);
        }
        assert_eq!(
            coarse.read(T0_COUNT, end, &mut irq),
            fine.read(T0_COUNT, end, &mut irq)
        );
    }

    /// Timer 2 wrapping at a target of 99, so one IRQ condition occurs per
    /// 100 cycles. `extra` adds the IRQ mode bits under test.
    fn wrapping_timer2(extra: u32) -> (Timers, Irq) {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        // Reset at target | IRQ at target
        timers.write(T2_MODE, (1 << 3) | (1 << 4) | extra, 0, &mut irq);
        timers.write(T2_TARGET, 99, 0, &mut irq);
        (timers, irq)
    }

    /// Run to the `n`-th target wrap and report whether IRQ2 was raised.
    fn wrap_raises_irq(timers: &mut Timers, irq: &mut Irq, n: u64) -> bool {
        irq.stat = 0;
        timers.read(T2_COUNT, n * 100, irq);
        irq.stat & (1 << 6) != 0
    }

    #[test]
    fn one_shot_irq_is_served_only_once() {
        let (mut timers, mut irq) = wrapping_timer2(0);
        assert!(wrap_raises_irq(&mut timers, &mut irq, 1));
        assert!(!wrap_raises_irq(&mut timers, &mut irq, 2));
        // Rewriting the mode re-arms it
        timers.write(T2_MODE, (1 << 3) | (1 << 4), 200, &mut irq);
        timers.write(T2_TARGET, 99, 200, &mut irq);
        assert!(wrap_raises_irq(&mut timers, &mut irq, 3));
    }

    #[test]
    fn repeat_irq_is_served_every_time() {
        let (mut timers, mut irq) = wrapping_timer2(1 << 6);
        for n in 1..=3 {
            assert!(wrap_raises_irq(&mut timers, &mut irq, n), "wrap {n}");
        }
    }

    #[test]
    fn toggle_mode_drives_the_line_every_second_condition() {
        let (mut timers, mut irq) = wrapping_timer2((1 << 6) | (1 << 7));
        assert!(wrap_raises_irq(&mut timers, &mut irq, 1));
        assert!(!wrap_raises_irq(&mut timers, &mut irq, 2));
        assert!(wrap_raises_irq(&mut timers, &mut irq, 3));
    }

    #[test]
    fn toggle_mode_inverts_the_request_bit() {
        let (mut timers, mut irq) = wrapping_timer2((1 << 6) | (1 << 7));
        assert_eq!(timers.read(T2_MODE, 0, &mut irq) & (1 << 10), 1 << 10);
        timers.read(T2_COUNT, 100, &mut irq);
        assert_eq!(timers.read(T2_MODE, 100, &mut irq) & (1 << 10), 0);
        timers.read(T2_COUNT, 200, &mut irq);
        assert_eq!(timers.read(T2_MODE, 200, &mut irq) & (1 << 10), 1 << 10);
    }

    #[test]
    fn pulse_mode_leaves_the_request_bit_set() {
        let (mut timers, mut irq) = wrapping_timer2(1 << 6);
        timers.read(T2_COUNT, 100, &mut irq);
        assert_eq!(timers.read(T2_MODE, 100, &mut irq) & (1 << 10), 1 << 10);
    }

    #[test]
    fn target_irq_still_fires_while_synchronized() {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        // Sync mode 0 (pause during hblank), IRQ on target
        timers.write(T0_MODE, 1 | (1 << 4), 0, &mut irq);
        timers.write(T0_COUNT + 8, 100, 0, &mut irq);
        timers.sync_all(1000, &mut irq);
        assert_eq!(irq.stat & (1 << 4), 1 << 4);
    }
}
