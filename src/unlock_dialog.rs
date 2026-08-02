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

use crate::dbus::gdm::{MessageKind, MessageSource, VerifierEvent, VerifierRequest};

/// Room for any password without reallocating; see [`UnlockDialog::clear_entry`].
const ENTRY_CAPACITY: usize = 512;

/// `IDLE_TIMEOUT = 2 * 60` — back to the clock after this long idle on the prompt
/// (`unlockDialog.js:25`, `:667`). The shield stays down; only the page changes.
pub const PROMPT_IDLE: Duration = Duration::from_secs(120);

/// `USER_READ_TIME` / `USER_READ_TIME_MIN` (`util.js:47-49`), comment and all:
///
/// > Give user 48ms to read each character of a PAM message
/// > or 2 seconds, whichever is longer
///
/// This is the whole reason a message queue exists rather than a single slot. gdm narrates faster
/// than anyone can read: a reader that fails on open reports its error and stops its conversation
/// in the same millisecond, and without a floor the error is drawn and overwritten inside one
/// frame. Live, that was "some text flashes below the password prompt" and no way to find out what
/// it said.
const USER_READ_TIME: Duration = Duration::from_millis(48);
const USER_READ_TIME_MIN: Duration = Duration::from_millis(2000);

/// `_getIntervalForMessage` (`util.js:248-254`) — how long `text` is owed on screen.
fn read_time(text: &str) -> Duration {
    // GNOME counts JS string length; chars is the same for anything a PAM module says and is the
    // right unit for "how long to read" regardless.
    (USER_READ_TIME * text.chars().count() as u32).max(USER_READ_TIME_MIN)
}

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
    /// Which conversation said it — see [`VerifierEvent::FilterMessages`].
    pub source: MessageSource,
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
    /// The prompt page has just come up, so the fingerprint reader may start listening.
    ///
    /// Separate from `request` because it is not a conversation the *user* drove: it rides the
    /// page transition, and it is a transition rather than a state because starting a service
    /// twice is an error.
    pub start_fingerprint: bool,
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
    /// A fingerprint error has reached the screen and has not been acted on yet — the view picks
    /// this up and wiggles (`authPrompt.js:485-490`).
    ///
    /// A flag drained by the caller rather than a field on [`UnlockEffects`] because a message can
    /// reach the screen from three different places — an event, a tick promoting it out of the
    /// queue, a filter dropping the one in front of it — and only one of those is a path that
    /// builds effects from scratch. One drain point cannot miss a case; three construction sites
    /// can.
    wiggle: bool,
    /// Earliest time [`message`](Self::message) may be replaced or cleared — `now` plus its
    /// [`read_time`], stamped when it went up. `None` when nothing is showing.
    message_until: Option<Duration>,
    /// What is waiting for the shown message to have had its time. `None` in the queue is a
    /// **clear**, which is how GNOME defers one too (`_filterServiceMessages` queues a null
    /// message "that will lead to clearing the prompt once done", `util.js:269-276`).
    message_queue: std::collections::VecDeque<Option<Message>>,
    /// The peek toggle: whether the password is currently shown in the clear
    /// (`st_password_entry_set_password_visible`, `st-password-entry.c:317-350`).
    peek: bool,
    /// `org.gnome.desktop.lockdown disable-show-password`. When set there is no toggle at all, and
    /// any peek already in effect is dropped (`on_disable_show_password_changed`, `:186-199`).
    peek_locked_down: bool,
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
            wiggle: false,
            message_until: None,
            message_queue: std::collections::VecDeque::new(),
            peek: false,
            peek_locked_down: false,
            last_activity: None,
        }
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// Replace the account's real name, once AccountsService has one.
    ///
    /// The login name is the fallback and is already right, so this only ever *improves* the
    /// label — an empty name from a machine with no AccountsService leaves it alone rather than
    /// blanking it (GNOME does blank it while loading, `userWidget.js:159-166`; showing the login
    /// name is friendlier and is what we had before this existed).
    pub fn set_real_name(&mut self, real_name: String) {
        self.user.real_name = real_name;
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

    /// Whether a fingerprint error has just landed, clearing the flag.
    ///
    /// Only the reader's errors wiggle. It is the service the user is *not* looking at — their eyes
    /// are on the entry — so its bad news has to ask for attention; a refused password does not,
    /// because they are already watching the thing that refused them.
    pub fn take_wiggle(&mut self) -> bool {
        std::mem::take(&mut self.wiggle)
    }

    /// When [`tick`](Self::tick) next has message work to do, so the caller can arm a timer.
    ///
    /// `None` when the queue is empty: a message with nothing behind it **stays on screen**. GNOME
    /// is the same — draining the queue emits `no-more-messages` and never touches the label
    /// (`finishMessageQueue`, `util.js:256-263`), so an error sits under the entry until something
    /// replaces or clears it rather than evaporating on a timer.
    pub fn message_deadline(&self) -> Option<Duration> {
        self.message_queue.front()?;
        self.message_until
    }

    /// The loudest message either on screen or already waiting.
    ///
    /// The queue is why this looks at both. A hint that arrives while an error is *queued* would
    /// otherwise slip in front of it and be gone before the error was ever drawn, which is the same
    /// bug the priority rule exists to stop, one step further along.
    fn loudest_message(&self) -> Option<MessageKind> {
        self.message
            .iter()
            .chain(self.message_queue.iter().flatten())
            .map(|m| m.kind)
            .max()
    }

    /// Show `message` — or clear, for `None` — once whatever is on screen has had its read time.
    ///
    /// Immediate when nothing is showing or its time is up, which is the common case: the queue
    /// only builds up when gdm says two things at once.
    fn queue_message(&mut self, message: Option<Message>, now: Duration) -> bool {
        if self.message_until.is_some_and(|until| now < until) {
            self.message_queue.push_back(message);
            return false;
        }
        self.show_message_now(message, now)
    }

    fn show_message_now(&mut self, message: Option<Message>, now: Duration) -> bool {
        let changed = self.message.as_ref() != message.as_ref();
        // Every route to the screen goes through here, which is the point of putting it here.
        if changed {
            self.wiggle |= message.as_ref().is_some_and(|m| {
                m.kind == MessageKind::Error && m.source == MessageSource::Fingerprint
            });
        }
        self.message_until = message.as_ref().map(|m| now + read_time(&m.text));
        self.message = message;
        changed
    }

    /// Clear immediately, dropping anything queued.
    ///
    /// For the paths where the message is not merely superseded but *gone*: the page it belonged to
    /// has been left, or the conversation it came from has been torn down. Nothing is owed read
    /// time on a screen that is no longer showing it.
    fn drop_messages(&mut self) -> bool {
        self.message_queue.clear();
        self.show_message_now(None, Duration::ZERO)
    }

    /// `_filterServiceMessages(source, ERROR)` (`util.js:269-276`) — drop everything `source` said
    /// that is quieter than an error, because its conversation has ended.
    ///
    /// **The read time does not protect these.** Everything else in this queue waits its turn, but
    /// a hint from a service that has stopped is no longer merely stale, it is *wrong*: it tells
    /// the user to place a finger on a reader the screen is no longer listening to, and leaving it
    /// up for its full interval is leaving a false instruction up on purpose. GNOME reaches the
    /// same place through `_queuePriorityMessage`, which clears the queue outright when the message
    /// on screen is one of the ones being filtered out (`:313-325`).
    ///
    /// Errors survive, from this service and any other: `ERROR` is the threshold, not the target.
    fn filter_messages(&mut self, source: MessageSource, now: Duration) -> bool {
        let doomed = |m: &Message| m.source == source && m.kind < MessageKind::Error;
        self.message_queue
            .retain(|m| !m.as_ref().is_some_and(doomed));
        if !self.message.as_ref().is_some_and(doomed) {
            return false;
        }
        // Whatever was behind it is owed its own time from now, not from when this one went up.
        let next = self.message_queue.pop_front().flatten();
        self.show_message_now(next, now)
    }

    /// Promote the next queued message if the shown one has had its time. `true` if the screen
    /// changed.
    fn advance_messages(&mut self, now: Duration) -> bool {
        let mut changed = false;
        while self.message_until.is_none_or(|until| now >= until) {
            let Some(next) = self.message_queue.pop_front() else {
                return changed;
            };
            changed |= self.show_message_now(next, now);
        }
        changed
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

    /// Whether gdm asked for a *secret*. **Fails closed**: with no question outstanding this is
    /// `true`, so a stale buffer can never be drawn in the clear.
    fn asked_for_secret(&self) -> bool {
        self.question.as_ref().is_none_or(|(_, secret)| *secret)
    }

    /// Whether gdm asked for a *secret*, regardless of whether the user is peeking at it.
    ///
    /// Distinct from [`is_secret`](Self::is_secret): the caps-lock warning is shown for the whole
    /// of a password question (`authPrompt.js:414` sets it from `secret`), including while the
    /// answer is revealed — caps lock still mangles what you type.
    pub fn asks_for_secret(&self) -> bool {
        self.asked_for_secret()
    }

    /// Whether the entry is masked right now — a secret prompt that the user has not chosen to
    /// reveal.
    pub fn is_secret(&self) -> bool {
        self.asked_for_secret() && !self.peek
    }

    /// The peek toggle's state, or `None` when there is none to show: a non-secret prompt has
    /// nothing to reveal, and `disable-show-password` removes it outright.
    pub fn peek(&self) -> Option<bool> {
        if !self.asked_for_secret() || self.peek_locked_down {
            return None;
        }
        Some(self.peek)
    }

    /// Show or hide the password (`st_password_entry_secondary_icon_clicked`, `:71-78`).
    pub fn toggle_peek(&mut self, now: Duration) -> UnlockEffects {
        self.last_activity = Some(now);
        if self.peek().is_none() {
            return UnlockEffects::default();
        }
        self.peek = !self.peek;
        UnlockEffects::redraw()
    }

    /// `org.gnome.desktop.lockdown disable-show-password`. Locking it down while a password is
    /// visible hides it again (`:191-193`) — otherwise the setting would not take effect until the
    /// next prompt, which is the moment it least helps.
    pub fn set_peek_locked_down(&mut self, locked_down: bool) {
        self.peek_locked_down = locked_down;
        if locked_down {
            self.peek = false;
        }
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
        // **The reader only listens while the prompt is up** (`_showPrompt` → `_ensureAuthPrompt`,
        // `unlockDialog.js:799-800`; GNOME builds the whole auth prompt here and not before). It is
        // not merely wasted work to start it earlier: the sensor lights up and asks for a finger,
        // so a screen showing nothing but a clock was demanding a fingerprint for a prompt the user
        // had not asked for.
        UnlockEffects {
            start_fingerprint: true,
            ..UnlockEffects::redraw()
        }
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
        // Not deferred: the message lives under the entry, and the entry is what we are leaving.
        // GNOME gets here by destroying the auth prompt outright (`_maybeDestroyAuthPrompt`).
        let cleared = self.drop_messages();
        if self.page == Page::Clock {
            return if cleared {
                UnlockEffects::redraw()
            } else {
                UnlockEffects::default()
            };
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
        // Queued rather than wiped: GNOME waits for pending messages before it resets or retries
        // (`await this._handlePendingMessages()`, `util.js:857-865`), so pressing Return the
        // instant an error appears does not swallow it.
        self.queue_message(None, now);
        UnlockEffects {
            request: Some(VerifierRequest::Answer(answer)),
            unlock: false,
            redraw: true,
            start_fingerprint: false,
        }
    }

    /// Escape — back to the clock (`on_key_press_event`, `authPrompt.js:242-248`).
    pub fn cancel(&mut self) -> UnlockEffects {
        self.show_clock()
    }

    /// Everything the dialog does on a clock rather than on an event: the message queue, and the
    /// two-minute idle escape (`:667`).
    ///
    /// Both live here because they are woken by different timers with very different periods — the
    /// queue by a deadline of its own, the escape by the panel's minute tick — and either may fire
    /// the other's work early. Draining the queue first also means a message queued *by* the escape
    /// is not left behind by it.
    pub fn tick(&mut self, now: Duration) -> UnlockEffects {
        let advanced = self.advance_messages(now);

        let escaping = self.page == Page::Prompt
            && self
                .last_activity
                .is_some_and(|last| now.saturating_sub(last) >= PROMPT_IDLE);
        if escaping {
            let mut effects = self.show_clock();
            effects.redraw |= advanced;
            return effects;
        }
        if advanced {
            UnlockEffects::redraw()
        } else {
            UnlockEffects::default()
        }
    }

    /// Whether a [`tick`](Self::tick) is still pending, so the caller keeps a timer armed.
    pub fn is_waiting_to_escape(&self) -> bool {
        self.page == Page::Prompt && self.last_activity.is_some()
    }

    /// Drive the state machine from gdm.
    ///
    /// `now` is only for the message queue — every other transition here is driven by gdm and has
    /// no clock of its own — but it has to come from the caller: a message's read time starts when
    /// the message is *shown*, and this is where that happens.
    pub fn on_verifier_event(&mut self, event: VerifierEvent, now: Duration) -> UnlockEffects {
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
                // Every fresh question re-masks: a peek is a decision about *this* answer, and
                // carrying it into the next one would reveal a password the user never chose to
                // show.
                self.peek = false;
                self.question = Some((clean_question(&question), secret));
                self.clear_entry();
                self.status = Status::Asking;
                UnlockEffects::redraw()
            }

            VerifierEvent::FilterMessages(source) => {
                if self.filter_messages(source, now) {
                    UnlockEffects::redraw()
                } else {
                    UnlockEffects::default()
                }
            }

            VerifierEvent::ShowMessage { text, kind, source } => {
                // `MessageType` is a **priority** (`util.js:58-63`), and GNOME's queue keeps the
                // loudest message showing rather than letting a later quiet one displace it
                // (`_queuePriorityMessage`, `:313-325`). It matters here because the two
                // conversations talk at once: pam_fprintd narrates on every scan, so without this
                // the fingerprint hint would wipe out "Sorry, that didn't work" a moment after the
                // user's password was refused, and they would never learn why.
                //
                // Louder-or-equal messages **queue** rather than replace, each owed its
                // [`read_time`]; only a quieter one is dropped outright. GNOME's plain
                // `_queueMessage` has no priority at all and would put an error behind a hint on
                // arrival order alone; keeping the drop means the reader's narration, which arrives
                // on every scan, cannot push the reason a password was refused off the end of a
                // queue the user is still reading.
                if self.loudest_message().is_some_and(|loudest| kind < loudest) {
                    return UnlockEffects::default();
                }
                if self.queue_message(Some(Message { text, kind, source }), now) {
                    UnlockEffects::redraw()
                } else {
                    UnlockEffects::default()
                }
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
                    start_fingerprint: false,
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
                    self.queue_message(
                        Some(Message {
                            text: "Authentication failed".to_owned(),
                            kind: MessageKind::Error,
                            source: MessageSource::Password,
                        }),
                        now,
                    );
                }
                UnlockEffects::redraw()
            }

            VerifierEvent::Reset => {
                self.status = Status::NotVerifying;
                self.clear_entry();
                self.question = None;
                // Deferred, not wiped. gdm resets the moment a conversation ends, which for a
                // reader that fails on open is the same millisecond it reported why — and GNOME
                // holds the reset back behind the message queue for exactly that reason
                // (`await this._handlePendingMessages()` before `_cancelAndReset`,
                // `util.js:857-865`).
                self.queue_message(None, now);
                UnlockEffects::redraw()
            }
        }
    }
}

/// Tidy PAM's prompt for display — `authPrompt.js:429-435`, comment and all:
///
/// > The question string comes directly from PAM, if it's "Password:" we replace it with our own
/// > to allow localization, if it's something else we remove the last colon and any trailing or
/// > leading spaces.
///
/// So the label really is "Password" with no colon; showing PAM's raw string is the tell that this
/// step was skipped.
fn clean_question(question: &str) -> String {
    if question == "Password:" || question == "Password: " {
        return "Password".to_owned();
    }
    // `[:：] *$` — the ASCII colon and the fullwidth one, plus any spaces after it.
    question
        .trim_end()
        .trim_end_matches([':', '：'])
        .trim()
        .to_owned()
}

/// The session's own user, for the prompt's avatar label.
///
/// GNOME reads this from AccountsService (`unlockDialog.js:757-759`). We read the passwd entry
/// directly: the login name, and the first GECOS field as the real name — which is exactly what
/// AccountsService itself seeds `real-name` from, so the common case agrees. It diverges for an
/// account whose real name was changed through AccountsService without touching GECOS; wiring
/// `org.freedesktop.Accounts` closes that and changes nothing else here.
pub fn session_user() -> UserInfo {
    // SAFETY: `getuid` is always successful and touches no memory.
    let uid = unsafe { libc::getuid() };
    user_for_uid(uid)
}

/// The same lookup for any uid — the polkit dialog authenticates as whoever polkitd names, which
/// is often `root` rather than us.
pub fn user_for_uid(uid: u32) -> UserInfo {
    let Some(entry) = crate::utils::passwd_entry(uid) else {
        return UserInfo::default();
    };
    UserInfo {
        name: entry.name,
        real_name: entry.real_name,
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
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
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

        let effects = d.on_verifier_event(VerifierEvent::Complete, T0);
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
                source: MessageSource::Password,
            },
            VerifierEvent::Failed,
            VerifierEvent::Reset,
        ];
        for event in events {
            let mut d = dialog();
            d.show_prompt(T0);
            let effects = d.on_verifier_event(event.clone(), T0);
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
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );

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

        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Username:".to_owned(),
                secret: false,
            },
            T0,
        );
        d.show_prompt(T0);
        d.type_char('a', T0);
        d.type_char('b', T0);
        assert_eq!(d.entry_display(), "ab", "an InfoQuery answer is shown");

        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        d.type_char('a', T0);
        assert_eq!(d.entry_display(), "●", "a SecretInfoQuery answer is masked");

        // And a verdict drops the question, so the buffer cannot be re-shown in the clear.
        d.on_verifier_event(VerifierEvent::Failed, T0);
        assert!(d.is_secret());
    }

    /// A refusal clears the entry, says so, and closes the entry until gdm asks again.
    #[test]
    fn a_failed_attempt_clears_the_entry_and_waits_for_gdm_to_re_ask() {
        let mut d = dialog();
        d.show_prompt(T0);
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        d.type_char('x', T0);
        d.submit(T0);

        d.on_verifier_event(VerifierEvent::Failed, T0);
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

        // gdm resets and re-asks; the dialog comes back live. The reset does not take the error
        // with it on the way out — it waits for it to have been readable.
        d.on_verifier_event(VerifierEvent::Reset, T0);
        assert!(
            d.message().is_some(),
            "the reset swallowed the error before anyone could read it"
        );
        d.tick(T0 + Duration::from_secs(3));
        assert!(d.message().is_none(), "the reset clears the old error");
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        assert!(d.is_entry_live(), "and the user can try again");
    }

    /// Two minutes idle on the prompt goes back to the clock — and takes the typing with it.
    #[test]
    fn the_prompt_escapes_to_the_clock_after_two_idle_minutes() {
        let mut d = dialog();
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
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

    /// The peek toggle reveals the password, and every guard around it holds.
    ///
    /// This is the one control that is *supposed* to show a password, so the things that must stay
    /// true around it are worth pinning: it does not exist on a non-secret prompt or under
    /// `disable-show-password`, locking that down hides an already-visible password rather than
    /// waiting for the next prompt, and a fresh question re-masks — a peek is a decision about the
    /// answer being typed, not a mode.
    #[test]
    fn the_peek_toggle_reveals_only_what_it_should() {
        let mut d = dialog();
        d.show_prompt(T0);
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        for c in "hunter2".chars() {
            d.type_char(c, T0);
        }
        assert_eq!(d.entry_display(), "●".repeat(7));
        assert_eq!(d.peek(), Some(false), "the toggle is there, and off");

        d.toggle_peek(T0);
        assert_eq!(d.peek(), Some(true));
        assert_eq!(d.entry_display(), "hunter2", "revealed on request");

        // A new question re-masks: the peek belonged to the answer that was refused.
        d.on_verifier_event(VerifierEvent::Failed, T0);
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        assert_eq!(d.peek(), Some(false), "a fresh question is masked again");

        // Lockdown removes the toggle, and takes effect immediately on a visible password.
        d.type_char('x', T0);
        d.toggle_peek(T0);
        assert_eq!(d.entry_display(), "x");
        d.set_peek_locked_down(true);
        assert_eq!(d.peek(), None, "no toggle under disable-show-password");
        assert_eq!(
            d.entry_display(),
            "●",
            "and what was showing is hidden again"
        );
        d.toggle_peek(T0);
        assert_eq!(
            d.entry_display(),
            "●",
            "the toggle does nothing while locked down"
        );

        // A non-secret prompt has nothing to reveal, so it has no toggle either.
        d.set_peek_locked_down(false);
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Username:".to_owned(),
                secret: false,
            },
            T0,
        );
        assert_eq!(d.peek(), None);
    }

    /// PAM's prompt is tidied before it is shown: the colon goes, and the common case is
    /// replaced outright (`authPrompt.js:429-435`).
    #[test]
    fn the_prompt_label_loses_its_colon() {
        assert_eq!(clean_question("Password:"), "Password");
        assert_eq!(clean_question("Password: "), "Password");
        // The general case: strip one trailing colon and the whitespace around it.
        assert_eq!(clean_question("Enter PIN:  "), "Enter PIN");
        assert_eq!(clean_question("パスワード："), "パスワード");
        // ...and a prompt that never had one is left alone.
        assert_eq!(clean_question("Touch the sensor"), "Touch the sensor");

        // It runs on the way in, so the dialog never holds the raw string.
        let mut d = dialog();
        d.on_verifier_event(
            VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            },
            T0,
        );
        assert_eq!(d.question(), Some("Password"));
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

    /// The fingerprint hint must not wipe out the error that says why the password was refused.
    ///
    /// The two conversations talk at the same time, and pam_fprintd narrates on *every* scan — so
    /// without a priority the sequence "password refused, finger brushed the sensor" ends with the
    /// user looking at "(or place finger on reader)" and no idea what went wrong. GNOME's
    /// `MessageType` is ordered for exactly this and its queue keeps the loudest message showing
    /// (`util.js:58-63`, `_queuePriorityMessage`, `:313-325`).
    #[test]
    fn a_hint_never_displaces_a_louder_message() {
        let mut d = dialog();

        let hint = || VerifierEvent::ShowMessage {
            text: "(or place finger on reader)".to_owned(),
            kind: MessageKind::Hint,
            source: MessageSource::Fingerprint,
        };
        let error = || VerifierEvent::ShowMessage {
            text: "Sorry, that didn't work".to_owned(),
            kind: MessageKind::Error,
            source: MessageSource::Password,
        };

        // Nothing showing: the hint takes the slot.
        d.on_verifier_event(hint(), T0);
        assert_eq!(d.message().map(|m| m.kind), Some(MessageKind::Hint));

        // The password is refused: the error is louder, so it queues rather than being dropped —
        // and takes the screen as soon as the hint has had its read time.
        d.on_verifier_event(error(), T0);

        // The reader narrates again while the error is still *waiting its turn*. Judging the
        // newcomer against what is on screen alone would let it in behind the error, so the error
        // would appear and then be wiped by a hint the user never needed. It has to lose to the
        // queue as well.
        d.on_verifier_event(hint(), T0);

        let mut t = T0 + read_time("(or place finger on reader)");
        d.tick(t);
        assert_eq!(
            d.message().map(|m| m.text.as_str()),
            Some("Sorry, that didn't work")
        );

        // ...and it is still the error a full read time later: the hint was dropped, not deferred.
        t += read_time("Sorry, that didn't work");
        d.tick(t);
        assert_eq!(
            d.message().map(|m| m.text.as_str()),
            Some("Sorry, that didn't work"),
            "the hint erased the reason the unlock failed"
        );

        // An equally loud message does replace, so a *second* error is not swallowed.
        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: "Sorry, that didn't work either".to_owned(),
                kind: MessageKind::Error,
                source: MessageSource::Password,
            },
            t,
        );
        assert_eq!(
            d.message().map(|m| m.text.as_str()),
            Some("Sorry, that didn't work either")
        );

        // ...and once the slot is cleared, the hint is welcome again.
        d.on_verifier_event(VerifierEvent::Reset, t);
        t += read_time("Sorry, that didn't work either");
        d.tick(t);
        d.on_verifier_event(hint(), t);
        assert_eq!(d.message().map(|m| m.kind), Some(MessageKind::Hint));
    }

    /// A message gets its read time even when gdm contradicts itself in the same millisecond.
    ///
    /// This is the fingerprint reader that fails on open: it reports why and its conversation stops
    /// in the same breath, so the error and the reset that clears it arrive together. Without a
    /// floor the label is written and blanked inside one frame — live, that was "some text flashes
    /// below the password prompt" and no way to find out what it said. GNOME's floor is 48 ms per
    /// character or two seconds, whichever is longer (`util.js:47-49`).
    #[test]
    fn a_message_is_readable_even_when_the_next_event_lands_on_top_of_it() {
        let mut d = dialog();
        let text = "Device reported an error during verify";

        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: text.to_owned(),
                kind: MessageKind::Error,
                source: MessageSource::Fingerprint,
            },
            T0,
        );
        // Same instant: the conversation that said it is torn down.
        d.on_verifier_event(VerifierEvent::Reset, T0);

        // Still up, and still up a second later.
        assert_eq!(d.message().map(|m| m.text.as_str()), Some(text));
        d.tick(T0 + Duration::from_millis(999));
        assert_eq!(
            d.message().map(|m| m.text.as_str()),
            Some(text),
            "the message was gone before it could be read"
        );

        // The floor is a real minimum, not a token delay.
        assert!(read_time(text) >= USER_READ_TIME_MIN);
        // ...and once it has been served, the clear that was waiting behind it happens.
        d.tick(T0 + read_time(text));
        assert!(d.message().is_none(), "the queued clear never arrived");

        // A message with nothing behind it is not on a timer at all — it stays until something
        // replaces it, exactly as GNOME's drained queue leaves the label alone.
        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: text.to_owned(),
                kind: MessageKind::Error,
                source: MessageSource::Fingerprint,
            },
            T0,
        );
        assert_eq!(d.message_deadline(), None, "nothing is queued behind it");
        d.tick(T0 + read_time(text) * 10);
        assert_eq!(d.message().map(|m| m.text.as_str()), Some(text));
    }

    /// A reader that dies takes its own instruction off the screen with it.
    ///
    /// The exact sequence a reader that fails on open produces, in order: it narrates (so we show
    /// the hint), gdm reports `ServiceUnavailable` with an **empty** message — that is not a
    /// mistake, `PAM_AUTHINFO_UNAVAIL` sets the text to the literal empty string
    /// (`gdm-session-worker.c:1272-1280`), which is why GNOME tests for it (`util.js:892-893`) —
    /// and the conversation stops.
    ///
    /// So there is no error to put on screen, and the only honest thing left is to stop telling the
    /// user to place a finger on a reader nothing is listening to. Leaving that hint up was the
    /// live report: the prompt said "(or place finger on reader)" while the sensor had already been
    /// given up on.
    #[test]
    fn a_reader_that_gives_up_stops_asking_for_a_finger() {
        let mut d = dialog();
        let hint = "(or place finger on reader)";

        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: hint.to_owned(),
                kind: MessageKind::Hint,
                source: MessageSource::Fingerprint,
            },
            T0,
        );
        assert_eq!(d.message().map(|m| m.text.as_str()), Some(hint));

        // The reader's conversation ends, in the same millisecond and with nothing to say.
        d.on_verifier_event(
            VerifierEvent::FilterMessages(MessageSource::Fingerprint),
            T0,
        );
        assert!(
            d.message().is_none(),
            "the prompt still asks for a finger the screen has stopped listening for"
        );

        // The password conversation is untouched by the reader's filter — it is the one way in,
        // and its messages are not the reader's to clear.
        let refused = "Sorry, that didn't work";
        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: refused.to_owned(),
                kind: MessageKind::Error,
                source: MessageSource::Password,
            },
            T0,
        );
        d.on_verifier_event(
            VerifierEvent::FilterMessages(MessageSource::Fingerprint),
            T0,
        );
        assert_eq!(d.message().map(|m| m.text.as_str()), Some(refused));

        // ...and an *error* from the reader outranks the threshold, so it survives its own
        // conversation ending. `Error` is the cut-off, not the target.
        let mut d = dialog();
        let problem = "Failed to match fingerprint";
        d.on_verifier_event(
            VerifierEvent::ShowMessage {
                text: problem.to_owned(),
                kind: MessageKind::Error,
                source: MessageSource::Fingerprint,
            },
            T0,
        );
        d.on_verifier_event(
            VerifierEvent::FilterMessages(MessageSource::Fingerprint),
            T0,
        );
        assert_eq!(d.message().map(|m| m.text.as_str()), Some(problem));
    }

    /// A hint waiting *behind* another is filtered too, not just the one on screen.
    ///
    /// The reader narrates on every scan, so a second hint lands while the first is still being
    /// read. If the filter only reached what is showing, the reader's conversation could end and
    /// the instruction would be promoted afterwards — the same false instruction, arriving late and
    /// from a service that no longer exists.
    #[test]
    fn a_queued_hint_dies_with_its_service() {
        let mut d = dialog();
        let hint = || VerifierEvent::ShowMessage {
            text: "(or place finger on reader)".to_owned(),
            kind: MessageKind::Hint,
            source: MessageSource::Fingerprint,
        };

        d.on_verifier_event(hint(), T0);
        // The reader scans again while the first is still owed its read time, so this one queues.
        d.on_verifier_event(hint(), T0);
        assert!(d.message_deadline().is_some(), "the second hint must queue");

        d.on_verifier_event(
            VerifierEvent::FilterMessages(MessageSource::Fingerprint),
            T0,
        );
        assert!(d.message().is_none());
        d.tick(T0 + read_time("(or place finger on reader)") * 3);
        assert!(
            d.message().is_none(),
            "a hint from a dead service was promoted after the fact"
        );
    }

    /// Only the **reader's errors** wiggle, and they wiggle however they reach the screen.
    ///
    /// GNOME's condition is both halves at once (`authPrompt.js:485-486`): `ERROR`, *and* from the
    /// fingerprint service. A refused password is an error too, and shaking for it would be
    /// shaking at somebody already looking at the thing that refused them. The hint is from the
    /// right service and must not shake either — it is not bad news, it is an offer.
    ///
    /// The second half matters as much as the first: a fingerprint error rarely arrives on an idle
    /// screen. It queues behind whatever is up, so the moment it becomes *visible* is a tick, not
    /// an event — a wiggle raised only where the event is handled would fire while the message was
    /// still invisible and be over before anyone saw it.
    #[test]
    fn only_the_readers_errors_wiggle() {
        let show = |kind, source| VerifierEvent::ShowMessage {
            text: "something happened".to_owned(),
            kind,
            source,
        };

        // A password error: no wiggle.
        let mut d = dialog();
        d.on_verifier_event(show(MessageKind::Error, MessageSource::Password), T0);
        assert!(!d.take_wiggle());

        // The reader's hint: no wiggle.
        let mut d = dialog();
        d.on_verifier_event(show(MessageKind::Hint, MessageSource::Fingerprint), T0);
        assert!(!d.take_wiggle());

        // The reader's error: wiggle, and **once** — a second drain must not shake again.
        let mut d = dialog();
        d.on_verifier_event(show(MessageKind::Error, MessageSource::Fingerprint), T0);
        assert!(d.take_wiggle());
        assert!(!d.take_wiggle(), "the wiggle was not drained");

        // ...and the same error arriving *behind* another message wiggles when it is promoted, not
        // when it was queued.
        let mut d = dialog();
        d.on_verifier_event(show(MessageKind::Error, MessageSource::Password), T0);
        assert!(!d.take_wiggle());
        d.on_verifier_event(show(MessageKind::Error, MessageSource::Fingerprint), T0);
        assert!(
            !d.take_wiggle(),
            "it wiggled while the message was still queued behind another"
        );
        d.tick(T0 + read_time("something happened"));
        assert!(
            d.take_wiggle(),
            "it never wiggled once it reached the screen"
        );
    }

    /// A long message is owed longer than the floor, at GNOME's 48 ms a character.
    #[test]
    fn the_read_time_grows_with_the_message() {
        assert_eq!(read_time(""), USER_READ_TIME_MIN);
        // Short enough that the floor still wins.
        assert_eq!(read_time("Sorry, that didn't work"), USER_READ_TIME_MIN);
        // Long enough that it does not.
        let long = "x".repeat(100);
        assert_eq!(read_time(&long), USER_READ_TIME * 100);
        assert!(read_time(&long) > USER_READ_TIME_MIN);
    }

    /// The reader is armed by the prompt coming up, and by nothing else.
    ///
    /// This is not an optimisation. An armed sensor **asks for a finger** — it lights up, and on
    /// hardware backed by a platform authenticator it puts a prompt in front of the user. Starting
    /// it with the channel meant a screen showing nothing but a clock was demanding a fingerprint
    /// for a prompt nobody had asked for. GNOME never has the problem because it does not build the
    /// auth prompt at all until `_showPrompt` (`unlockDialog.js:799-800`); we open the channel
    /// early on purpose, so the reader has to be held back separately.
    #[test]
    fn the_reader_is_armed_by_the_prompt_and_not_by_the_lock() {
        let mut d = dialog();

        // A conversation beginning, a question arriving, a message: none of these is the prompt
        // being shown, and none of them may arm the reader.
        assert!(
            !d.on_verifier_event(VerifierEvent::Reset, T0)
                .start_fingerprint
        );
        assert!(
            !d.on_verifier_event(
                VerifierEvent::AskQuestion {
                    question: "Password:".to_owned(),
                    secret: true,
                },
                T0
            )
            .start_fingerprint,
            "a question arriving while the clock is up must not arm the reader"
        );
        assert_eq!(d.page(), Page::Clock, "and none of that raised the prompt");

        // Raising the prompt does.
        assert!(d.show_prompt(T0).start_fingerprint);
        assert_eq!(d.page(), Page::Prompt);

        // Raising it again while it is already up is not a transition, so there is nothing to arm.
        assert!(!d.show_prompt(T0).start_fingerprint);

        // Back to the clock and up again: armed once more. The verifier task makes this idempotent
        // — gdm errors on a service that is already running — but the *page* has genuinely changed.
        d.show_clock();
        assert!(d.show_prompt(T0).start_fingerprint);
    }
}
