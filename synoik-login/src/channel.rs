// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A verification channel: a private peer-to-peer D-Bus socket serving GDM's `UserVerifier` to one
//! client, with one worker per PAM conversation (`docs/fork/greeter.md` §2.1-§2.3, §2.5).
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

use crate::ipc::{Frame, Secret};
use crate::worker::spawn_worker;

const SESSION_PATH: &str = "/org/gnome/DisplayManager/Session";
const VERIFIER_IFACE: &str = "org.gnome.DisplayManager.UserVerifier";
/// How long the client has to connect to the address it was handed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Who a channel verifies, and for whom.
pub struct Target {
    pub user: String,
    /// The only uid that may connect: the caller that asked for the channel.
    pub client_uid: u32,
    /// The logind session to unlock once the user is verified.
    pub session: String,
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

    let shared = Arc::new(Shared {
        target,
        conversations: Mutex::new(HashMap::new()),
        connection: OnceLock::new(),
    });
    let connection = zbus::blocking::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(
            SESSION_PATH,
            Verifier {
                shared: shared.clone(),
            },
        )?
        .build()
        .context("serving the channel")?;
    let _ = shared.connection.set(connection.clone());
    eprintln!("synoik-login: channel for {} connected", shared.target.user);

    // Until the client goes away: then every conversation it was driving goes with it.
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
    /// Running conversations by wire service name, each its worker's pid.
    conversations: Mutex<HashMap<String, Conversation>>,
    connection: OnceLock<zbus::blocking::Connection>,
}

struct Conversation {
    pid: libc::pid_t,
    to_worker: UnixStream,
}

impl Shared {
    fn emit<B>(&self, member: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let connection = self.connection.wait();
        eprintln!("synoik-login: channel for {}: {member}", self.target.user);
        let result =
            connection.emit_signal(None::<&str>, SESSION_PATH, VERIFIER_IFACE, member, body);
        match result {
            // The client hung up first — it cancels, then closes. Nothing is listening for this.
            Err(zbus::Error::InputOutput(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(err) => eprintln!("synoik-login: emitting {member}: {err}"),
            Ok(()) => {}
        }
    }

    fn stop(&self, service: &str) {
        if let Some(conversation) = self.conversations.lock().unwrap().get(service) {
            // SAFETY: signalling our own worker.
            unsafe { libc::kill(conversation.pid, libc::SIGTERM) };
        }
    }

    fn stop_all(&self) {
        for conversation in self.conversations.lock().unwrap().values() {
            // SAFETY: signalling our own worker.
            unsafe { libc::kill(conversation.pid, libc::SIGTERM) };
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
                Frame::Complete => {
                    self.emit("VerificationComplete", &(&service,));
                    if let Err(err) = unlock_session(&self.target.session) {
                        eprintln!(
                            "synoik-login: unlocking session {}: {err:#}",
                            self.target.session
                        );
                    }
                }
                Frame::Failed(text) => {
                    self.emit("Problem", &(&service, text));
                    self.emit("VerificationFailed", &(&service,));
                    self.stop(&service);
                }
                Frame::Unavailable(text) => {
                    self.emit("ServiceUnavailable", &(&service, text));
                    self.emit("VerificationFailed", &(&service,));
                    self.stop(&service);
                }
                Frame::Opened(_) | Frame::Answer(_) => (),
            }
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
        self.emit("ConversationStopped", &(&service,));
    }
}

struct Verifier {
    shared: Arc<Shared>,
}

impl Verifier {
    fn begin(&self, service: String) -> fdo::Result<()> {
        let shared = &self.shared;
        eprintln!(
            "synoik-login: channel for {}: begin {service}",
            shared.target.user
        );
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

        let (ours, theirs) = UnixStream::pair().map_err(|e| fdo::Error::IOError(e.to_string()))?;
        let args = ["verify", pam, &shared.target.user];
        let pid = spawn_worker(&args, theirs.as_raw_fd())
            .map_err(|e| fdo::Error::Failed(format!("{e:#}")))?;
        drop(theirs);
        let to_worker = ours
            .try_clone()
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        shared
            .conversations
            .lock()
            .unwrap()
            .insert(service.clone(), Conversation { pid, to_worker });
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

    fn begin_verification(&self, service_name: String) -> fdo::Result<()> {
        self.begin(service_name)
    }

    fn begin_verification_for_user(
        &self,
        service_name: String,
        username: String,
    ) -> fdo::Result<()> {
        if username != self.shared.target.user {
            return Err(fdo::Error::AccessDenied(
                "this channel verifies another user".into(),
            ));
        }
        self.begin(service_name)
    }

    fn answer_query(&self, service_name: String, answer: String) -> fdo::Result<()> {
        eprintln!(
            "synoik-login: channel for {}: AnswerQuery {service_name}",
            self.shared.target.user
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
        eprintln!(
            "synoik-login: channel for {}: Cancel",
            self.shared.target.user
        );
        self.shared.stop_all();
    }
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

fn unlock_session(id: &str) -> anyhow::Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "UnlockSession",
        &(id,),
    )?;
    Ok(())
}
