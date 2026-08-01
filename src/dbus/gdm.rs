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

use futures_util::StreamExt;
use zbus::proxy;

/// The PAM service gdm runs for password authentication (`js/gdm/util.js:27`).
const PASSWORD_SERVICE: &str = "gdm-password";
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
    /// Open a channel for `username` and start `gdm-password`. Answered by
    /// [`VerifierEvent::Ready`] or [`VerifierEvent::Unavailable`] carrying the same `epoch`.
    Begin { username: String, epoch: Epoch },
    /// Answer the outstanding query. **Carries a password** — see the `Debug` impl below.
    Answer(String),
    /// Tear the conversation down: the shield was raised, or the user cancelled.
    Cancel,
}

/// Hand-written so a stray `{:?}` cannot put a password in the journal. `derive(Debug)` would print
/// it, and every type embedding this one would inherit that.
impl std::fmt::Debug for VerifierRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin { username, epoch } => f
                .debug_struct("Begin")
                .field("username", username)
                .field("epoch", epoch)
                .finish(),
            Self::Answer(answer) => write!(f, "Answer(<redacted, {} chars>)", answer.len()),
            Self::Cancel => write!(f, "Cancel"),
        }
    }
}

/// A message shown under the entry, and how it reads (`js/gdm/util.js:728-782`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
}

/// What the verifier tells the compositor.
#[derive(Debug, Clone)]
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
            VerifierRequest::Begin { username, epoch } => {
                // Close the previous channel *before* opening another: gdm keeps a PAM worker per
                // channel, and an orphan would go on emitting into the compositor — including a
                // `Complete` that would raise a shield it knows nothing about.
                if let Some(old) = session.take() {
                    old.close().await;
                }
                match Session::open(&system, &username, to_niri.clone()).await {
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
            pump_signals(stream, to_niri, conn.clone(), username.to_owned()),
            "gdm-user-verifier-signals",
        );

        begin(&conn, username)
            .await
            .context("BeginVerificationForUser")?;

        Ok(Self { conn, pump })
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

async fn begin(conn: &zbus::Connection, username: &str) -> zbus::Result<()> {
    conn.call_method(
        None::<&str>,
        SESSION_PATH,
        Some(VERIFIER_IFACE),
        "BeginVerificationForUser",
        &(PASSWORD_SERVICE, username),
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
) {
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

        // Every signal but `Reset` leads with the service name. Only `gdm-password` is started
        // here, so anything else is a service we did not ask for (`serviceIsForeground`,
        // `util.js:600-604`).
        let is_ours = |msg: &zbus::Message| {
            msg.body()
                .deserialize::<(String,)>()
                .map(|(s,)| s == PASSWORD_SERVICE)
                .unwrap_or(false)
        };
        let text_arg = |msg: &zbus::Message| {
            msg.body()
                .deserialize::<(String, String)>()
                .ok()
                .filter(|(s, _)| s == PASSWORD_SERVICE)
                .map(|(_, v)| v)
        };

        let event = match member.as_str() {
            "Reset" => Some(VerifierEvent::Reset),
            "Info" => text_arg(&msg).map(|text| VerifierEvent::ShowMessage {
                text,
                kind: MessageKind::Info,
            }),
            "Problem" | "ServiceUnavailable" => {
                text_arg(&msg).map(|text| VerifierEvent::ShowMessage {
                    text,
                    kind: MessageKind::Error,
                })
            }
            "InfoQuery" => text_arg(&msg).map(|question| VerifierEvent::AskQuestion {
                question,
                secret: false,
            }),
            "SecretInfoQuery" => text_arg(&msg).map(|question| VerifierEvent::AskQuestion {
                question,
                secret: true,
            }),
            "VerificationComplete" => is_ours(&msg).then_some(VerifierEvent::Complete),

            // The conversation ended without accepting. gdm will not re-ask, so start another one
            // here — this is `_retry` (`util.js:866`). Without it, one wrong password leaves a lock
            // screen that can never be answered again.
            "ConversationStopped" => {
                if !is_ours(&msg) {
                    continue;
                }
                if let Err(err) = begin(&conn, &username).await {
                    warn!("gdm: could not restart verification after a refusal: {err:?}");
                    let _ = to_niri.send(VerifierEvent::Lost);
                    break;
                }
                Some(VerifierEvent::Failed)
            }

            _ => None,
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
