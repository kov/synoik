// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

/// What gnome-software says is pending, from `org.gnome.Software.OfflineUpdates.GetState`.
///
/// Lives here rather than in [`crate::dbus::gnome_software`] because the *policy* — which states
/// offer an install, what confirming then means — is what the dialog and the tests care about; the
/// bus module is only the transport. See `docs/fork/end-session-dialog-port.md` §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OfflineUpdateState {
    /// gnome-software isn't running, didn't answer, or answered something we don't understand.
    /// The dialog shows no checkbox and behaves as though the feature did not exist.
    #[default]
    Unavailable,
    /// Running, nothing pending.
    None,
    /// An update is downloaded and can be scheduled.
    Prepared,
    /// An update is already scheduled for the next reboot.
    Scheduled,
}

impl OfflineUpdateState {
    /// Map `GetState`'s reply. An unrecognized string is [`Unavailable`](Self::Unavailable), not a
    /// guess: the interface is explicitly unstable, so a future state we've never heard of must
    /// fail closed rather than be lumped in with "there is an update to install".
    pub fn from_wire(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "prepared" => Self::Prepared,
            "scheduled" => Self::Scheduled,
            other => {
                warn!("unknown org.gnome.Software.OfflineUpdates state {other:?}");
                Self::Unavailable
            }
        }
    }

    /// Whether there is something for the checkbox to offer to install.
    pub fn has_pending_update(self) -> bool {
        matches!(self, Self::Prepared | Self::Scheduled)
    }
}

/// How the dialog is *presented* — its title, description and action label — as opposed to which
/// action it confirms.
///
/// gnome-shell's `DialogType` has a fourth member, `UPDATE_RESTART`
/// (`js/ui/endSessionDialog.js:130`), that gnome-session never sends: the shell promotes a
/// `RESTART` to it itself when gnome-software says an update is already `scheduled` (`:684-687`).
/// The user has already asked for the update, so asking again with a checkbox would be redundant;
/// instead the whole dialog changes register to "Restart & Install Updates".
///
/// This is derived from `(kind, state)` rather than being a fourth [`EndSessionType`] variant on
/// purpose: `EndSessionType` is the *wire* enum, and an unexpected `3` arriving in `Open` must not
/// be able to turn into a reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    Logout = 0,
    Shutdown = 1,
    Restart = 2,
    UpdateRestart = 3,
}

impl Presentation {
    /// Promote a restart to [`UpdateRestart`](Self::UpdateRestart) when an update is already
    /// scheduled. Only `Scheduled` promotes: `Prepared` means the update is downloaded but nobody
    /// has asked for it, which is the checkbox's job.
    pub fn for_dialog(kind: EndSessionType, state: OfflineUpdateState) -> Self {
        match (kind, state) {
            (EndSessionType::Restart, OfflineUpdateState::Scheduled) => Self::UpdateRestart,
            (EndSessionType::Logout, _) => Self::Logout,
            (EndSessionType::Shutdown, _) => Self::Shutdown,
            (EndSessionType::Restart, _) => Self::Restart,
        }
    }

    /// Whether this presentation carries the "Install pending software updates" checkbox: a pending
    /// update *and* a presentation that offers one.
    ///
    /// Logout never does — you cannot install updates by logging out, and GNOME sets `checkBoxText`
    /// only on the shutdown and restart content (`:77`/`:96`). Neither does `UpdateRestart`: it
    /// exists precisely because the answer is already yes.
    fn offers_updates(self, state: OfflineUpdateState) -> bool {
        matches!(self, Self::Shutdown | Self::Restart) && state.has_pending_update()
    }
}

/// What gnome-software should do once the updates are applied (`SetAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostUpdateAction {
    Reboot,
    Shutdown,
}

impl PostUpdateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
        }
    }
}

/// What confirming the dialog asks us to do about the pending offline update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// No checkbox was on screen — don't touch gnome-software at all.
    Untouched,
    /// Ticked: schedule the update and tell gnome-software what to do afterwards.
    Install(PostUpdateAction),
    /// On screen and unticked: drop whatever was prepared (`Cancel`).
    Discard,
}

/// The outcome of confirming the dialog: which session action, and what to do about updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmation {
    pub kind: EndSessionType,
    pub updates: UpdateDecision,
}

impl Confirmation {
    /// The signal gnome-session must receive, given whether `SetAction` was accepted.
    ///
    /// **This is where "Power Off" can legitimately become `ConfirmedReboot`.** Offline updates are
    /// applied during a reboot, and gnome-software powers the machine off once they are in — so
    /// asking it to `shutdown` afterwards and then rebooting *is* the power-off the user asked for
    /// (`js/ui/endSessionDialog.js:475-487`). If gnome-software could not take the instruction
    /// (`NOT_SUPPORTED`, or any other failure) the machine would come back up instead of shutting
    /// down, so the signal must stay `ConfirmedShutdown`.
    pub fn signal(&self, set_action_accepted: bool) -> &'static str {
        if set_action_accepted
            && self.updates == UpdateDecision::Install(PostUpdateAction::Shutdown)
        {
            return "ConfirmedReboot";
        }
        self.kind.confirmed_signal()
    }
}

#[derive(Debug)]
struct Dialog {
    kind: EndSessionType,
    /// Monotonic deadline at which the countdown auto-confirms, or `None` when `Open` requested no
    /// timeout (`seconds == 0`).
    deadline: Option<Duration>,
    /// gnome-software's answer, once it lands. The query is asynchronous, so this starts
    /// [`OfflineUpdateState::Unavailable`] and the checkbox appears when the reply arrives.
    updates: OfflineUpdateState,
    /// Whether the user wants the pending update installed. Defaults to ticked when the checkbox
    /// appears, matching gnome-shell (`:718`); the low-battery gate that can start it unticked is
    /// backlog, see the port doc §4.
    install_updates: bool,
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
        self.dialog = Some(Dialog {
            kind,
            deadline,
            updates: OfflineUpdateState::default(),
            install_updates: false,
        });
    }

    /// gnome-software answered the `GetState` we sent when the dialog opened.
    ///
    /// Ignored if no dialog is open — the reply is asynchronous, so it can arrive after the user
    /// has already confirmed or cancelled, and it must not resurrect anything.
    pub fn set_offline_update_state(&mut self, state: OfflineUpdateState) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        dialog.updates = state;
        // Ticked by default whenever the box appears (`js/ui/endSessionDialog.js:718`).
        dialog.install_updates = Presentation::for_dialog(dialog.kind, state).offers_updates(state);
    }

    /// How the open dialog presents itself, which the answer from gnome-software can change under
    /// it: a restart whose update turns out to be `scheduled` becomes an "Restart & Install
    /// Updates" dialog when the reply lands. `None` when no dialog is open.
    pub fn presentation(&self) -> Option<Presentation> {
        self.dialog
            .as_ref()
            .map(|d| Presentation::for_dialog(d.kind, d.updates))
    }

    /// Whether the update checkbox is currently on screen.
    pub fn offers_updates(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|d| Presentation::for_dialog(d.kind, d.updates).offers_updates(d.updates))
    }

    /// Whether that checkbox is ticked. Always false when it isn't shown.
    pub fn install_updates(&self) -> bool {
        self.offers_updates() && self.dialog.as_ref().is_some_and(|d| d.install_updates)
    }

    /// The user clicked the checkbox (or pressed Space on it). A no-op when it isn't on screen, so
    /// a stray key can never arm an install that was never offered.
    pub fn toggle_install_updates(&mut self) {
        if !self.offers_updates() {
            return;
        }
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.install_updates = !dialog.install_updates;
        }
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
    /// return what to do — the session action, and what became of the offline update. `None` if no
    /// dialog is open.
    pub fn confirm(&mut self) -> Option<Confirmation> {
        let presentation = self.presentation()?;
        let offers = self.offers_updates();
        self.dialog.take().map(|d| Confirmation {
            kind: d.kind,
            updates: Self::decide(presentation, offers, d.install_updates),
        })
    }

    /// What confirming means for the pending update.
    ///
    /// The checkbox being *on screen* is what makes this our business at all: with no box there is
    /// nothing the user said about updates, so we say nothing to gnome-software
    /// (`js/ui/endSessionDialog.js:469`). With a box, unticking it is an explicit "not this time"
    /// and gets a `Cancel`.
    ///
    /// The exception is [`Presentation::UpdateRestart`], which has no box because the whole dialog
    /// *is* the question: confirming it re-asserts `reboot` unconditionally
    /// (`js/ui/endSessionDialog.js:494-496`). That call is a no-op restatement when the scheduled
    /// action was already a reboot, and corrective when it was not — and it has to be, because
    /// `GetState` reports only that an update is scheduled, never which action was scheduled with
    /// it. The dialog can't read the action back, so it sets it.
    fn decide(presentation: Presentation, offers: bool, install: bool) -> UpdateDecision {
        if !offers {
            return match presentation {
                Presentation::UpdateRestart => UpdateDecision::Install(PostUpdateAction::Reboot),
                _ => UpdateDecision::Untouched,
            };
        }
        if !install {
            return UpdateDecision::Discard;
        }
        match presentation {
            Presentation::Restart | Presentation::UpdateRestart => {
                UpdateDecision::Install(PostUpdateAction::Reboot)
            }
            Presentation::Shutdown => UpdateDecision::Install(PostUpdateAction::Shutdown),
            // Unreachable: logout never offers the box (`Presentation::offers_updates`).
            Presentation::Logout => UpdateDecision::Untouched,
        }
    }

    /// The countdown reached zero: auto-confirm the default action, exactly as [`Self::confirm`]
    /// would. `None` if no dialog is open or it isn't counting down / hasn't expired yet.
    pub fn tick(&mut self, now: Duration) -> Option<Confirmation> {
        let expired = self
            .dialog
            .as_ref()
            .and_then(|d| d.deadline)
            .is_some_and(|deadline| now >= deadline);
        // Auto-confirm is the same act as clicking the button, updates included: the box was on
        // screen, ticked by default, and the user let it run out.
        expired.then(|| self.confirm().unwrap())
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
        assert_eq!(
            e.confirm(),
            Some(Confirmation {
                kind: EndSessionType::Restart,
                // Nothing was asked about updates, so nothing is said about them.
                updates: UpdateDecision::Untouched,
            })
        );
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
        assert_eq!(
            e.tick(s(60)),
            Some(Confirmation {
                kind: EndSessionType::Shutdown,
                updates: UpdateDecision::Untouched,
            })
        );
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
}

#[cfg(test)]
mod offline_update_tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn open(kind: EndSessionType, state: OfflineUpdateState) -> EndSession {
        let mut e = EndSession::new();
        e.open(kind, 60, s(0));
        e.set_offline_update_state(state);
        e
    }

    /// The box is offered only where GNOME sets `checkBoxText` — shutdown and restart — and only
    /// when something is actually pending. Logging out installs nothing, and a restart whose update
    /// is already `scheduled` is promoted to [`Presentation::UpdateRestart`], which asks with its
    /// title instead of a checkbox.
    #[test]
    fn only_shutdown_and_restart_offer_the_checkbox() {
        for state in [OfflineUpdateState::Prepared, OfflineUpdateState::Scheduled] {
            assert!(
                open(EndSessionType::Shutdown, state).offers_updates(),
                "power off offers the box whichever way the update is pending ({state:?})"
            );
            assert!(
                !open(EndSessionType::Logout, state).offers_updates(),
                "logout must never offer to install updates ({state:?})"
            );
        }
        assert!(
            open(EndSessionType::Restart, OfflineUpdateState::Prepared).offers_updates(),
            "a prepared update is the checkbox's question to ask"
        );
        assert!(
            !open(EndSessionType::Restart, OfflineUpdateState::Scheduled).offers_updates(),
            "a scheduled update is already answered — the dialog changes instead"
        );
        for state in [OfflineUpdateState::None, OfflineUpdateState::Unavailable] {
            assert!(
                !open(EndSessionType::Shutdown, state).offers_updates(),
                "{state:?}"
            );
            assert!(
                !open(EndSessionType::Restart, state).offers_updates(),
                "{state:?}"
            );
        }
    }

    /// The reply is asynchronous, so the dialog opens with no box and gains one when the answer
    /// lands. Until then nothing about updates is on screen or decided.
    #[test]
    fn the_checkbox_appears_only_once_gnome_software_answers() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Restart, 60, s(0));
        assert!(
            !e.offers_updates(),
            "nothing offered before the answer arrives"
        );
        assert!(!e.install_updates());

        e.set_offline_update_state(OfflineUpdateState::Prepared);
        assert!(e.offers_updates());
        assert!(e.install_updates(), "ticked by default once offered");
    }

    /// An answer that arrives after the user already acted must not resurrect a closed dialog.
    #[test]
    fn a_late_answer_is_dropped() {
        let mut e = EndSession::new();
        e.open(EndSessionType::Shutdown, 60, s(0));
        assert!(e.confirm().is_some());
        e.set_offline_update_state(OfflineUpdateState::Prepared);
        assert!(!e.is_open(), "a late reply must not reopen the dialog");
        assert!(!e.offers_updates());
    }

    /// Toggling only does something when the box is on screen — a stray Space must never arm an
    /// install that was never offered.
    #[test]
    fn toggling_without_a_checkbox_does_nothing() {
        let mut e = open(EndSessionType::Shutdown, OfflineUpdateState::None);
        e.toggle_install_updates();
        assert!(!e.install_updates());
        assert_eq!(e.confirm().unwrap().updates, UpdateDecision::Untouched);
    }

    /// **The four combinations.** This is the table in `docs/fork/end-session-dialog-port.md` §3.1,
    /// and the reason it is pinned is the third row: confirming "Power Off" with the box ticked
    /// legitimately puts `ConfirmedReboot` on the wire, because the update is applied across a
    /// reboot and gnome-software powers off afterwards. Read cold, that looks like a bug.
    #[test]
    fn confirming_maps_the_checkbox_to_a_decision_and_a_signal() {
        // Restart, ticked → schedule a reboot, and it is a reboot either way.
        let mut e = open(EndSessionType::Restart, OfflineUpdateState::Prepared);
        let c = e.confirm().unwrap();
        assert_eq!(c.updates, UpdateDecision::Install(PostUpdateAction::Reboot));
        assert_eq!(c.signal(true), "ConfirmedReboot");
        assert_eq!(c.signal(false), "ConfirmedReboot");

        // Power off, ticked and ACCEPTED → reboot now, gnome-software powers off after.
        let mut e = open(EndSessionType::Shutdown, OfflineUpdateState::Prepared);
        let c = e.confirm().unwrap();
        assert_eq!(
            c.updates,
            UpdateDecision::Install(PostUpdateAction::Shutdown)
        );
        assert_eq!(
            c.signal(true),
            "ConfirmedReboot",
            "an accepted post-update shutdown reboots now and powers off later"
        );

        // Power off, ticked but REFUSED → the backend cannot power off afterwards, so rebooting
        // would leave the machine on. Stay a shutdown.
        assert_eq!(
            c.signal(false),
            "ConfirmedShutdown",
            "a refused SetAction must NOT be turned into a reboot"
        );

        // Unticked → drop what was prepared, and the signal is the plain one.
        let mut e = open(EndSessionType::Shutdown, OfflineUpdateState::Prepared);
        e.toggle_install_updates();
        let c = e.confirm().unwrap();
        assert_eq!(c.updates, UpdateDecision::Discard);
        assert_eq!(c.signal(true), "ConfirmedShutdown");
        assert_eq!(c.signal(false), "ConfirmedShutdown");

        // No box at all → say nothing to gnome-software.
        let mut e = open(EndSessionType::Shutdown, OfflineUpdateState::Unavailable);
        let c = e.confirm().unwrap();
        assert_eq!(c.updates, UpdateDecision::Untouched);
        assert_eq!(c.signal(true), "ConfirmedShutdown");
    }

    /// Letting the countdown run out is the same act as clicking the button, updates included —
    /// the box was on screen and ticked, and the user let it stand.
    #[test]
    fn the_countdown_auto_confirms_the_update_too() {
        let mut e = open(EndSessionType::Restart, OfflineUpdateState::Prepared);
        assert!(
            e.offers_updates(),
            "this is the checkbox path, not the promoted one"
        );
        let c = e.tick(s(60)).unwrap();
        assert_eq!(c.updates, UpdateDecision::Install(PostUpdateAction::Reboot));
    }

    /// **The promotion.** A restart whose update is already scheduled becomes gnome-shell's fourth
    /// dialog *presentation* — `UPDATE_RESTART` (`js/ui/endSessionDialog.js:684-687`) — which the
    /// wire enum never carries. The user already asked for the update, so the dialog says so in its
    /// title instead of asking again with a checkbox.
    #[test]
    fn a_scheduled_update_promotes_a_restart_to_update_restart() {
        for (kind, state, expected) in [
            (
                EndSessionType::Restart,
                OfflineUpdateState::Scheduled,
                Presentation::UpdateRestart,
            ),
            // Prepared is downloaded-but-unasked-for: that is the checkbox's question.
            (
                EndSessionType::Restart,
                OfflineUpdateState::Prepared,
                Presentation::Restart,
            ),
            (
                EndSessionType::Restart,
                OfflineUpdateState::None,
                Presentation::Restart,
            ),
            // Only a restart promotes. GNOME has the shutdown half drafted but unshipped (the
            // `unusedFuture*ForTranslation` strings, `:120-121`), so power-off keeps the checkbox.
            (
                EndSessionType::Shutdown,
                OfflineUpdateState::Scheduled,
                Presentation::Shutdown,
            ),
            (
                EndSessionType::Logout,
                OfflineUpdateState::Scheduled,
                Presentation::Logout,
            ),
        ] {
            assert_eq!(
                open(kind, state).presentation(),
                Some(expected),
                "{kind:?} + {state:?}"
            );
        }
    }

    /// The promoted dialog has no checkbox, but confirming it still schedules the reboot: the
    /// dialog itself is the question, and `GetState` cannot tell us which action was scheduled, so
    /// we re-assert it (`:494-496`).
    #[test]
    fn confirming_a_promoted_restart_re_asserts_the_reboot() {
        let mut e = open(EndSessionType::Restart, OfflineUpdateState::Scheduled);
        assert!(!e.offers_updates(), "no checkbox on the promoted dialog");
        assert!(!e.install_updates());

        let c = e.confirm().unwrap();
        assert_eq!(
            c.updates,
            UpdateDecision::Install(PostUpdateAction::Reboot),
            "the promoted dialog installs even though nothing was ticked",
        );
        // Nothing to rewrite: a restart is a reboot however `SetAction` went.
        assert_eq!(c.signal(true), "ConfirmedReboot");
        assert_eq!(c.signal(false), "ConfirmedReboot");
    }

    /// Space on a dialog with no checkbox must not arm anything — the promoted dialog has no box to
    /// toggle, and toggling must not be able to turn its unconditional install into a `Cancel`.
    #[test]
    fn the_promoted_dialog_cannot_be_toggled_out_of_installing() {
        let mut e = open(EndSessionType::Restart, OfflineUpdateState::Scheduled);
        e.toggle_install_updates();
        assert_eq!(
            e.confirm().unwrap().updates,
            UpdateDecision::Install(PostUpdateAction::Reboot),
        );
    }

    /// Cancelling the dialog says nothing to gnome-software at all: `Cancel` is what unticking the
    /// box means, and Escape is not unticking it — it is walking away from the whole question.
    #[test]
    fn cancelling_the_dialog_decides_nothing_about_updates() {
        let mut e = open(EndSessionType::Shutdown, OfflineUpdateState::Prepared);
        assert!(e.cancel());
        assert!(e.confirm().is_none(), "nothing left to confirm");
    }
}
