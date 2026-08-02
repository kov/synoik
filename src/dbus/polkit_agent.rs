//! The session's polkit authentication agent.
//!
//! GNOME registers one per session (`js/ui/components/polkitAgent.js`, backed by
//! `src/shell-polkit-authentication-agent.c`) and puts up the "Authentication Required" dialog.
//! A session with no agent registered is not merely un-prompted: **every polkit action that
//! needs authentication fails outright**, because polkitd has nothing to ask with. Mounting an
//! encrypted volume, enrolling a fingerprint, installing updates and every Settings panel with a
//! lock button all fail the same silent way.
//!
//! # The two halves
//!
//! **The bus half.** We export `org.freedesktop.PolicyKit1.AuthenticationAgent` and hand polkitd
//! our object path with `RegisterAuthenticationAgent`. polkitd then calls `BeginAuthentication`
//! on us, and *the call does not return until the user is done* — its reply is the answer.
//!
//! **The PAM half.** GNOME gets this from `libpolkit-agent-1`'s `PolkitAgentSession`, which does
//! nothing more exotic than spawn the setuid `polkit-agent-helper-1` and speak a line protocol to
//! it ([`HelperMessage`]). We spawn it ourselves rather than link the library: the protocol is six
//! messages, and linking it would mean subclassing `PolkitAgentListener` from Rust and marrying a
//! `GMainContext` to the compositor's calloop.
//!
//! The trust boundary is unchanged either way — the helper is setuid root and does the PAM
//! conversation; we only carry text to and from it, and it is polkitd, not us, that decides what
//! the authentication was worth.

use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};

use futures_util::StreamExt as _;
use zbus::zvariant::{OwnedValue, Value};

/// polkit's default agent object path (`polkitagentlistener.c:402`). Agents may pick their own;
/// there is no reason to.
const AGENT_PATH: &str = "/org/freedesktop/PolicyKit1/AuthenticationAgent";
const AUTHORITY_NAME: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";

/// Where the setuid helper lives. polkit compiles its prefix in, so this is per-distro rather than
/// standardised; probe instead of guessing, and say so loudly when none of them is there, because
/// the failure is otherwise a dialog that can never succeed.
const HELPER_PATHS: &[&str] = &[
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-1/polkit-agent-helper-1",
    "/usr/lib/x86_64-linux-gnu/polkit-1/polkit-agent-helper-1",
    "/usr/lib/aarch64-linux-gnu/polkit-1/polkit-agent-helper-1",
    "/usr/local/lib/polkit-1/polkit-agent-helper-1",
];

/// Distinguishes one PAM conversation from the next.
///
/// Killing the helper makes its stdout hit EOF, and the reader thread reports that as a
/// [`HelperMessage::Completed`] with `false` — indistinguishable, at the receiving end, from PAM
/// actually refusing the password. The dialog answers a refusal by starting *another* conversation
/// (`polkitAgent.js:272`), so without this a cancel would be followed by a fresh helper nobody
/// asked for.
type Epoch = u64;

/// What the agent tells the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolkitToNiri {
    /// polkitd wants an authentication. Raise the dialog.
    Begin(Box<BeginRequest>),
    /// polkitd withdrew the request it had asked for — the action was answered another way, or the
    /// caller went away. Take the dialog down without answering.
    Cancel,
    /// The helper asked the user something. `echo_on` decides whether the entry shows what is
    /// typed: get it backwards and a password is drawn on the screen.
    Request {
        prompt: String,
        echo_on: bool,
    },
    /// PAM had something to say (`PAM_ERROR_MSG` / `PAM_TEXT_INFO`).
    ShowError(String),
    ShowInfo(String),
    /// The conversation ended. `true` means the helper told polkitd the user authenticated; the
    /// dialog is done. `false` means it did not, and GNOME's dialog immediately starts another
    /// (`polkitAgent.js:258-272`) — the user gets to try again.
    Completed(bool),
}

/// Everything the dialog needs to put itself up, resolved off the compositor thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginRequest {
    pub action_id: String,
    /// polkit's own description of the action — the dialog's body text.
    pub message: String,
    /// Who we will authenticate as — see [`choose_user`].
    pub user_name: String,
    /// Whether the account we will authenticate as has no password at all.
    ///
    /// This is not cosmetic. For such an account, *starting* the PAM conversation is what performs
    /// the authentication — so the dialog must not start one until the user has confirmed, or the
    /// action is authorised by a prompt nobody ever saw. GNOME says exactly this
    /// (`polkitAgent.js:373-376`) and answers it with a second mode; resolved here, off the
    /// compositor thread, because it takes a D-Bus round trip.
    pub passwordless: bool,
    /// That account's picture, if AccountsService has one on disk. Usually `None`, because the
    /// account is usually `root`; the dialog then draws the themed default.
    pub avatar: Option<std::path::PathBuf>,
}

/// What the compositor asks of the agent.
#[derive(Clone)]
pub enum PolkitRequest {
    /// Start a PAM conversation as `user_name`. Sent when the dialog opens, and again after every
    /// refusal.
    Initiate { user_name: String },
    /// Answer the outstanding [`PolkitToNiri::Request`]. **Carries a password** — see the `Debug`
    /// impl below.
    Respond(String),
    /// The dialog is finished. `dismissed` is polkitd's answer: `false` completes the call
    /// normally, `true` fails it with `Cancelled`, which is what tells the requesting program the
    /// user said no rather than that authentication was refused.
    Done { dismissed: bool },
}

/// Hand-written so a stray `{:?}` cannot put a password in the journal. `derive(Debug)` would
/// print it, and every type embedding this one would inherit that.
impl std::fmt::Debug for PolkitRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initiate { user_name } => f
                .debug_struct("Initiate")
                .field("user_name", user_name)
                .finish(),
            Self::Respond(response) => write!(f, "Respond(<redacted, {} chars>)", response.len()),
            Self::Done { dismissed } => f
                .debug_struct("Done")
                .field("dismissed", dismissed)
                .finish(),
        }
    }
}

/// One line of `polkit-agent-helper-1`'s output (`polkitagentsession.c:468-506`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperMessage {
    Prompt {
        text: String,
        echo_on: bool,
    },
    Error(String),
    Info(String),
    /// `SUCCESS` or `FAILURE`. Also synthesised as `false` when the helper's stdout closes without
    /// either, which is what a kill, a crash or a spawn failure all look like from here — polkit
    /// treats those identically (`:451-456`, `:514-515`).
    Completed(bool),
}

/// Parse one line of helper output, or `None` if it is not a line we know.
///
/// polkit treats an unrecognised line as a failed conversation rather than noise
/// (`polkitagentsession.c:504-508`), and so do we: the helper is a trusted process speaking a
/// fixed protocol, so a line we cannot read means something is wrong enough to stop.
///
/// The payload is `g_strescape`d by the helper (`polkitagenthelper-pam.c:55`), which escapes every
/// byte outside printable ASCII — so a localised PAM prompt arrives as octal escapes of its
/// **UTF-8 bytes**, not of its characters. Unescaping per-character would mangle every non-English
/// prompt, which is why [`unescape`] works in bytes.
pub fn parse_helper_line(line: &str) -> Option<HelperMessage> {
    // Order and prefixes are polkit's. The four payload messages carry a trailing space in the
    // prefix; the two terminal ones are matched as prefixes with no payload at all.
    let with = |prefix: &str| line.strip_prefix(prefix);
    if let Some(rest) = with("PAM_PROMPT_ECHO_OFF ") {
        Some(HelperMessage::Prompt {
            text: unescape(rest),
            echo_on: false,
        })
    } else if let Some(rest) = with("PAM_PROMPT_ECHO_ON ") {
        Some(HelperMessage::Prompt {
            text: unescape(rest),
            echo_on: true,
        })
    } else if let Some(rest) = with("PAM_ERROR_MSG ") {
        Some(HelperMessage::Error(unescape(rest)))
    } else if let Some(rest) = with("PAM_TEXT_INFO ") {
        Some(HelperMessage::Info(unescape(rest)))
    } else if line.starts_with("SUCCESS") {
        Some(HelperMessage::Completed(true))
    } else if line.starts_with("FAILURE") {
        Some(HelperMessage::Completed(false))
    } else {
        None
    }
}

/// Undo `g_strescape`, which is what the helper encodes its payloads with.
///
/// glib's inverse is `g_strcompress`: backslash-octal for up to three digits, the usual C letter
/// escapes, and anything else after a backslash passes through as itself (that last case is how
/// `\\` and `\"` are handled, so they need no arm of their own).
///
/// Bytes in, bytes out, decoded once at the end — see [`parse_helper_line`] for why.
fn unescape(escaped: &str) -> String {
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = bytes.get(i) else {
            // A trailing backslash: glib warns and stops. Nothing left to emit either way.
            break;
        };
        match c {
            b'0'..=b'7' => {
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 3 {
                    match bytes.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(d - b'0');
                            i += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
                continue;
            }
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            // Also covers `\\` and `\"`.
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Start the agent. Returns the system-bus connection to keep alive and the sender the compositor
/// drives the dialog's side of the conversation with.
pub fn start(
    to_niri: calloop::channel::Sender<PolkitToNiri>,
) -> anyhow::Result<(
    zbus::blocking::Connection,
    async_channel::Sender<PolkitRequest>,
)> {
    let (to_agent, from_niri) = async_channel::unbounded();
    let (calls_tx, calls_rx) = async_channel::unbounded();

    let conn = zbus::blocking::connection::Builder::system()?
        .serve_at(AGENT_PATH, AuthenticationAgent { calls: calls_tx })?
        .build()?;

    register(&conn)?;

    let (helper_tx, helper_rx) = async_channel::unbounded();
    conn.inner()
        .executor()
        .spawn(
            run(calls_rx, from_niri, helper_rx, helper_tx, to_niri),
            "polkit-agent",
        )
        .detach();

    Ok((conn, to_agent))
}

/// The subject we register for: our logind session, as `(sa{sv})`.
///
/// polkitd refuses any session but the caller's own ("Passed session and the session the caller is
/// in differs", `polkitbackendinteractiveauthority.c:2521-2527`), and it derives *our* session the
/// same way [`crate::dbus::freedesktop_login1::resolve_session_path`] had to: `sd_pid_get_session`
/// first, then the user's graphical session, because a shell started as a user service is outside
/// the session scope and the pid lookup fails
/// (`polkitbackendsessionmonitor-systemd.c:378-390`). Reading the id off the session object we
/// already resolved gets the same answer without a second guess.
fn subject(
    conn: &zbus::blocking::Connection,
) -> anyhow::Result<(String, Vec<(String, OwnedValue)>)> {
    use anyhow::Context as _;

    let path = crate::dbus::freedesktop_login1::session_path()
        .context("we have no logind session to register an agent for")?;
    let session = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )?;
    let id: String = session
        .get_property("Id")
        .context("reading the session Id")?;

    let details = vec![("session-id".to_owned(), Value::from(id).try_into()?)];
    Ok(("unix-session".to_owned(), details))
}

fn register(conn: &zbus::blocking::Connection) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let subject = subject(conn)?;
    // polkit's own choice of locale, and its own fallback (`polkitagentlistener.c:146-148`). It is
    // what polkitd localises the action's description with, so an empty string here would give the
    // dialog untranslated body text.
    let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_owned());

    let authority =
        zbus::blocking::Proxy::new(conn, AUTHORITY_NAME, AUTHORITY_PATH, AUTHORITY_IFACE)?;
    authority
        .call_method(
            "RegisterAuthenticationAgent",
            &(subject, locale, AGENT_PATH),
        )
        .context("RegisterAuthenticationAgent")?;

    debug!("registered as the session's polkit authentication agent");
    Ok(())
}

/// The interface polkitd calls. Only polkitd can: the shipped bus policy denies the interface to
/// everyone else (`/usr/share/dbus-1/system.d/org.freedesktop.PolicyKit1.conf`), which is why
/// upstream does no caller check here either (`polkitagentlistener.c:287-289`).
struct AuthenticationAgent {
    calls: async_channel::Sender<AgentCall>,
}

/// What polkitd asked for, on its way to [`Agent`].
enum AgentCall {
    Begin(Box<Begin>),
    Cancel { cookie: String },
}

/// A `BeginAuthentication` call in flight: what was asked, and the reply channel that finishes it.
struct Begin {
    request: BeginRequest,
    cookie: String,
    done: async_channel::Sender<bool>,
}

/// polkit's own error domain (`polkiterror.c:39-42`). `Cancelled` is the one that matters: it is
/// how the requesting program learns the user dismissed the dialog rather than failed the
/// password, and `fdo::Error` has no name that means it.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
enum PolkitError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl AuthenticationAgent {
    /// Ask the user to authenticate. **Does not return until they are done** — polkitd waits on
    /// this reply, and returning early would be indistinguishable from a dismissal.
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        _icon_name: String,
        _details: std::collections::HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, std::collections::HashMap<String, OwnedValue>)>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<(), PolkitError> {
        let names = user_names(&identities);
        if names.len() > 1 {
            // Upstream's message, and upstream's behaviour: one identity is considered, and the
            // user is not offered a chooser (`polkitAgent.js:52-54`).
            debug!(
                "polkit: {} identities can authenticate {action_id}; considering one",
                names.len()
            );
        }
        let Some(user_name) = choose_user(&names) else {
            // Nobody we can authenticate as. Failing is honest; a dialog with no user would be a
            // prompt that cannot succeed.
            warn!("polkit: no usable identities for {action_id}");
            return Err(PolkitError::Failed("no usable identities".to_owned()));
        };

        // Resolved here rather than in the dialog because it is a D-Bus round trip, and because
        // getting it wrong the *other* way authorises the action with no prompt at all. An
        // account AccountsService cannot speak for reads as having a password.
        let account = crate::dbus::accounts_service::account_for(conn, &user_name).await;
        let passwordless = account
            .as_ref()
            .is_some_and(|account| account.password_mode.is_none());
        let avatar = account
            .and_then(|account| account.icon_file)
            .map(|icon| icon.path);

        let (done_tx, done_rx) = async_channel::bounded(1);
        let begin = Begin {
            request: BeginRequest {
                action_id,
                message,
                user_name,
                passwordless,
                avatar,
            },
            cookie,
            done: done_tx,
        };
        if self
            .calls
            .send(AgentCall::Begin(Box::new(begin)))
            .await
            .is_err()
        {
            return Err(PolkitError::Failed("the agent is gone".to_owned()));
        }

        match done_rx.recv().await {
            Ok(false) => Ok(()),
            // The message is upstream's (`shell-polkit-authentication-agent.c:342`).
            Ok(true) => Err(PolkitError::Cancelled(
                "Authentication dialog was dismissed by the user".to_owned(),
            )),
            Err(_) => Err(PolkitError::Failed("the agent is gone".to_owned())),
        }
    }

    /// polkitd withdrawing a request it made. The reply to `BeginAuthentication` is still owed;
    /// [`Agent::cancel`] sends it.
    async fn cancel_authentication(&self, cookie: String) {
        let _ = self.calls.send(AgentCall::Cancel { cookie }).await;
    }
}

/// Pick the account to authenticate as, from the identities polkitd offered
/// (`polkitAgent.js:57-61`).
///
/// Ourselves if we are in the list, then `root`, then whoever is first. The order matters: an
/// `auth_admin` action lists every administrator, and asking for *our own* password when we are one
/// of them is the difference between a prompt the user can answer and one they cannot.
fn choose_user(names: &[String]) -> Option<String> {
    let ours = crate::unlock_dialog::session_user().name;
    for candidate in [ours.as_str(), "root"] {
        if names.iter().any(|name| name == candidate) {
            return Some(candidate.to_owned());
        }
    }
    names.first().cloned()
}

/// Turn polkit's `a(sa{sv})` identity list into usernames.
///
/// Only `unix-user` identities mean anything to us; upstream warns and skips the rest
/// (`shell-polkit-authentication-agent.c:216-224`). A uid with no passwd entry is skipped the same
/// way — offering a name we cannot resolve would just fail later, inside the helper.
fn user_names(
    identities: &[(String, std::collections::HashMap<String, OwnedValue>)],
) -> Vec<String> {
    identities
        .iter()
        .filter(|(kind, _)| kind == "unix-user")
        .filter_map(|(_, details)| details.get("uid"))
        .filter_map(|uid| u32::try_from(uid).ok())
        .filter_map(|uid| crate::utils::passwd_entry(uid).map(|entry| entry.name))
        .collect()
}

/// The agent's own loop: one dialog at a time, and one helper under it.
async fn run(
    calls: async_channel::Receiver<AgentCall>,
    requests: async_channel::Receiver<PolkitRequest>,
    helper_events: async_channel::Receiver<(Epoch, HelperMessage)>,
    helper_tx: async_channel::Sender<(Epoch, HelperMessage)>,
    to_niri: calloop::channel::Sender<PolkitToNiri>,
) {
    let mut agent = Agent {
        to_niri,
        helper_tx,
        scheduled: VecDeque::new(),
        current: None,
        session: None,
        epoch: 0,
    };

    enum Event {
        Call(AgentCall),
        Request(PolkitRequest),
        Helper(Epoch, HelperMessage),
    }

    // Pinned because `async_channel::Receiver` is `!Unpin` — it parks an event listener in place.
    let mut events = std::pin::pin!(futures_util::stream::select(
        futures_util::stream::select(calls.map(Event::Call), requests.map(Event::Request)),
        helper_events.map(|(epoch, msg)| Event::Helper(epoch, msg)),
    ));

    while let Some(event) = events.next().await {
        match event {
            Event::Call(AgentCall::Begin(begin)) => agent.schedule(*begin),
            Event::Call(AgentCall::Cancel { cookie }) => agent.cancel(&cookie),
            Event::Request(request) => agent.on_request(request),
            Event::Helper(epoch, msg) => agent.on_helper(epoch, msg),
        }
    }
}

struct Agent {
    to_niri: calloop::channel::Sender<PolkitToNiri>,
    helper_tx: async_channel::Sender<(Epoch, HelperMessage)>,
    /// Requests waiting their turn. polkitd may ask for a second authentication while the first
    /// dialog is up, and upstream queues rather than stacking dialogs
    /// (`shell-polkit-authentication-agent.c:407-408`, `:356-370`).
    scheduled: VecDeque<Begin>,
    current: Option<Begin>,
    session: Option<HelperSession>,
    epoch: Epoch,
}

impl Agent {
    fn schedule(&mut self, begin: Begin) {
        self.scheduled.push_back(begin);
        self.process_next();
    }

    fn process_next(&mut self) {
        if self.current.is_some() {
            return;
        }
        let Some(begin) = self.scheduled.pop_front() else {
            return;
        };
        let _ = self
            .to_niri
            .send(PolkitToNiri::Begin(Box::new(begin.request.clone())));
        self.current = Some(begin);
    }

    /// polkitd withdrew `cookie`.
    ///
    /// A withdrawn request is **not** a dismissal: polkitd asked us to stop, so the reply it is
    /// owed is a plain success. Upstream draws the same distinction
    /// (`shell-polkit-authentication-agent.c:266-281`, which completes with `dismissed = FALSE`).
    fn cancel(&mut self, cookie: &str) {
        if self.current.as_ref().is_some_and(|c| c.cookie == cookie) {
            let _ = self.to_niri.send(PolkitToNiri::Cancel);
            self.finish(false);
            return;
        }
        // Not the one on screen — drop it from the queue and answer it where it stands.
        if let Some(index) = self.scheduled.iter().position(|b| b.cookie == cookie) {
            if let Some(begin) = self.scheduled.remove(index) {
                let _ = begin.done.try_send(false);
            }
        }
    }

    fn on_request(&mut self, request: PolkitRequest) {
        match request {
            PolkitRequest::Initiate { user_name } => {
                let Some(current) = self.current.as_ref() else {
                    warn!("polkit: asked to authenticate with no request on screen");
                    return;
                };
                self.epoch += 1;
                let session = HelperSession::spawn(
                    &user_name,
                    &current.cookie,
                    self.epoch,
                    self.helper_tx.clone(),
                );
                // Replacing rather than assigning after: the old helper must die before the new
                // one starts, or two PAM conversations answer the same entry.
                self.session = None;
                match session {
                    Ok(session) => self.session = Some(session),
                    Err(err) => {
                        warn!("polkit: could not start the authentication helper: {err:#}");
                        let _ = self.to_niri.send(PolkitToNiri::Completed(false));
                    }
                }
            }
            PolkitRequest::Respond(response) => {
                let Some(session) = self.session.as_mut() else {
                    warn!("polkit: a response arrived with no conversation open");
                    return;
                };
                if let Err(err) = session.respond(&response) {
                    warn!("polkit: error answering the helper: {err:?}");
                }
            }
            PolkitRequest::Done { dismissed } => {
                self.finish(dismissed);
            }
        }
    }

    fn on_helper(&mut self, epoch: Epoch, msg: HelperMessage) {
        if epoch != self.epoch {
            // A dead conversation's parting words — see [`Epoch`].
            return;
        }
        let event = match msg {
            HelperMessage::Prompt { text, echo_on } => PolkitToNiri::Request {
                prompt: text,
                echo_on,
            },
            HelperMessage::Error(text) => PolkitToNiri::ShowError(text),
            HelperMessage::Info(text) => PolkitToNiri::ShowInfo(text),
            HelperMessage::Completed(ok) => {
                self.session = None;
                PolkitToNiri::Completed(ok)
            }
        };
        let _ = self.to_niri.send(event);
    }

    /// Answer the outstanding `BeginAuthentication` and move on to whatever was queued behind it.
    fn finish(&mut self, dismissed: bool) {
        self.session = None;
        // Bumped so anything the dying helper says next is ignored, including the
        // `Completed(false)` its EOF produces.
        self.epoch += 1;
        if let Some(current) = self.current.take() {
            let _ = current.done.try_send(dismissed);
        }
        self.process_next();
    }
}

/// A running `polkit-agent-helper-1`.
///
/// The child itself is owned by the reader thread, which reaps it when its stdout closes; we keep
/// only what is needed to talk to it and to end it. Waiting on the executor would block every
/// other D-Bus task in the process for as long as PAM takes.
struct HelperSession {
    pid: libc::pid_t,
    stdin: std::process::ChildStdin,
}

impl HelperSession {
    fn spawn(
        user_name: &str,
        cookie: &str,
        epoch: Epoch,
        events: async_channel::Sender<(Epoch, HelperMessage)>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let helper = HELPER_PATHS
            .iter()
            .find(|path| std::path::Path::new(path).exists())
            .context("polkit-agent-helper-1 is not installed anywhere we know to look")?;

        let mut child = std::process::Command::new(helper)
            .arg(user_name)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {helper}"))?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        // The cookie goes on stdin rather than the command line, and has since CVE-2015-4625:
        // argv is world-readable through /proc (`polkitagenthelperprivate.c:50-57`).
        stdin
            .write_all(cookie.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .context("writing the cookie to the helper")?;

        let pid = child.id() as libc::pid_t;
        std::thread::Builder::new()
            .name("polkit-helper".to_owned())
            .spawn(move || read_helper(child, stdout, epoch, events))
            .context("starting the helper's reader thread")?;

        Ok(Self { pid, stdin })
    }

    fn respond(&mut self, response: &str) -> std::io::Result<()> {
        // Raw, then a newline: the helper reads a line and uses it as-is, so unlike its own output
        // this direction is not escaped (`polkitagentsession.c:530-543`).
        self.stdin.write_all(response.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }
}

impl Drop for HelperSession {
    fn drop(&mut self) {
        // SIGTERM, as polkit does (`polkitagentsession.c:365-400`), so PAM gets to clean up. The
        // reader thread sees the resulting EOF and reaps the child; nothing here waits.
        // SAFETY: `kill` touches no memory of ours, and the pid is one we spawned and have not
        // reaped — the reader thread owns the `Child`, so it cannot have been recycled.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
    }
}

/// Read the helper's side of the conversation until it stops talking.
fn read_helper(
    mut child: std::process::Child,
    stdout: std::process::ChildStdout,
    epoch: Epoch,
    events: async_channel::Sender<(Epoch, HelperMessage)>,
) {
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    let mut completed = false;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => (),
            Err(err) => {
                warn!("polkit: error reading from the authentication helper: {err:?}");
                break;
            }
        }
        let Some(msg) = parse_helper_line(line.trim_end_matches('\n')) else {
            warn!("polkit: unknown line from the authentication helper");
            break;
        };
        completed = matches!(msg, HelperMessage::Completed(_));
        if events.send_blocking((epoch, msg)).is_err() || completed {
            break;
        }
    }

    // EOF without a verdict is a failure, not a silence: the helper was killed, crashed, or never
    // got going, and a dialog left waiting for a message that will not come is a dead prompt.
    if !completed {
        let _ = events.send_blocking((epoch, HelperMessage::Completed(false)));
    }

    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_protocol_is_parsed() {
        assert_eq!(
            parse_helper_line("PAM_PROMPT_ECHO_OFF Password: "),
            Some(HelperMessage::Prompt {
                text: "Password: ".to_owned(),
                echo_on: false,
            })
        );
        assert_eq!(
            parse_helper_line("PAM_PROMPT_ECHO_ON Login: "),
            Some(HelperMessage::Prompt {
                text: "Login: ".to_owned(),
                echo_on: true,
            })
        );
        assert_eq!(
            parse_helper_line("PAM_ERROR_MSG Authentication failure"),
            Some(HelperMessage::Error("Authentication failure".to_owned()))
        );
        assert_eq!(
            parse_helper_line("PAM_TEXT_INFO Place your finger on the reader"),
            Some(HelperMessage::Info(
                "Place your finger on the reader".to_owned()
            ))
        );
        assert_eq!(
            parse_helper_line("SUCCESS"),
            Some(HelperMessage::Completed(true))
        );
        assert_eq!(
            parse_helper_line("FAILURE"),
            Some(HelperMessage::Completed(false))
        );
    }

    /// The four payload prefixes end in a space, so the bare word is not one of them. Without the
    /// space a prompt of `"_ECHO_ONx"` would parse as an echo-on prompt — and, worse, a
    /// `PAM_PROMPT_ECHO_ON`-prefixed line would match the ECHO_OFF arm's payload.
    #[test]
    fn a_prefix_without_its_payload_is_not_a_message() {
        assert_eq!(parse_helper_line("PAM_PROMPT_ECHO_OFF"), None);
        assert_eq!(parse_helper_line("PAM_ERROR_MSG"), None);
        assert_eq!(parse_helper_line(""), None);
        assert_eq!(parse_helper_line("nonsense"), None);
    }

    /// `PAM_PROMPT_ECHO_ON` is a prefix of nothing, but `PAM_PROMPT_ECHO_OFF` and
    /// `PAM_PROMPT_ECHO_ON` share the first 18 characters — check the longer one is not eaten by
    /// the shorter arm.
    #[test]
    fn echo_off_is_not_read_as_echo_on() {
        let Some(HelperMessage::Prompt { echo_on, .. }) =
            parse_helper_line("PAM_PROMPT_ECHO_OFF Password: ")
        else {
            panic!("not a prompt");
        };
        assert!(!echo_on, "an ECHO_OFF prompt must not echo");
    }

    /// The helper escapes with `g_strescape`, so anything outside printable ASCII arrives as octal
    /// escapes of its **UTF-8 bytes**. Unescaping a character at a time would turn a Portuguese
    /// prompt into mojibake, and nothing in an English test would notice.
    #[test]
    fn a_non_ascii_prompt_survives_unescaping() {
        // "Senha:" with the accented form PAM would actually send: `ã` is 0xC3 0xA3.
        let escaped = "PAM_PROMPT_ECHO_OFF Senh\\303\\243:";
        assert_eq!(
            parse_helper_line(escaped),
            Some(HelperMessage::Prompt {
                text: "Senhã:".to_owned(),
                echo_on: false,
            })
        );
    }

    #[test]
    fn the_c_escapes_are_undone() {
        assert_eq!(unescape(r"a\tb"), "a\tb");
        assert_eq!(unescape(r"a\nb"), "a\nb");
        assert_eq!(unescape(r"a\\b"), r"a\b");
        assert_eq!(unescape(r#"a\"b"#), "a\"b");
        assert_eq!(unescape(r"\010"), "\u{8}");
        // Three digits at most, so a fourth is literal text.
        assert_eq!(unescape(r"\1010"), "A0");
        // An unknown escape passes the character through, as g_strcompress does.
        assert_eq!(unescape(r"\q"), "q");
        // A trailing backslash has nothing to escape.
        assert_eq!(unescape(r"ab\"), "ab");
    }

    /// polkit offers identities as `unix-user` uid tuples; anything else (a `unix-group`, say) is
    /// not something we can put a password box under, and a uid with no passwd entry is not either.
    #[test]
    fn only_resolvable_unix_users_are_offered() {
        let mut root = std::collections::HashMap::new();
        root.insert("uid".to_owned(), Value::from(0u32).try_into().unwrap());
        let mut group = std::collections::HashMap::new();
        group.insert("gid".to_owned(), Value::from(0u32).try_into().unwrap());
        let mut absent = std::collections::HashMap::new();
        absent.insert(
            "uid".to_owned(),
            Value::from(4_000_000u32).try_into().unwrap(),
        );

        let names = user_names(&[
            ("unix-group".to_owned(), group),
            ("unix-user".to_owned(), root),
            ("unix-user".to_owned(), absent),
        ]);
        assert_eq!(names, vec!["root".to_owned()]);
    }

    /// The order in `choose_user` is ours-then-root-then-first, and the first arm is the one that
    /// matters: an `auth_admin` action lists every administrator, so a list containing both us and
    /// root must pick *us* — asking for root's password when the user has their own is a prompt
    /// they may well not be able to answer.
    #[test]
    fn we_authenticate_as_ourselves_before_root() {
        let ours = crate::unlock_dialog::session_user().name;
        assert!(!ours.is_empty(), "the test user must resolve");

        let both = vec!["root".to_owned(), ours.clone()];
        assert_eq!(
            choose_user(&both),
            Some(ours.clone()),
            "ours wins over root"
        );

        let root_only = vec!["daemon".to_owned(), "root".to_owned()];
        assert_eq!(
            choose_user(&root_only),
            Some("root".to_owned()),
            "root wins when we are not offered"
        );

        let neither = vec!["daemon".to_owned(), "bin".to_owned()];
        assert_eq!(
            choose_user(&neither),
            Some("daemon".to_owned()),
            "otherwise the first identity polkitd offered"
        );

        assert_eq!(choose_user(&[]), None, "nobody to ask");
    }
}
