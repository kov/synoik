// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::collections::VecDeque;
use std::time::Duration;

const HISTORY_LIMIT: Duration = Duration::from_millis(150);
const DECELERATION_TOUCHPAD: f64 = 0.997;

#[derive(Debug)]
pub struct SwipeTracker {
    history: VecDeque<Event>,
    pos: f64,
}

#[derive(Debug, Clone, Copy)]
struct Event {
    delta: f64,
    timestamp: Duration,
}

impl SwipeTracker {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            pos: 0.,
        }
    }

    /// Pushes a new reading into the tracker.
    pub fn push(&mut self, delta: f64, timestamp: Duration) {
        // For the events that we care about, timestamps should always increase
        // monotonically.
        if let Some(last) = self.history.back() {
            if timestamp < last.timestamp {
                trace!(
                    "ignoring event with timestamp {timestamp:?} earlier than last {:?}",
                    last.timestamp
                );
                return;
            }
        }

        self.history.push_back(Event { delta, timestamp });
        self.pos += delta;

        self.trim_history();
    }

    /// Returns the current gesture position.
    pub fn pos(&self) -> f64 {
        self.pos
    }

    /// Computes the current gesture velocity, per second (`calculateVelocity`,
    /// `swipeTracker.js:69-82`). The first event only marks when the window opens: its delta was
    /// travelled before it, so it is left out of the distance covered within the window.
    pub fn velocity(&self) -> f64 {
        let (Some(first), Some(last)) = (self.history.front(), self.history.back()) else {
            return 0.;
        };

        let total_time = (last.timestamp - first.timestamp).as_secs_f64();
        if total_time == 0. {
            return 0.;
        }

        let total_delta = self
            .history
            .iter()
            .skip(1)
            .map(|event| event.delta)
            .sum::<f64>();
        total_delta / total_time
    }

    /// Ends the gesture at `timestamp`: drops the events that are older than the history window
    /// as of the release, as `_endTouchpadGesture` trims before it reads the velocity
    /// (`swipeTracker.js:668-679`). A release after a pause reads as standing still; one in
    /// motion keeps the speed it was moving at — the lift itself adds no event that would dilute
    /// it.
    pub fn release(&mut self, timestamp: Duration) {
        while let Some(first) = self.history.front() {
            if timestamp <= first.timestamp + HISTORY_LIMIT {
                break;
            }

            let _ = self.history.pop_front();
        }
    }

    /// Computes the gesture end position after decelerating to a halt.
    pub fn projected_end_pos(&self) -> f64 {
        let vel = self.velocity();
        self.pos - vel / (1000. * DECELERATION_TOUCHPAD.ln())
    }

    fn trim_history(&mut self) {
        if let Some(&Event { timestamp, .. }) = self.history.back() {
            self.release(timestamp);
        }
    }
}

/// The release speed, in pixels per millisecond, past which a swipe is a flick that carries on to
/// the next snap point rather than settling on the nearest one (`swipeTracker.js:22-23`).
pub const VELOCITY_THRESHOLD_TOUCH: f64 = 0.3;
pub const VELOCITY_THRESHOLD_TOUCHPAD: f64 = 0.6;

/// Where a released swipe settles over the integer snap points within `bounds` —
/// `_getEndProgress` and `_findPointForProjection` (`swipeTracker.js:536-631`).
///
/// `initial` is the progress the swipe began at, `progress` where it was released, and
/// `velocity` the release speed in pixels per millisecond: the unit the reference compares
/// against its threshold *and* projects with before the projection is added to progress. A flick
/// always moves at least one snap point from where the swipe began, however short it was.
pub fn end_progress(
    initial: f64,
    progress: f64,
    bounds: (f64, f64),
    velocity: f64,
    is_touchpad: bool,
) -> f64 {
    const DECELERATION_TOUCH: f64 = 0.998;
    const VELOCITY_CURVE_THRESHOLD: f64 = 2.;
    const DECELERATION_PARABOLA_MULTIPLIER: f64 = 0.35;

    let (threshold, decel) = if is_touchpad {
        (VELOCITY_THRESHOLD_TOUCHPAD, DECELERATION_TOUCHPAD)
    } else {
        (VELOCITY_THRESHOLD_TOUCH, DECELERATION_TOUCH)
    };

    if velocity.abs() < threshold {
        return progress.round().clamp(bounds.0, bounds.1);
    }

    let slope = decel / (1. - decel) / 1000.;
    let pos = if velocity.abs() > VELOCITY_CURVE_THRESHOLD {
        let c = slope / 2. / DECELERATION_PARABOLA_MULTIPLIER;
        let x = velocity.abs() - VELOCITY_CURVE_THRESHOLD + c;
        slope * VELOCITY_CURVE_THRESHOLD + DECELERATION_PARABOLA_MULTIPLIER * x * x
            - DECELERATION_PARABOLA_MULTIPLIER * c * c
    } else {
        velocity.abs() * slope
    };
    let pos = (pos * velocity.signum() + progress).clamp(bounds.0, bounds.1);

    let initial = initial.round();
    let (prev, next) = (pos.floor(), pos.ceil());
    if velocity > 0. && prev == initial {
        next
    } else if velocity < 0. && next == initial {
        prev
    } else {
        pos.round()
    }
}
