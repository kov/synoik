// SPDX-License-Identifier: GPL-3.0-only

//! The compositor's own timers, keyed on its own clock.
//!
//! A calloop `Timer` fires off the machine's monotonic clock, which nothing in the compositor
//! owns. That is the right source for a session and the wrong one for a test: the harness steps
//! [`Clock`](crate::animation::Clock) frame by frame without spending wall-clock, so a calloop
//! timer set for 500 ms simply never comes due — a drag countdown, a key repeat or a deadline
//! could not be reproduced without sleeping for it.
//!
//! So the deadline lives here instead, on the same clock everything else is timed against, and
//! the wheel is dispatched from the loop's turn. calloop's role shrinks to *waking the loop up*:
//! [`Timers::wakeup_in`] says when the loop must next come round, and the caller arms an
//! otherwise-empty source with it. A session waits on that source; a harness pumps the loop with
//! a zero timeout and never needs it. **Both dispatch the same wheel, in the same place in the
//! turn, through the same callbacks** — only who woke the loop differs.
//!
//! Timers are keyed on [`Clock::now_unadjusted`](crate::animation::Clock::now_unadjusted): the
//! rate knob is animation slowdown, and a key repeat or a save deadline must not stretch with it.

use std::time::Duration;

/// A pending timer's handle, for cancelling it before it comes due.
///
/// Stamped with a generation, so cancelling a timer that has already fired (or already been
/// cancelled) is a no-op rather than a cancellation of whoever took the slot next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerToken(u64);

/// What a timer's callback wants next: `None` to be done, `Some(d)` to come due again in `d`.
///
/// The two forms calloop offered (`TimeoutAction::Drop`, `TimeoutAction::ToDuration`), minus the
/// third: an absolute `ToInstant` on a clock the caller does not own is exactly the thing this
/// module exists to remove.
pub type Again = Option<Duration>;

type Callback<D> = Box<dyn FnMut(&mut D) -> Again>;

struct Entry<D> {
    token: TimerToken,
    deadline: Duration,
    callback: Callback<D>,
}

/// Deadlines on the compositor's clock, in the order they come due.
pub struct Timers<D> {
    entries: Vec<Entry<D>>,
    next_token: u64,
    /// Tokens cancelled from inside a callback, while their entry is out of `entries` being
    /// dispatched. Reentrancy is the normal case here — a key repeat cancels the pointer's
    /// cooldown, a transaction's deadline cancels its siblings — so a cancel must be able to name
    /// an entry the wheel is not currently holding.
    cancelled_while_dispatching: Vec<TimerToken>,
    dispatching: bool,
}

impl<D> Default for Timers<D> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_token: 0,
            cancelled_while_dispatching: Vec::new(),
            dispatching: false,
        }
    }
}

impl<D> Timers<D> {
    /// Come due at `deadline`, an instant on the compositor's clock.
    pub fn insert_at(
        &mut self,
        deadline: Duration,
        callback: impl FnMut(&mut D) -> Again + 'static,
    ) -> TimerToken {
        let token = TimerToken(self.next_token);
        self.next_token += 1;
        self.entries.push(Entry {
            token,
            deadline,
            callback: Box::new(callback),
        });
        token
    }

    /// Cancel a pending timer. Cancelling one that already fired is a no-op.
    pub fn cancel(&mut self, token: TimerToken) {
        if self.dispatching {
            self.cancelled_while_dispatching.push(token);
        }
        self.entries.retain(|e| e.token != token);
    }

    /// Whether `token` is still pending.
    pub fn is_pending(&self, token: TimerToken) -> bool {
        self.entries.iter().any(|e| e.token == token)
    }

    /// The earliest deadline, or `None` when nothing is pending.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.entries.iter().map(|e| e.deadline).min()
    }

    /// How long the loop may sleep before it must come round to dispatch — `None` when nothing is
    /// pending and it may sleep indefinitely. Zero when something is already due.
    pub fn wakeup_in(&self, now: Duration) -> Option<Duration> {
        self.next_deadline().map(|d| d.saturating_sub(now))
    }

    /// Run every timer due at `now`, in deadline order, and reschedule the ones that ask for it.
    ///
    /// Takes the wheel by accessor rather than by `&mut self`, because a callback gets the whole
    /// of `data` and the wheel lives *on* it: a timer that arms or cancels another timer — a key
    /// repeat re-arming, a transaction deadline cancelling its siblings — is the normal case, not
    /// the exception. Each entry is lifted out of the wheel before its callback runs, so the wheel
    /// the callback reaches is the real one, mid-dispatch.
    pub fn dispatch_from(data: &mut D, now: Duration, wheel: fn(&mut D) -> &mut Timers<D>) {
        // Collected up front: a callback that re-arms for an instant already past would otherwise
        // spin this loop forever. One firing per timer per dispatch, which is what a calloop timer
        // gave too — anything queued for the same instant waits for the next turn.
        let due: Vec<TimerToken> = {
            let this = wheel(data);
            let mut due: Vec<_> = this
                .entries
                .iter()
                .filter(|e| e.deadline <= now)
                .map(|e| (e.deadline, e.token))
                .collect();
            due.sort_unstable();
            due.into_iter().map(|(_, token)| token).collect()
        };

        for token in due {
            let this = wheel(data);
            let Some(idx) = this.entries.iter().position(|e| e.token == token) else {
                // Cancelled by an earlier callback in this same dispatch.
                continue;
            };
            let mut entry = this.entries.remove(idx);
            this.dispatching = true;

            let again = (entry.callback)(data);

            let this = wheel(data);
            this.dispatching = false;
            let cancelled = this.cancelled_while_dispatching.contains(&entry.token);
            this.cancelled_while_dispatching.clear();

            if let Some(after) = again {
                if !cancelled {
                    entry.deadline = now + after;
                    this.entries.push(entry);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// Stands in for `State`: the wheel lives on the data its callbacks are handed, which is what
    /// lets a timer arm or cancel another one.
    #[derive(Default)]
    struct Harness {
        timers: Timers<Harness>,
        fired: Vec<&'static str>,
    }

    impl Harness {
        fn wheel(&mut self) -> &mut Timers<Harness> {
            &mut self.timers
        }

        fn dispatch(&mut self, now: Duration) {
            Timers::dispatch_from(self, now, Harness::wheel);
        }
    }

    #[test]
    fn a_timer_fires_once_its_deadline_passes_and_not_before() {
        let mut h = Harness::default();
        h.timers.insert_at(10 * MS, |h: &mut Harness| {
            h.fired.push("a");
            None
        });

        h.dispatch(9 * MS);
        assert!(h.fired.is_empty(), "not due yet");
        h.dispatch(10 * MS);
        assert_eq!(h.fired, ["a"]);
        h.dispatch(100 * MS);
        assert_eq!(h.fired, ["a"], "and it does not fire twice");
    }

    #[test]
    fn timers_due_at_once_fire_in_deadline_order() {
        let mut h = Harness::default();
        h.timers.insert_at(30 * MS, |h: &mut Harness| {
            h.fired.push("late");
            None
        });
        h.timers.insert_at(10 * MS, |h: &mut Harness| {
            h.fired.push("early");
            None
        });
        h.timers.insert_at(20 * MS, |h: &mut Harness| {
            h.fired.push("middle");
            None
        });

        h.dispatch(100 * MS);
        assert_eq!(h.fired, ["early", "middle", "late"]);
    }

    #[test]
    fn a_callback_that_asks_again_comes_due_from_the_dispatched_instant() {
        let mut h = Harness::default();
        let mut count = 0;
        h.timers.insert_at(10 * MS, move |h: &mut Harness| {
            count += 1;
            h.fired.push("tick");
            (count < 3).then_some(5 * MS)
        });

        h.dispatch(10 * MS);
        h.dispatch(14 * MS);
        assert_eq!(h.fired.len(), 1, "the repeat is not due until 15ms");
        h.dispatch(15 * MS);
        h.dispatch(20 * MS);
        assert_eq!(h.fired.len(), 3);
        h.dispatch(100 * MS);
        assert_eq!(h.fired.len(), 3, "and it stopped when it said so");
    }

    #[test]
    fn a_cancelled_timer_never_fires() {
        let mut h = Harness::default();
        let token = h.timers.insert_at(10 * MS, |h: &mut Harness| {
            h.fired.push("cancelled");
            None
        });
        assert!(h.timers.is_pending(token));
        h.timers.cancel(token);
        assert!(!h.timers.is_pending(token));

        h.dispatch(100 * MS);
        assert!(h.fired.is_empty());
        // Cancelling again, or cancelling one that already fired, is a no-op and not a panic.
        h.timers.cancel(token);
    }

    #[test]
    fn a_callback_can_cancel_a_timer_that_would_fire_in_the_same_dispatch() {
        let mut h = Harness::default();
        let victim = h.timers.insert_at(20 * MS, |h: &mut Harness| {
            h.fired.push("victim");
            None
        });
        h.timers.insert_at(10 * MS, move |h: &mut Harness| {
            h.fired.push("first");
            h.timers.cancel(victim);
            None
        });

        h.dispatch(100 * MS);
        assert_eq!(
            h.fired,
            ["first"],
            "the victim was cancelled before its turn"
        );
    }

    #[test]
    fn a_callback_can_cancel_itself_and_is_not_rescheduled() {
        let mut h = Harness::default();
        // The entry is out of the wheel while it runs, so cancelling its own token has nothing to
        // remove — the cancellation has to be remembered and beat the reschedule.
        let own = std::rc::Rc::new(std::cell::Cell::new(None));
        let seen = own.clone();
        let token = h.timers.insert_at(10 * MS, move |h: &mut Harness| {
            h.fired.push("once");
            h.timers.cancel(seen.get().unwrap());
            Some(5 * MS)
        });
        own.set(Some(token));

        h.dispatch(10 * MS);
        h.dispatch(100 * MS);
        assert_eq!(h.fired, ["once"]);
    }

    #[test]
    fn a_callback_can_arm_a_new_timer() {
        let mut h = Harness::default();
        h.timers.insert_at(10 * MS, |h: &mut Harness| {
            h.fired.push("first");
            h.timers.insert_at(20 * MS, |h: &mut Harness| {
                h.fired.push("armed by the first");
                None
            });
            None
        });

        h.dispatch(10 * MS);
        assert_eq!(h.fired, ["first"]);
        h.dispatch(20 * MS);
        assert_eq!(h.fired, ["first", "armed by the first"]);
    }

    #[test]
    fn the_wakeup_is_the_earliest_deadline_and_nothing_when_idle() {
        let mut h = Harness::default();
        assert_eq!(
            h.timers.wakeup_in(Duration::ZERO),
            None,
            "an idle loop may sleep"
        );

        h.timers.insert_at(30 * MS, |_: &mut Harness| None);
        let token = h.timers.insert_at(10 * MS, |_: &mut Harness| None);
        assert_eq!(h.timers.wakeup_in(Duration::ZERO), Some(10 * MS));
        assert_eq!(
            h.timers.wakeup_in(50 * MS),
            Some(Duration::ZERO),
            "already due: come round now"
        );

        h.timers.cancel(token);
        assert_eq!(h.timers.wakeup_in(Duration::ZERO), Some(30 * MS));
    }
}
