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
/// `Reboot` actions. Maps to the like-named method on `org.gnome.SessionManager`, which then calls
/// `EndSessionDialog.Open` back on us. (This is the trigger side; [`EndSession`] is the dialog
/// side.)
#[derive(Debug, Clone, Copy)]
pub enum SessionRequest {
    Logout,
    PowerOff,
    Reboot,
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
}

impl SessionDrain {
    /// Start draining at `now`, giving clients [`DRAIN_TIMEOUT`].
    pub fn new(now: Duration) -> Self {
        Self {
            deadline: now + DRAIN_TIMEOUT,
        }
    }

    /// The monotonic time [`Self::poll`] gives up at, so the caller can arm one timer.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Is the drain over? `windows_left` is the number of client toplevels still mapped.
    ///
    /// The zero check comes first, so a drain that finishes in the same poll as its deadline
    /// reports the good outcome rather than a spurious timeout.
    pub fn poll(&self, now: Duration, windows_left: usize) -> Option<DrainOutcome> {
        if windows_left == 0 {
            Some(DrainOutcome::ClientsGone)
        } else if now >= self.deadline {
            Some(DrainOutcome::TimedOut)
        } else {
            None
        }
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

    #[test]
    fn drain_waits_for_windows_then_ends_on_the_last_one() {
        let drain = SessionDrain::new(s(0));
        assert_eq!(drain.poll(s(0), 2), None);
        assert_eq!(drain.poll(s(1), 1), None);
        assert_eq!(drain.poll(s(2), 0), Some(DrainOutcome::ClientsGone));
    }

    #[test]
    fn drain_gives_up_at_the_deadline() {
        let drain = SessionDrain::new(s(0));
        assert_eq!(drain.deadline(), DRAIN_TIMEOUT);
        assert_eq!(drain.poll(drain.deadline() - s(1), 1), None);
        assert_eq!(
            drain.poll(drain.deadline(), 1),
            Some(DrainOutcome::TimedOut)
        );
    }

    /// The last window going away *at* the deadline is a clean drain, not a timeout: an app that
    /// used its whole budget still exited on its own terms, and logging it as a timeout would send
    /// anyone reading the journal after the real failure mode.
    #[test]
    fn drain_finishing_on_the_deadline_is_not_a_timeout() {
        let drain = SessionDrain::new(s(0));
        assert_eq!(
            drain.poll(drain.deadline(), 0),
            Some(DrainOutcome::ClientsGone)
        );
    }

    /// Nothing open when the stop arrives — the drain must not hold the session up for five
    /// seconds on an empty desktop.
    #[test]
    fn drain_with_no_windows_is_over_immediately() {
        let drain = SessionDrain::new(s(0));
        assert_eq!(drain.poll(s(0), 0), Some(DrainOutcome::ClientsGone));
    }
}
