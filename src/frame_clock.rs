// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::num::NonZeroU64;
use std::time::Duration;

use crate::utils::get_monotonic_time;

#[derive(Debug)]
pub struct FrameClock {
    last_presentation_time: Option<Duration>,
    refresh_interval_ns: Option<NonZeroU64>,
    vrr: bool,
    render_time: RenderTimeEstimate,
}

/// How long a frame recently took from the start of `redraw` to being handed to KMS.
///
/// Ported from mutter's two-tier maximum (`clutter/clutter/clutter-frame-clock.c:416-457`,
/// `:600-607`): a short-term running max that rises immediately, and a long-term max that only
/// decays — halfway toward the short-term one, once a second. Rising fast and falling slowly is
/// the point. The estimate exists to answer "can this frame still make that vblank", and the cost
/// of guessing too low (a missed vblank, and an animation clock frozen at a time we never reach)
/// is worse than the cost of guessing too high (aiming one cycle further out).
///
/// A mean would be the wrong statistic here for the same reason: frame cost on this stack is
/// dominated by a submit round trip whose duration is bimodal, and the tail is what misses.
#[derive(Debug, Default)]
struct RenderTimeEstimate {
    shortterm_max: Duration,
    longterm_max: Duration,
    /// Presentation time the long-term max was last promoted at; promotion is once a second.
    longterm_promotion: Option<Duration>,
}

impl RenderTimeEstimate {
    /// Fold in one frame's measured render time. `refresh_interval` bounds it: a single
    /// catastrophic frame (a cold bake, a stall while descheduled) must not poison the estimate
    /// into aiming seconds ahead.
    fn record(&mut self, render_time: Duration, refresh_interval: Duration) {
        self.shortterm_max = self
            .shortterm_max
            .max(render_time.min(refresh_interval * 3));
    }

    /// Decay the long-term max toward the short-term one, once a second — mutter's
    /// `maybe_update_longterm_max_duration_us`.
    fn promote(&mut self, presentation_time: Duration) {
        let due = self
            .longterm_promotion
            .is_none_or(|last| presentation_time.saturating_sub(last) >= Duration::from_secs(1));
        if !due {
            return;
        }

        self.longterm_max = if self.longterm_max > self.shortterm_max {
            // Exponential drop-off toward the short-term max.
            self.longterm_max - (self.longterm_max - self.shortterm_max) / 2
        } else {
            self.shortterm_max
        };
        self.shortterm_max = Duration::ZERO;
        self.longterm_promotion = Some(presentation_time);
    }

    fn get(&self) -> Duration {
        self.longterm_max.max(self.shortterm_max)
    }
}

impl FrameClock {
    pub fn new(refresh_interval: Option<Duration>, vrr: bool) -> Self {
        let refresh_interval_ns = if let Some(interval) = &refresh_interval {
            assert_eq!(interval.as_secs(), 0);
            Some(NonZeroU64::new(interval.subsec_nanos().into()).unwrap())
        } else {
            None
        };

        Self {
            last_presentation_time: None,
            refresh_interval_ns,
            vrr,
            render_time: RenderTimeEstimate::default(),
        }
    }

    /// Report how long the frame that was just handed to KMS took to build, measured from the
    /// start of the redraw. Feeds [`Self::next_presentation_time`]'s choice of target.
    pub fn record_render_time(&mut self, render_time: Duration) {
        let Some(refresh_interval) = self.refresh_interval() else {
            return;
        };
        self.render_time.record(render_time, refresh_interval);
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        self.refresh_interval_ns
            .map(|r| Duration::from_nanos(r.get()))
    }

    pub fn set_vrr(&mut self, vrr: bool) {
        if self.vrr == vrr {
            return;
        }

        self.vrr = vrr;
        self.last_presentation_time = None;
    }

    pub fn vrr(&self) -> bool {
        self.vrr
    }

    pub fn presented(&mut self, presentation_time: Duration) {
        if presentation_time.is_zero() {
            // Not interested in these.
            return;
        }

        self.render_time.promote(presentation_time);
        self.last_presentation_time = Some(presentation_time);
    }

    pub fn next_presentation_time(&self) -> Duration {
        let mut now = get_monotonic_time();

        let Some(refresh_interval_ns) = self.refresh_interval_ns else {
            return now;
        };
        let Some(last_presentation_time) = self.last_presentation_time else {
            return now;
        };

        let refresh_interval_ns = refresh_interval_ns.get();

        if now <= last_presentation_time {
            // Got an early VBlank.
            let orig_now = now;
            now += Duration::from_nanos(refresh_interval_ns);

            if now < last_presentation_time {
                // Not sure when this can happen.
                error!(
                    now = ?orig_now,
                    ?last_presentation_time,
                    "got a 2+ early VBlank, {:?} until presentation",
                    last_presentation_time - now,
                );
                now = last_presentation_time + Duration::from_nanos(refresh_interval_ns);
            }
        }

        let since_last = now - last_presentation_time;
        let since_last_ns =
            since_last.as_secs() * 1_000_000_000 + u64::from(since_last.subsec_nanos());
        let to_next_ns = (since_last_ns / refresh_interval_ns + 1) * refresh_interval_ns;

        // If VRR is enabled and more than one frame passed since last presentation, assume that we
        // can present immediately.
        if self.vrr && to_next_ns > refresh_interval_ns {
            return now;
        }

        let refresh_interval = Duration::from_nanos(refresh_interval_ns);
        let target = last_presentation_time + Duration::from_nanos(to_next_ns);
        self.reachable(target, now, refresh_interval)
    }

    /// Advance `target` past any vblank this frame cannot reach, given what recent frames cost.
    ///
    /// This is the honest half of mutter's frame-clock arithmetic
    /// (`clutter-frame-clock.c:1060-1073`): mutter *also* pushes its target forward when the frame
    /// cannot make it, and the reason to copy that is not throughput but **correctness**. The
    /// caller freezes the animation clock at this time (`Synoik::redraw`), so a target we cannot
    /// reach means every animation on that frame is sampled for a moment that has already passed
    /// by the time it lights up — a whole refresh interval of stale, on exactly the frames that
    /// were already late.
    ///
    /// What this deliberately does **not** do is delay the frame to hit the target. Mutter
    /// dispatches immediately after an idle period too (`should_update_now`, `:894-923`) —
    /// "this results in lowest average latency for sporadic user input" — and that is the
    /// common case here: on a live seat 7 242 of 7 778 late presentations followed a 3-10 cycle
    /// gap. Waiting for a deadline would guarantee the later vblank *and* add latency. See
    /// `docs/fork/late-frame-populations.md`.
    fn reachable(&self, target: Duration, now: Duration, refresh_interval: Duration) -> Duration {
        // Bounded at two cycles of advance for the same reason mutter clamps its estimate: aiming
        // further out than that on one bad frame would jump every animation ahead of where it
        // renders, which reads far worse than the missed vblank it avoids.
        let estimate = self.render_time.get();
        let mut target = target;
        for _ in 0..2 {
            if target.saturating_sub(estimate) >= now {
                break;
            }
            target += refresh_interval;
        }
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH: Duration = Duration::from_micros(16_667);

    fn clock() -> FrameClock {
        FrameClock::new(Some(REFRESH), false)
    }

    /// With nothing measured yet, the target is the next vblank and nothing else — a fresh clock
    /// must not start guessing that frames are expensive.
    #[test]
    fn an_unmeasured_clock_aims_at_the_next_vblank() {
        let c = clock();
        let now = Duration::from_secs(10);
        let target = now + Duration::from_millis(1);
        assert_eq!(c.reachable(target, now, REFRESH), target);
    }

    /// The whole point: a frame that costs 5 ms and has 1 ms of the cycle left cannot present at
    /// the next vblank, so the target — and therefore the animation clock frozen at it — moves to
    /// the one it can actually reach.
    #[test]
    fn a_target_we_cannot_reach_advances_one_cycle() {
        let mut c = clock();
        c.record_render_time(Duration::from_millis(5));

        let now = Duration::from_secs(10);
        let target = now + Duration::from_millis(1);
        assert_eq!(
            c.reachable(target, now, REFRESH),
            target + REFRESH,
            "1ms left for a 5ms frame: the next vblank is not reachable",
        );

        // …and with the whole cycle ahead of it, the same frame keeps the near target.
        let target = now + Duration::from_millis(15);
        assert_eq!(
            c.reachable(target, now, REFRESH),
            target,
            "15ms left for a 5ms frame: no reason to aim further out",
        );
    }

    /// One catastrophic frame must not throw the target seconds into the future: a cold bake or a
    /// stall while descheduled is exactly the frame whose cost does not repeat, and an animation
    /// clock jumped ahead of what renders reads far worse than the miss it avoids.
    #[test]
    fn the_advance_is_bounded_at_two_cycles() {
        let mut c = clock();
        c.record_render_time(Duration::from_secs(3));

        let now = Duration::from_secs(10);
        let target = now + Duration::from_micros(1);
        let reached = c.reachable(target, now, REFRESH);
        assert_eq!(
            reached,
            target + REFRESH * 2,
            "a 3s frame must advance the target by two cycles and stop, not chase its own estimate",
        );
    }

    /// The estimate rises on the frame that is slow and falls only halfway per promotion. Rising
    /// fast is what keeps a slow stretch from missing every vblank in it; falling slowly is what
    /// keeps one fast frame from undoing that.
    #[test]
    fn the_estimate_rises_at_once_and_decays_by_halves() {
        let mut e = RenderTimeEstimate::default();
        e.record(Duration::from_millis(8), REFRESH);
        assert_eq!(e.get(), Duration::from_millis(8), "a slow frame counts now");

        // Promotion carries the short-term max into the long term and resets it.
        e.promote(Duration::from_secs(1));
        assert_eq!(e.get(), Duration::from_millis(8));

        // A quiet second: the long-term max drops halfway toward the (zero) short-term max, not
        // straight to it.
        e.promote(Duration::from_secs(2));
        assert_eq!(e.get(), Duration::from_millis(4));
        e.promote(Duration::from_secs(3));
        assert_eq!(e.get(), Duration::from_millis(2));

        // Promotion is once a second; a presentation sooner than that changes nothing.
        e.promote(Duration::from_millis(3500));
        assert_eq!(e.get(), Duration::from_millis(2));
    }

    /// A single frame's measurement is clamped before it ever reaches the estimate, so the decay
    /// above never has to walk down from a multi-second outlier.
    #[test]
    fn one_frame_cannot_poison_the_estimate() {
        let mut e = RenderTimeEstimate::default();
        e.record(Duration::from_secs(3), REFRESH);
        assert_eq!(e.get(), REFRESH * 3);
    }
}
