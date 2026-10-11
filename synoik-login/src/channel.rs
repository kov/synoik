// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A channel: a private peer-to-peer D-Bus socket serving GDM's `UserVerifier` to one client, with
//! one worker per PAM conversation (`docs/fork/greeter.md` §2.1-§2.5). A login channel (the
//! greeter's) serves GDM's `Greeter` beside it.
//!
//! The signals and their order are GDM 51's, because the clients are written against it
//! (`src/dbus/gdm.rs`): a question or message as the worker relays it; on success
//! `VerificationComplete` with the worker left running; on a refusal `Problem` (or
//! `ServiceUnavailable`) and `VerificationFailed`; and `ConversationStopped` whenever a worker
//! exits (`daemon/gdm-session.c:214-243`, `330-350`, `2295-2326`).

use std::collections::HashMap;
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context as _};
use zbus::fdo;
use zbus::zvariant::OwnedObjectPath;

use crate::ipc::{Frame, Secret};
use crate::worker::spawn_worker;

const SESSION_PATH: &str = "/org/gnome/DisplayManager/Session";
const VERIFIER_IFACE: &str = "org.gnome.DisplayManager.UserVerifier";
const GREETER_IFACE: &str = "org.gnome.DisplayManager.Greeter";
/// How long the client has to connect to the address it was handed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// What a login starts in its new session.
const SESSION_PROGRAM: &str = "synoik-session";

/// What a channel is for.
pub enum Kind {
    /// Verify `user` again and unlock and front their running `session` (§2.5).
    Reauth { user: String, session: String },
    /// The greeter's login channel: verify whoever it names and open a new session (§2.4).
    Login,
}

pub struct Target {
    pub kind: Kind,
    /// The only uid that may connect: the caller that asked for the channel.
    pub client_uid: u32,
}

/// The PAM service behind a service name on the wire. The wire keeps GDM's names, so the client
/// speaks to real GDM unchanged (§2.1).
fn pam_service(wire: &str) -> Option<&'static str> {
    match wire {
        "gdm-password" => Some("synoik-password"),
        "gdm-fingerprint" => Some("synoik-fingerprint"),
        _ => None,
    }
}

/// Open a channel for `target`; returns its D-Bus address. An abstract socket: no file to label
/// or clean up, and nothing reachable but by the address.
pub fn open(target: Target) -> anyhow::Result<String> {
    let name = format!("synoik-login/{}", random_hex(16)?);
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let listener = UnixListener::bind_addr(&addr).context("binding a channel")?;
    std::thread::spawn(move || {
        if let Err(err) = serve(listener, target) {
            eprintln!("synoik-login: channel: {err:#}");
        }
    });
    Ok(format!("unix:abstract={name}"))
}

fn serve(listener: UnixListener, target: Target) -> anyhow::Result<()> {
    let stream = accept_one(&listener)?;
    drop(listener);
    let uid = peer_uid(&stream)?;
    if uid != target.client_uid {
        bail!(
            "uid {uid} connected to a channel for uid {}",
            target.client_uid
        );
    }

    let login = matches!(target.kind, Kind::Login);
    let shared = Arc::new(Shared {
        user: Mutex::new(match &target.kind {
            Kind::Reauth { user, .. } => Some(user.clone()),
            Kind::Login => None,
        }),
        target,
        conversations: Mutex::new(HashMap::new()),
        winner: Mutex::new(None),
        connection: OnceLock::new(),
    });
    let mut builder = zbus::blocking::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(
            SESSION_PATH,
            Verifier {
                shared: shared.clone(),
            },
        )?;
    if login {
        builder = builder.serve_at(
            SESSION_PATH,
            Greeter {
                shared: shared.clone(),
            },
        )?;
    }
    let connection = builder.build().context("serving the channel")?;
    let _ = shared.connection.set(connection.clone());
    eprintln!("synoik-login: {} connected", shared.name());

    // Until the client goes away: then every conversation it was driving goes with it. A session
    // already started is no longer one of them.
    for message in zbus::blocking::MessageIterator::from(&connection) {
        if message.is_err() {
            break;
        }
    }
    shared.stop_all();
    Ok(())
}

struct Shared {
    target: Target,
    /// Who the conversations verify: fixed for reauthentication, named per conversation by the
    /// greeter on a login channel.
    user: Mutex<Option<String>>,
    /// Running conversations by wire service name.
    conversations: Mutex<HashMap<String, Conversation>>,
    /// A login channel's winning conversation: the first to verify (§2.4), and the session it
    /// opened once it has.
    winner: Mutex<Option<Winner>>,
    connection: OnceLock<zbus::blocking::Connection>,
}

struct Conversation {
    pid: libc::pid_t,
    to_worker: UnixStream,
    /// Past `VerificationComplete`: the worker waits on the daemon, not inside PAM.
    verified: bool,
}

struct Winner {
    service: String,
    session: Option<String>,
}

impl Shared {
    fn name(&self) -> String {
        match &self.target.kind {
            Kind::Reauth { user, .. } => format!("channel for {user}"),
            Kind::Login => "login channel".to_owned(),
        }
    }

    fn emit_on<B>(&self, iface: &str, member: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let connection = self.connection.wait();
        eprintln!("synoik-login: {}: {member}", self.name());
        let result = connection.emit_signal(None::<&str>, SESSION_PATH, iface, member, body);
        match result {
            // The client hung up first — it cancels, then closes. Nothing is listening for this.
            Err(zbus::Error::InputOutput(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(err) => eprintln!("synoik-login: emitting {member}: {err}"),
            Ok(()) => {}
        }
    }

    fn emit<B>(&self, member: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.emit_on(VERIFIER_IFACE, member, body);
    }

    fn stop(&self, service: &str) {
        if let Some(conversation) = self.conversations.lock().unwrap().get(service) {
            end(conversation);
        }
    }

    fn stop_all(&self) {
        for conversation in self.conversations.lock().unwrap().values() {
            end(conversation);
        }
    }

    /// Relay one worker's frames until it exits.
    fn pump(self: Arc<Self>, service: String, pid: libc::pid_t, mut from_worker: UnixStream) {
        self.emit("ConversationStarted", &(&service,));
        loop {
            let frame = match Frame::read_from(&mut from_worker) {
                Ok(Some(frame)) => frame,
                Ok(None) | Err(_) => break,
            };
            match frame {
                Frame::InfoQuery(text) => self.emit("InfoQuery", &(&service, text)),
                Frame::SecretInfoQuery(text) => self.emit("SecretInfoQuery", &(&service, text)),
                Frame::Info(text) => self.emit("Info", &(&service, text)),
                Frame::Problem(text) => self.emit("Problem", &(&service, text)),
                Frame::Complete => self.verified(&service),
                Frame::Opened(id) => {
                    if let Some(winner) = self.winner.lock().unwrap().as_mut() {
                        winner.session = Some(id.clone());
                    }
                    self.emit_on(GREETER_IFACE, "SessionOpened", &(&service, id));
                }
                Frame::Failed(text) => {
                    self.lose(&service);
                    self.emit("Problem", &(&service, text));
                    self.emit("VerificationFailed", &(&service,));
                    self.stop(&service);
                }
                Frame::Unavailable(text) => {
                    self.lose(&service);
                    self.emit("ServiceUnavailable", &(&service, text));
                    self.emit("VerificationFailed", &(&service,));
                    self.stop(&service);
                }
                Frame::Answer(_) | Frame::Open | Frame::Start => (),
            }
        }
        // A started session's worker shut its end and went on to parent the session; the daemon
        // has it now.
        let ours = self
            .conversations
            .lock()
            .unwrap()
            .get(&service)
            .is_some_and(|c| c.pid == pid);
        if !ours {
            return;
        }
        // Drain whatever the worker still had to say; it is ending.
        let _ = from_worker.read_to_end(&mut Vec::new());
        // SAFETY: reaping our own worker (the daemon's reaper may already have).
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        {
            let mut conversations = self.conversations.lock().unwrap();
            if conversations.get(&service).is_some_and(|c| c.pid == pid) {
                conversations.remove(&service);
            }
        }
        self.lose(&service);
        self.emit("ConversationStopped", &(&service,));
    }

    fn verified(&self, service: &str) {
        if let Some(conversation) = self.conversations.lock().unwrap().get_mut(service) {
            conversation.verified = true;
        }
        match &self.target.kind {
            Kind::Reauth { session, .. } => {
                self.emit("VerificationComplete", &(service,));
                // Unlocked and fronted: the lock screen's own session, or — from the greeter —
                // the session the user already has (`gdm-manager.c:947-970`).
                for method in ["UnlockSession", "ActivateSession"] {
                    if let Err(err) = logind_session_call(method, session) {
                        eprintln!("synoik-login: {method} {session}: {err:#}");
                    }
                }
            }
            Kind::Login => {
                // The first to verify wins and is the only one that opens a session (§2.4).
                {
                    let mut winner = self.winner.lock().unwrap();
                    if winner.is_some() {
                        drop(winner);
                        self.stop(service);
                        return;
                    }
                    *winner = Some(Winner {
                        service: service.to_owned(),
                        session: None,
                    });
                }
                let conversations = self.conversations.lock().unwrap();
                for (other, conversation) in conversations.iter() {
                    if other != service {
                        end(conversation);
                    }
                }
                if let Some(conversation) = conversations.get(service) {
                    let _ = Frame::Open.write_to(&mut &conversation.to_worker);
                }
                drop(conversations);
                self.emit("VerificationComplete", &(service,));
            }
        }
    }

    /// `service` is out: if it had won, the login is open to the next one to verify.
    fn lose(&self, service: &str) {
        let mut winner = self.winner.lock().unwrap();
        if winner.as_ref().is_some_and(|w| w.service == service) {
            *winner = None;
        }
    }
}

/// End a conversation. One still inside PAM is killed — `pam_fprintd` blocks there with no
/// question outstanding; one past verification is waiting on us, so closing its socket lets it
/// drop its credentials and end its transaction (§2.3).
fn end(conversation: &Conversation) {
    if conversation.verified {
        let _ = conversation.to_worker.shutdown(std::net::Shutdown::Both);
    } else {
        // SAFETY: signalling our own worker.
        unsafe { libc::kill(conversation.pid, libc::SIGTERM) };
    }
}

struct Verifier {
    shared: Arc<Shared>,
}

impl Verifier {
    fn begin(&self, service: String) -> fdo::Result<()> {
        let shared = &self.shared;
        eprintln!("synoik-login: {}: begin {service}", shared.name());
        if shared.conversations.lock().unwrap().contains_key(&service) {
            return Err(fdo::Error::Failed(format!("{service} is already running")));
        }
        let Some(pam) = pam_service(&service) else {
            // As GDM reports a service with no PAM file: unavailable, failed, stopped.
            let shared = shared.clone();
            std::thread::spawn(move || {
                shared.emit("ServiceUnavailable", &(&service, ""));
                shared.emit("VerificationFailed", &(&service,));
                shared.emit("ConversationStopped", &(&service,));
            });
            return Ok(());
        };
        let Some(user) = shared.user.lock().unwrap().clone() else {
            return Err(fdo::Error::Failed("no user to verify".into()));
        };

        let (ours, theirs) = UnixStream::pair().map_err(|e| fdo::Error::IOError(e.to_string()))?;
        let args: Vec<&str> = match shared.target.kind {
            Kind::Reauth { .. } => vec!["verify", pam, &user],
            Kind::Login => vec!["login", pam, &user, "--", SESSION_PROGRAM],
        };
        let pid = spawn_worker(&args, theirs.as_raw_fd())
            .map_err(|e| fdo::Error::Failed(format!("{e:#}")))?;
        drop(theirs);
        let to_worker = ours
            .try_clone()
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        shared.conversations.lock().unwrap().insert(
            service.clone(),
            Conversation {
                pid,
                to_worker,
                verified: false,
            },
        );
        let shared = shared.clone();
        std::thread::spawn(move || shared.pump(service, pid, ours));
        Ok(())
    }
}

#[zbus::interface(name = "org.gnome.DisplayManager.UserVerifier")]
impl Verifier {
    /// Extensions (choice lists, custom JSON) serve smartcard and switchable auth, which we do
    /// not offer; accepting the call keeps libgdm-shaped clients happy.
    fn enable_extensions(&self, _extensions: Vec<String>) {}

    /// Verification without a user is PAM asking for the name; the greeter always names one
    /// (§4.2), so a login channel refuses it.
    fn begin_verification(&self, service_name: String) -> fdo::Result<()> {
        if matches!(self.shared.target.kind, Kind::Login) {
            return Err(fdo::Error::NotSupported(
                "name the user: BeginVerificationForUser".into(),
            ));
        }
        self.begin(service_name)
    }

    fn begin_verification_for_user(
        &self,
        service_name: String,
        username: String,
    ) -> fdo::Result<()> {
        {
            let mut user = self.shared.user.lock().unwrap();
            match &self.shared.target.kind {
                Kind::Reauth { .. } => {
                    if user.as_deref() != Some(username.as_str()) {
                        return Err(fdo::Error::AccessDenied(
                            "this channel verifies another user".into(),
                        ));
                    }
                }
                Kind::Login => {
                    let busy = !self.shared.conversations.lock().unwrap().is_empty();
                    if busy && user.as_deref() != Some(username.as_str()) {
                        return Err(fdo::Error::Failed(
                            "already verifying another user; cancel first".into(),
                        ));
                    }
                    *user = Some(username);
                }
            }
        }
        self.begin(service_name)
    }

    fn answer_query(&self, service_name: String, answer: String) -> fdo::Result<()> {
        eprintln!(
            "synoik-login: {}: AnswerQuery {service_name}",
            self.shared.name()
        );
        let answer = Secret::new(answer.into_bytes());
        let conversations = self.shared.conversations.lock().unwrap();
        let Some(conversation) = conversations.get(&service_name) else {
            return Err(fdo::Error::Failed(format!("{service_name} is not running")));
        };
        Frame::Answer(answer)
            .write_to(&mut &conversation.to_worker)
            .map_err(|e| fdo::Error::IOError(e.to_string()))
    }

    fn cancel(&self) {
        eprintln!("synoik-login: {}: Cancel", self.shared.name());
        self.shared.stop_all();
    }
}

struct Greeter {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.gnome.DisplayManager.Greeter")]
impl Greeter {
    /// There is one session type (greeter.md §0).
    fn select_session(&self, _session: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("there is one session".into()))
    }

    /// The user is the one each conversation names; nothing to remember here.
    fn select_user(&self, _username: String) {}

    /// Autologin is the daemon's own, once per boot (§2.6), never the greeter's to ask for.
    fn begin_auto_login(&self, _username: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "autologin is not the greeter's".into(),
        ))
    }

    fn get_timed_login_details(&self) -> fdo::Result<(bool, String, i32)> {
        Err(fdo::Error::NotSupported("there is no timed login".into()))
    }

    /// Start the session the winning conversation opened. From here it is the daemon's: the
    /// greeter closing this channel must not end it.
    fn start_session_when_ready(
        &self,
        service_name: String,
        should_start_session: bool,
    ) -> fdo::Result<()> {
        let session = {
            let winner = self.shared.winner.lock().unwrap();
            match winner.as_ref() {
                Some(w) if w.service == service_name => w.session.clone(),
                _ => {
                    return Err(fdo::Error::Failed(format!(
                        "{service_name} has no session to start"
                    )))
                }
            }
        };
        let Some(session) = session else {
            return Err(fdo::Error::Failed("the session is not open yet".into()));
        };
        if !should_start_session {
            self.shared.stop(&service_name);
            return Ok(());
        }
        let conversation = self
            .shared
            .conversations
            .lock()
            .unwrap()
            .remove(&service_name)
            .ok_or_else(|| fdo::Error::Failed(format!("{service_name} is gone")))?;
        Frame::Start
            .write_to(&mut &conversation.to_worker)
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        eprintln!(
            "synoik-login: starting session {session} (worker {})",
            conversation.pid
        );
        crate::daemon::adopt(conversation.pid, &session);
        Ok(())
    }

    /// End the user's other graphical sessions — the ones gnome-shell found from
    /// `SessionOpened`'s id and asked to stop (`gdm-session.c:1802-1822`).
    fn stop_conflicting_session(&self) -> fdo::Result<()> {
        let (Some(user), Some(session)) = (
            self.shared.user.lock().unwrap().clone(),
            self.shared
                .winner
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|w| w.session.clone()),
        ) else {
            return Err(fdo::Error::Failed("no session has opened".into()));
        };
        stop_conflicting(&user, &session).map_err(|e| fdo::Error::Failed(format!("{e:#}")))
    }
}

fn stop_conflicting(user: &str, keep: &str) -> anyhow::Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    let reply = connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "ListSessions",
        &(),
    )?;
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        reply.body().deserialize()?;
    for (id, _, name, _, path) in sessions {
        if name != user || id == keep {
            continue;
        }
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )?;
        let kind: String = proxy.get_property("Type")?;
        let state: String = proxy.get_property("State")?;
        if !matches!(kind.as_str(), "wayland" | "x11")
            || !matches!(state.as_str(), "active" | "online")
        {
            continue;
        }
        eprintln!("synoik-login: ending {user}'s conflicting session {id}");
        logind_session_call("TerminateSession", &id)?;
    }
    Ok(())
}

fn accept_one(listener: &UnixListener) -> anyhow::Result<UnixStream> {
    let mut pfd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let ready = unsafe { libc::poll(&mut pfd, 1, CONNECT_TIMEOUT.as_millis() as libc::c_int) };
    if ready <= 0 {
        bail!("nobody connected");
    }
    let (stream, _) = listener.accept().context("accepting")?;
    Ok(stream)
}

fn peer_uid(stream: &UnixStream) -> anyhow::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of the size given.
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(cred.uid)
}

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn logind_session_call(method: &str, id: &str) -> anyhow::Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        method,
        &(id,),
    )?;
    Ok(())
}
