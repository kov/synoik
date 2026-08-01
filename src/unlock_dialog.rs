//! The unlock dialog's state — `UnlockDialog` + `AuthPrompt` + `ShellUserVerifier`
//! (`js/ui/unlockDialog.js`, `js/gdm/authPrompt.js`, `js/gdm/util.js`), minus their actors.
//!
//! The shield has two pages (`unlockDialog.js:600-606`): the clock, and this prompt. Everything
//! here is a pure state machine over [`crate::dbus::gdm`]'s events and the user's keystrokes, so
//! the whole authentication flow — including the parts that are awkward to reach live, like a
//! conversation that stops without a verdict — is testable without a bus, a seat, or PAM.
//!
//! # The one rule
//!
//! [`Status::Verified`] is reachable from exactly one place: gdm's `VerificationComplete`. No
//! keystroke, timeout, error path or `Default` may produce it. That is the whole security surface
//! of the lock screen, and it is small on purpose — grep for `Verified` and every write should be
//! in `on_verifier_event`.

use std::time::Duration;

use crate::dbus::gdm::{MessageKind, VerifierEvent, VerifierRequest};

/// Room for any password without reallocating; see [`UnlockDialog::clear_entry`].
const ENTRY_CAPACITY: usize = 512;

/// `IDLE_TIMEOUT = 2 * 60` — back to the clock after this long idle on the prompt
/// (`unlockDialog.js:25`, `:667`). The shield stays down; only the page changes.
pub const PROMPT_IDLE: Duration = Duration::from_secs(120);

/// Which page of the shield is up (`_showClock` / `_showPrompt`, `:786-830`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Clock,
    Prompt,
}

/// `AuthPromptStatus` (`authPrompt.js:34-40`), trimmed to the states this port reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No conversation. The entry is insensitive (`unlockDialog.js:766-767`).
    NotVerifying,
    /// gdm asked a question and is waiting for the answer — the entry is live.
    Asking,
    /// The answer is with PAM. The entry is insensitive until a verdict arrives, or a second
    /// Return would queue an answer to a question nobody asked.
    Answered,
    /// PAM refused. gdm resets the conversation, so this is a transient the user can retry from.
    Failed,
    /// PAM accepted. Only [`UnlockDialog::on_verifier_event`] may write this.
    Verified,
}

/// A line under the entry (`_onShowMessage`, `authPrompt.js`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub kind: MessageKind,
}

/// Who is being asked. Both come from AccountsService in GNOME; `real_name` falls back to the
/// login name, which is what `UserWidget` shows for an account with none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub real_name: String,
}

impl UserInfo {
    /// The label under the avatar — the real name if the account has one.
    pub fn display_name(&self) -> &str {
        if self.real_name.is_empty() {
            &self.name
        } else {
            &self.real_name
        }
    }
}

/// What the caller must do after driving the dialog.
#[derive(Debug, Clone, Default)]
pub struct UnlockEffects {
    /// Send this to [`crate::dbus::gdm`].
    pub request: Option<VerifierRequest>,
    /// PAM accepted: raise the shield. The **only** path out of a locked screen.
    pub unlock: bool,
    /// Something visible changed.
    pub redraw: bool,
}

impl UnlockEffects {
    fn redraw() -> Self {
        Self {
            redraw: true,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub struct UnlockDialog {
    page: Page,
    status: Status,
    user: UserInfo,
    /// What the user has typed. Cleared on every verdict and on every reset — a password must not
    /// outlive the question it answers.
    entry: String,
    /// gdm's prompt text, and whether to mask the entry. `secret` comes from which signal asked
    /// (`SecretInfoQuery` vs `InfoQuery`); defaulting it to `false` would draw a password in the
    /// clear, so there is no default — no question means no entry.
    question: Option<(String, bool)>,
    message: Option<Message>,
    /// Monotonic instant of the last interaction with the prompt, for [`PROMPT_IDLE`].
    last_activity: Option<Duration>,
}

impl UnlockDialog {
    pub fn new(user: UserInfo) -> Self {
        Self {
            page: Page::Clock,
            status: Status::NotVerifying,
            user,
            // Pre-sized so typing never reallocates: a `String` that grows 0→8→16 leaves the
            // earlier buffers on the heap, unzeroed, holding a prefix of the password.
            entry: String::with_capacity(ENTRY_CAPACITY),
            question: None,
            message: None,
            last_activity: None,
        }
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn user(&self) -> &UserInfo {
        &self.user
    }

    pub fn set_user(&mut self, user: UserInfo) {
        self.user = user;
    }

    pub fn message(&self) -> Option<&Message> {
        self.message.as_ref()
    }

    /// gdm's prompt text, if it has asked something.
    pub fn question(&self) -> Option<&str> {
        self.question.as_ref().map(|(q, _)| q.as_str())
    }

    /// Empty the entry, **zeroing what was there**.
    ///
    /// `String::clear` only sets the length to zero; the bytes stay in the allocation until it is
    /// reused or freed, so a password would sit in the compositor's heap for the rest of the
    /// session and land in any core dump. This is not a complete defence — see the exposure note
    /// in `docs/fork/lock-screen-port.md` — but it is the cheap half.
    fn clear_entry(&mut self) {
        // SAFETY: the buffer is overwritten with ASCII zeros, which is valid UTF-8, and then
        // truncated to nothing.
        unsafe {
            let bytes = self.entry.as_mut_vec();
            bytes.iter_mut().for_each(|b| {
                // `write_volatile` so the fill is not optimised away as a dead store.
                std::ptr::write_volatile(b, 0);
            });
            bytes.clear();
        }
        if self.entry.capacity() < ENTRY_CAPACITY {
            self.entry.reserve(ENTRY_CAPACITY - self.entry.capacity());
        }
    }

    /// Whether the entry must be masked. **Fails closed**: with no question outstanding this is
    /// `true`, so a stale buffer can never be drawn in the clear.
    pub fn is_secret(&self) -> bool {
        self.question.as_ref().is_none_or(|(_, secret)| *secret)
    }

    /// What to draw in the entry — already masked when it must be.
    pub fn entry_display(&self) -> String {
        if self.is_secret() {
            "\u{25cf}".repeat(self.entry.chars().count())
        } else {
            self.entry.clone()
        }
    }

    /// Whether the entry accepts input: only while gdm is actually waiting for an answer
    /// (`updateSensitivity`, `unlockDialog.js:766-767`).
    pub fn is_entry_live(&self) -> bool {
        self.page == Page::Prompt && self.status == Status::Asking
    }

    /// Raise the prompt page (`_showPrompt`, `:806`). Idempotent.
    pub fn show_prompt(&mut self, now: Duration) -> UnlockEffects {
        self.last_activity = Some(now);
        if self.page == Page::Prompt {
            return UnlockEffects::default();
        }
        self.page = Page::Prompt;
        UnlockEffects::redraw()
    }

    /// Back to the clock (`_showClock` / `_escape`). Drops whatever was typed.
    ///
    /// Does **not** unlock and does not cancel the conversation: GNOME keeps the auth prompt
    /// across a page flip and only destroys it once the crossfade lands
    /// (`_maybeDestroyAuthPrompt`, `:795`). Keeping it means a user who wanders back finds gdm
    /// still waiting rather than a channel that has to be rebuilt.
    pub fn show_clock(&mut self) -> UnlockEffects {
        self.clear_entry();
        self.last_activity = None;
        if self.page == Page::Clock {
            return UnlockEffects::default();
        }
        self.page = Page::Clock;
        UnlockEffects::redraw()
    }

    /// A printable character typed on the shield.
    ///
    /// On the clock page this raises the prompt **and keeps the character**
    /// (`vfunc_key_press_event`, `:672-692`): typing your password blind from the clock does not
    /// eat the first letter. Getting this wrong is invisible to a test that clicks first.
    pub fn type_char(&mut self, c: char, now: Duration) -> UnlockEffects {
        let mut effects = self.show_prompt(now);
        self.last_activity = Some(now);
        if self.is_entry_live() {
            self.entry.push(c);
            effects.redraw = true;
        }
        effects
    }

    pub fn backspace(&mut self, now: Duration) -> UnlockEffects {
        self.last_activity = Some(now);
        if !self.is_entry_live() || self.entry.pop().is_none() {
            return UnlockEffects::default();
        }
        UnlockEffects::redraw()
    }

    /// Return — send the answer to gdm (`_onNext`).
    pub fn submit(&mut self, now: Duration) -> UnlockEffects {
        self.last_activity = Some(now);
        if !self.is_entry_live() {
            return UnlockEffects::default();
        }
        // Clone, then zero ours: `mem::take` would hand the request our allocation and leave the
        // dialog with a fresh zero-capacity `String`, restarting the reallocation ladder that
        // `ENTRY_CAPACITY` exists to avoid.
        let answer = self.entry.clone();
        self.clear_entry();
        self.status = Status::Answered;
        self.message = None;
        UnlockEffects {
            request: Some(VerifierRequest::Answer(answer)),
            unlock: false,
            redraw: true,
        }
    }

    /// Escape — back to the clock (`on_key_press_event`, `authPrompt.js:242-248`).
    pub fn cancel(&mut self) -> UnlockEffects {
        self.show_clock()
    }

    /// Two minutes idle on the prompt drops back to the clock (`:667`).
    pub fn tick(&mut self, now: Duration) -> UnlockEffects {
        let Some(last) = self.last_activity else {
            return UnlockEffects::default();
        };
        if self.page != Page::Prompt || now.saturating_sub(last) < PROMPT_IDLE {
            return UnlockEffects::default();
        }
        self.show_clock()
    }

    /// Whether a [`tick`](Self::tick) is still pending, so the caller keeps a timer armed.
    pub fn is_waiting_to_escape(&self) -> bool {
        self.page == Page::Prompt && self.last_activity.is_some()
    }

    /// Drive the state machine from gdm.
    pub fn on_verifier_event(&mut self, event: VerifierEvent) -> UnlockEffects {
        match event {
            // The channel opening changes nothing on screen; the shield uses it to decide whether
            // it may lock at all.
            VerifierEvent::Ready(_) => UnlockEffects::default(),

            // The channel is gone. The shield drops its lock in response (so the user is not
            // trapped), which puts us back on the clock anyway.
            VerifierEvent::Lost => {
                self.status = Status::NotVerifying;
                self.question = None;
                self.clear_entry();
                self.page = Page::Clock;
                UnlockEffects::redraw()
            }

            VerifierEvent::Unavailable(..) => {
                self.status = Status::NotVerifying;
                self.question = None;
                self.clear_entry();
                // Deliberately no message: the shield does not lock without a channel, so there is
                // nothing for the user to do about it and a scary string on a screensaver is worse
                // than silence. The reason is in the journal.
                UnlockEffects::redraw()
            }

            VerifierEvent::AskQuestion { question, secret } => {
                self.question = Some((question, secret));
                self.clear_entry();
                self.status = Status::Asking;
                UnlockEffects::redraw()
            }

            VerifierEvent::ShowMessage { text, kind } => {
                self.message = Some(Message { text, kind });
                UnlockEffects::redraw()
            }

            // The one write of `Verified`.
            VerifierEvent::Complete => {
                self.status = Status::Verified;
                self.clear_entry();
                self.question = None;
                UnlockEffects {
                    request: None,
                    unlock: true,
                    redraw: true,
                }
            }

            VerifierEvent::Failed => {
                self.status = Status::Failed;
                self.clear_entry();
                // gdm re-asks on its own (it emits `Reset` then a fresh `SecretInfoQuery`), so the
                // question is dropped and the entry goes insensitive until it does. Leaving it
                // live would let the user type into a conversation that is no longer listening.
                self.question = None;
                if self.message.is_none() {
                    self.message = Some(Message {
                        text: "Authentication failed".to_owned(),
                        kind: MessageKind::Error,
                    });
                }
                UnlockEffects::redraw()
            }

            VerifierEvent::Reset => {
                self.status = Status::NotVerifying;
                self.clear_entry();
                self.question = None;
                self.message = None;
                UnlockEffects::redraw()
            }
        }
    }
}

/// The session's own user, for the prompt's avatar label.
///
/// GNOME reads this from AccountsService (`unlockDialog.js:757-759`). We read the passwd entry
/// directly: the login name, and the first GECOS field as the real name — which is exactly what
/// AccountsService itself seeds `real-name` from, so the common case agrees. It diverges for an
/// account whose real name was changed through AccountsService without touching GECOS; wiring
/// `org.freedesktop.Accounts` closes that and changes nothing else here.
pub fn session_user() -> UserInfo {
    // SAFETY: `getpwuid` returns a pointer into a static buffer. We copy both strings out before
    // returning, and make no other libc call that could reuse the buffer in between.
    unsafe {
        let uid = libc::getuid();
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return UserInfo::default();
        }
        let cstr = |p: *const libc::c_char| {
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let name = cstr((*pw).pw_name);
        // GECOS is comma-separated; the first field is the full name.
        let real_name = cstr((*pw).pw_gecos)
            .split(',')
            .next()
            .unwrap_or_default()
            .to_owned();
        UserInfo { name, real_name }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(1_000);

    fn dialog() -> UnlockDialog {
        UnlockDialog::new(UserInfo {
            name: "gsrs".to_owned(),
            real_name: "Test User".to_owned(),
        })
    }

    /// Ask, answer, accept — and the password is gone from the dialog the moment it is sent.
    #[test]
    fn a_successful_conversation_unlocks_and_keeps_no_copy_of_the_password() {
        let mut d = dialog();
        d.show_prompt(T0);
        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
        assert!(d.is_entry_live());

        for c in "hunter2".chars() {
            d.type_char(c, T0);
        }
        assert_eq!(
            d.entry_display(),
            "●".repeat(7),
            "and it is masked on screen"
        );

        let effects = d.submit(T0);
        match effects.request {
            Some(VerifierRequest::Answer(answer)) => assert_eq!(answer, "hunter2"),
            other => panic!("expected an Answer, got {other:?}"),
        }
        assert_eq!(d.entry_display(), "", "the buffer is emptied by the send");
        assert_eq!(d.status(), Status::Answered);
        assert!(!d.is_entry_live(), "a second Return must not answer twice");

        let effects = d.on_verifier_event(VerifierEvent::Complete);
        assert!(effects.unlock);
        assert_eq!(d.status(), Status::Verified);
    }

    /// **The security property.** Nothing but gdm's `VerificationComplete` unlocks.
    ///
    /// A dialog that unlocked on a stray keystroke, a timeout, or a failed attempt would be a
    /// lock screen in name only, and every one of those paths *looks* fine in a UI test. So this
    /// walks the whole input surface and asserts the negative.
    #[test]
    fn nothing_except_gdms_verdict_can_unlock() {
        let events = [
            VerifierEvent::Ready(1),
            VerifierEvent::Unavailable(1, "no gdm".to_owned()),
            VerifierEvent::Lost,
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            VerifierEvent::ShowMessage {
                text: "hi".to_owned(),
                kind: MessageKind::Info,
            },
            VerifierEvent::Failed,
            VerifierEvent::Reset,
        ];
        for event in events {
            let mut d = dialog();
            d.show_prompt(T0);
            let effects = d.on_verifier_event(event.clone());
            assert!(!effects.unlock, "{event:?} must not unlock");
            assert_ne!(d.status(), Status::Verified, "{event:?} must not verify");
        }

        // ...and neither does any local input, including on a fresh dialog that gdm never spoke to.
        let mut d = dialog();
        for effects in [
            d.show_prompt(T0),
            d.type_char('x', T0),
            d.backspace(T0),
            d.submit(T0),
            d.cancel(),
            d.tick(T0 + PROMPT_IDLE * 2),
        ] {
            assert!(!effects.unlock, "local input must never unlock");
        }
        assert_ne!(d.status(), Status::Verified);
    }

    /// A character typed on the *clock* page raises the prompt and is kept.
    ///
    /// `vfunc_key_press_event` (`:686-689`) shows the prompt and then feeds the character in, so
    /// typing a password blind does not eat its first letter. A test that clicks to the prompt
    /// first cannot see this.
    #[test]
    fn typing_from_the_clock_page_keeps_the_first_character() {
        let mut d = dialog();
        assert_eq!(d.page(), Page::Clock);

        // gdm has already asked — which is the real ordering: the channel opens when the screen
        // locks, long before anyone walks up to it.
        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

        d.type_char('h', T0);
        assert_eq!(d.page(), Page::Prompt, "the keystroke raised the prompt");
        assert_eq!(d.entry_display(), "●", "...and was not eaten by the flip");
    }

    /// The mask fails closed: with no question outstanding the entry is treated as secret.
    ///
    /// `secret` comes from *which signal* asked — `SecretInfoQuery` vs `InfoQuery`. Defaulting the
    /// unknown case to "visible" would print a password on the lock screen, so the default is the
    /// safe one and this pins it.
    #[test]
    fn the_entry_masks_unless_gdm_said_the_answer_is_visible() {
        let mut d = dialog();
        assert!(d.is_secret(), "no question yet — fail closed");

        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Username:".to_owned(),
            secret: false,
        });
        d.show_prompt(T0);
        d.type_char('a', T0);
        d.type_char('b', T0);
        assert_eq!(d.entry_display(), "ab", "an InfoQuery answer is shown");

        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
        d.type_char('a', T0);
        assert_eq!(d.entry_display(), "●", "a SecretInfoQuery answer is masked");

        // And a verdict drops the question, so the buffer cannot be re-shown in the clear.
        d.on_verifier_event(VerifierEvent::Failed);
        assert!(d.is_secret());
    }

    /// A refusal clears the entry, says so, and closes the entry until gdm asks again.
    #[test]
    fn a_failed_attempt_clears_the_entry_and_waits_for_gdm_to_re_ask() {
        let mut d = dialog();
        d.show_prompt(T0);
        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
        d.type_char('x', T0);
        d.submit(T0);

        d.on_verifier_event(VerifierEvent::Failed);
        assert_eq!(d.status(), Status::Failed);
        assert_eq!(d.entry_display(), "");
        assert!(
            !d.is_entry_live(),
            "typing into a conversation that stopped listening is a dead end"
        );
        assert!(
            d.message().is_some(),
            "the user is told why nothing happened"
        );

        // gdm resets and re-asks; the dialog comes back live.
        d.on_verifier_event(VerifierEvent::Reset);
        assert!(d.message().is_none(), "the reset clears the old error");
        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
        assert!(d.is_entry_live(), "and the user can try again");
    }

    /// Two minutes idle on the prompt goes back to the clock — and takes the typing with it.
    #[test]
    fn the_prompt_escapes_to_the_clock_after_two_idle_minutes() {
        let mut d = dialog();
        d.on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
        d.show_prompt(T0);
        d.type_char('x', T0);

        assert!(!d.tick(T0 + PROMPT_IDLE / 2).redraw, "not yet");
        assert_eq!(d.page(), Page::Prompt);

        assert!(d.tick(T0 + PROMPT_IDLE).redraw);
        assert_eq!(d.page(), Page::Clock);
        assert_eq!(
            d.entry_display(),
            "",
            "a half-typed password must not sit on an unattended screen"
        );

        // Activity re-arms it: the watch is on idleness, not on when the prompt opened.
        d.show_prompt(T0 + PROMPT_IDLE);
        d.type_char('y', T0 + PROMPT_IDLE);
        assert!(!d.tick(T0 + PROMPT_IDLE + PROMPT_IDLE / 2).redraw);
        assert_eq!(d.page(), Page::Prompt);
    }

    /// The label under the avatar is the real name, falling back to the login name.
    #[test]
    fn the_display_name_falls_back_to_the_login_name() {
        let mut user = UserInfo {
            name: "gsrs".to_owned(),
            real_name: "Test User".to_owned(),
        };
        assert_eq!(user.display_name(), "Test User");
        user.real_name.clear();
        assert_eq!(user.display_name(), "gsrs");
    }
}
