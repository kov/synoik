// SPDX-License-Identifier: GPL-3.0-only

//! Pressure barriers: an edge only triggers once the pointer has *pushed* into it.
//!
//! Port of gnome-shell's `PressureBarrier` (`js/ui/layout.js:1267-1408`). GNOME builds these on
//! mutter's `MetaBarrier`, which physically blocks the pointer and reports the motion it swallowed.
//! We have no barrier machinery, but we don't need it at the edges of the screen:
//! `on_pointer_motion` already clamps the pointer to the output, so the motion the clamp threw away
//! *is* the pressure. See [`Barrier::push`] for the part of the model that doesn't survive the
//! translation.

use std::collections::VecDeque;
use std::time::Duration;

use smithay::utils::{Logical, Point, Size};

/// `HOT_CORNER_PRESSURE_THRESHOLD` (`layout.js:24`): pixels of inward push needed to trigger.
pub const HOT_CORNER_THRESHOLD: f64 = 100.;

/// `HOT_CORNER_PRESSURE_TIMEOUT` (`layout.js:25`): the window over which that push must happen.
pub const HOT_CORNER_TIMEOUT: Duration = Duration::from_millis(1000);

/// Per-event cap on accumulated pressure (`layout.js:1401`). One violent flick shouldn't spend the
/// whole budget; the barrier wants sustained intent.
const MAX_DISTANCE_PER_EVENT: f64 = 15.;

/// One motion event's push against a barrier, decomposed in the barrier's own frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Push {
    /// Distance moved *into* the barrier (perpendicular), i.e. what the clamp swallowed.
    pub across: f64,
    /// Distance moved *along* the barrier (parallel).
    pub along: f64,
    /// Event timestamp.
    pub time: Duration,
}

/// Which edge of an output a [`Segment`] runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// Whether motion along this edge is horizontal.
    fn is_horizontal(self) -> bool {
        matches!(self, Edge::Top | Edge::Bottom)
    }
}

/// One straight run of an output's edge that a [`Barrier`] listens on, in that output's local
/// logical coordinates. A hot corner is two of these (`layout.js:1195-1233`); a full-width edge
/// trigger is one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub edge: Edge,
    /// Where the segment starts along the edge — x for [`Edge::Top`]/[`Edge::Bottom`], y
    /// otherwise.
    pub from: f64,
    /// Where it ends, exclusive of nothing in particular: the pointer is clamped to whole pixels
    /// of the output, so an inclusive test is what the caller wants.
    pub to: f64,
}

impl Segment {
    /// A segment `len` px long, running from the start of the edge.
    pub fn from_start(edge: Edge, len: f64) -> Self {
        Self {
            edge,
            from: 0.,
            to: len,
        }
    }

    /// Whether the pointer is resting against this segment. `pos` is the pointer position within
    /// the output *after* clamping; the clamp lands it on the last pixel, hence the `- 1.`.
    pub fn contains(&self, output_size: Size<f64, Logical>, pos: Point<f64, Logical>) -> bool {
        let (against, along) = match self.edge {
            Edge::Left => (pos.x <= 0., pos.y),
            Edge::Right => (pos.x >= output_size.w - 1., pos.y),
            Edge::Top => (pos.y <= 0., pos.x),
            Edge::Bottom => (pos.y >= output_size.h - 1., pos.x),
        };
        against && (self.from..=self.to).contains(&along)
    }

    /// The push this motion makes against the segment, or `None` if the pointer isn't against it
    /// or isn't pushing outward.
    ///
    /// `discarded` is the motion the output clamp threw away (unclamped minus clamped position) —
    /// our stand-in for the delta mutter's barrier reports when it blocks a crossing.
    pub fn push(
        &self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        discarded: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        time: Duration,
    ) -> Option<Push> {
        if !self.contains(output_size, pos) {
            return None;
        }

        // Only motion *into* the edge counts; the clamp can also fire on the opposite axis.
        let across = match self.edge {
            Edge::Left => -discarded.x,
            Edge::Right => discarded.x,
            Edge::Top => -discarded.y,
            Edge::Bottom => discarded.y,
        };
        if across <= 0. {
            return None;
        }

        let along = if self.edge.is_horizontal() {
            delta.x.abs()
        } else {
            delta.y.abs()
        };

        Some(Push {
            across,
            along,
            time,
        })
    }
}

/// Accumulates inward pressure against one logical barrier (which may be several edge segments)
/// and fires once it exceeds the threshold within the timeout window.
#[derive(Debug)]
pub struct Barrier {
    threshold: f64,
    timeout: Duration,
    /// `(time, distance)` pairs still inside the timeout window, oldest first.
    events: VecDeque<(Duration, f64)>,
    pressure: f64,
    last_time: Duration,
    /// Set on trigger, cleared by [`Barrier::leave`]. Keeps a pointer that is resting against a
    /// triggered barrier from re-triggering it every frame (`layout.js:1375-1377`).
    triggered: bool,
    /// Whether the pointer was against the barrier as of the last event.
    engaged: bool,
}

impl Barrier {
    pub fn new(threshold: f64, timeout: Duration) -> Self {
        Self {
            threshold,
            timeout,
            events: VecDeque::new(),
            pressure: 0.,
            last_time: Duration::ZERO,
            triggered: false,
            engaged: false,
        }
    }

    /// A hot corner's barrier: 100 px of push within 1 s.
    pub fn hot_corner() -> Self {
        Self::new(HOT_CORNER_THRESHOLD, HOT_CORNER_TIMEOUT)
    }

    /// Feed one push against the barrier. Returns `true` on the event that trips it.
    ///
    /// The one piece of mutter's model we can't reproduce: a real barrier reports a hit for the
    /// event that *first* runs into it, carrying the full delta of the attempted crossing. Our
    /// pressure is whatever the output clamp discarded, so the event that merely arrives at the
    /// edge contributes nothing and only the pushes after it count. With a 100 px threshold that
    /// costs at most one event's worth of travel.
    pub fn push(&mut self, push: Push) -> bool {
        self.engaged = true;

        // Wait for the pointer to leave before this barrier can fire again.
        if self.triggered {
            return false;
        }

        // A single decisive shove trips it outright (`layout.js:1385-1388`).
        if push.across >= self.threshold {
            self.trigger();
            return true;
        }

        // Sliding along the edge is not pushing into it: someone travelling to a corner across the
        // top of the screen shouldn't arrive with the barrier already half spent.
        if push.along > push.across {
            return false;
        }

        self.last_time = push.time;
        self.trim();

        let distance = push.across.min(MAX_DISTANCE_PER_EVENT);
        self.events.push_back((push.time, distance));
        self.pressure += distance;

        if self.pressure >= self.threshold {
            self.trigger();
            return true;
        }

        false
    }

    /// Trip the barrier outright, subject to its latch — for callers that know the pointer is at
    /// the trigger but can't measure a push (see [`crate::synoik::Synoik::touch_hot_corner`]).
    pub fn shove(&mut self, time: Duration) -> bool {
        self.push(Push {
            across: self.threshold,
            along: 0.,
            time,
        })
    }

    /// The pointer is no longer against the barrier: forget the pressure it built, and re-arm.
    pub fn leave(&mut self) {
        if !self.engaged && !self.triggered && self.events.is_empty() {
            return;
        }
        self.engaged = false;
        self.triggered = false;
        self.reset();
    }

    /// Whether the pointer was against the barrier as of the last event.
    pub fn is_engaged(&self) -> bool {
        self.engaged
    }

    #[cfg(test)]
    fn pressure(&self) -> f64 {
        self.pressure
    }

    fn trigger(&mut self) {
        self.triggered = true;
        self.reset();
    }

    fn reset(&mut self) {
        self.events.clear();
        self.pressure = 0.;
        self.last_time = Duration::ZERO;
    }

    /// Drop events that fell out of the timeout window, refunding their pressure.
    fn trim(&mut self) {
        let cutoff = self.last_time.saturating_sub(self.timeout);
        while let Some(&(time, distance)) = self.events.front() {
            if time >= cutoff {
                break;
            }
            self.pressure -= distance;
            self.events.pop_front();
        }
        // Nothing guarantees the arithmetic lands back on zero exactly.
        self.pressure = self.pressure.max(0.);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(across: f64, at_ms: u64) -> Push {
        Push {
            across,
            along: 0.,
            time: Duration::from_millis(at_ms),
        }
    }

    #[test]
    fn a_gentle_nudge_does_not_trigger() {
        let mut b = Barrier::hot_corner();
        assert!(!b.push(push(5., 0)));
        assert!(!b.push(push(5., 16)));
        assert_eq!(b.pressure(), 10.);
    }

    #[test]
    fn sustained_pushing_triggers() {
        let mut b = Barrier::hot_corner();
        // 10 px per event, one event per frame: 100 px lands on the tenth.
        for i in 0..9 {
            assert!(!b.push(push(10., i * 16)));
        }
        assert!(b.push(push(10., 9 * 16)));
    }

    #[test]
    fn one_decisive_shove_triggers_immediately() {
        let mut b = Barrier::hot_corner();
        assert!(b.push(push(120., 0)));
    }

    /// The per-event cap (`layout.js:1401`) means near-threshold shoves still take several events.
    #[test]
    fn a_single_event_is_capped_at_fifteen() {
        let mut b = Barrier::hot_corner();
        assert!(!b.push(push(99., 0)));
        assert_eq!(b.pressure(), 15.);
    }

    #[test]
    fn pressure_decays_out_of_the_timeout_window() {
        let mut b = Barrier::hot_corner();
        for i in 0..6 {
            assert!(!b.push(push(15., i * 100)));
        }
        assert_eq!(b.pressure(), 90.);

        // Two seconds later every one of those has aged out; the new push stands alone.
        assert!(!b.push(push(15., 2500)));
        assert_eq!(b.pressure(), 15.);
    }

    #[test]
    fn sliding_along_the_barrier_is_not_pressure() {
        let mut b = Barrier::hot_corner();
        for i in 0..20 {
            let p = Push {
                across: 5.,
                along: 40.,
                time: Duration::from_millis(i * 16),
            };
            assert!(!b.push(p));
        }
        assert_eq!(b.pressure(), 0.);
    }

    #[test]
    fn a_triggered_barrier_stays_latched_until_the_pointer_leaves() {
        let mut b = Barrier::hot_corner();
        assert!(b.push(push(120., 0)));

        // Resting against it, still pushing: no second trigger.
        for i in 1..20 {
            assert!(!b.push(push(120., i * 16)));
        }

        b.leave();
        assert!(b.push(push(120., 1000)));
    }

    #[test]
    fn leaving_forgets_accumulated_pressure() {
        let mut b = Barrier::hot_corner();
        for i in 0..6 {
            assert!(!b.push(push(15., i * 16)));
        }
        assert_eq!(b.pressure(), 90.);

        b.leave();
        assert!(!b.push(push(15., 200)));
        assert_eq!(b.pressure(), 15.);
    }
}
