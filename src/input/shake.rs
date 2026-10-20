// SPDX-License-Identifier: GPL-3.0-only

//! Shake-to-locate: a pointer shaken back and forth grows, so it can be found on a large or busy
//! screen, and shrinks back once the shaking stops.
//!
//! GNOME has no such thing; the Jiggle extension and macOS do. The extension (`history.js`) sums
//! the turning angle over a 500 ms window of pointer samples taken every 10 ms and triggers past
//! 500°, about three reversals. Summed angles also grow on a wobbly long sweep, and our samples
//! come per event rather than per tick, so this counts reversals directly: a turn back on one
//! axis after travelling at least [`STROKE`] the other way. The window and the count are the
//! extension's; so are the growth to three times the size, eased out over 250 ms, and the
//! shrink, eased in over 150 ms (`ScalingEffect.js`).

use std::collections::VecDeque;
use std::time::Duration;

use smithay::utils::{Logical, Point};

use crate::animation::{Animation, Clock, Curve};

/// How far, in logical pixels, the pointer travels one way before turning back counts.
const STROKE: f64 = 40.;

/// How many reversals on one axis within [`WINDOW`] make a shake.
const REVERSALS: usize = 3;

/// How far back reversals count, and how long a pause forgets a half-made stroke.
pub const WINDOW: Duration = Duration::from_millis(500);

/// How big a shaken pointer grows.
pub const MAX_ZOOM: f64 = 3.;

const GROW_MS: u64 = 250;
const SHRINK_MS: u64 = 150;

/// One axis's turns: which way the pointer is going and the furthest it got that way.
#[derive(Debug, Default)]
struct Axis {
    /// -1 or 1 once a stroke is under way, 0 before.
    direction: i8,
    /// Where the stroke so far reached; before one, where the pointer started.
    extreme: f64,
    reversals: VecDeque<Duration>,
}

impl Axis {
    fn restart(&mut self, at: f64) {
        self.direction = 0;
        self.extreme = at;
    }

    fn push(&mut self, at: f64, now: Duration) {
        let travel = at - self.extreme;
        match self.direction {
            0 => {
                if travel.abs() >= STROKE {
                    self.direction = if travel > 0. { 1 } else { -1 };
                    self.extreme = at;
                }
            }
            direction => {
                let forward = travel * f64::from(direction);
                if forward > 0. {
                    self.extreme = at;
                } else if -forward >= STROKE {
                    self.direction = -direction;
                    self.extreme = at;
                    self.reversals.push_back(now);
                }
            }
        }
    }

    fn reversals_since(&mut self, since: Duration) -> usize {
        while self.reversals.front().is_some_and(|t| *t < since) {
            self.reversals.pop_front();
        }
        self.reversals.len()
    }
}

/// The detector and the zoom it drives.
#[derive(Debug, Default)]
pub struct Shake {
    x: Axis,
    y: Axis,
    last_motion: Option<Duration>,
    /// The zoom on its way to, or back from, [`MAX_ZOOM`]; `None` at rest.
    zoom: Option<Animation>,
}

impl Shake {
    /// The pointer moved to `pos` at `now` (the compositor's unadjusted clock). Returns whether
    /// this started the pointer growing — the caller then owes a redraw and a check, in
    /// [`WINDOW`], on whether it still is shaking.
    pub fn motion(&mut self, now: Duration, pos: Point<f64, Logical>, clock: &Clock) -> bool {
        if self
            .last_motion
            .is_none_or(|last| now.saturating_sub(last) > WINDOW)
        {
            self.x.restart(pos.x);
            self.y.restart(pos.y);
        }
        self.last_motion = Some(now);
        self.x.push(pos.x, now);
        self.y.push(pos.y, now);

        if self.is_grown() || !self.is_shaking(now) {
            return false;
        }
        let from = self.zoom();
        self.zoom = Some(Animation::ease(
            clock.clone(),
            from,
            MAX_ZOOM,
            0.,
            GROW_MS,
            Curve::EaseOutQuad,
        ));
        true
    }

    /// Whether enough reversals happened within the last [`WINDOW`].
    pub fn is_shaking(&mut self, now: Duration) -> bool {
        let since = now.saturating_sub(WINDOW);
        self.x.reversals_since(since) >= REVERSALS || self.y.reversals_since(since) >= REVERSALS
    }

    /// Shrink back if the shaking stopped. Returns whether the pointer is still held grown —
    /// the caller then checks again later.
    pub fn check(&mut self, now: Duration, clock: &Clock) -> bool {
        if !self.is_grown() {
            return false;
        }
        if self.is_shaking(now) {
            return true;
        }
        self.zoom = Some(Animation::ease(
            clock.clone(),
            self.zoom(),
            1.,
            0.,
            SHRINK_MS,
            Curve::EaseInQuad,
        ));
        false
    }

    /// Whether the zoom is headed for [`MAX_ZOOM`] or there.
    fn is_grown(&self) -> bool {
        self.zoom.as_ref().is_some_and(|zoom| zoom.to() > 1.)
    }

    /// How much bigger than normal the pointer is drawn.
    pub fn zoom(&self) -> f64 {
        self.zoom.as_ref().map_or(1., |zoom| zoom.value().max(1.))
    }

    /// Whether the zoom is changing, so frames are owed.
    pub fn is_animating(&self) -> bool {
        self.zoom.as_ref().is_some_and(|zoom| !zoom.is_done())
    }

    /// Forget everything and draw the pointer at its size: the setting went off.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// Feed `points` 10 ms apart, as a 100 Hz mouse would, from `start`. Returns whether any
    /// started the growth.
    fn feed(shake: &mut Shake, clock: &Clock, start: u64, points: &[(f64, f64)]) -> bool {
        let mut grew = false;
        for (i, (x, y)) in points.iter().enumerate() {
            grew |= shake.motion(ms(start + 10 * i as u64), Point::from((*x, *y)), clock);
        }
        grew
    }

    /// Back and forth along x, `amplitude` either side of 500, `strokes` times, each stroke
    /// `steps` events long.
    fn zigzag(amplitude: f64, strokes: usize, steps: usize) -> Vec<(f64, f64)> {
        let mut points = Vec::new();
        for stroke in 0..strokes {
            let (from, to) = if stroke % 2 == 0 {
                (-amplitude, amplitude)
            } else {
                (amplitude, -amplitude)
            };
            for step in 0..steps {
                let t = (step + 1) as f64 / steps as f64;
                points.push((500. + from + (to - from) * t, 300.));
            }
        }
        points
    }

    #[test]
    fn a_quick_shake_grows_the_pointer() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut shake = Shake::default();
        // Four 100 px strokes of 80 ms each: three reversals in 320 ms.
        assert!(feed(&mut shake, &clock, 1000, &zigzag(50., 4, 8)));
        assert!(shake.is_animating());
    }

    /// The same shape, slowly: three reversals over more than the window is not a shake.
    #[test]
    fn a_slow_back_and_forth_does_not() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut shake = Shake::default();
        assert!(!feed(&mut shake, &clock, 1000, &zigzag(50., 4, 30)));
        assert_eq!(shake.zoom(), 1.);
    }

    /// A fast sweep across the screen, even a wobbly one, has no reversals.
    #[test]
    fn a_fast_sweep_does_not() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut shake = Shake::default();
        let sweep: Vec<_> = (0..40)
            .map(|i| {
                (
                    f64::from(i) * 40.,
                    300. + if i % 2 == 0 { 15. } else { -15. },
                )
            })
            .collect();
        assert!(!feed(&mut shake, &clock, 1000, &sweep));
    }

    /// Jitter smaller than a stroke is not a shake, however fast.
    #[test]
    fn small_jitter_does_not() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut shake = Shake::default();
        assert!(!feed(&mut shake, &clock, 1000, &zigzag(15., 20, 2)));
    }

    /// Grown, it stays grown while the shaking goes on, and shrinks once it stops.
    #[test]
    fn the_pointer_shrinks_after_the_shaking_stops() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut shake = Shake::default();
        assert!(feed(&mut shake, &clock, 1000, &zigzag(50., 4, 8)));

        clock.set_unadjusted(ms(1000 + 320 + 300));
        assert_eq!(shake.zoom(), MAX_ZOOM, "fully grown");
        assert!(shake.check(ms(1320), &clock), "still shaking just after");

        assert!(!shake.check(ms(1320) + WINDOW, &clock), "stopped");
        assert!(shake.is_animating());
        clock.set_unadjusted(ms(1320 + 300 + 500 + SHRINK_MS));
        assert_eq!(shake.zoom(), 1.);
        assert!(!shake.is_animating());
    }
}
