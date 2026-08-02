//! The "Authentication Required" dialog's state machine (`js/ui/components/polkitAgent.js`).
//!
//! The interactive surface is [`crate::ui::polkit_dialog`]; everything about *what* the dialog is
//! doing lives here, so it can be driven by a test without a renderer.
//!
//! Two things about this dialog are easy to get wrong and both are here:
//!
//! - **It does not appear when the request does.** GNOME builds it on `BeginAuthentication` but
//!   only opens it once PAM has actually asked something (`_ensureOpen` from `_onSessionRequest`,
//!   `:297`). A dialog raised earlier would sit there with no entry, and a modal grab, while PAM
//!   decided whether it wanted anything.
//! - **A passwordless account must not start a conversation.** Starting one *is* the authentication
//!   for such an account, so the dialog opens first and initiates only on confirmation
//!   ([`Mode::Confirm`], `:364-384`).

use std::time::Duration;

use crate::dbus::polkit_agent::{BeginRequest, PolkitRequest, PolkitToNiri};
use crate::unlock_dialog::{clean_question, UserInfo};

/// Room for any password without reallocating — a `String` that grows 0→8→16 leaves the earlier
/// buffers on the heap, unzeroed, holding a prefix of what was typed.
const ENTRY_CAPACITY: usize = 512;

/// `DELAYED_RESET_TIMEOUT` (`polkitAgent.js:26`).
///
/// When a conversation ends, the entry does not vanish at once: the next one usually asks its
/// question within a frame or two, and hiding the entry in between would make every retry flash.
/// It only resets if nothing has asked by then.
pub const DELAYED_RESET: Duration = Duration::from_millis(200);

/// What the dialog is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The ordinary case: PAM is running and will ask for a password.
    Auth,
    /// The account has no password, so there is nothing to ask. The dialog is a confirmation, and
    /// pressing Authenticate is what starts — and thereby completes — the conversation.
    Confirm,
}

/// A line under the entry. The two are styled differently and GNOME keeps two separate labels for
/// them (`polkitAgent.js:115-129`), so this is not one string with a colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `PAM_ERROR_MSG`, or our own "that didn't work" when PAM refused without saying why.
    Error(String),
    /// `PAM_TEXT_INFO` — "Place your finger on the reader", typically.
    Info(String),
}

impl Message {
    pub fn text(&self) -> &str {
        match self {
            Message::Error(text) | Message::Info(text) => text,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Message::Error(_))
    }
}

/// Which control has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Entry,
    Cancel,
    Authenticate,
}

/// What the caller must do after driving the dialog.
#[derive(Default)]
pub struct PolkitEffects {
    /// Send this to [`crate::dbus::polkit_agent`].
    pub request: Option<PolkitRequest>,
    /// Something visible changed.
    pub redraw: bool,
    /// The entry should shake — PAM refused (`polkitAgent.js:268`).
    pub wiggle: bool,
    /// Arm (or re-arm) the [`DELAYED_RESET`] timer from now.
    pub arm_reset: bool,
    /// The dialog has finished and should come down.
    pub close: bool,
}

/// Hand-written so a stray `{:?}` cannot put a password in the journal by way of `request`.
impl std::fmt::Debug for PolkitEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolkitEffects")
            .field("request", &self.request)
            .field("redraw", &self.redraw)
            .field("wiggle", &self.wiggle)
            .field("arm_reset", &self.arm_reset)
            .field("close", &self.close)
            .finish()
    }
}

impl PolkitEffects {
    fn redraw() -> Self {
        Self {
            redraw: true,
            ..Default::default()
        }
    }
}

/// GNOME's fallback when PAM refuses without explaining itself (`polkitAgent.js:263`).
pub const GENERIC_FAILURE: &str = "Sorry, that didn’t work. Please try again.";
/// The dialog's title (`polkitAgent.js:44`).
pub const TITLE: &str = "Authentication Required";
/// The label under a root avatar (`polkitAgent.js:84`). Not the account's real name — for `root`
/// GNOME says what the authority *is*, not who it belongs to.
pub const ADMINISTRATOR: &str = "Administrator";

pub struct PolkitDialog {
    /// `None` when no request is in flight at all.
    request: Option<BeginRequest>,
    /// Whether the dialog is actually on screen. False between `Begin` and PAM's first question —
    /// see the module docs.
    open: bool,
    mode: Mode,
    user: UserInfo,
    /// PAM's question, cleaned for display, and whether the entry masks what is typed. `None`
    /// means no question is outstanding and the entry is not shown.
    ///
    /// `echo_on` has no default because there is no safe one: assuming it would either mask a
    /// username or print a password.
    question: Option<(String, bool)>,
    /// What has been typed. Cleared on every verdict and every question — a password must not
    /// outlive the question it answers.
    entry: String,
    /// False between submitting and the next question: GNOME makes the entry and the OK button
    /// insensitive so a second Enter cannot send the same password twice (`:225-226`).
    entry_live: bool,
    message: Option<Message>,
    focus: Focus,
    caps_warning: bool,
}

impl PolkitDialog {
    pub fn new() -> Self {
        Self {
            request: None,
            open: false,
            mode: Mode::Auth,
            user: UserInfo::default(),
            question: None,
            entry: String::with_capacity(ENTRY_CAPACITY),
            entry_live: false,
            message: None,
            focus: Focus::Entry,
            caps_warning: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn user(&self) -> &UserInfo {
        &self.user
    }

    /// Whether the account being authenticated is `root`, which the label and its colour both
    /// depend on (`polkitAgent.js:78-84`, `_dialogs.scss:156-158`).
    pub fn is_root(&self) -> bool {
        self.user.name == "root"
    }

    /// The label under the avatar.
    pub fn user_label(&self) -> &str {
        if self.is_root() {
            ADMINISTRATOR
        } else {
            self.user.display_name()
        }
    }

    /// polkit's description of the action — the dialog's body text.
    pub fn action_message(&self) -> &str {
        self.request.as_ref().map_or("", |r| r.message.as_str())
    }

    pub fn message(&self) -> Option<&Message> {
        self.message.as_ref()
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn caps_warning(&self) -> bool {
        self.caps_warning
    }

    /// The entry's placeholder — PAM's question, cleaned.
    pub fn question(&self) -> Option<&str> {
        self.question.as_ref().map(|(text, _)| text.as_str())
    }

    /// Whether an entry is on screen at all. It appears with PAM's first question and goes away
    /// again [`DELAYED_RESET`] after the conversation ends.
    pub fn shows_entry(&self) -> bool {
        self.question.is_some()
    }

    /// Whether the entry accepts input right now.
    pub fn is_entry_live(&self) -> bool {
        self.entry_live
    }

    /// What to draw in the entry: the text, or one bullet per character when PAM said not to echo.
    pub fn entry_display(&self) -> String {
        match self.question {
            Some((_, true)) => self.entry.clone(),
            // U+25CF, `st_password_entry`'s mask character.
            _ => "\u{25cf}".repeat(self.entry.chars().count()),
        }
    }

    /// Whether Authenticate can be pressed. In [`Mode::Confirm`] there is nothing to type, so it is
    /// live from the start (`:371`); otherwise it needs text (`:157-159`).
    pub fn can_authenticate(&self) -> bool {
        match self.mode {
            Mode::Confirm => true,
            Mode::Auth => self.entry_live && !self.entry.is_empty(),
        }
    }

    pub fn set_caps_warning(&mut self, warn: bool) -> bool {
        let changed = self.caps_warning != warn;
        self.caps_warning = warn;
        changed
    }

    /// polkitd wants an authentication.
    pub fn begin(&mut self, request: BeginRequest) -> PolkitEffects {
        self.user = crate::unlock_dialog::user_for_name(&request.user_name);
        self.mode = if request.passwordless {
            Mode::Confirm
        } else {
            Mode::Auth
        };
        self.question = None;
        self.clear_entry();
        self.entry_live = false;
        self.message = None;
        self.request = Some(request);

        match self.mode {
            // Nothing to ask, so the dialog goes up now and the conversation waits on the button.
            Mode::Confirm => {
                self.open = true;
                self.focus = Focus::Authenticate;
                PolkitEffects::redraw()
            }
            // Start PAM, but stay off screen until it asks something.
            Mode::Auth => {
                self.focus = Focus::Entry;
                PolkitEffects {
                    request: Some(self.initiate()),
                    ..Default::default()
                }
            }
        }
    }

    fn initiate(&self) -> PolkitRequest {
        PolkitRequest::Initiate {
            user_name: self.user.name.clone(),
        }
    }

    /// Zero the entry before dropping its contents, and keep the allocation.
    fn clear_entry(&mut self) {
        self.entry.clear();
    }

    /// An event from the agent.
    pub fn on_agent_event(&mut self, event: PolkitToNiri) -> PolkitEffects {
        match event {
            PolkitToNiri::Begin(request) => self.begin(*request),

            PolkitToNiri::Cancel => {
                // polkitd withdrew it; the agent has already answered the call, so this is only a
                // teardown.
                self.reset();
                PolkitEffects {
                    redraw: true,
                    close: true,
                    ..Default::default()
                }
            }

            // PAM asked something. This is what puts the dialog on screen.
            PolkitToNiri::Request { prompt, echo_on } => {
                self.question = Some((clean_question(&prompt), echo_on));
                self.clear_entry();
                self.entry_live = true;
                self.open = true;
                self.focus = Focus::Entry;
                PolkitEffects::redraw()
            }

            PolkitToNiri::ShowError(text) => {
                self.clear_entry();
                self.message = Some(Message::Error(text));
                self.open = true;
                PolkitEffects::redraw()
            }

            PolkitToNiri::ShowInfo(text) => {
                self.clear_entry();
                self.message = Some(Message::Info(text));
                self.open = true;
                PolkitEffects::redraw()
            }

            PolkitToNiri::Completed(true) => {
                self.reset();
                PolkitEffects {
                    request: Some(PolkitRequest::Done { dismissed: false }),
                    redraw: true,
                    close: true,
                    ..Default::default()
                }
            }

            // PAM refused. GNOME does not give up: it explains, shakes, and starts another
            // conversation (`:252-273`). The explanation is only synthesised when PAM did not
            // provide one — its own error is more specific than ours could be.
            PolkitToNiri::Completed(false) => {
                let mut effects = PolkitEffects {
                    request: Some(self.initiate()),
                    redraw: true,
                    arm_reset: true,
                    ..Default::default()
                };
                if !self.message.as_ref().is_some_and(Message::is_error) {
                    self.message = Some(Message::Error(GENERIC_FAILURE.to_owned()));
                    effects.wiggle = true;
                }
                self.entry_live = false;
                self.clear_entry();
                effects
            }
        }
    }

    /// The [`DELAYED_RESET`] timer fired with no new question in the meantime: put the dialog back
    /// to a bare confirmation until PAM has something to ask (`resetDialog`, `:333-342`).
    pub fn on_reset_timeout(&mut self) -> PolkitEffects {
        if self.entry_live {
            // A question arrived after the timer was armed; nothing to reset.
            return PolkitEffects::default();
        }
        self.question = None;
        self.focus = Focus::Cancel;
        PolkitEffects::redraw()
    }

    pub fn type_char(&mut self, c: char) -> PolkitEffects {
        if !self.entry_live || self.focus != Focus::Entry {
            return PolkitEffects::default();
        }
        self.entry.push(c);
        PolkitEffects::redraw()
    }

    pub fn backspace(&mut self) -> PolkitEffects {
        if !self.entry_live || self.focus != Focus::Entry {
            return PolkitEffects::default();
        }
        if self.entry.pop().is_none() {
            return PolkitEffects::default();
        }
        PolkitEffects::redraw()
    }

    /// Move focus on Tab / arrows. Cancel and Authenticate are always reachable; the entry only
    /// when there is one.
    pub fn cycle_focus(&mut self, forward: bool) -> PolkitEffects {
        let stops: &[Focus] = if self.shows_entry() {
            &[Focus::Entry, Focus::Cancel, Focus::Authenticate]
        } else {
            &[Focus::Cancel, Focus::Authenticate]
        };
        let at = stops.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % stops.len()
        } else {
            (at + stops.len() - 1) % stops.len()
        };
        self.focus = stops[next];
        PolkitEffects::redraw()
    }

    pub fn set_focus(&mut self, focus: Focus) -> PolkitEffects {
        if self.focus == focus {
            return PolkitEffects::default();
        }
        self.focus = focus;
        PolkitEffects::redraw()
    }

    /// Activate the Authenticate button, or press Enter in the entry — the same thing, except in
    /// [`Mode::Confirm`], where there is nothing to send and pressing it starts the conversation
    /// that authenticates (`_onAuthenticateButtonPressed`, `:236-241`).
    pub fn authenticate(&mut self) -> PolkitEffects {
        if self.mode == Mode::Confirm {
            return PolkitEffects {
                request: Some(self.initiate()),
                redraw: true,
                ..Default::default()
            };
        }
        if !self.entry_live || self.entry.is_empty() {
            return PolkitEffects::default();
        }

        let response = std::mem::replace(&mut self.entry, String::with_capacity(ENTRY_CAPACITY));
        // Insensitive until the next question, so a second Enter cannot resend it.
        self.entry_live = false;
        // "When the user responds, dismiss already shown info and error texts (if any)"
        // (`:229-233`).
        self.message = None;
        PolkitEffects {
            request: Some(PolkitRequest::Respond(response)),
            redraw: true,
            ..Default::default()
        }
    }

    /// The user said no. This is the path that must reach polkitd as `Cancelled`, not as a failed
    /// authentication — it is how the requesting program learns to stop rather than to retry.
    pub fn cancel(&mut self) -> PolkitEffects {
        self.reset();
        PolkitEffects {
            request: Some(PolkitRequest::Done { dismissed: true }),
            redraw: true,
            close: true,
            ..Default::default()
        }
    }

    fn reset(&mut self) {
        self.request = None;
        self.open = false;
        self.question = None;
        self.clear_entry();
        self.entry_live = false;
        self.message = None;
        self.caps_warning = false;
    }
}

impl Default for PolkitDialog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(user_name: &str, passwordless: bool) -> BeginRequest {
        BeginRequest {
            action_id: "org.freedesktop.test".to_owned(),
            message: "Authentication is required to test".to_owned(),
            user_name: user_name.to_owned(),
            passwordless,
        }
    }

    fn asked(dialog: &mut PolkitDialog) {
        dialog.on_agent_event(PolkitToNiri::Request {
            prompt: "Password:".to_owned(),
            echo_on: false,
        });
    }

    /// The dialog stays off screen between `BeginAuthentication` and PAM's first question
    /// (`_ensureOpen` is only reached from `_onSessionRequest`, `polkitAgent.js:297`).
    ///
    /// Opening on `Begin` instead would put a modal grab on the seat with nothing in it for as long
    /// as PAM took to decide it wanted anything — and PAM is free to take seconds.
    #[test]
    fn the_dialog_waits_for_pam_to_ask() {
        let mut dialog = PolkitDialog::new();
        let effects = dialog.begin(request("root", false));

        assert!(!dialog.is_open(), "nothing is on screen yet");
        assert!(!dialog.shows_entry(), "and there is no entry to type into");
        assert!(
            matches!(effects.request, Some(PolkitRequest::Initiate { .. })),
            "but PAM has been started"
        );

        asked(&mut dialog);
        assert!(dialog.is_open(), "PAM asked, so now it is on screen");
        assert!(dialog.shows_entry());
        assert_eq!(dialog.question(), Some("Password"));
    }

    /// A passwordless account must not have a conversation started for it until the user has
    /// confirmed: for such an account, starting one *is* the authentication (`:373-376`). Get this
    /// backwards and the action is authorised by a dialog that was never on screen.
    #[test]
    fn a_passwordless_account_confirms_before_anything_starts() {
        let mut dialog = PolkitDialog::new();
        let effects = dialog.begin(request("root", true));

        assert_eq!(dialog.mode(), Mode::Confirm);
        assert!(
            effects.request.is_none(),
            "no conversation may be started before the user confirms"
        );
        assert!(dialog.is_open(), "the dialog goes up instead");
        assert_eq!(dialog.focus(), Focus::Authenticate);
        assert!(
            dialog.can_authenticate(),
            "there is nothing to type, so the button is live"
        );

        let effects = dialog.authenticate();
        assert!(
            matches!(effects.request, Some(PolkitRequest::Initiate { .. })),
            "confirming is what starts it"
        );
    }

    /// PAM refusing is not the end: GNOME explains, shakes, and starts another conversation.
    #[test]
    fn a_refusal_explains_itself_and_tries_again() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);
        dialog.type_char('x');
        dialog.authenticate();

        let effects = dialog.on_agent_event(PolkitToNiri::Completed(false));
        assert!(
            matches!(effects.request, Some(PolkitRequest::Initiate { .. })),
            "a refusal starts another conversation"
        );
        assert!(effects.wiggle, "and shakes the entry");
        assert_eq!(
            dialog.message().map(Message::text),
            Some(GENERIC_FAILURE),
            "with an explanation, since PAM gave none"
        );
        assert!(dialog.is_open(), "the dialog stays up for the retry");
    }

    /// ...but only when PAM did not explain itself. PAM's own error names the actual problem —
    /// an expired account, a locked one, a fingerprint that did not match — and replacing it with
    /// "that didn't work" throws that away (`:258`).
    #[test]
    fn pams_own_error_is_not_overwritten() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);

        dialog.on_agent_event(PolkitToNiri::ShowError("Account expired".to_owned()));
        let effects = dialog.on_agent_event(PolkitToNiri::Completed(false));

        assert_eq!(
            dialog.message().map(Message::text),
            Some("Account expired"),
            "PAM said something more useful than we could"
        );
        assert!(
            !effects.wiggle,
            "and the shake belongs to the message we would have added"
        );
    }

    /// An info message must not be mistaken for an explanation: "Place your finger on the reader"
    /// is not a reason the password was refused, so a refusal after one still says so.
    #[test]
    fn an_info_message_does_not_count_as_an_explanation() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);

        dialog.on_agent_event(PolkitToNiri::ShowInfo("Swipe your finger".to_owned()));
        let effects = dialog.on_agent_event(PolkitToNiri::Completed(false));

        assert_eq!(dialog.message().map(Message::text), Some(GENERIC_FAILURE));
        assert!(effects.wiggle);
    }

    /// Submitting hands the password over and leaves nothing behind, and a second Enter before the
    /// next question sends nothing — PAM has one answer per question, and a resend would spend the
    /// next attempt on a password the user did not retype.
    #[test]
    fn a_password_is_sent_once_and_not_kept() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);
        for c in "hunter2".chars() {
            dialog.type_char(c);
        }
        assert!(dialog.can_authenticate());

        let effects = dialog.authenticate();
        match effects.request {
            Some(PolkitRequest::Respond(response)) => assert_eq!(response, "hunter2"),
            other => panic!("expected a response, got {other:?}"),
        }
        assert_eq!(dialog.entry_display(), "", "nothing is left in the entry");
        assert!(!dialog.is_entry_live(), "and it stops accepting input");

        assert!(dialog.authenticate().request.is_none(), "no second send");
        assert!(
            dialog.type_char('x').redraw.eq(&false),
            "and no more typing"
        );
    }

    /// The entry masks what is typed unless PAM said to echo it. Getting this backwards draws a
    /// password on the screen, so it is pinned in both directions.
    #[test]
    fn the_entry_masks_what_pam_said_to_mask() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));

        dialog.on_agent_event(PolkitToNiri::Request {
            prompt: "Password:".to_owned(),
            echo_on: false,
        });
        for c in "abc".chars() {
            dialog.type_char(c);
        }
        assert_eq!(dialog.entry_display(), "●●●", "echo off is masked");

        dialog.on_agent_event(PolkitToNiri::Request {
            prompt: "Login:".to_owned(),
            echo_on: true,
        });
        for c in "abc".chars() {
            dialog.type_char(c);
        }
        assert_eq!(dialog.entry_display(), "abc", "echo on is shown");
    }

    /// Cancelling must reach polkitd as a dismissal, not as a failure. The two mean different
    /// things to the program that asked: dismissed says the user declined, so stop asking.
    #[test]
    fn cancelling_is_a_dismissal() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);

        let effects = dialog.cancel();
        assert!(
            matches!(
                effects.request,
                Some(PolkitRequest::Done { dismissed: true })
            ),
            "cancelling is a dismissal"
        );
        assert!(effects.close);
        assert!(!dialog.is_open());
    }

    /// ...and succeeding is not.
    #[test]
    fn succeeding_is_not_a_dismissal() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);

        let effects = dialog.on_agent_event(PolkitToNiri::Completed(true));
        assert!(
            matches!(
                effects.request,
                Some(PolkitRequest::Done { dismissed: false })
            ),
            "authenticating is not a dismissal"
        );
        assert!(effects.close);
    }

    /// The root account gets the authority's name, not the account's, and that is what the warning
    /// colour hangs off (`:78-84`).
    #[test]
    fn root_is_labelled_administrator() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        assert!(dialog.is_root());
        assert_eq!(dialog.user_label(), ADMINISTRATOR);
    }

    /// The delayed reset only fires if nothing asked in the meantime. Without the guard, a
    /// question that lands inside the 200 ms window would have its entry taken away again.
    #[test]
    fn a_question_beats_the_delayed_reset() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", false));
        asked(&mut dialog);
        dialog.type_char('x');
        dialog.authenticate();
        let effects = dialog.on_agent_event(PolkitToNiri::Completed(false));
        assert!(effects.arm_reset, "the reset timer is armed on a refusal");

        // The next conversation asks before the timer fires.
        asked(&mut dialog);
        dialog.on_reset_timeout();
        assert!(
            dialog.shows_entry(),
            "the entry that just got its question must survive the stale timer"
        );
        assert!(dialog.is_entry_live());

        // With nothing asking, it does reset.
        dialog.type_char('y');
        dialog.authenticate();
        dialog.on_agent_event(PolkitToNiri::Completed(false));
        dialog.on_reset_timeout();
        assert!(!dialog.shows_entry(), "nothing asked, so the entry goes");
        assert_eq!(dialog.focus(), Focus::Cancel);
    }

    /// Focus skips the entry when there is not one, so Tab cannot land on a control that is not
    /// drawn.
    #[test]
    fn focus_only_visits_controls_that_exist() {
        let mut dialog = PolkitDialog::new();
        dialog.begin(request("root", true));
        assert!(!dialog.shows_entry());

        let mut seen = Vec::new();
        for _ in 0..4 {
            dialog.cycle_focus(true);
            seen.push(dialog.focus());
        }
        assert!(
            !seen.contains(&Focus::Entry),
            "Tab must not focus an entry that is not there: {seen:?}"
        );

        asked(&mut dialog);
        let mut seen = Vec::new();
        for _ in 0..3 {
            dialog.cycle_focus(true);
            seen.push(dialog.focus());
        }
        assert!(
            seen.contains(&Focus::Entry),
            "once there is an entry, Tab reaches it: {seen:?}"
        );
    }
}
