//! The gdm reauthentication channel — how the lock screen checks a password.
//!
//! **No PAM runs in this process.** GNOME's unlock dialog does not authenticate; it asks gdm to,
//! over `Gdm.Client`'s reauthentication channel (`js/gdm/util.js:508-540`), and gdm runs the PAM
//! conversation in its own worker. That is both the faithful port and what the
//! untrusted-content-process-seam rule would ask for independently: the thing handling a password
//! and talking to PAM modules is not the compositor.
//!
//! # The shape of the channel
//!
//! 1. On the **system** bus, `org.gnome.DisplayManager.Manager.OpenReauthenticationChannel(user)`
//!    returns a **D-Bus address**, not an object path.
//! 2. That address is a **peer-to-peer** socket — libgdm connects with
//!    `G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT` and *without* `MESSAGE_BUS_CONNECTION`
//!    (`gdm-client.c:373-378`). No bus name, no `Hello`, and zbus needs its `p2p` feature.
//! 3. On that connection, `org.gnome.DisplayManager.UserVerifier` lives at
//!    **`/org/gnome/DisplayManager/Session`** (`gdm-client.c:345-348`'s `SESSION_DBUS_PATH`) — not
//!    at any path containing "UserVerifier", which is the obvious wrong guess.
//! 4. `BeginVerificationForUser("gdm-password", user)` starts a PAM conversation, driven by
//!    signals: `SecretInfoQuery` for the password prompt, `InfoQuery` for a visible one,
//!    `Info`/`Problem` for messages, then `VerificationComplete` or a stop. Answers go back with
//!    `AnswerQuery(service, answer)`.
//!
//! # Why this does not use a `#[proxy]`
//!
//! zbus's generated proxies require a destination (`proxy/builder.rs:96-98`, no p2p exemption) and
//! a peer connection has no bus names to give one. Neither escape works:
//!
//! - a unique-looking destination (`:1.0`) makes the signal filter compare it against each
//!   message's sender (`proxy/mod.rs:1151-1157`), and **p2p messages carry no sender**, so every
//!   signal is silently dropped and the dialog hangs forever;
//! - a well-known-looking one sends `GetNameOwner` down the peer socket, which gdm has no
//!   `org.freedesktop.DBus` to answer — it error-replies, zbus falls back to "no owner"
//!   (`proxy/mod.rs:1209-1215`), and the filter then passes. That *works*, by accident of gdm
//!   choosing to error rather than ignore.
//!
//! Building on the second would be a trap for whoever touched it next, so the peer side uses raw
//! [`zbus::Connection::call_method`] — whose destination is an `Option` — and its own
//! `MessageStream`. The system-bus half is an ordinary proxy.
//!
//! # Retry is ours to drive
//!
//! gdm does **not** re-ask after a refusal. `report_and_stop_conversation`
//! (`daemon/gdm-session.c:210-237`) emits `VerificationFailed` and then stops the conversation;
//! `gdm_session_reset` is only reached from session start/migration. GNOME re-begins from the
//! client: `util.js:929` → `_retry` (`:866`) → `_startService` (`:678`) issues another
//! `BeginVerificationForUser`, and `_canRetry` (`:839-841`) is unconditionally true in unlock mode
//! because `authPrompt.js:162-168` passes `reauthenticationOnly: true`.
//!
//! Without that, one mistyped password is an unrecoverable lock screen — and a silent one, since
//! `gdm_session_answer_query` (`gdm-session.c:3063-3078`) no-ops on a dead conversation after the
//! D-Bus handler has already replied successfully.
//!
//! Note also that a refusal arrives **twice** (`VerificationFailed` *and* `ConversationStopped`).
//! GNOME does not subscribe to `VerificationFailed` at all (`util.js:566-579` lists every signal it
//! connects, and that is not among them); failure is driven off the stop. This does the same, or a
//! single refusal would begin two conversations.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use zbus::proxy;

/// The PAM service gdm runs for password authentication (`js/gdm/util.js:27`).
const PASSWORD_SERVICE: &str = "gdm-password";
/// ...and the one it runs for the reader, **beside** the password rather than instead of it
/// (`FINGERPRINT_SERVICE_NAME`, `util.js:28`; `_maybeStartFingerprintVerification`, `:714-719`).
const FINGERPRINT_SERVICE: &str = "gdm-fingerprint";
/// `SESSION_DBUS_PATH` (`gdm-client.c:37`) — where the verifier sits on the p2p connection.
const SESSION_PATH: &str = "/org/gnome/DisplayManager/Session";
const VERIFIER_IFACE: &str = "org.gnome.DisplayManager.UserVerifier";

#[proxy(
    interface = "org.gnome.DisplayManager.Manager",
    default_service = "org.gnome.DisplayManager",
    default_path = "/org/gnome/DisplayManager/Manager"
)]
trait DisplayManager {
    /// Returns a peer-to-peer D-Bus **address**, not an object path.
    fn open_reauthentication_channel(&self, username: &str) -> zbus::Result<String>;
}

/// Which lock a request or event belongs to.
///
/// Opening a channel is a D-Bus round trip that can take a zbus timeout to fail, and a shield can
/// be raised and re-locked inside that window. Without this, the answer to lock #1 satisfies lock
/// #2's gate — leaving the screen covered but *unlocked*, or locked with no conversation behind it.
pub type Epoch = u64;

/// What the compositor asks of the verifier.
#[derive(Clone)]
pub enum VerifierRequest {
    /// Open a channel for `username` and start `gdm-password` — plus `gdm-fingerprint` when
    /// `reader` says there is one. Answered by [`VerifierEvent::Ready`] or
    /// [`VerifierEvent::Unavailable`] carrying the same `epoch`.
    Begin {
        username: String,
        epoch: Epoch,
        /// What the fprintd probe found. `None` starts one service, not two.
        reader: crate::dbus::fprintd::ReaderType,
    },
    /// Answer the outstanding query. **Carries a password** — see the `Debug` impl below.
    Answer(String),
    /// The prompt page came up: start `gdm-fingerprint` beside the password conversation, if there
    /// is a reader and it is not already running.
    StartFingerprint,
    /// Tear the conversation down: the shield was raised, or the user cancelled.
    Cancel,
}

/// Hand-written so a stray `{:?}` cannot put a password in the journal. `derive(Debug)` would print
/// it, and every type embedding this one would inherit that.
impl std::fmt::Debug for VerifierRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin {
                username,
                epoch,
                reader,
            } => f
                .debug_struct("Begin")
                .field("username", username)
                .field("epoch", epoch)
                .field("reader", reader)
                .finish(),
            Self::Answer(answer) => write!(f, "Answer(<redacted, {} chars>)", answer.len()),
            Self::StartFingerprint => write!(f, "StartFingerprint"),
            Self::Cancel => write!(f, "Cancel"),
        }
    }
}

/// A message shown under the entry, and how it reads (`js/gdm/util.js:728-782`).
///
/// The order is GNOME's `MessageType` (`util.js:58-63`) and it is a **priority**, not a taxonomy:
/// its queue keeps the highest-priority message showing and drops lower ones that would displace it
/// (`_queuePriorityMessage`, `:313-325`). That ordering is why a fingerprint hint cannot wipe out
/// the error explaining why the last password was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageKind {
    /// An aside rather than a statement — today, only "(or place finger on reader)".
    Hint,
    Info,
    Error,
}

/// What the verifier tells the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifierEvent {
    /// A channel is open and the conversation has begun. **This is what makes locking safe** —
    /// until it arrives, the shield must not enter `locked`.
    Ready(Epoch),
    /// No channel: gdm absent, the reauth denied, or the conversation never started. The shield
    /// stays merely active.
    Unavailable(Epoch, String),
    /// gdm wants an answer. `secret` distinguishes `SecretInfoQuery` (mask the entry) from
    /// `InfoQuery` (show it) — get it backwards and a password is drawn on the lock screen.
    AskQuestion {
        question: String,
        secret: bool,
    },
    ShowMessage {
        text: String,
        kind: MessageKind,
    },
    /// PAM accepted. The one event that may raise a locked shield.
    Complete,
    /// The conversation ended without accepting. A fresh one has already been started, so the
    /// dialog can expect a new question.
    Failed,
    /// The channel died and could not be rebuilt — gdm went away mid-conversation. The prompt has
    /// nothing behind it; say so rather than leave the user typing into a dead socket.
    Lost,
    /// The conversation was reset; clear the entry and any message (`util.js:807-815`).
    Reset,
}

/// Run the verifier client. Returns the system-bus connection to keep alive, and the sender the
/// compositor drives it with.
pub fn start(
    to_niri: calloop::channel::Sender<VerifierEvent>,
) -> anyhow::Result<(
    zbus::blocking::Connection,
    async_channel::Sender<VerifierRequest>,
)> {
    let conn = zbus::blocking::Connection::system()?;
    let system = conn.inner().clone();
    let (tx, rx) = async_channel::unbounded();

    conn.inner()
        .executor()
        .spawn(run(system, rx, to_niri), "gdm-user-verifier")
        .detach();

    Ok((conn, tx))
}

async fn run(
    system: zbus::Connection,
    requests: async_channel::Receiver<VerifierRequest>,
    to_niri: calloop::channel::Sender<VerifierEvent>,
) {
    // One conversation at a time, held across requests so `Answer` reaches the channel that asked.
    let mut session: Option<Session> = None;

    while let Ok(request) = requests.recv().await {
        match request {
            VerifierRequest::Begin {
                username,
                epoch,
                reader,
            } => {
                // Close the previous channel *before* opening another: gdm keeps a PAM worker per
                // channel, and an orphan would go on emitting into the compositor — including a
                // `Complete` that would raise a shield it knows nothing about.
                if let Some(old) = session.take() {
                    old.close().await;
                }
                match Session::open(&system, &username, reader, to_niri.clone()).await {
                    Ok(new) => {
                        session = Some(new);
                        let _ = to_niri.send(VerifierEvent::Ready(epoch));
                    }
                    Err(err) => {
                        // Loud: this is the difference between "locked" and "blanked", and a user
                        // whose screen never locks deserves a reason in the journal.
                        warn!("gdm: no reauthentication channel: {err:#}");
                        let _ = to_niri.send(VerifierEvent::Unavailable(epoch, format!("{err:#}")));
                    }
                }
            }

            // The prompt came up. Start the reader now, not when the channel was opened: until
            // this moment the screen was showing a clock, and an armed sensor asks for a finger.
            VerifierRequest::StartFingerprint => {
                let Some(open) = session.as_mut() else {
                    continue;
                };
                open.start_fingerprint().await;
            }

            VerifierRequest::Answer(answer) => {
                let Some(open) = session.as_ref() else {
                    warn!("gdm: an answer arrived with no conversation open");
                    continue;
                };
                if let Err(err) = open.answer(&answer).await {
                    warn!("gdm: error answering the query: {err:?}");
                }
            }

            VerifierRequest::Cancel => {
                if let Some(open) = session.take() {
                    open.close().await;
                }
            }
        }
    }
}

/// One open reauthentication channel.
struct Session {
    conn: zbus::Connection,
    username: String,
    reader: crate::dbus::fprintd::ReaderType,
    /// Whether `gdm-fingerprint` has been begun on this channel. gdm errors on a service that is
    /// already running, and the prompt page can be raised more than once per lock.
    fingerprint_started: bool,
    /// The signal pump, **held rather than detached** so it can be stopped. A detached pump owns a
    /// `MessageStream`, which owns the connection — so dropping the session would neither close
    /// the socket nor stop the forwarding, and the orphan would go on emitting into the
    /// compositor.
    ///
    /// Dropping this cancels it (`zbus::Task`'s `Drop`), which is the whole mechanism.
    pump: zbus::Task<()>,
}

impl Session {
    async fn open(
        system: &zbus::Connection,
        username: &str,
        reader: crate::dbus::fprintd::ReaderType,
        to_niri: calloop::channel::Sender<VerifierEvent>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let manager = DisplayManagerProxy::new(system)
            .await
            .context("connecting to gdm on the system bus")?;
        let address = manager
            .open_reauthentication_channel(username)
            .await
            .context("OpenReauthenticationChannel")?;

        // `.p2p()`: gdm's channel is not a message bus. Building it as one would hang waiting for a
        // `Hello` reply that never comes.
        let conn = zbus::conn::Builder::address(address.as_str())
            .context("parsing the reauthentication address")?
            .p2p()
            .build()
            .await
            .context("connecting to the reauthentication channel")?;

        // Take the stream before beginning, so the first `SecretInfoQuery` — which gdm sends
        // promptly — cannot land before anything is listening.
        let stream = zbus::MessageStream::from(&conn);

        let pump = conn.executor().spawn(
            pump_signals(stream, to_niri, conn.clone(), username.to_owned(), reader),
            "gdm-user-verifier-signals",
        );

        begin(&conn, PASSWORD_SERVICE, username)
            .await
            .context("BeginVerificationForUser")?;

        // The reader is **not** started here. It runs beside the password conversation
        // (`_maybeStartFingerprintVerification`, `util.js:714-719`) but only once the prompt is
        // actually up — see [`VerifierRequest::StartFingerprint`] and `UnlockDialog::show_prompt`.
        Ok(Self {
            conn,
            pump,
            username: username.to_owned(),
            reader,
            fingerprint_started: false,
        })
    }

    /// Start `gdm-fingerprint`, once.
    ///
    /// Idempotent because the page can be raised and dropped repeatedly, and gdm errors on a
    /// service that is already running. A failure is not a failure of the lock — the password
    /// conversation is already up, and the shield must not become unanswerable because a sensor
    /// would not start.
    async fn start_fingerprint(&mut self) {
        if !self.reader.is_present() || self.fingerprint_started {
            return;
        }
        self.fingerprint_started = true;
        if let Err(err) = begin(&self.conn, FINGERPRINT_SERVICE, &self.username).await {
            warn!("gdm: could not start fingerprint verification: {err:?}");
        }
    }

    async fn answer(&self, answer: &str) -> zbus::Result<()> {
        self.conn
            .call_method(
                None::<&str>,
                SESSION_PATH,
                Some(VERIFIER_IFACE),
                "AnswerQuery",
                &(PASSWORD_SERVICE, answer),
            )
            .await
            .map(|_| ())
    }

    /// Cancel the conversation and stop the pump, so gdm tears its PAM worker down and nothing from
    /// this channel can reach the compositor again.
    async fn close(self) {
        let cancelled = self
            .conn
            .call_method(
                None::<&str>,
                SESSION_PATH,
                Some(VERIFIER_IFACE),
                "Cancel",
                &(),
            )
            .await;
        if let Err(err) = cancelled {
            // Not fatal: dropping the connection ends the conversation anyway.
            debug!("gdm: error cancelling the conversation: {err:?}");
        }
        // Dropping the task cancels it, releasing the `MessageStream` and with it the last owner
        // of the connection — so the socket closes and gdm's worker goes away.
        drop(self.pump);
        drop(self.conn);
    }
}

async fn begin(conn: &zbus::Connection, service: &str, username: &str) -> zbus::Result<()> {
    conn.call_method(
        None::<&str>,
        SESSION_PATH,
        Some(VERIFIER_IFACE),
        "BeginVerificationForUser",
        &(service, username),
    )
    .await
    .map(|_| ())
}

/// Forward the verifier's signals, and drive the client-side retry.
async fn pump_signals(
    mut stream: zbus::MessageStream,
    to_niri: calloop::channel::Sender<VerifierEvent>,
    conn: zbus::Connection,
    username: String,
    reader: crate::dbus::fprintd::ReaderType,
) {
    let mut fingerprint = FingerprintState::default();
    // When the reader's conversation last started. `None` until it has been started at all, which
    // is why an unarmed reader can never look like one that stopped immediately.
    let mut fingerprint_began: Option<Instant> = None;

    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        if header.message_type() != zbus::message::Type::Signal
            || header.interface().map(|i| i.as_str()) != Some(VERIFIER_IFACE)
            || header.path().map(|p| p.as_str()) != Some(SESSION_PATH)
        {
            continue;
        }
        let Some(member) = header.member().map(|m| m.as_str().to_owned()) else {
            continue;
        };

        // Every signal but `Reset` leads with the service name, and which service it names decides
        // what the message *means*. `gdm-password` is the foreground service
        // (`serviceIsForeground`, `util.js:600-604`); `gdm-fingerprint` is a background one
        // whose messages are handled quite differently — see below.
        let service_of = |msg: &zbus::Message| {
            msg.body()
                .deserialize::<(String,)>()
                .ok()
                .map(|(s,)| s)
                .or_else(|| {
                    msg.body()
                        .deserialize::<(String, String)>()
                        .ok()
                        .map(|(s, _)| s)
                })
        };
        let service = service_of(&msg);
        let text = msg
            .body()
            .deserialize::<(String, String)>()
            .ok()
            .map(|(_, v)| v);

        // gdm announces each conversation starting (`_onConversationStarted`, `util.js:901`). That
        // is the only place the reader's clock can be started from: the *first* start is issued by
        // the session when the prompt comes up, not by this loop, so without this a reader that
        // stops immediately the very first time would look like one that had never run.
        if member == "ConversationStarted" && service.as_deref() == Some(FINGERPRINT_SERVICE) {
            fingerprint_began = Some(Instant::now());
        }

        // How long the reader's conversation lasted, which `route` cannot know: it sees one signal
        // at a time and has no clock.
        fingerprint.stopped_immediately =
            fingerprint_began.is_some_and(|began| began.elapsed() < FINGERPRINT_MIN_ALIVE);

        let event = match route(&member, service.as_deref(), text, reader, fingerprint) {
            Routed::Ignore => continue,
            Routed::Event(event) => Some(event),

            Routed::GiveUp { service, event } => {
                if service == FINGERPRINT_SERVICE {
                    fingerprint.unavailable = true;
                    debug!("gdm: not offering the fingerprint reader again for this lock");
                }
                event
            }

            // The conversation ended without accepting. gdm will not re-ask, so start another one
            // here — this is `_retry` (`util.js:866`). Without it, one wrong password leaves a lock
            // screen that can never be answered again.
            Routed::Restart { service, event } => {
                if service == FINGERPRINT_SERVICE {
                    fingerprint.immediate_stops = if fingerprint.stopped_immediately {
                        fingerprint.immediate_stops + 1
                    } else {
                        // A conversation that lasted long enough to be a person clears the count:
                        // the budget is for a service that will not run, not a user having a bad
                        // day with the sensor.
                        0
                    };
                }
                if let Err(err) = begin(&conn, service, &username).await {
                    warn!("gdm: could not restart {service} after a refusal: {err:?}");
                    // Only the password service dying is fatal to the lock: without it there is no
                    // way in at all. A reader that will not restart just stops being an option.
                    if service == PASSWORD_SERVICE {
                        let _ = to_niri.send(VerifierEvent::Lost);
                        break;
                    }
                    fingerprint.unavailable = true;
                    continue;
                }
                if service == FINGERPRINT_SERVICE {
                    fingerprint_began = Some(Instant::now());
                }
                event
            }
        };

        if let Some(event) = event {
            if to_niri.send(event).is_err() {
                break;
            }
        }
    }

    // Falling out means the peer socket closed — gdm went away mid-conversation. GNOME watches for
    // exactly this (`util.js:513-514` connects the connection's `closed`); without it the dialog
    // sits in "answered" forever behind a shield that still reports itself locked.
    warn!("gdm: the reauthentication channel closed");
    let _ = to_niri.send(VerifierEvent::Lost);
}

/// How long a fingerprint conversation must survive for its ending to be a *person* failing to
/// scan.
///
/// pam_fprintd lets the user retry several times before it gives up, so a `ConversationStopped`
/// normally arrives many seconds in. One that comes back straight away is the service declining —
/// the reader was cancelled, unplugged, or has no enrolled prints — and restarting it just spins,
/// re-arming the sensor as fast as it can refuse.
const FINGERPRINT_MIN_ALIVE: Duration = Duration::from_millis(500);

/// How many immediate stops before the reader stops being offered for this lock.
///
/// More than one because the first can be a race with the channel coming up; small because each one
/// is a round trip and a sensor blinking at somebody who is trying to type.
const FINGERPRINT_MAX_IMMEDIATE_STOPS: u32 = 3;

/// What the pump knows about the reader's conversation that a single signal cannot say.
#[derive(Debug, Clone, Copy, Default)]
struct FingerprintState {
    /// Consecutive stops that came back faster than a person could fail.
    immediate_stops: u32,
    /// It reported `ServiceUnavailable`, so it is not coming back (`_unavailableServices`,
    /// `util.js:888-890`, and the early return in `_onConversationStopped`, `:920-921`).
    unavailable: bool,
    /// Whether the last stop was an immediate one, filled in by the pump from its own clock.
    stopped_immediately: bool,
}

/// What a verifier signal means, decided without touching the connection.
///
/// Split out of the pump because this is where every policy decision in the port lives — which
/// service may put a question in the entry, whose messages are shown and whose are replaced, which
/// conversation to re-begin when one stops — and inside an async D-Bus loop none of it can be
/// tested. The pump is left with the two things that genuinely need the socket: re-beginning a
/// service, and sending the event on.
#[derive(Debug, PartialEq)]
enum Routed {
    /// Not ours, or nothing to say.
    Ignore,
    Event(VerifierEvent),
    /// This service's conversation ended: re-begin it, then emit `event` if there is one.
    Restart {
        service: &'static str,
        event: Option<VerifierEvent>,
    },
    /// This service is finished for the rest of this lock: do **not** re-begin it. Emit `event` if
    /// there is one.
    GiveUp {
        service: &'static str,
        event: Option<VerifierEvent>,
    },
}

fn route(
    member: &str,
    service: Option<&str>,
    text: Option<String>,
    reader: crate::dbus::fprintd::ReaderType,
    fingerprint: FingerprintState,
) -> Routed {
    // `Reset` is the one signal that does not name a service.
    if member == "Reset" {
        return Routed::Event(VerifierEvent::Reset);
    }

    let is_foreground = service == Some(PASSWORD_SERVICE);
    // `serviceIsFingerprint` requires a *detected reader*, not just the name (`util.js:616-619`) —
    // without one we never started the service, so a signal bearing its name is not ours.
    let is_fingerprint = reader.is_present() && service == Some(FINGERPRINT_SERVICE);
    if !is_foreground && !is_fingerprint {
        return Routed::Ignore;
    }

    match member {
        // **The reader's own `Info` text is thrown away** (`_onInfo`, `util.js:727-747`).
        // pam_fprintd narrates ("Place your finger on the fingerprint reader"), but it is not the
        // foreground service, so its narration would read as an instruction about the password
        // prompt. GNOME substitutes its own parenthetical aside at HINT priority, and which one
        // depends on the reader's shape.
        "Info" if is_fingerprint => match reader.hint() {
            Some(hint) => Routed::Event(VerifierEvent::ShowMessage {
                text: hint.to_owned(),
                kind: MessageKind::Hint,
            }),
            None => Routed::Ignore,
        },
        "Info" => match text {
            Some(text) => Routed::Event(VerifierEvent::ShowMessage {
                text,
                kind: MessageKind::Info,
            }),
            None => Routed::Ignore,
        },

        // A `Problem` **is** shown, from either service (`_onProblem`, `util.js:749-751`): a finger
        // that did not read is something the user can do something about. GNOME also counts these
        // towards `allowed-failures`, which on the unlock screen is moot — `_canRetry` is
        // unconditionally true when `_reauthOnly` (`:839-842`), and that is always our case, so the
        // fingerprint conversation is never failed from this side.
        "Problem" => match text {
            Some(text) => Routed::Event(VerifierEvent::ShowMessage {
                text,
                kind: MessageKind::Error,
            }),
            None => Routed::Ignore,
        },

        // `ServiceUnavailable` is a service saying it cannot run *at all*, which is different from
        // one refusing an attempt. GNOME remembers it and never retries that service
        // (`_onServiceUnavailable`, `util.js:888-890`; the early return in
        // `_onConversationStopped`, `:920-921`) — the stop that follows would otherwise restart it
        // straight into the same refusal.
        "ServiceUnavailable" => Routed::GiveUp {
            service: if is_fingerprint {
                FINGERPRINT_SERVICE
            } else {
                PASSWORD_SERVICE
            },
            event: text.map(|text| VerifierEvent::ShowMessage {
                text,
                kind: MessageKind::Error,
            }),
        },

        // Questions only ever come from the foreground service (`_onInfoQuery`, `:790-795`).
        // pam_fprintd asks none, but a background service that did must not be allowed to put a
        // prompt in the entry: the answer would be typed into the wrong conversation, and on this
        // screen that answer is a password.
        "InfoQuery" | "SecretInfoQuery" if !is_foreground => Routed::Ignore,
        "InfoQuery" => match text {
            Some(question) => Routed::Event(VerifierEvent::AskQuestion {
                question,
                secret: false,
            }),
            None => Routed::Ignore,
        },
        "SecretInfoQuery" => match text {
            Some(question) => Routed::Event(VerifierEvent::AskQuestion {
                question,
                secret: true,
            }),
            None => Routed::Ignore,
        },

        // Either service may be the one that succeeds — that is the whole point of running them
        // together.
        "VerificationComplete" => Routed::Event(VerifierEvent::Complete),

        // Restart **only the service that stopped**. The two conversations are independent, and
        // re-beginning the password one because the reader gave up would throw away a half-typed
        // answer. GNOME arrives at the same place from the other direction: `_retry` restarts the
        // default service, and `_maybeStartFingerprintVerification` starts the reader only if it is
        // not already running (`util.js:866`, `:714-719`).
        //
        // A `Failed` goes with the password stopping and not with the reader's: it clears the entry
        // and drops the question, which is right when the answer was refused and wrong when the
        // sensor timed out beside a user who is still typing.
        //
        // And the reader is given up on rather than restarted once it stops answering: a
        // conversation that ends the moment it starts is not somebody failing to scan, and
        // re-beginning it is a spin that re-arms the sensor as fast as it can decline. The password
        // conversation is deliberately not treated this way — it is the only way back in, so it is
        // retried whatever it does.
        "ConversationStopped" if is_fingerprint => {
            if fingerprint.unavailable
                || (fingerprint.stopped_immediately
                    && fingerprint.immediate_stops + 1 >= FINGERPRINT_MAX_IMMEDIATE_STOPS)
            {
                Routed::GiveUp {
                    service: FINGERPRINT_SERVICE,
                    event: None,
                }
            } else {
                Routed::Restart {
                    service: FINGERPRINT_SERVICE,
                    event: None,
                }
            }
        }
        "ConversationStopped" => Routed::Restart {
            service: PASSWORD_SERVICE,
            event: Some(VerifierEvent::Failed),
        },

        _ => Routed::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::fprintd::ReaderType;

    /// A reader that is running normally: nothing given up on, nothing stopping early.
    fn healthy() -> FingerprintState {
        FingerprintState::default()
    }

    fn msg(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    /// The reader's narration is thrown away and replaced by GNOME's own aside.
    ///
    /// pam_fprintd talks: "Place your finger on the fingerprint reader", "Swipe your finger again".
    /// Passing that straight through puts an imperative sentence directly under a prompt that says
    /// "Password:", where it reads as an instruction about the password — and it is written for
    /// whatever hardware pam_fprintd found, not for what we told the user to expect. GNOME
    /// substitutes a parenthetical aside chosen from the *scan type* (`_onInfo`,
    /// `util.js:727-747`).
    #[test]
    fn the_readers_own_narration_never_reaches_the_screen() {
        let narration = "Place your finger on the fingerprint reader";

        let press = route(
            "Info",
            Some(FINGERPRINT_SERVICE),
            msg(narration),
            ReaderType::Press,
            healthy(),
        );
        assert_eq!(
            press,
            Routed::Event(VerifierEvent::ShowMessage {
                text: "(or place finger on reader)".to_owned(),
                kind: MessageKind::Hint,
            })
        );

        // A swipe reader gets the other one: telling somebody to press a swipe sensor is an
        // instruction that cannot be followed, and they have no way to tell it is us that is wrong.
        let swipe = route(
            "Info",
            Some(FINGERPRINT_SERVICE),
            msg(narration),
            ReaderType::Swipe,
            healthy(),
        );
        assert_eq!(
            swipe,
            Routed::Event(VerifierEvent::ShowMessage {
                text: "(or swipe finger across reader)".to_owned(),
                kind: MessageKind::Hint,
            })
        );

        // The password service's own `Info` is passed through untouched — the substitution is for
        // the *background* service only.
        assert_eq!(
            route(
                "Info",
                Some(PASSWORD_SERVICE),
                msg("hello"),
                ReaderType::Press,
                healthy()
            ),
            Routed::Event(VerifierEvent::ShowMessage {
                text: "hello".to_owned(),
                kind: MessageKind::Info,
            })
        );
    }

    /// Only the foreground service may put a question in the entry.
    ///
    /// The entry is where a password is typed. A background service that asked a question and got
    /// it would be handed the user's password for a conversation they did not think they were
    /// answering — so the gate is on *which service asked*, not on whether a question looks
    /// plausible (`_onInfoQuery`/`_onSecretInfoQuery`, `util.js:790-800`).
    #[test]
    fn only_the_foreground_service_can_ask_for_the_password() {
        for member in ["InfoQuery", "SecretInfoQuery"] {
            assert_eq!(
                route(
                    member,
                    Some(FINGERPRINT_SERVICE),
                    msg("?"),
                    ReaderType::Press,
                    healthy()
                ),
                Routed::Ignore,
                "{member} from the reader must not reach the entry"
            );
            assert!(matches!(
                route(
                    member,
                    Some(PASSWORD_SERVICE),
                    msg("?"),
                    ReaderType::Press,
                    healthy()
                ),
                Routed::Event(VerifierEvent::AskQuestion { .. })
            ));
        }

        // And the secret flag is not guessed from the text: it is which signal arrived.
        assert_eq!(
            route(
                "SecretInfoQuery",
                Some(PASSWORD_SERVICE),
                msg("Password:"),
                ReaderType::None,
                healthy()
            ),
            Routed::Event(VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            })
        );
    }

    /// With no reader detected, everything bearing the fingerprint service's name is ignored.
    ///
    /// `serviceIsFingerprint` is a hardware test, not a name test (`util.js:616-619`). We only
    /// start the service when a reader was found, so a signal naming it without one is a
    /// service we never asked for — and treating it as ours would let it show messages and,
    /// worse, complete a verification.
    #[test]
    fn a_fingerprint_signal_with_no_reader_is_not_ours() {
        for member in [
            "Info",
            "Problem",
            "VerificationComplete",
            "ConversationStopped",
        ] {
            assert_eq!(
                route(
                    member,
                    Some(FINGERPRINT_SERVICE),
                    msg("x"),
                    ReaderType::None,
                    healthy()
                ),
                Routed::Ignore,
                "{member} was acted on with no reader detected"
            );
        }
        // A service nobody has heard of is ignored whatever the reader says.
        assert_eq!(
            route(
                "VerificationComplete",
                Some("gdm-smartcard"),
                None,
                ReaderType::Press,
                healthy()
            ),
            Routed::Ignore
        );
    }

    /// A conversation that stops restarts **itself**, and only the password's stop is a failure.
    ///
    /// The two run in parallel, so "one of them ended" is not "authentication failed". Restarting
    /// the password service because the sensor timed out would clear an entry the user is part-way
    /// through typing; emitting `Failed` for it would do the same and put an error on screen for
    /// something the user was not doing.
    #[test]
    fn each_conversation_restarts_only_itself() {
        assert_eq!(
            route(
                "ConversationStopped",
                Some(PASSWORD_SERVICE),
                None,
                ReaderType::Press,
                healthy()
            ),
            Routed::Restart {
                service: PASSWORD_SERVICE,
                event: Some(VerifierEvent::Failed),
            }
        );
        assert_eq!(
            route(
                "ConversationStopped",
                Some(FINGERPRINT_SERVICE),
                None,
                ReaderType::Press,
                healthy()
            ),
            Routed::Restart {
                service: FINGERPRINT_SERVICE,
                event: None,
            },
            "the reader giving up must not clear the entry or report a failure"
        );
    }

    /// Either service may be the one that succeeds, and a `Problem` from either is shown.
    #[test]
    fn both_services_can_finish_and_both_can_complain() {
        for service in [PASSWORD_SERVICE, FINGERPRINT_SERVICE] {
            assert_eq!(
                route(
                    "VerificationComplete",
                    Some(service),
                    None,
                    ReaderType::Press,
                    healthy()
                ),
                Routed::Event(VerifierEvent::Complete),
                "{service} must be able to unlock"
            );
            assert_eq!(
                route(
                    "Problem",
                    Some(service),
                    msg("no good"),
                    ReaderType::Press,
                    healthy()
                ),
                Routed::Event(VerifierEvent::ShowMessage {
                    text: "no good".to_owned(),
                    kind: MessageKind::Error,
                }),
                "{service}'s Problem must be shown"
            );
        }
    }

    /// A reader that will not run is given up on, instead of being restarted forever.
    ///
    /// pam_fprintd lets a person retry several times before the conversation ends, so a stop
    /// normally arrives seconds in. One that comes back immediately means the service is declining
    /// — cancelled, unplugged, nothing enrolled — and restarting it re-arms the sensor as fast as
    /// it can refuse. Live on a real reader that was a permanent loop, with the sensor asking
    /// for a finger over and over at somebody trying to type their password.
    #[test]
    fn a_reader_that_keeps_declining_is_given_up_on() {
        let stop = |fp| {
            route(
                "ConversationStopped",
                Some(FINGERPRINT_SERVICE),
                None,
                ReaderType::Press,
                fp,
            )
        };
        let restart = Routed::Restart {
            service: FINGERPRINT_SERVICE,
            event: None,
        };
        let give_up = Routed::GiveUp {
            service: FINGERPRINT_SERVICE,
            event: None,
        };

        // A stop after a conversation that lasted: somebody failed to scan. Keep offering it.
        let mut fp = FingerprintState::default();
        assert_eq!(
            stop(fp),
            restart,
            "a real failed scan must not disable the reader"
        );

        // Stops that come back instantly: counted, and given up on at the budget.
        fp.stopped_immediately = true;
        for n in 0..FINGERPRINT_MAX_IMMEDIATE_STOPS - 1 {
            fp.immediate_stops = n;
            assert_eq!(stop(fp), restart, "gave up after only {n} immediate stops");
        }
        fp.immediate_stops = FINGERPRINT_MAX_IMMEDIATE_STOPS - 1;
        assert_eq!(stop(fp), give_up, "the reader was restarted forever");

        // A conversation that lasted resets the count — the budget is for a service that will not
        // run, not for a person having a bad day with the sensor.
        fp.stopped_immediately = false;
        assert_eq!(stop(fp), restart);
    }

    /// `ServiceUnavailable` ends that service for the lock, and its message is still shown.
    ///
    /// It is a service saying it cannot run at all, as opposed to refusing one attempt — GNOME
    /// remembers it and the stop that follows is not retried (`util.js:888-890`, `:920-921`).
    /// Restarting into the same refusal is the loop this exists to prevent.
    #[test]
    fn an_unavailable_service_is_not_started_again() {
        assert_eq!(
            route(
                "ServiceUnavailable",
                Some(FINGERPRINT_SERVICE),
                msg("no such device"),
                ReaderType::Press,
                healthy(),
            ),
            Routed::GiveUp {
                service: FINGERPRINT_SERVICE,
                event: Some(VerifierEvent::ShowMessage {
                    text: "no such device".to_owned(),
                    kind: MessageKind::Error,
                }),
            }
        );

        // ...and once marked, even a slow stop does not bring it back.
        let fp = FingerprintState {
            unavailable: true,
            ..FingerprintState::default()
        };
        assert_eq!(
            route(
                "ConversationStopped",
                Some(FINGERPRINT_SERVICE),
                None,
                ReaderType::Press,
                fp,
            ),
            Routed::GiveUp {
                service: FINGERPRINT_SERVICE,
                event: None,
            }
        );
    }

    /// The password conversation is retried whatever it does — it is the only way back in.
    ///
    /// The give-up rules are deliberately for the reader alone. A password service that stopped
    /// immediately and was given up on would leave a lock screen with nothing left to answer.
    #[test]
    fn the_password_conversation_is_never_given_up_on() {
        let fp = FingerprintState {
            immediate_stops: 99,
            stopped_immediately: true,
            unavailable: true,
        };
        assert_eq!(
            route(
                "ConversationStopped",
                Some(PASSWORD_SERVICE),
                None,
                ReaderType::Press,
                fp,
            ),
            Routed::Restart {
                service: PASSWORD_SERVICE,
                event: Some(VerifierEvent::Failed),
            }
        );
    }
}
