//! Root counters (timers 0..2).
//!
//! Lazy catch-up model: counters advance only when their registers are
//! accessed, the system forces a sync (once per vblank), or the scheduler
//! wakes a timer at [`Timers::next_deadline`], computing elapsed ticks from
//! the CPU cycle count. The blanking windows the synchronization modes gate
//! on are derived analytically from the current [`VideoTiming`] and the
//! cycle the field started at, rather than driven by GPU scanout events.
//! `next_deadline` gives a conservative lower bound on the next IRQ-relevant
//! crossing so a scheduled wake-up raises the IRQ at the end of the
//! instruction that reaches it, even when the program never touches the
//! timer's registers; the exact
//! accounting still happens in [`Timers::catch_up`], which the wake-up
//! calls.

use crate::bus::Irq;
use crate::gpu::VideoTiming;

/// Cycles in `[origin, t)` whose phase falls in the leading `blank` cycles
/// of each `period`. Blanking sits at the start of the period so that phase
/// 0 of a field is the vblank edge the system raises IRQ0 on.
fn blanking_before(t: u64, origin: u64, period: u64, blank: u64) -> u64 {
    let d = t.saturating_sub(origin);
    (d / period) * blank + (d % period).min(blank)
}

/// Cycles in `[from, to)` spent blanking.
fn blanking_within(from: u64, to: u64, origin: u64, period: u64, blank: u64) -> u64 {
    blanking_before(to, origin, period, blank) - blanking_before(from, origin, period, blank)
}

/// Clock source as "`num` CPU cycles yield `den` ticks". A ratio, because
/// the dotclock never divides the CPU clock evenly.
fn tick_ratio(idx: usize, mode: u32, timing: VideoTiming) -> (u64, u64) {
    let source = (mode >> 8) & 3;
    match (idx, source) {
        (0, 1 | 3) => timing.dotclock,
        (1, 1 | 3) => (timing.cycles_per_line, 1),
        (2, 2 | 3) => (8, 1),
        _ => (1, 1),
    }
}

/// Period and leading blanking window counters 0/1 gate every sync mode on:
/// a line for counter 0, a field for counter 1.
fn period_blank(idx: usize, timing: VideoTiming) -> (u64, u64) {
    if idx == 0 {
        (timing.cycles_per_line, timing.hblank_cycles)
    } else {
        (timing.cycles_per_frame(), timing.vblank_cycles())
    }
}

/// Slack on a reset-on-edge sync mode's reachability cap. The field origin
/// is taken at the end of the instruction that crossed vblank, so the
/// period straddling it runs longer than a bare period by that
/// instruction's overshoot and can deliver a few more ticks than the cap
/// predicts. The pad covers ordinary bus wait states with room to spare; a
/// DMA stall can overshoot further, and then a target just past the cap
/// fires at the next vblank's catch-up rather than on time. Padding more
/// only adds harmless early wake-ups, but it would also start treating
/// genuinely unreachable targets as reachable.
const EDGE_OVERSHOOT_MARGIN_CYCLES: u64 = 2048;

#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Timer {
    counter: u32,
    mode: u32,
    target: u32,
    /// CPU cycle of the last catch-up.
    last_sync: u64,
    /// Sub-tick remainder for divided clock sources, in CPU cycles scaled
    /// by the clock source's tick numerator.
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
    /// CPU cycle the current field started at. Blanking phase is measured
    /// from here rather than from cycle zero: the field length changes with
    /// the display region, so vblank edges are not multiples of a period.
    frame_origin: u64,
}

impl Timers {
    pub fn new() -> Self {
        Self {
            t: [Timer::default(); 3],
            frame_origin: 0,
        }
    }

    pub fn read(&mut self, p: u32, now: u64, timing: VideoTiming, irq: &mut Irq) -> u32 {
        let idx = ((p - 0x1f80_1100) >> 4) as usize;
        self.catch_up(idx, now, timing, irq);
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

    pub fn write(&mut self, p: u32, val: u32, now: u64, timing: VideoTiming, irq: &mut Irq) {
        let idx = ((p - 0x1f80_1100) >> 4) as usize;
        self.catch_up(idx, now, timing, irq);
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

    /// Advance all timers to close out the current field before
    /// [`Timers::set_frame_origin`] moves the boundary blanking phase is
    /// measured from, so `catch_up` applies each origin only to the interval
    /// it was in effect for. Also the fallback for a crossing
    /// [`Timers::next_deadline`] treats as unreachable (see
    /// `EDGE_OVERSHOOT_MARGIN_CYCLES`).
    pub fn sync_all(&mut self, now: u64, timing: VideoTiming, irq: &mut Irq) {
        for idx in 0..3 {
            self.catch_up(idx, now, timing, irq);
        }
    }

    /// Start a new field at `cycle`. Call after [`Timers::sync_all`] has
    /// closed out the field that just ended.
    pub fn set_frame_origin(&mut self, cycle: u64) {
        self.frame_origin = cycle;
    }

    /// Lower bound on the cycle of timer `idx`'s next IRQ-relevant crossing
    /// (target or overflow), computed as if the counter ran ungated at its
    /// clock ratio from `last_sync`. Gating pauses and reset-on-edge modes
    /// only ever delay a crossing relative to that ungated model, so the
    /// true crossing is never earlier than this estimate; the caller
    /// re-arms from [`Timers::catch_up`]'s resulting state, so a wake-up
    /// that lands early just retries. Returns `None` when nothing can raise
    /// this timer's IRQ again: both IRQ bits off, a fired one-shot, counter
    /// 2 stopped dead by its sync mode, or a reset-on-edge mode whose
    /// target sits beyond what one period can ever deliver.
    pub fn next_deadline(&self, idx: usize, timing: VideoTiming) -> Option<u64> {
        let t = &self.t[idx];
        let mode = t.mode;
        let sync = (mode >> 1) & 3;
        // Counter 2 stops dead in sync modes 0 and 3 (see catch_up)
        if idx == 2 && mode & 1 != 0 && (sync == 0 || sync == 3) {
            return None;
        }
        let irq_target = mode & (1 << 4) != 0;
        let irq_overflow = mode & (1 << 5) != 0;
        let repeat = mode & (1 << 6) != 0;
        if (!irq_target && !irq_overflow) || (!repeat && t.irq_fired) {
            return None;
        }
        let (num, den) = tick_ratio(idx, mode, timing);
        let target = (t.target & 0xffff) as u64;
        let counter = t.counter as u64;
        let to_target = (irq_target && counter <= target).then(|| target + 1 - counter);
        // A free-running counter already past its target can only cross it
        // again after the wrap; advance only sees the crossing on a later
        // call, so the wrap has to be a wake-up even with the overflow IRQ
        // off.
        let to_wrap =
            (irq_overflow || (irq_target && counter > target)).then(|| 0x1_0000 - counter);
        // Counters 0/1 in sync modes 1/2 reset to 0 (not 0x10000) at every
        // edge, so a counter already past its target does not need to wrap
        // at all: the next edge zeroes it, and it re-crosses the target
        // `target + 1` ticks later. The edge itself is at or after
        // `last_sync` and zeroes `frac` too, so using the current, possibly
        // larger `frac` below still only pulls this estimate earlier,
        // keeping it a valid lower bound.
        let resets_on_edge = idx < 2 && mode & 1 != 0 && (sync == 1 || sync == 2);
        let to_target_after_reset =
            (resets_on_edge && irq_target && counter > target).then(|| target + 1);
        // Reset-on-edge modes cap the ticks one period can deliver. Anything
        // needing more never arrives; do not poll for it. The cap is padded
        // past a bare period: see `EDGE_OVERSHOOT_MARGIN_CYCLES`.
        let cap = resets_on_edge.then(|| {
            let (period, blank) = period_blank(idx, timing);
            let counting = (if sync == 1 { period } else { blank }) + EDGE_OVERSHOOT_MARGIN_CYCLES;
            (counting * den).div_ceil(num)
        });
        let reachable = |n: u64| cap.is_none_or(|c| n <= c);
        let ticks = [to_target, to_wrap, to_target_after_reset]
            .into_iter()
            .flatten()
            .filter(|&n| reachable(n))
            .min()?;
        // Smallest cycle count c with (c * den + frac) / num >= ticks
        let cycles = (ticks * num).saturating_sub(t.frac).div_ceil(den);
        Some(t.last_sync + cycles)
    }

    /// Advance timer `idx` to `now`, the exact accounting (blanking edges
    /// included). Idempotent: calling it again with the same or an earlier
    /// `now` does nothing. This is the handler a scheduled wake-up runs.
    pub(crate) fn catch_up(&mut self, idx: usize, now: u64, timing: VideoTiming, irq: &mut Irq) {
        // Check before overwriting `last_sync`: writing an earlier `now`
        // first and only then bailing out would move the sync point
        // backwards, double-counting the cycles in between on the next call.
        if now <= self.t[idx].last_sync {
            return;
        }
        let from = std::mem::replace(&mut self.t[idx].last_sync, now);
        let mode = self.t[idx].mode;
        if mode & 1 == 0 {
            self.advance(idx, now - from, timing, irq);
            return;
        }

        let sync = (mode >> 1) & 3;
        if idx == 2 {
            // Counter 2 either stops dead (0/3) or runs free (1/2)
            if sync == 1 || sync == 2 {
                self.advance(idx, now - from, timing, irq);
            }
            return;
        }

        // Counters 0 and 1 gate on hblank and vblank respectively. Walk the
        // interval one blanking edge at a time: the resetting modes have to
        // observe each edge, not just the interval as a whole.
        let (period, blank) = period_blank(idx, timing);
        let origin = self.frame_origin;
        let mut cursor = from;
        while cursor < now {
            let edge = origin + (cursor.saturating_sub(origin) / period + 1) * period;
            let end = edge.min(now);
            let cycles = match sync {
                // Pause during blanking
                0 => (end - cursor) - blanking_within(cursor, end, origin, period, blank),
                // Reset on the edge, and only count while blanking
                2 => blanking_within(cursor, end, origin, period, blank),
                // Paused until the first edge, free-running after it
                3 if !self.t[idx].sync_started => 0,
                _ => end - cursor,
            };
            self.advance(idx, cycles, timing, irq);
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
    fn advance(&mut self, idx: usize, cycles: u64, timing: VideoTiming, irq: &mut Irq) {
        let t = &mut self.t[idx];

        let (num, den) = tick_ratio(idx, t.mode, timing);
        let elapsed = cycles * den + t.frac;
        let ticks = elapsed / num;
        t.frac = elapsed % num;
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
    const T0_TARGET: u32 = 0x1f80_1108;
    const T1_COUNT: u32 = 0x1f80_1110;
    const T1_MODE: u32 = 0x1f80_1114;
    const T2_COUNT: u32 = 0x1f80_1120;
    const T2_MODE: u32 = 0x1f80_1124;
    const T2_TARGET: u32 = 0x1f80_1128;

    const NTSC: VideoTiming = VideoTiming::NTSC;
    const PAL: VideoTiming = VideoTiming::PAL;
    const CYCLES_PER_LINE: u64 = NTSC.cycles_per_line;
    const CYCLES_PER_FRAME: u64 = NTSC.cycles_per_frame();
    const HBLANK_CYCLES: u64 = NTSC.hblank_cycles;

    /// Write mode at cycle 0, run to `now`, and read the counter back.
    fn run(mode_addr: u32, count_addr: u32, mode: u32, now: u64) -> u32 {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        timers.write(mode_addr, mode, 0, NTSC, &mut irq);
        timers.read(count_addr, now, NTSC, &mut irq)
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
        coarse.write(T0_MODE, 1, 0, NTSC, &mut irq);
        fine.write(T0_MODE, 1, 0, NTSC, &mut irq);
        let end = 3 * CYCLES_PER_LINE;
        for now in (1..=end).step_by(97) {
            fine.read(T0_COUNT, now, NTSC, &mut irq);
        }
        assert_eq!(
            coarse.read(T0_COUNT, end, NTSC, &mut irq),
            fine.read(T0_COUNT, end, NTSC, &mut irq)
        );
    }

    /// Timer 2 wrapping at a target of 99, so one IRQ condition occurs per
    /// 100 cycles. `extra` adds the IRQ mode bits under test.
    fn wrapping_timer2(extra: u32) -> (Timers, Irq) {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        // Reset at target | IRQ at target
        timers.write(T2_MODE, (1 << 3) | (1 << 4) | extra, 0, NTSC, &mut irq);
        timers.write(T2_TARGET, 99, 0, NTSC, &mut irq);
        (timers, irq)
    }

    /// Run to the `n`-th target wrap and report whether IRQ2 was raised.
    fn wrap_raises_irq(timers: &mut Timers, irq: &mut Irq, n: u64) -> bool {
        irq.stat = 0;
        timers.read(T2_COUNT, n * 100, NTSC, irq);
        irq.stat & (1 << 6) != 0
    }

    #[test]
    fn one_shot_irq_is_served_only_once() {
        let (mut timers, mut irq) = wrapping_timer2(0);
        assert!(wrap_raises_irq(&mut timers, &mut irq, 1));
        assert!(!wrap_raises_irq(&mut timers, &mut irq, 2));
        // Rewriting the mode re-arms it
        timers.write(T2_MODE, (1 << 3) | (1 << 4), 200, NTSC, &mut irq);
        timers.write(T2_TARGET, 99, 200, NTSC, &mut irq);
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
        assert_eq!(timers.read(T2_MODE, 0, NTSC, &mut irq) & (1 << 10), 1 << 10);
        timers.read(T2_COUNT, 100, NTSC, &mut irq);
        assert_eq!(timers.read(T2_MODE, 100, NTSC, &mut irq) & (1 << 10), 0);
        timers.read(T2_COUNT, 200, NTSC, &mut irq);
        assert_eq!(
            timers.read(T2_MODE, 200, NTSC, &mut irq) & (1 << 10),
            1 << 10
        );
    }

    #[test]
    fn pulse_mode_leaves_the_request_bit_set() {
        let (mut timers, mut irq) = wrapping_timer2(1 << 6);
        timers.read(T2_COUNT, 100, NTSC, &mut irq);
        assert_eq!(
            timers.read(T2_MODE, 100, NTSC, &mut irq) & (1 << 10),
            1 << 10
        );
    }

    #[test]
    fn dotclock_ticks_match_the_documented_dots_per_line() {
        // psx-spx dots per NTSC scanline: 320pix 426.6, 640pix 853.2,
        // 256pix 341.3 — the fractional dot is dropped
        for (dot_vclk, expected) in [(8u64, 426), (4, 853), (10, 341)] {
            let timing = VideoTiming {
                dotclock: (dot_vclk * crate::CPU_CLOCK_HZ, 53_693_175),
                ..NTSC
            };
            let mut timers = Timers::new();
            let mut irq = Irq::default();
            timers.write(T0_MODE, 1 << 8, 0, timing, &mut irq); // dotclock source
            let ticks = timers.read(T0_COUNT, timing.cycles_per_line, timing, &mut irq);
            assert_eq!(ticks, expected, "{dot_vclk} video clocks per dot");
        }
    }

    #[test]
    fn a_pal_field_runs_at_the_pal_refresh_rate() {
        let hz = crate::CPU_CLOCK_HZ as f64 / PAL.cycles_per_frame() as f64;
        assert!((hz - 49.76).abs() < 0.05, "{hz} Hz");
        let hz = crate::CPU_CLOCK_HZ as f64 / NTSC.cycles_per_frame() as f64;
        assert!((hz - 59.83).abs() < 0.05, "{hz} Hz");
    }

    #[test]
    fn blanking_phase_follows_the_field_origin() {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        // A field boundary that is not a multiple of the field length, as a
        // switch between regions leaves behind
        let origin = 12_345;
        timers.set_frame_origin(origin);
        let mode = 1 | (1 << 1); // sync enabled, reset at the vblank edge
        timers.write(T1_MODE, mode, origin, PAL, &mut irq);
        let past_edge = 100;
        let now = origin + PAL.cycles_per_frame() + past_edge;
        assert_eq!(timers.read(T1_COUNT, now, PAL, &mut irq), past_edge as u32);
    }

    #[test]
    fn target_irq_still_fires_while_synchronized() {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        // Sync mode 0 (pause during hblank), IRQ on target
        timers.write(T0_MODE, 1 | (1 << 4), 0, NTSC, &mut irq);
        timers.write(T0_COUNT + 8, 100, 0, NTSC, &mut irq);
        timers.sync_all(1000, NTSC, &mut irq);
        assert_eq!(irq.stat & (1 << 4), 1 << 4);
    }

    /// Register addresses for timer `idx`: (count, mode, target).
    fn regs(idx: usize) -> (u32, u32, u32) {
        let base = 0x1f80_1100 + (idx as u32) * 0x10;
        (base, base + 4, base + 8)
    }

    #[test]
    fn wakeups_reach_the_crossing_on_the_same_cycle_as_polling() {
        let one = 1u32 << 4;
        let src = |s: u32| s << 8;
        let sync = |m: u32| 1 | (m << 1);
        let limit = 2 * CYCLES_PER_FRAME + 1000;

        // (idx, mode, target, an extra COUNT write applied after mode)
        let configs: &[(usize, u32, u32, Option<u32>)] = &[
            (0, one, 0x8000, None),
            (0, one | src(1), 0x4000, None),
            (1, one, 0x8000, None),
            (1, one | src(1), 0x40, None),
            (2, one, 0x8000, None),
            (2, one | src(2), 0x2000, None),
            (0, one | sync(0), 0x8000, None),
            (0, one | sync(1), 0x400, None),
            (0, one | sync(2), 0x40, None),
            (0, one | sync(3), 0x8000, None),
            (1, one | sync(0), 0x8000, None),
            (1, one | sync(1), 0x8000, None),
            (1, one | sync(2), 0x4000, None),
            (1, one | sync(3), 0x8000, None),
            (2, 1 << 5, 0, None), // overflow IRQ, fires at 0x10000
            // Counter written above the target after the mode write: the
            // crossing only happens after the wrap.
            (2, one, 0x10, Some(0x20)),
            // Reset-on-edge modes restart at 0, not at the wrap: a counter
            // pushed past the target still re-crosses it, one period later,
            // well short of 0x10000.
            (0, one | sync(1), 0x400, Some(0x800)),
            (0, one | sync(2), 0x40, Some(0x100)),
            // Same, but arrived at through a genuine repeat-mode firing
            // rather than a raw write: the counter is left just above the
            // target (no reset-on-target bit here), and the next crossing
            // has to wait for the following edge.
            (0, one | sync(1) | (1 << 6), 0x100, Some(0x101)),
        ];

        for &(idx, mode, target, extra) in configs {
            let (count_addr, mode_addr, target_addr) = regs(idx);
            let bit = 1 << (4 + idx as u32);

            let mut wake = Timers::new();
            let mut irq_wake = Irq::default();
            wake.write(target_addr, target, 0, NTSC, &mut irq_wake);
            wake.write(mode_addr, mode, 0, NTSC, &mut irq_wake);
            if let Some(count) = extra {
                wake.write(count_addr, count, 0, NTSC, &mut irq_wake);
            }
            let mut now = 0u64;
            let wake_cycle = loop {
                let d = wake
                    .next_deadline(idx, NTSC)
                    .expect("a pending IRQ condition always has a deadline");
                assert!(d > now, "idx {idx}: wake-up must move forward");
                assert!(d <= limit, "idx {idx}: wake-up loop exceeded the limit");
                now = d;
                wake.catch_up(idx, now, NTSC, &mut irq_wake);
                if irq_wake.stat & bit != 0 {
                    break now;
                }
            };

            let mut poll = Timers::new();
            let mut irq_poll = Irq::default();
            poll.write(target_addr, target, 0, NTSC, &mut irq_poll);
            poll.write(mode_addr, mode, 0, NTSC, &mut irq_poll);
            if let Some(count) = extra {
                poll.write(count_addr, count, 0, NTSC, &mut irq_poll);
            }
            let poll_cycle = (1..=limit)
                .find(|&c| {
                    poll.read(count_addr, c, NTSC, &mut irq_poll);
                    irq_poll.stat & bit != 0
                })
                .unwrap_or_else(|| panic!("idx {idx}: polling never saw the IRQ within the limit"));

            assert_eq!(
                wake_cycle, poll_cycle,
                "idx {idx} mode {mode:#x} target {target:#x}"
            );
            if idx == 2 && mode == (1 << 5) && target == 0 {
                assert_eq!(wake_cycle, 0x10000);
            }
            if idx == 2 && mode == one && target == 0x10 && extra == Some(0x20) {
                assert_eq!(wake_cycle, 0x10000 - 0x20 + 0x11);
            }
        }
    }

    #[test]
    fn repeat_mode_serves_every_crossing_between_wakeups() {
        let mut timers = Timers::new();
        let mut irq = Irq::default();
        let one = 1u32 << 4;
        let src = |s: u32| s << 8;
        // Free-running (no sync gate), repeat, reset-on-target
        let mode = one | src(2) | (1 << 6) | (1 << 3);
        timers.write(T2_TARGET, 0x1000, 0, NTSC, &mut irq);
        timers.write(T2_MODE, mode, 0, NTSC, &mut irq);

        let limit = 10 * CYCLES_PER_FRAME;
        let mut now = 0u64;
        let mut raises = 0u32;
        while now < limit {
            let d = timers
                .next_deadline(2, NTSC)
                .expect("a repeating timer always has a next crossing");
            now = d;
            irq.stat = 0;
            timers.catch_up(2, now, NTSC, &mut irq);
            if irq.stat & (1 << 6) != 0 {
                raises += 1;
            }
        }
        assert!((172..=173).contains(&raises), "raises = {raises}");
    }

    #[test]
    fn unreachable_targets_schedule_nothing() {
        let one = 1u32 << 4;
        let sync = |m: u32| 1 | (m << 1);

        // A period's worth of hblank-gated counting cannot reach the target.
        let mut t = Timers::new();
        let mut irq = Irq::default();
        t.write(T0_TARGET, 0x8000, 0, NTSC, &mut irq);
        t.write(T0_MODE, one | sync(1), 0, NTSC, &mut irq);
        assert_eq!(t.next_deadline(0, NTSC), None);
        t.write(T0_COUNT, 0x7ff0, 0, NTSC, &mut irq);
        assert_eq!(t.next_deadline(0, NTSC), Some(0x11));

        // Counter 2 stopped dead by sync mode 0.
        let mut t2 = Timers::new();
        let mut irq2 = Irq::default();
        t2.write(T2_TARGET, 0x10, 0, NTSC, &mut irq2);
        t2.write(T2_MODE, one | sync(0), 0, NTSC, &mut irq2);
        assert_eq!(t2.next_deadline(2, NTSC), None);

        // A one-shot timer that has already fired.
        let mut t3 = Timers::new();
        let mut irq3 = Irq::default();
        t3.write(T2_TARGET, 0x10, 0, NTSC, &mut irq3);
        t3.write(T2_MODE, one, 0, NTSC, &mut irq3);
        t3.catch_up(2, 0x11, NTSC, &mut irq3);
        assert_eq!(t3.next_deadline(2, NTSC), None);

        // Neither IRQ bit set: nothing can ever raise this timer's IRQ.
        let mut t4 = Timers::new();
        let mut irq4 = Irq::default();
        t4.write(T0_TARGET, 0x8000, 0, NTSC, &mut irq4);
        t4.write(T0_MODE, 0, 0, NTSC, &mut irq4);
        assert_eq!(t4.next_deadline(0, NTSC), None);
    }
}
