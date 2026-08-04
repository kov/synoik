//! The state machine behind `org.gnome.SessionManager.EndSessionDialog` (see
//! `dbus::gnome_session`).
//!
//! When the user asks to log out, power off, or restart, gnome-session doesn't do it directly: it
//! asks the shell to put up a confirmation dialog by calling `Open(type, timestamp, seconds,
//! inhibitors)` on this interface, then waits for us to emit `ConfirmedLogout` / `ConfirmedReboot`
//! / `ConfirmedShutdown` (proceed) or `Canceled` (abort). gnome-shell implements this in
//! `js/ui/endSessionDialog.js`; this is the compositor-side equivalent of its dialog lifecycle,
//! kept pure so the semantics are unit-testable without a bus, a clock, or a renderer.
//!
//! Behaviour ground-truthed from `endSessionDialog.js`:
//!
//! - Each dialog type has exactly one confirm button (plus Cancel): logout → `ConfirmedLogout`,
//!   shutdown → `ConfirmedShutdown`, restart → `ConfirmedReboot`.
//! - The dialog counts down from `seconds` and, on expiry, auto-confirms its default (only) action
//!   — the same thing clicking the button does. gnome-session always passes a non-zero timeout in
//!   practice; we treat `0` as "no countdown, stay open" rather than "confirm immediately", so an
//!   unexpected `0` can never trigger a surprise power-off.
//!
//! Time-in/outcome-out like [`crate::idle_monitor`]: `now` is monotonic (`Clock::now_unadjusted`),
//! and confirming/expiry is returned as an [`EndSessionType`] for the D-Bus layer to turn into the
//! matching signal — never a side effect here.

use std::time::Duration;

/// Which action the dialog confirms. The `u32` values are the `type` argument of `Open`, matching
/// `GSM_SHELL_END_SESSION_DIALOG_TYPE_*` / `endSessionDialog.js`'s `DialogType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndSessionType {
    Logout = 0,
    Shutdown = 1,
    Restart = 2,
}

impl EndSessionType {
    /// Map the `Open` `type` argument. gnome-session only ever sends 0/1/2; anything else falls
    /// back to the least-destructive action (logout) rather than guessing a power-off.
    pub fn from_u32(ty: u32) -> Self {
        match ty {
            1 => EndSessionType::Shutdown,
            2 => EndSessionType::Restart,
            0 => EndSessionType::Logout,
            other => {
                warn!("unknown EndSessionDialog type {other}, treating as logout");
                EndSessionType::Logout
            }
        }
    }

    /// The D-Bus signal name that confirms this action, which gnome-session waits for. Restart maps
    /// to `ConfirmedReboot` (mutter's naming), not `ConfirmedRestart`.
    pub fn confirmed_signal(self) -> &'static str {
        match self {
            EndSessionType::Logout => "ConfirmedLogout",
            EndSessionType::Shutdown => "ConfirmedShutdown",
            EndSessionType::Restart => "ConfirmedReboot",
        }
    }
}

/// A session action to ask gnome-session to *start* — the compositor's `Logout` / `PowerOff` /
/// `Reboot` / `Suspend` actions. Maps to the like-named method on `org.gnome.SessionManager`, which
/// for the first three calls `EndSessionDialog.Open` back on us. (This is the trigger side;
/// [`EndSession`] is the dialog side.)
///
/// `Suspend` is the odd one: it ends nothing and opens no dialog, gnome-session just forwards it to
/// logind. It rides along because gnome-shell asks the *same proxy* for it
/// (`this._session.SuspendAsync()`, `js/misc/systemActions.js:509`) rather than talking to logind
/// itself — so keeping it here is what makes the quick-settings power rows one mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRequest {
    Logout,
    PowerOff,
    Reboot,
    Suspend,
}

#[derive(Debug)]
struct Dialog {
    kind: EndSessionType,
    /// Monotonic deadline at which the countdown auto-confirms, or `None` when `Open` requested no
    /// timeout (`seconds == 0`).
    deadline: Option<Duration>,
}

/// The confirmation-dialog lifecycle. At most one dialog is open at a time (a second `Open`
/// replaces the first, matching gnome-shell, which reuses its single dialog instance).
#[derive(Debug, Default)]
pub struct EndSession {
    dialog: Option<Dialog>,
}

impl EndSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Open`: put up (or replace) the dialog for `kind`, counting down from `total_seconds` (0 =
    /// no countdown).
    pub fn open(&mut self, kind: EndSessionType, total_seconds: u64, now: Duration) {
        let deadline = (total_seconds > 0).then(|| now + Duration::from_secs(total_seconds));
        self.dialog = Some(Dialog { kind, deadline });
    }

    pub fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub fn kind(&self) -> Option<EndSessionType> {
        self.dialog.as_ref().map(|d| d.kind)
    }

    /// Whole seconds until the countdown auto-confirms, for the dialog's description. `None` when
    /// no dialog is open or `Open` requested no timeout.
    pub fn seconds_left(&self, now: Duration) -> Option<u64> {
        let deadline = self.dialog.as_ref()?.deadline?;
        Some(deadline.saturating_sub(now).as_secs())
    }

    /// The monotonic time [`Self::tick`] will next auto-confirm, so the caller can arm one timer.
    /// `None` when nothing is counting down.
    pub fn deadline(&self) -> Option<Duration> {
        self.dialog.as_ref()?.deadline
    }

    /// The user confirmed the action (clicked the button, pressed Enter): close the dialog and
    /// return its type so the D-Bus layer emits the matching `Confirmed*` signal. `None` if no
    /// dialog is open.
    pub fn confirm(&mut self) -> Option<EndSessionType> {
        self.dialog.take().map(|d| d.kind)
    }

    /// The countdown reached zero: auto-confirm the default action, exactly as [`Self::confirm`]
    /// would. `None` if no dialog is open or it isn't counting down / hasn't expired yet.
    pub fn tick(&mut self, now: Duration) -> Option<EndSessionType> {
        let expired = self
            .dialog
            .as_ref()
            .and_then(|d| d.deadline)
            .is_some_and(|deadline| now >= deadline);
        expired.then(|| self.dialog.take().unwrap().kind)
    }

    /// The user cancelled (Cancel button or Esc): close the dialog. Returns whether one was open,
    /// so the caller emits `Canceled` (then `Closed`) only when there was something to cancel.
    pub fn cancel(&mut self) -> bool {
        self.dialog.take().is_some()
    }

    /// gnome-session called `Close` to dismiss the dialog itself (e.g. the request was withdrawn):
    /// just hide it, emitting no signal. Returns whether one was open.
    pub fn close(&mut self) -> bool {
        self.dialog.take().is_some()
    }
}

/// How long the compositor stays up after being told to stop, waiting for clients to finish.
///
/// The two clocks this sits between are both 5 s: the app scopes' `TimeoutStopSec` (gnome-session's
/// `app-gnome-.scope.d/override.conf`, after which systemd SIGKILLs the app) and our own unit's
/// `TimeoutStopSec` (`org.gnome.Shell@.service:30`). Outliving the first is the whole point, so the
/// budget matches it; we buy room against the second with `EXTEND_TIMEOUT_USEC`.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the last window being gone has to hold before we believe the clients are done.
///
/// The window count is an approximation of "the app has finished", and it is early: a toolkit that
/// handles SIGTERM destroys its toplevels near the *start* of shutdown and then keeps talking to
/// the socket — tearing down GL contexts, dmabuf feedback, the registry — until it disconnects.
/// Leaving at unmap would reopen the same `Broken pipe` on a shorter fuse, and precisely for the
/// apps that shut down gracefully rather than dying where they stand. (An app killed outright never
/// unmaps at all: the compositor sees the disconnect, so window-gone and client-gone coincide.)
///
/// This is a settle, not a poll interval — it restarts if a window reappears, and the overall
/// [`DRAIN_TIMEOUT`] still bounds it. A stricter oracle would count live client connections, but it
/// needs an allowlist for the clients that are *ours* (xwayland-satellite holds a connection for as
/// long as it runs, whether or not any X window is left), which is a bigger change than the risk
/// justifies. Noted in `docs/fork/session-end.md`.
pub const DRAIN_SETTLE: Duration = Duration::from_millis(500);

/// Why the drain ended, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every client window went away on its own — the good case.
    ClientsGone,
    /// The budget ran out with windows still up. They lose their socket when we exit, which is the
    /// behaviour this whole mechanism exists to avoid, so it is worth a warning.
    TimedOut,
}

/// The compositor is stopping and is staying alive until its clients are gone.
///
/// Nothing in GNOME does this. mutter's `meta_context_terminate` is a bare `g_main_loop_quit`
/// (`meta-context.c:519-530`), and `meta_wayland_compositor_prepare_shutdown` then calls
/// `wl_display_destroy_clients` (`meta-wayland.c:822`) — a hard disconnect, not a wait. GNOME
/// survives that because the app scopes are SIGTERMed a few inert target-job hops before the shell
/// is, and apps usually win the race. Measured on our own session (journal, 2026-08-03 14:10:15)
/// the head start is 341 ms and Epiphany needed 903 ms: the app finished 562 ms *after* the
/// compositor had been told to stop. That margin is luck, and losing it is the `Broken pipe` abort
/// recorded in `docs/fork/overview-port.md`. So we keep serving Wayland until the windows are gone.
///
/// Pure and time-in/outcome-out like [`EndSession`]: `now` is monotonic and the caller supplies the
/// window count, so the policy is testable without a compositor.
#[derive(Debug)]
pub struct SessionDrain {
    deadline: Duration,
    /// When the window count last reached zero, i.e. when the [`DRAIN_SETTLE`] wait started.
    /// Cleared if a window comes back, so the settle always covers the most recent one.
    empty_since: Option<Duration>,
    /// Whether this drain has ever seen a window. **The settle is only owed if it has** — see
    /// [`Self::poll`].
    saw_windows: bool,
}

impl SessionDrain {
    /// Start draining at `now`, giving clients [`DRAIN_TIMEOUT`].
    pub fn new(now: Duration) -> Self {
        Self {
            deadline: now + DRAIN_TIMEOUT,
            empty_since: None,
            saw_windows: false,
        }
    }

    /// The monotonic time this drain gives up at, whatever the clients are doing.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Is the drain over? `windows_left` is the number of client toplevels still mapped.
    ///
    /// Windows *going away* starts the [`DRAIN_SETTLE`] wait rather than ending the drain; see that
    /// constant for why unmap is not the same as done. Hitting the deadline while settling still
    /// reports [`DrainOutcome::ClientsGone`] — the windows *are* gone, and warning about clients
    /// that overstayed would point at the wrong thing.
    ///
    /// **A drain that never sees a window owes no settle and finishes at once.** The settle exists
    /// to outlive a toolkit that unmapped its toplevel and is still using the socket; if nothing
    /// ever unmapped there is no such toolkit, and waiting is 500 ms spent on a client that was
    /// never there. That is most of a logout from an empty desktop, and — because the confirm drain
    /// has already emptied the desktop by the time the stopping drain starts — half a second of
    /// *every* logout: measured on the seat (journal, 2026-08-03 22:44:27) at 475 ms and 501 ms,
    /// back to back, on a session that never had a client window. `Niri::start_session_drain`
    /// already assumes this ("on an empty desktop that poll can take us all the way to process
    /// exit"); the unconditional settle quietly made it untrue.
    ///
    /// The residual risk is an app launched moments before the logout that has not mapped yet: it
    /// used to get whatever was left of one settle, and now gets nothing. Its scope is still
    /// SIGTERMed either way, and 500 ms was never a guarantee for it.
    pub fn poll(&mut self, now: Duration, windows_left: usize) -> Option<DrainOutcome> {
        if windows_left > 0 {
            self.saw_windows = true;
            self.empty_since = None;
            return (now >= self.deadline).then_some(DrainOutcome::TimedOut);
        }

        if !self.saw_windows {
            return Some(DrainOutcome::ClientsGone);
        }

        let settled_at = *self.empty_since.get_or_insert(now) + DRAIN_SETTLE;
        (now >= settled_at || now >= self.deadline).then_some(DrainOutcome::ClientsGone)
    }

    /// How long until [`Self::poll`] could next change its answer, so the caller can arm one timer.
    /// Zero when that moment has passed.
    pub fn next_wakeup(&self, now: Duration) -> Duration {
        let mut next = self.deadline;
        if let Some(empty_since) = self.empty_since {
            next = next.min(empty_since + DRAIN_SETTLE);
        }
        next.saturating_sub(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn open_sets_kind_and_is_open() {
        let mut e = EndSession::new();
        assert!(!e.is_open());
        e.open(EndSessionType::Shutdown, 60, s(0));
        assert!(e.is_open());
        assert_eq!(e.kind(), Some(EndSessionType::Shutdown));
    }

    #[test]
    fn confirm_closes_and_returns_kind() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Restart, 60, s(0));
        assert_eq!(e.confirm(), Some(EndSessionType::Restart));
        assert!(!e.is_open());
        // A second confirm has nothing to confirm.
        assert_eq!(e.confirm(), None);
    }

    #[test]
    fn cancel_closes_and_reports_whether_open() {
        let mut e = EndSession::new();
        assert!(!e.cancel(), "cancel with no dialog reports not-open");
        e.open(EndSessionType::Logout, 60, s(0));
        assert!(e.cancel());
        assert!(!e.is_open());
    }

    #[test]
    fn countdown_auto_confirms_the_default_action_at_expiry() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Shutdown, 60, s(0));
        assert_eq!(e.deadline(), Some(s(60)));

        // Before expiry: nothing, and the description counts down.
        assert_eq!(e.tick(s(59)), None);
        assert_eq!(e.seconds_left(s(59)), Some(1));
        assert!(e.is_open());

        // At expiry: auto-confirms the shutdown and closes.
        assert_eq!(e.tick(s(60)), Some(EndSessionType::Shutdown));
        assert!(!e.is_open());
        assert_eq!(e.tick(s(61)), None, "nothing left to auto-confirm");
    }

    #[test]
    fn zero_timeout_stays_open_and_never_auto_confirms() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Shutdown, 0, s(0));
        assert_eq!(e.deadline(), None);
        assert_eq!(e.seconds_left(s(0)), None);
        assert_eq!(
            e.tick(s(100_000)),
            None,
            "a 0-second timeout must never surprise-confirm a power-off"
        );
        assert!(e.is_open());
    }

    #[test]
    fn seconds_left_counts_down_and_saturates() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Logout, 60, s(10));
        assert_eq!(e.seconds_left(s(10)), Some(60));
        assert_eq!(e.seconds_left(s(40)), Some(30));
        // Past the deadline it saturates at 0 rather than underflowing.
        assert_eq!(e.seconds_left(s(999)), Some(0));
    }

    #[test]
    fn reopen_replaces_the_dialog() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Logout, 60, s(0));
        e.open(EndSessionType::Restart, 30, s(5));
        assert_eq!(e.kind(), Some(EndSessionType::Restart));
        assert_eq!(e.deadline(), Some(s(35)));
    }

    #[test]
    fn confirmed_signal_matches_the_protocol_names() {
        assert_eq!(EndSessionType::Logout.confirmed_signal(), "ConfirmedLogout");
        assert_eq!(
            EndSessionType::Shutdown.confirmed_signal(),
            "ConfirmedShutdown"
        );
        // Restart confirms with ConfirmedReboot, not ConfirmedRestart.
        assert_eq!(
            EndSessionType::Restart.confirmed_signal(),
            "ConfirmedReboot"
        );
    }

    #[test]
    fn from_u32_maps_known_types_and_defaults_to_logout() {
        assert_eq!(EndSessionType::from_u32(0), EndSessionType::Logout);
        assert_eq!(EndSessionType::from_u32(1), EndSessionType::Shutdown);
        assert_eq!(EndSessionType::from_u32(2), EndSessionType::Restart);
        assert_eq!(EndSessionType::from_u32(99), EndSessionType::Logout);
    }

    #[test]
    fn close_hides_without_a_signal() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Logout, 60, s(0));
        assert!(e.close());
        assert!(!e.is_open());
        assert!(!e.close());
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn drain_waits_for_windows_then_settles_after_the_last_one() {
        let mut drain = SessionDrain::new(s(0));
        assert_eq!(drain.poll(s(0), 2), None);
        assert_eq!(drain.poll(s(1), 1), None);
        // The last window unmapping starts the settle, it does not end the drain.
        assert_eq!(drain.poll(s(2), 0), None);
        assert_eq!(drain.poll(s(2) + DRAIN_SETTLE - ms(1), 0), None);
        assert_eq!(
            drain.poll(s(2) + DRAIN_SETTLE, 0),
            Some(DrainOutcome::ClientsGone)
        );
    }

    /// The settle exists because unmap is not "done" — a toolkit that handles SIGTERM destroys its
    /// toplevels early and keeps using the socket afterwards. Leaving at unmap would reopen the
    /// `Broken pipe` for exactly the apps that shut down gracefully.
    #[test]
    fn a_window_reappearing_restarts_the_settle() {
        let mut drain = SessionDrain::new(s(0));
        // A window was up, so a settle is owed once it goes.
        assert_eq!(drain.poll(ms(0), 1), None);
        assert_eq!(drain.poll(ms(1), 0), None);
        // Something mapped again part-way through the settle.
        assert_eq!(drain.poll(ms(200), 1), None);
        // The old settle must not carry over and fire early.
        assert_eq!(drain.poll(ms(400), 0), None);
        assert_eq!(drain.poll(ms(400) + DRAIN_SETTLE - ms(1), 0), None);
        assert_eq!(
            drain.poll(ms(400) + DRAIN_SETTLE, 0),
            Some(DrainOutcome::ClientsGone)
        );
    }

    #[test]
    fn drain_gives_up_at_the_deadline() {
        let mut drain = SessionDrain::new(s(0));
        assert_eq!(drain.deadline(), DRAIN_TIMEOUT);
        assert_eq!(drain.poll(drain.deadline() - s(1), 1), None);
        assert_eq!(
            drain.poll(drain.deadline(), 1),
            Some(DrainOutcome::TimedOut)
        );
    }

    /// The deadline landing mid-settle reports a clean drain: the windows *are* gone, and warning
    /// about clients that overstayed would send anyone reading the journal after the wrong thing.
    #[test]
    fn the_deadline_during_a_settle_is_not_a_timeout() {
        let mut drain = SessionDrain::new(s(0));
        let just_before = drain.deadline() - ms(1);
        assert_eq!(drain.poll(s(0), 1), None, "a window, so a settle is owed");
        assert_eq!(drain.poll(just_before, 0), None, "settle has not elapsed");
        assert_eq!(
            drain.poll(drain.deadline(), 0),
            Some(DrainOutcome::ClientsGone)
        );
    }

    /// Nothing open when the stop arrives: the drain is over on the first poll, with **no**
    /// settle. The settle outlives a toolkit that unmapped and is still on the socket; nothing
    /// unmapped here, so there is nobody to outlive.
    ///
    /// This is worth half a second on every logout, not just an empty one — by the time the
    /// *stopping* drain starts, the confirm drain has already emptied the desktop, so it too
    /// begins at zero. Measured on the seat before the fix (journal, 2026-08-03 22:44:27): 475 ms
    /// then 501 ms, back to back, on a session that never had a client window.
    #[test]
    fn a_drain_that_never_sees_a_window_is_over_at_once() {
        let mut drain = SessionDrain::new(s(0));
        assert_eq!(drain.poll(s(0), 0), Some(DrainOutcome::ClientsGone));

        // ...but one that *did* see a window still pays the settle when it goes, even if the
        // window is gone by the very next poll.
        let mut drain = SessionDrain::new(s(0));
        assert_eq!(drain.poll(s(0), 1), None);
        assert_eq!(
            drain.poll(s(0), 0),
            None,
            "the settle is owed once a window unmaps"
        );
        assert_eq!(drain.poll(DRAIN_SETTLE, 0), Some(DrainOutcome::ClientsGone));
    }

    /// The caller arms one timer off this, so it must shorten to the settle and never overshoot the
    /// deadline — an overshoot is a compositor that sits there after its clients have gone.
    #[test]
    fn next_wakeup_is_the_earlier_of_settle_and_deadline() {
        let mut drain = SessionDrain::new(s(0));
        assert_eq!(drain.next_wakeup(s(0)), DRAIN_TIMEOUT);

        drain.poll(s(1), 1);
        assert_eq!(drain.next_wakeup(s(1)), DRAIN_TIMEOUT - s(1));

        // Windows gone: the settle is now the nearer wakeup.
        drain.poll(s(1), 0);
        assert_eq!(drain.next_wakeup(s(1)), DRAIN_SETTLE);

        // And it never runs backwards once everything has passed.
        assert_eq!(drain.next_wakeup(s(600)), Duration::ZERO);
    }
}
