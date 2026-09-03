// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use crate::utils::get_monotonic_time;

/// Shareable lazy clock that can change rate.
///
/// The clock will fetch the time once and then retain it until explicitly cleared with
/// [`Clock::clear`].
#[derive(Debug, Default, Clone)]
pub struct Clock {
    inner: Rc<RefCell<AdjustableClock>>,
}

#[derive(Debug, Default)]
struct LazyClock {
    time: Option<Duration>,
    /// While set, [`clear`](Self::clear) does nothing, so the pinned time survives the end of an
    /// event-loop iteration instead of being re-fetched from the monotonic clock. See
    /// [`Clock::freeze`].
    frozen: bool,
    /// Added to every monotonic read, so that leaving a freeze cannot move time **backwards**.
    /// A freeze that drove the clock past real time (a test stepping frame by frame faster than
    /// wall time, which is every test) would otherwise rewind on unfreeze, and an animation that
    /// had finished would read as unfinished again — for as many seconds as the test had
    /// fast-forwarded. Zero in a session, where nothing freezes.
    offset: Duration,
}

/// Clock that can adjust its rate.
#[derive(Debug)]
struct AdjustableClock {
    inner: LazyClock,
    current_time: Duration,
    last_seen_time: Duration,
    rate: f64,
    complete_instantly: bool,
}

impl Clock {
    /// Creates a new clock with the given time.
    pub fn with_time(time: Duration) -> Self {
        let clock = AdjustableClock::new(LazyClock::with_time(time));
        Self {
            inner: Rc::new(RefCell::new(clock)),
        }
    }

    /// Returns the current time.
    pub fn now(&self) -> Duration {
        self.inner.borrow_mut().now()
    }

    /// Returns the underlying time not adjusted for rate change.
    pub fn now_unadjusted(&self) -> Duration {
        self.inner.borrow_mut().inner.now()
    }

    /// Sets the unadjusted clock time.
    pub fn set_unadjusted(&mut self, time: Duration) {
        self.inner.borrow_mut().inner.set(time);
    }

    /// Clears the stored time so it's re-fetched again next.
    pub fn clear(&mut self) {
        self.inner.borrow_mut().inner.clear();
    }

    /// Pin the clock at its current time until [`unfreeze`](Self::unfreeze), so that time advances
    /// **only** through [`set_unadjusted`](Self::set_unadjusted).
    ///
    /// Normally the compositor clears the lazy time at the end of every event-loop iteration
    /// (`Synoik::refresh`), so the next read comes from the monotonic clock and animations advance
    /// by real elapsed wall time. That is right in a session and wrong in a test: a headless test
    /// that round-trips a client mid-animation then advances by however long that round trip
    /// happened to take, which on a loaded machine is enough to finish the animation and walk
    /// straight past the frames under test. Freezing makes the sampled instant a property of the
    /// test rather than of the machine.
    ///
    /// Intended for tests; nothing in a session freezes the clock. The compositor's own writer
    /// — `Synoik::redraw`, pinning the clock at each frame's target presentation time — checks
    /// [`is_frozen`](Self::is_frozen) and stands down, since that target is derived from real
    /// time and would put the machine back in charge of a clock a test had taken over.
    pub fn freeze(&mut self) {
        let mut clock = self.inner.borrow_mut();
        // Materialize the current time first: freezing an unset lazy clock would otherwise pin it
        // at whatever the *next* reader happened to fetch.
        let now = clock.inner.now();
        clock.inner.set(now);
        clock.inner.frozen = true;
    }

    /// Whether the clock is frozen. A helper that freezes temporarily reads this first so it can
    /// leave the clock as it found it — a test that froze deliberately must not be un-frozen by a
    /// helper it called in the middle.
    pub fn is_frozen(&self) -> bool {
        self.inner.borrow().inner.frozen
    }

    /// Let the clock follow the monotonic clock again. See [`freeze`](Self::freeze).
    ///
    /// Time never goes backwards across this: a freeze that stepped past real time keeps its lead
    /// as a fixed offset on every later read.
    pub fn unfreeze(&mut self) {
        let mut clock = self.inner.borrow_mut();
        let pinned = clock.inner.now();
        let following = get_monotonic_time() + clock.inner.offset;
        clock.inner.offset += pinned.saturating_sub(following);
        clock.inner.frozen = false;
        clock.inner.clear();
    }

    /// Gets the clock rate.
    pub fn rate(&self) -> f64 {
        self.inner.borrow().rate()
    }

    /// Sets the clock rate.
    pub fn set_rate(&mut self, rate: f64) {
        self.inner.borrow_mut().set_rate(rate);
    }

    /// Returns whether animations should complete instantly.
    pub fn should_complete_instantly(&self) -> bool {
        self.inner.borrow().should_complete_instantly()
    }

    /// Sets whether animations should complete instantly.
    pub fn set_complete_instantly(&mut self, value: bool) {
        self.inner.borrow_mut().set_complete_instantly(value);
    }
}

impl PartialEq for Clock {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for Clock {}

impl LazyClock {
    pub fn with_time(time: Duration) -> Self {
        Self {
            time: Some(time),
            frozen: false,
            offset: Duration::ZERO,
        }
    }

    pub fn clear(&mut self) {
        if !self.frozen {
            self.time = None;
        }
    }

    pub fn set(&mut self, time: Duration) {
        self.time = Some(time);
    }

    pub fn now(&mut self) -> Duration {
        let offset = self.offset;
        *self
            .time
            .get_or_insert_with(|| get_monotonic_time() + offset)
    }
}

impl AdjustableClock {
    pub fn new(mut inner: LazyClock) -> Self {
        let time = inner.now();
        Self {
            inner,
            current_time: time,
            last_seen_time: time,
            rate: 1.,
            complete_instantly: false,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn set_rate(&mut self, rate: f64) {
        self.rate = rate.clamp(0., 1000.);
    }

    pub fn should_complete_instantly(&self) -> bool {
        self.complete_instantly
    }

    pub fn set_complete_instantly(&mut self, value: bool) {
        self.complete_instantly = value;
    }

    pub fn now(&mut self) -> Duration {
        let time = self.inner.now();

        if self.last_seen_time == time {
            return self.current_time;
        }

        if self.last_seen_time < time {
            let delta = time - self.last_seen_time;
            let delta = delta.mul_f64(self.rate);
            self.current_time = self.current_time.saturating_add(delta);
        } else {
            let delta = self.last_seen_time - time;
            let delta = delta.mul_f64(self.rate);
            self.current_time = self.current_time.saturating_sub(delta);
        }

        self.last_seen_time = time;
        self.current_time
    }
}

impl Default for AdjustableClock {
    fn default() -> Self {
        Self::new(LazyClock::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_clock() {
        let mut clock = Clock::with_time(Duration::ZERO);
        assert_eq!(clock.now(), Duration::ZERO);

        clock.set_unadjusted(Duration::from_millis(100));
        assert_eq!(clock.now(), Duration::from_millis(100));

        clock.set_unadjusted(Duration::from_millis(200));
        assert_eq!(clock.now(), Duration::from_millis(200));
    }

    /// A test drives the clock forward far faster than wall time; coming out of a freeze must not
    /// hand back the seconds it skipped, or every animation it just finished starts running again.
    #[test]
    fn unfreezing_never_moves_time_backwards() {
        let mut clock = Clock::default();
        clock.freeze();
        let ahead = clock.now_unadjusted() + Duration::from_secs(30);
        clock.set_unadjusted(ahead);
        clock.unfreeze();

        clock.clear();
        assert!(
            clock.now_unadjusted() >= ahead,
            "the clock rewound out of a freeze"
        );
    }

    #[test]
    fn rate_change() {
        let mut clock = Clock::with_time(Duration::ZERO);
        clock.set_rate(0.5);

        clock.set_unadjusted(Duration::from_millis(100));
        assert_eq!(clock.now_unadjusted(), Duration::from_millis(100));
        assert_eq!(clock.now(), Duration::from_millis(50));

        clock.set_unadjusted(Duration::from_millis(200));
        assert_eq!(clock.now_unadjusted(), Duration::from_millis(200));
        assert_eq!(clock.now(), Duration::from_millis(100));

        clock.set_unadjusted(Duration::from_millis(150));
        assert_eq!(clock.now_unadjusted(), Duration::from_millis(150));
        assert_eq!(clock.now(), Duration::from_millis(75));

        clock.set_rate(2.0);

        clock.set_unadjusted(Duration::from_millis(250));
        assert_eq!(clock.now_unadjusted(), Duration::from_millis(250));
        assert_eq!(clock.now(), Duration::from_millis(275));
    }
}
