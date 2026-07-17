//! The state machine behind `org.gnome.Mutter.IdleMonitor` (see `dbus::mutter_idle_monitor`).
//!
//! gnome-settings-daemon's power plugin hard-codes this bus name to drive screen dim, blank, and
//! auto-suspend: it adds *idle watches* ("tell me when the user has been idle for N ms") and *user-
//! active watches* ("tell me the moment the user comes back"). The compositor already detects
//! activity (every input event calls [`crate::niri::Niri::notify_activity`]); this turns that into
//! the watch/fire bookkeeping the D-Bus interface exposes.
//!
//! Pure and time-in/fired-out so the semantics are unit-testable without a bus or a clock: every
//! method takes `now` (monotonic, from `Clock::now_unadjusted`) and firing is returned, never a
//! side effect. Semantics are ground truth from mutter's `meta-idle-monitor.c`:
//!
//! - An **idle watch** fires once each time idle reaches its interval, and re-arms only when the
//!   user next becomes active — so it fires at most once per idle period, never repeatedly.
//! - A **user-active watch** fires once the next time the user becomes active, then removes itself.
//! - Adding an idle watch schedules it from the *last activity*, not from now, so one added while
//!   the user is already idle past its interval fires at the next refresh.

use std::collections::HashMap;
use std::time::Duration;

/// A fired watch: its id, and the bus name of the client that owns it (`WatchFired` is unicast to
/// that name, like an accelerator grab's activation signal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub id: u32,
    pub owner: String,
}

#[derive(Debug)]
enum Watch {
    /// Fires when the user has been idle for `interval`. `fired` is whether it has already fired
    /// for the *current* idle period; cleared when the user becomes active.
    Idle {
        interval: Duration,
        owner: String,
        fired: bool,
    },
    /// Fires once the next time the user becomes active, then removes itself.
    UserActive { owner: String },
}

impl Watch {
    fn owner(&self) -> &str {
        match self {
            Watch::Idle { owner, .. } | Watch::UserActive { owner } => owner,
        }
    }
}

#[derive(Debug)]
pub struct IdleMonitor {
    /// Monotonic time of the last activity (or the last `ResetIdletime`).
    last_activity: Duration,
    watches: HashMap<u32, Watch>,
    /// Watch ids are "guaranteed to be greater than zero" (the interface contract), and never
    /// reused, so a client that removes a watch can't collide with a later one.
    next_id: u32,
}

impl IdleMonitor {
    pub fn new(now: Duration) -> Self {
        Self {
            last_activity: now,
            watches: HashMap::new(),
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Milliseconds since the last activity — `GetIdletime`.
    pub fn idletime_ms(&self, now: Duration) -> u64 {
        now.saturating_sub(self.last_activity).as_millis() as u64
    }

    /// `AddIdleWatch`: fire `WatchFired` for `owner` when idle reaches `interval_ms`.
    pub fn add_idle_watch(&mut self, interval_ms: u64, owner: String) -> u32 {
        let id = self.alloc_id();
        self.watches.insert(
            id,
            Watch::Idle {
                interval: Duration::from_millis(interval_ms),
                owner,
                fired: false,
            },
        );
        id
    }

    /// `AddUserActiveWatch`: fire once for `owner` the next time the user becomes active.
    pub fn add_user_active_watch(&mut self, owner: String) -> u32 {
        let id = self.alloc_id();
        self.watches.insert(id, Watch::UserActive { owner });
        id
    }

    /// `RemoveWatch`. Unknown ids are ignored, matching mutter.
    pub fn remove_watch(&mut self, id: u32) {
        self.watches.remove(&id);
    }

    /// Drop every watch owned by `owner` — called when that client drops off the bus, so a crashed
    /// client doesn't leak watches (mutter watches the bus name for the same reason).
    pub fn remove_watches_for_owner(&mut self, owner: &str) {
        self.watches.retain(|_, w| w.owner() != owner);
    }

    /// The user became active (any input, or `ResetIdletime`): reset the idle clock, fire and
    /// remove every user-active watch, and re-arm the idle watches for the next period.
    pub fn on_activity(&mut self, now: Duration) -> Vec<Fired> {
        self.last_activity = now;

        let mut fired = Vec::new();
        self.watches.retain(|&id, watch| match watch {
            Watch::UserActive { owner } => {
                fired.push(Fired {
                    id,
                    owner: owner.clone(),
                });
                false
            }
            Watch::Idle { fired: f, .. } => {
                *f = false;
                true
            }
        });
        fired
    }

    /// Fire any idle watch whose interval has elapsed since the last activity and that has not
    /// already fired this idle period. Idempotent: calling it again without new activity fires
    /// nothing more. Drive it from a timer armed at [`Self::next_wakeup`].
    pub fn refresh(&mut self, now: Duration) -> Vec<Fired> {
        let idle = now.saturating_sub(self.last_activity);
        let mut fired = Vec::new();
        for (&id, watch) in self.watches.iter_mut() {
            if let Watch::Idle {
                interval,
                owner,
                fired: f,
            } = watch
            {
                if !*f && idle >= *interval {
                    *f = true;
                    fired.push(Fired {
                        id,
                        owner: owner.clone(),
                    });
                }
            }
        }
        fired
    }

    /// The earliest monotonic time [`Self::refresh`] would next fire something, so a caller can arm
    /// one timer instead of one per watch. `None` if nothing is pending (no idle watch, or all have
    /// already fired this period). May be in the past — meaning "fire at the next opportunity".
    pub fn next_wakeup(&self) -> Option<Duration> {
        self.watches
            .values()
            .filter_map(|w| match w {
                Watch::Idle {
                    interval,
                    fired: false,
                    ..
                } => Some(self.last_activity + *interval),
                _ => None,
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn owner() -> String {
        ":1.42".to_owned()
    }

    #[test]
    fn idletime_counts_from_last_activity() {
        let mut m = IdleMonitor::new(ms(1000));
        assert_eq!(m.idletime_ms(ms(1000)), 0);
        assert_eq!(m.idletime_ms(ms(3500)), 2500);
        m.on_activity(ms(4000));
        assert_eq!(m.idletime_ms(ms(4200)), 200);
    }

    #[test]
    fn idle_watch_fires_once_per_period_and_rearms_on_activity() {
        let mut m = IdleMonitor::new(ms(0));
        let id = m.add_idle_watch(5000, owner());

        // Not yet idle enough.
        assert!(m.refresh(ms(4999)).is_empty());
        // Crosses the interval: fires once.
        assert_eq!(
            m.refresh(ms(5000)),
            vec![Fired { id, owner: owner() }]
        );
        // Still idle, but it already fired this period: silent.
        assert!(m.refresh(ms(9000)).is_empty());

        // Activity re-arms it for the next period.
        assert!(m.on_activity(ms(10_000)).is_empty());
        assert!(m.refresh(ms(14_999)).is_empty());
        assert_eq!(
            m.refresh(ms(15_000)),
            vec![Fired { id, owner: owner() }]
        );
    }

    #[test]
    fn idle_watch_added_while_already_idle_fires_at_next_refresh() {
        let mut m = IdleMonitor::new(ms(0));
        // No activity since t=0; at t=10s add a 5s watch — already idle past it.
        let id = m.add_idle_watch(5000, owner());
        assert_eq!(
            m.refresh(ms(10_000)),
            vec![Fired { id, owner: owner() }],
            "a watch added while already idle past its interval must fire at the next refresh"
        );
    }

    #[test]
    fn user_active_watch_fires_once_on_activity_then_self_removes() {
        let mut m = IdleMonitor::new(ms(0));
        let id = m.add_user_active_watch(owner());

        // It never fires from idle passing — only from activity.
        assert!(m.refresh(ms(100_000)).is_empty());

        assert_eq!(
            m.on_activity(ms(100_001)),
            vec![Fired { id, owner: owner() }]
        );
        // Self-removed: a second activity does nothing.
        assert!(m.on_activity(ms(100_002)).is_empty());
    }

    #[test]
    fn remove_watch_stops_it_firing() {
        let mut m = IdleMonitor::new(ms(0));
        let id = m.add_idle_watch(1000, owner());
        m.remove_watch(id);
        assert!(m.refresh(ms(5000)).is_empty());
    }

    #[test]
    fn remove_watches_for_owner_drops_only_that_owner() {
        let mut m = IdleMonitor::new(ms(0));
        let a = m.add_idle_watch(1000, ":1.1".to_owned());
        let b = m.add_idle_watch(1000, ":1.2".to_owned());
        m.remove_watches_for_owner(":1.1");
        let fired = m.refresh(ms(2000));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, b);
        assert_ne!(fired[0].id, a);
    }

    #[test]
    fn next_wakeup_is_the_earliest_pending_idle_deadline() {
        let mut m = IdleMonitor::new(ms(1000));
        assert_eq!(m.next_wakeup(), None, "no watches, nothing pending");

        m.add_idle_watch(5000, owner());
        m.add_idle_watch(2000, owner());
        // Earliest deadline = last_activity(1000) + min interval(2000).
        assert_eq!(m.next_wakeup(), Some(ms(3000)));

        // Once the 2s one fires, the next deadline is the 5s one.
        m.refresh(ms(3000));
        assert_eq!(m.next_wakeup(), Some(ms(6000)));
    }

    #[test]
    fn reset_via_on_activity_reschedules_next_wakeup() {
        let mut m = IdleMonitor::new(ms(0));
        m.add_idle_watch(5000, owner());
        assert_eq!(m.next_wakeup(), Some(ms(5000)));
        m.on_activity(ms(2000));
        assert_eq!(m.next_wakeup(), Some(ms(7000)));
    }

    #[test]
    fn ids_are_positive_and_never_reused() {
        let mut m = IdleMonitor::new(ms(0));
        let a = m.add_idle_watch(1000, owner());
        let b = m.add_user_active_watch(owner());
        assert!(a > 0 && b > a);
        m.remove_watch(a);
        let c = m.add_idle_watch(1000, owner());
        assert!(c > b, "ids must not be reused after removal");
    }
}
