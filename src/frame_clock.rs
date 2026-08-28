// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::utils::get_monotonic_time;

/// Slack added on top of the measured render time when picking a dispatch deadline — mutter's
/// `clutter_max_render_time_constant_us` (`clutter/clutter/clutter-main.c:61`), same 1 ms.
///
/// Mutter also adds the display's vblank duration, which it gets from the DRM mode timings
/// (`meta_calculate_drm_mode_vblank_duration_us`, `src/backends/native/meta-kms-crtc.c:891`), and
/// arms a hardware deadline timer we do not have. Ours stands in for both, and 1 ms is measurably
/// too thin on this stack — parity needs ~4 ms (see the sweep in `docs/fork/foundation.md` §3).
/// It is a dial rather than a constant because it is calibration against the machine's wake floor,
/// and that floor is a measurement, not a property: it moved 19× on 2026-08-28. So a re-calibration
/// has to cost no rebuild and no relogin — `SYNOIK_RENDER_TIME_MARGIN_MS` at startup,
/// [`set_render_time_margin`] at runtime, with `tools/timer-probe` run first.
const RENDER_TIME_MARGIN_DEFAULT_US: u64 = 1000;

static RENDER_TIME_MARGIN_US: AtomicU64 = AtomicU64::new(RENDER_TIME_MARGIN_DEFAULT_US);
static RENDER_TIME_MARGIN_INIT: OnceLock<()> = OnceLock::new();

fn render_time_margin() -> Duration {
    RENDER_TIME_MARGIN_INIT.get_or_init(|| {
        let us = std::env::var("SYNOIK_RENDER_TIME_MARGIN_MS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|ms| ms.is_finite() && *ms >= 0.)
            .map(|ms| (ms * 1000.) as u64)
            .unwrap_or(RENDER_TIME_MARGIN_DEFAULT_US);
        RENDER_TIME_MARGIN_US.store(us, Ordering::Relaxed);
    });
    Duration::from_micros(RENDER_TIME_MARGIN_US.load(Ordering::Relaxed))
}

/// Set the deadline slack for the rest of the session; returns the new value.
pub fn set_render_time_margin(margin: Duration) -> Duration {
    // Through the same initialiser, or the env default would overwrite this on the next frame.
    render_time_margin();
    RENDER_TIME_MARGIN_US.store(margin.as_micros() as u64, Ordering::Relaxed);
    margin
}

/// The deadline slack currently in force.
pub fn render_time_margin_now() -> Duration {
    render_time_margin()
}

/// Whether frames are held until their dispatch deadline. Off by default;
/// `SYNOIK_DEADLINE_DISPATCH=1` starts a session with it on, and [`set_deadline_dispatch`] flips it
/// at runtime.
///
/// **Off by default for want of a measured benefit, not because it costs too much.** Against a
/// continuous client the drop rate reaches parity with immediate dispatch at a **4 ms** margin, at
/// both 60 Hz and 120 Hz; 1 ms still runs 3–3.7× the baseline. The parity point does not scale with
/// the refresh interval, which is what a margin paying for wake jitter and estimate error should
/// do. But the margin is absolute while the cycle is not, so 4 ms is 24% of a 60 Hz cycle and 48%
/// of a 120 Hz one — at high refresh the feature buys proportionally little freshness for the same
/// safety. And it can never *beat* immediate dispatch on drop rate; by construction the best it can
/// do is cost nothing. What it buys — input and animation sampled closer to the photons — is real
/// and *not* measurable with frame-perf, so turning this on waits on an input-to-photon number,
/// which needs host-side timestamps. `docs/fork/foundation.md` §3 has the sweep and the two traps
/// it walked into, and §4 the wake floor the margin is calibrated against.
///
/// Runtime-switchable rather than read once, because measuring it needs an A/B *within* one
/// session: comparing two logins compares two different sets of background work, and on the first
/// attempt that difference (a polkit dialog and a gnome-software refresh in one arm and not the
/// other) was larger than the effect being measured.
static DEADLINE_DISPATCH: AtomicBool = AtomicBool::new(false);
static DEADLINE_DISPATCH_INIT: OnceLock<()> = OnceLock::new();

fn deadline_dispatch() -> bool {
    DEADLINE_DISPATCH_INIT.get_or_init(|| {
        let on = std::env::var_os("SYNOIK_DEADLINE_DISPATCH").is_some_and(|v| v == "1");
        DEADLINE_DISPATCH.store(on, Ordering::Relaxed);
    });
    DEADLINE_DISPATCH.load(Ordering::Relaxed)
}

/// Whether frames are currently being held until their deadline.
pub fn deadline_dispatch_enabled() -> bool {
    deadline_dispatch()
}

/// Turn deadline dispatch on or off for the rest of the session; returns the new state.
pub fn set_deadline_dispatch(enabled: bool) -> bool {
    // Through the same initialiser, or the first call would be overwritten by the env default the
    // next time a frame clock asks.
    deadline_dispatch();
    DEADLINE_DISPATCH.store(enabled, Ordering::Relaxed);
    enabled
}

/// When to start building the next frame, and which presentation to aim it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Build it now.
    Now { target: Duration },
    /// Build it at `at`, aiming at `target` — mutter's deadline dispatch. The target is chosen
    /// *here*, when the deadline is armed, and must be carried to the redraw rather than
    /// recomputed when the timer fires: `at` is by construction the moment
    /// [`FrameClock::next_presentation_time`] would flip to the vblank after this one.
    At { at: Duration, target: Duration },
}

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

/// Where a moment sits in the display's refresh cadence.
#[derive(Debug, Clone, Copy)]
enum Cadence {
    /// There is no cadence to pace against — no mode, nothing presented yet, or VRR with the
    /// display already idle. Present as soon as we can; the payload is `now`, possibly nudged
    /// forward past an early vblank.
    Immediate(Duration),
    Locked {
        /// The next vblank at or after `now`.
        boundary: Duration,
        /// How far that boundary is from the last presentation — one refresh interval means the
        /// display is running continuously, more means we have been idle.
        since_last_presentation: Duration,
        /// `now`, nudged forward past an early vblank.
        now: Duration,
        refresh_interval: Duration,
    },
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

    /// When to aim this frame's content at — the next vblank the frame can still reach, which is
    /// also the time the caller freezes the animation clock at.
    pub fn next_presentation_time(&self) -> Duration {
        self.next_target(true)
    }

    /// When the next vblank is due, whatever we can or cannot get done by then.
    ///
    /// This is a property of the *display*, so it is what the estimated-vblank fallback timer
    /// waits on: that timer stands in for a page flip we never made, and pushing it out by what a
    /// frame would have cost would slow the fallback cadence for a cost no frame is paying.
    /// [`next_presentation_time`](Self::next_presentation_time) is the one that answers "aim at
    /// what?" and is allowed to skip a vblank; this one is not.
    pub fn next_vblank_estimate(&self) -> Duration {
        self.next_target(false)
    }

    /// Where in the display's cadence `now` sits, if there is a cadence to sit in.
    fn cadence(&self, mut now: Duration) -> Cadence {
        let Some(refresh_interval_ns) = self.refresh_interval_ns else {
            return Cadence::Immediate(now);
        };
        let Some(last_presentation_time) = self.last_presentation_time else {
            return Cadence::Immediate(now);
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
            return Cadence::Immediate(now);
        }

        Cadence::Locked {
            boundary: last_presentation_time + Duration::from_nanos(to_next_ns),
            since_last_presentation: Duration::from_nanos(to_next_ns),
            now,
            refresh_interval: Duration::from_nanos(refresh_interval_ns),
        }
    }

    fn next_target(&self, advance_past_unreachable: bool) -> Duration {
        self.next_target_from(get_monotonic_time(), advance_past_unreachable)
    }

    /// [`next_target`](Self::next_target) against an explicit `now`, so a test can ask twice and
    /// get two answers about the same instant. Reading the real clock per call made
    /// `the_vblank_estimate_ignores_what_a_frame_costs` compare two calls that could straddle a
    /// vblank boundary, and it failed by exactly one refresh period whenever they did — rare on an
    /// idle machine, reproducible under the load of `SYNOIK_VK_VALIDATION=1`. Same seam, and same
    /// reason, as [`dispatch_from`](Self::dispatch_from).
    fn next_target_from(&self, now: Duration, advance_past_unreachable: bool) -> Duration {
        match self.cadence(now) {
            Cadence::Immediate(now) => now,
            Cadence::Locked {
                boundary,
                now,
                refresh_interval,
                ..
            } => {
                if advance_past_unreachable {
                    self.reachable(boundary, now, refresh_interval)
                } else {
                    boundary
                }
            }
        }
    }

    /// When to start building the next frame, and what to aim it at.
    ///
    /// Mutter's deadline dispatch (`clutter_frame_clock_schedule_update`,
    /// `clutter/clutter/clutter-frame-clock.c:1299-1383`): in the *continuous* case, hold the frame
    /// until `vblank − max_render_time` instead of building it the moment the last one was
    /// presented. Nothing about that reduces missed vblanks — a frame started right after a vblank
    /// already has the whole interval — what it buys is **latency**: input and animation are
    /// sampled a whole refresh interval closer to the photons. Building at the top of the cycle
    /// then idling for 14 ms means every event in those 14 ms waits for the frame after next.
    ///
    /// Every other case dispatches immediately, exactly as before this existed:
    ///
    /// - no cadence yet (no mode, no presentation, VRR gone quiet) — nothing to aim at;
    /// - an **idle period**, i.e. the next vblank is more than one interval out. Mutter
    ///   short-circuits here too (`should_update_now`, `:894-923`): "lowest average latency for
    ///   sporadic user input". This is the common case on a live seat, and the reason the miss
    ///   population documented in `docs/fork/foundation.md` §3 is untouched by this;
    /// - no measured render time yet — the deadline would be a guess, and guessing low costs a
    ///   vblank;
    /// - the deadline has already passed, which is just "we are late, go".
    pub fn next_dispatch(&self) -> Dispatch {
        self.dispatch_from(get_monotonic_time())
    }

    fn dispatch_from(&self, now: Duration) -> Dispatch {
        self.dispatch_from_with(now, deadline_dispatch(), render_time_margin())
    }

    /// The decision itself, with the two session-global dials passed in. Tests drive *this*: the
    /// dials are process-wide atomics, and a test that flipped one would be flipping it for every
    /// other test running beside it.
    fn dispatch_from_with(
        &self,
        now: Duration,
        deadline_dispatch: bool,
        margin: Duration,
    ) -> Dispatch {
        let (boundary, since_last_presentation, now, refresh_interval) = match self.cadence(now) {
            Cadence::Immediate(now) => return Dispatch::Now { target: now },
            Cadence::Locked {
                boundary,
                since_last_presentation,
                now,
                refresh_interval,
            } => (boundary, since_last_presentation, now, refresh_interval),
        };

        let immediately = Dispatch::Now {
            target: self.reachable(boundary, now, refresh_interval),
        };

        let idle = since_last_presentation > refresh_interval;
        let estimate = self.render_time.get();
        if idle || estimate.is_zero() || !deadline_dispatch {
            return immediately;
        }

        let at = boundary.saturating_sub(estimate + margin);
        if at <= now {
            return immediately;
        }

        Dispatch::At {
            at,
            target: boundary,
        }
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
    /// `docs/fork/foundation.md` §3.
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

    impl FrameClock {
        /// The dispatch decision with deadline dispatch forced on and the shipped margin, without
        /// touching the process-wide dials the real path reads.
        fn held(&self, now: Duration) -> Dispatch {
            self.dispatch_from_with(
                now,
                true,
                Duration::from_micros(RENDER_TIME_MARGIN_DEFAULT_US),
            )
        }
    }

    /// Set up a clock that has presented at `last` and knows frames cost `cost`.
    fn continuous(last: Duration, cost: Duration) -> FrameClock {
        let mut c = clock();
        c.presented(last);
        c.record_render_time(cost);
        c
    }

    /// The continuous case, which is the only one deadline dispatch touches: hold the frame until
    /// the vblank minus what it costs, and aim it at that vblank.
    #[test]
    fn a_continuous_frame_is_held_until_its_deadline() {
        let last = Duration::from_secs(10);
        let c = continuous(last, Duration::from_millis(4));

        // A moment after the presentation: the next vblank is one interval out.
        let now = last + Duration::from_micros(200);
        assert_eq!(
            c.held(now),
            Dispatch::At {
                at: last + REFRESH - Duration::from_millis(5),
                target: last + REFRESH,
            },
            "4ms of frame plus the 1ms constant, held off the vblank it aims at",
        );
    }

    /// The deadline must not be so late that the frame it releases cannot make the vblank — the
    /// point where it lands is exactly the point where the target would otherwise move on.
    #[test]
    fn the_deadline_lands_where_the_target_would_slip() {
        let last = Duration::from_secs(10);
        let c = continuous(last, Duration::from_millis(4));

        let Dispatch::At { at, target } = c.held(last + Duration::from_micros(200)) else {
            panic!("expected a held frame");
        };
        assert_eq!(
            c.reachable(target, at, REFRESH),
            target,
            "the frame released at the deadline must still reach the vblank it was aimed at",
        );
    }

    /// After an idle period, dispatch immediately — mutter's `should_update_now` short-circuit.
    /// This is the dominant miss population on a live seat, and it is untouched by all of the
    /// above by design.
    #[test]
    fn an_idle_period_dispatches_at_once() {
        let last = Duration::from_secs(10);
        let c = continuous(last, Duration::from_millis(4));

        // Five cycles of nothing, then something to draw.
        let now = last + REFRESH * 5 + Duration::from_millis(1);
        assert_eq!(
            c.dispatch_from(now),
            Dispatch::Now {
                target: last + REFRESH * 6
            },
        );
    }

    /// A clock that has measured nothing has no business guessing a deadline: guessing low costs
    /// the vblank the deadline was supposed to protect.
    #[test]
    fn an_unmeasured_clock_dispatches_at_once() {
        let last = Duration::from_secs(10);
        let mut c = clock();
        c.presented(last);
        assert_eq!(
            c.dispatch_from(last + Duration::from_micros(200)),
            Dispatch::Now {
                target: last + REFRESH
            },
        );
    }

    /// Being past the deadline already is just "we are late" — go now, and let `reachable` decide
    /// which vblank is still in front of us.
    #[test]
    fn a_deadline_already_passed_dispatches_at_once() {
        let last = Duration::from_secs(10);
        let c = continuous(last, Duration::from_millis(14));

        let now = last + Duration::from_millis(3);
        assert_eq!(
            c.dispatch_from(now),
            Dispatch::Now {
                target: last + REFRESH * 2
            },
            "a 14ms frame with 13.6ms left is late for this vblank and holds for none of it",
        );
    }

    /// Nothing to pace against means nothing to hold for.
    #[test]
    fn a_clock_without_a_cadence_dispatches_at_once() {
        let c = clock();
        assert!(matches!(
            c.dispatch_from(Duration::from_secs(10)),
            Dispatch::Now { .. }
        ));
    }

    /// The fallback timer must not inherit the advance. It stands in for a vblank that happens
    /// whether or not we had anything to put in it, so pacing it by what a frame *would* have
    /// cost would slow the no-draw cadence — and with `unfinished_animations_remain`, that
    /// cadence is what ticks the animation.
    #[test]
    fn the_vblank_estimate_ignores_what_a_frame_costs() {
        let mut c = clock();
        // Seed a last presentation so both paths do real arithmetic rather than returning `now`,
        // and pin `now` — every question below is about the *same* instant, and asking the real
        // clock per call means two of them can land either side of a vblank.
        let now = Duration::from_secs(1000);
        c.presented(now);
        let now = now + REFRESH / 2;
        let unloaded = c.next_target_from(now, false);

        c.record_render_time(REFRESH * 2);
        assert_eq!(
            c.next_target_from(now, false),
            unloaded,
            "a slow frame moved the display's own vblank estimate",
        );
        assert!(
            c.next_target_from(now, true) > unloaded,
            "…while the frame's target must have moved past the vblank it cannot reach",
        );
    }
}
