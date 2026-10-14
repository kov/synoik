// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! plymouthd's client protocol, answered by the outer and the initramfs splash
//! (`docs/fork/boot-splash.md` §3), so every tool that talks to plymouth — systemd's password agent
//! and unit updates, PackageKit and dnf5 offline updates, dracut, util-linux — talks to them
//! unchanged.
//!
//! Three layers: [`Parser`] turns the byte stream into [`Request`]s, [`Splash`] is what they say
//! the screen should show, and [`Server`] holds the socket and the connections and keeps replies in
//! request order. Drawing the splash is the caller's; it reads [`Server::splash`] and reports back
//! through [`Server::answer_password`] and [`Server::finish_quit`].

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;

use crate::peer_cred;

/// plymouthd's socket: the abstract name, without its leading NUL (`ply-boot-protocol.h:25`).
pub const SOCKET_NAME: &[u8] = b"/org/freedesktop/plymouthd";

/// Where plymouthd keeps its pid, which gates systemd's plymouth password agent
/// (`systemd-ask-password-plymouth.path`'s `ConditionPathExists=`).
pub const PID_FILE: &str = "/run/plymouth/pid";

// Replies (`ply-boot-protocol.h:51-55`).
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const ANSWER: u8 = 0x02;
const NO_ANSWER: u8 = 0x05;

/// A request's argument follows when the byte after the command is this
/// (`ply-boot-client.c:362-389`, `ply-boot-server.c:229-274`).
const HAS_ARGUMENT: u8 = 0x02;

/// One request, by its command byte (`ply-boot-protocol.h:27-49`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `P`
    Ping,
    /// `U`: pid 1 names each unit it starts.
    Update(String),
    /// `C`
    ChangeMode(String),
    /// `u`: offline-update progress, as plymouthd reads it with `strtol`.
    SystemUpdate(String),
    /// `S`
    Sysinit,
    /// `!`
    Error,
    /// `$`
    ShowSplash,
    /// `H`
    HideSplash,
    /// `D`
    Deactivate,
    /// `r`
    Reactivate,
    /// `Q`, with `--retain-splash` or not.
    Quit { retain: bool },
    /// `l`
    Reload,
    /// `*`, its prompt optional.
    Password(Option<String>),
    /// `c`
    CachedPassword,
    /// `W`
    Question(Option<String>),
    /// `M`
    ShowMessage(String),
    /// `m`
    HideMessage(String),
    /// `K`
    WatchKeystroke,
    /// `L`
    IgnoreKeystroke,
    /// `A`
    PauseProgress,
    /// `a`
    UnpauseProgress,
    /// `R`
    NewRoot,
    /// `V`
    HasActiveVt,
    /// Any other byte.
    Unknown(u8),
}

impl Request {
    fn new(command: u8, argument: Option<Vec<u8>>) -> Self {
        let text = || argument.as_deref().map(text);
        let or_empty = || text().unwrap_or_default();
        match command {
            b'P' => Self::Ping,
            b'U' => Self::Update(or_empty()),
            b'C' => Self::ChangeMode(or_empty()),
            b'u' => Self::SystemUpdate(or_empty()),
            b'S' => Self::Sysinit,
            b'!' => Self::Error,
            b'$' => Self::ShowSplash,
            b'H' => Self::HideSplash,
            b'D' => Self::Deactivate,
            b'r' => Self::Reactivate,
            // The CLI sends one byte, 1 for `--retain-splash`; util-linux sends no argument at all,
            // which plymouthd dereferences as NULL (`ply-boot-server.c:530`).
            b'Q' => Self::Quit {
                retain: argument.as_deref().and_then(|a| a.first()) == Some(&1),
            },
            b'l' => Self::Reload,
            b'*' => Self::Password(text().filter(|t| !t.is_empty())),
            b'c' => Self::CachedPassword,
            b'W' => Self::Question(text().filter(|t| !t.is_empty())),
            b'M' => Self::ShowMessage(or_empty()),
            b'm' => Self::HideMessage(or_empty()),
            b'K' => Self::WatchKeystroke,
            b'L' => Self::IgnoreKeystroke,
            b'A' => Self::PauseProgress,
            b'a' => Self::UnpauseProgress,
            b'R' => Self::NewRoot,
            b'V' => Self::HasActiveVt,
            other => Self::Unknown(other),
        }
    }
}

/// An argument up to its NUL.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The request stream of one connection. Clients write several requests at once (systemd sends
/// `M…` and `A` in one write) and nothing says a request arrives in one read.
#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole request, if one has arrived.
    pub fn next_request(&mut self) -> Option<Request> {
        let (&command, rest) = self.buffer.split_first()?;
        let (&marker, rest) = rest.split_first()?;
        if marker != HAS_ARGUMENT {
            self.buffer.drain(..2);
            return Some(Request::new(command, None));
        }
        // The length counts the NUL. A 255-byte argument wraps it to 0: an empty argument, where
        // plymouthd asserts (`ply-utils.c:367`).
        let (&len, rest) = rest.split_first()?;
        let len = usize::from(len);
        if rest.len() < len {
            return None;
        }
        let argument = rest[..len].to_vec();
        self.buffer.drain(..3 + len);
        Some(Request::new(command, Some(argument)))
    }
}

/// What a reply says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ack,
    Nak,
    Answer(String),
    /// The splash cannot prompt: the client asks some other way.
    NoAnswer,
}

impl Reply {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Ack => out.push(ACK),
            Self::Nak => out.push(NAK),
            Self::NoAnswer => out.push(NO_ANSWER),
            Self::Answer(answer) => {
                out.push(ANSWER);
                // Little-endian (`ply-utils.c:311-322`), and no NUL.
                out.extend_from_slice(&(answer.len() as u32).to_le_bytes());
                out.extend_from_slice(answer.as_bytes());
            }
        }
    }
}

/// The screen plymouth's mode asks for (`plymouth change-mode`, `plymouth.c:828-841`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    BootUp,
    Shutdown,
    Reboot,
    Updates,
    SystemUpgrade,
    FirmwareUpgrade,
    SystemReset,
}

impl Mode {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "boot-up" => Self::BootUp,
            "shutdown" => Self::Shutdown,
            "reboot" => Self::Reboot,
            "updates" => Self::Updates,
            "system-upgrade" => Self::SystemUpgrade,
            "firmware-upgrade" => Self::FirmwareUpgrade,
            "system-reset" => Self::SystemReset,
            _ => return None,
        })
    }
}

/// What the requests so far say the splash shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Splash {
    pub mode: Mode,
    /// `$` since the last quit.
    pub shown: bool,
    /// `H`: the splash stops animating and keeps its last frame.
    pub frozen: bool,
    /// Shown in order; `m` removes exact matches, an empty `M` clears them all.
    pub messages: Vec<String>,
    /// The offline update's percentage, once one is reported.
    pub progress: Option<u8>,
    /// `A`: the progress stays where it is until `a`.
    pub progress_paused: bool,
    /// The password the splash is asking for, if any: the prompt of the oldest unanswered `*`.
    pub password_prompt: Option<Prompt>,
}

/// A password request on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: Option<String>,
}

/// What changed for the outer, from [`Server::take_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// [`Server::splash`] changed: redraw.
    Changed,
    /// A client asked the splash to quit. The outer ends it, then calls [`Server::finish_quit`].
    /// Without `retain`, the console goes back to text (§2.4).
    Quit { retain: bool },
}

/// `strtol` of the percentage, clamped: plymouthd takes a parse failure as 0
/// (`ply-boot-server.c:464-484`).
fn percent(text: &str) -> u8 {
    let text = text.trim_start();
    let (negative, text) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits = &text[..text.bytes().take_while(u8::is_ascii_digit).count()];
    if negative || digits.is_empty() {
        return 0;
    }
    // Only an overflow fails here, and `strtol` saturates.
    digits.parse::<u64>().map_or(100, |n| n.min(100) as u8)
}

/// A request waiting on the outer, holding its connection's later requests behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Password,
    Quit,
}

struct Connection {
    stream: UnixStream,
    uid: u32,
    parser: Parser,
    /// Parsed but not yet answered, oldest first: replies go out in request order.
    queue: VecDeque<Request>,
    waiting: Option<Waiting>,
    out: Vec<u8>,
    /// The peer stopped reading or writing: drop it once nothing is left to say.
    closed: bool,
}

/// The socket, its connections and the splash they describe.
pub struct Server {
    /// `None` until [`Self::try_bind`] succeeds: at boot the name is plymouthd's until it quits,
    /// and the splash is on screen before that.
    listener: Option<UnixListener>,
    connections: HashMap<u64, Connection>,
    next_id: u64,
    /// plymouthd answers root only (`ply-boot-server.c:265-285`).
    allowed_uid: u32,
    splash: Splash,
    /// Connections whose `*` waits on the splash, with their prompts, oldest first.
    password_waiters: VecDeque<(u64, Option<String>)>,
    events: Vec<Event>,
    pid_file: Option<PathBuf>,
    /// Connections that are over, for the caller to stop watching before it drops them: a
    /// watch outliving its fd could match the next fd given that number.
    closing: Vec<(u64, UnixStream)>,
}

impl Server {
    /// A server answering only `allowed_uid`, not yet listening.
    pub fn new(allowed_uid: u32) -> Self {
        Self {
            listener: None,
            connections: HashMap::new(),
            next_id: 1,
            allowed_uid,
            splash: Splash::default(),
            password_waiters: VecDeque::new(),
            events: Vec::new(),
            pid_file: None,
            closing: Vec::new(),
        }
    }

    /// Listen on abstract `name`, answering only `allowed_uid`.
    #[cfg(test)]
    pub fn bind(name: &[u8], allowed_uid: u32) -> io::Result<Self> {
        let mut server = Self::new(allowed_uid);
        server.try_bind(name)?;
        Ok(server)
    }

    /// Start listening on abstract `name`. `AddrInUse` while plymouthd still holds it.
    pub fn try_bind(&mut self, name: &[u8]) -> io::Result<()> {
        let addr = SocketAddr::from_abstract_name(name)?;
        let listener = UnixListener::bind_addr(&addr)?;
        listener.set_nonblocking(true)?;
        self.listener = Some(listener);
        Ok(())
    }

    /// Keep `path` holding our pid while a splash is up, as plymouthd does with its own.
    pub fn set_pid_file(&mut self, path: PathBuf) {
        self.pid_file = Some(path);
        self.sync_pid_file();
    }

    pub fn listener_fd(&self) -> Option<BorrowedFd<'_>> {
        self.listener.as_ref().map(|l| l.as_fd())
    }

    /// Whether connection `id` is over, waiting in [`Self::take_closing`].
    pub fn is_closing(&self, id: u64) -> bool {
        self.closing.iter().any(|(c, _)| *c == id)
    }

    /// The connections that ended since the last call. Stop watching each, then drop it: the drop
    /// is what closes it, and the close is what `plymouth --wait` waits for.
    pub fn take_closing(&mut self) -> Vec<(u64, UnixStream)> {
        std::mem::take(&mut self.closing)
    }

    pub fn connection_fd(&self, id: u64) -> Option<BorrowedFd<'_>> {
        self.connections.get(&id).map(|c| c.stream.as_fd())
    }

    pub fn splash(&self) -> &Splash {
        &self.splash
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Accept every pending connection; returns their ids, for the caller to watch.
    pub fn accept(&mut self) -> Vec<u64> {
        let mut accepted = Vec::new();
        let Some(listener) = &self.listener else {
            return accepted;
        };
        loop {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    warn!("plymouth socket: accept: {err}");
                    break;
                }
            };
            let uid = match peer_cred(stream.as_fd()) {
                Ok(cred) => cred.uid,
                Err(err) => {
                    warn!("plymouth socket: no peer credentials: {err}");
                    continue;
                }
            };
            if let Err(err) = stream.set_nonblocking(true) {
                warn!("plymouth socket: {err}");
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.connections.insert(
                id,
                Connection {
                    stream,
                    uid,
                    parser: Parser::default(),
                    queue: VecDeque::new(),
                    waiting: None,
                    out: Vec::new(),
                    closed: false,
                },
            );
            accepted.push(id);
        }
        accepted
    }

    /// Read what connection `id` sent and answer it. Returns false once the connection is gone,
    /// for the caller to stop watching it.
    pub fn on_readable(&mut self, id: u64) -> bool {
        let Some(conn) = self.connections.get_mut(&id) else {
            return false;
        };
        let mut buf = [0u8; 1024];
        loop {
            match conn.stream.read(&mut buf) {
                Ok(0) => {
                    conn.closed = true;
                    break;
                }
                Ok(n) => conn.parser.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    conn.closed = true;
                    break;
                }
            }
        }
        while let Some(request) = conn.parser.next_request() {
            conn.queue.push_back(request);
        }
        self.serve(id);
        self.reap(id)
    }

    /// Accept and read everything pending, and close what ended. For driving the server without
    /// an event loop.
    #[cfg(test)]
    pub fn dispatch(&mut self) {
        self.accept();
        let ids: Vec<u64> = self.connections.keys().copied().collect();
        for id in ids {
            self.on_readable(id);
        }
        self.take_closing();
    }

    /// Show the splash, as `$` does: the outer's own start, where nobody sends one.
    pub fn show(&mut self) {
        self.show_splash();
    }

    /// End the splash without a client asking: a session's screen replaced it. As a `Q` that
    /// has completed, without the event.
    pub fn end(&mut self) {
        self.end_splash();
        self.finish_quit();
    }

    /// Answer the password on screen: `Some` with what was typed, `None` when it was cancelled
    /// or the splash cannot ask (the client then asks some other way).
    pub fn answer_password(&mut self, answer: Option<String>) {
        let Some((id, _)) = self.password_waiters.pop_front() else {
            return;
        };
        let reply = answer.map_or(Reply::NoAnswer, Reply::Answer);
        self.resume(id, Waiting::Password, &reply);
        self.update_prompt();
    }

    /// The splash has ended as `Event::Quit` asked: acknowledge every waiting quit, and close the
    /// connections with nothing pending, which is how `plymouth --wait` learns the splash is gone.
    pub fn finish_quit(&mut self) {
        let waiting: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, c)| c.waiting == Some(Waiting::Quit))
            .map(|(&id, _)| id)
            .collect();
        for id in waiting {
            self.resume(id, Waiting::Quit, &Reply::Ack);
        }
        let idle: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, c)| c.waiting.is_none() && c.queue.is_empty())
            .map(|(&id, _)| id)
            .collect();
        for id in idle {
            if let Some(conn) = self.connections.get_mut(&id) {
                conn.closed = true;
            }
            self.reap(id);
        }
    }

    /// Answer `id`'s waiting request, then whatever it queued behind it.
    fn resume(&mut self, id: u64, waiting: Waiting, reply: &Reply) {
        let Some(conn) = self.connections.get_mut(&id) else {
            return;
        };
        if conn.waiting != Some(waiting) {
            return;
        }
        conn.waiting = None;
        reply.encode(&mut conn.out);
        self.serve(id);
        self.reap(id);
    }

    /// Answer `id`'s queued requests in order, stopping at one that waits on the outer.
    fn serve(&mut self, id: u64) {
        loop {
            let Some(conn) = self.connections.get_mut(&id) else {
                return;
            };
            if conn.waiting.is_some() {
                break;
            }
            let Some(request) = conn.queue.pop_front() else {
                break;
            };
            let reply = if conn.uid == self.allowed_uid {
                self.handle(id, request)
            } else {
                Some(Reply::Nak)
            };
            let conn = self.connections.get_mut(&id).expect("still connected");
            match reply {
                Some(reply) => reply.encode(&mut conn.out),
                None => break,
            }
        }
        self.flush(id);
    }

    /// Apply one request; `None` when its reply waits on the outer.
    fn handle(&mut self, id: u64, request: Request) -> Option<Reply> {
        let reply = match request {
            Request::Ping
            | Request::Update(_)
            | Request::Sysinit
            | Request::Error
            | Request::Deactivate
            | Request::Reactivate
            | Request::Reload
            | Request::NewRoot
            | Request::IgnoreKeystroke => Reply::Ack,
            Request::ChangeMode(name) => {
                match Mode::parse(&name) {
                    Some(mode) if mode != self.splash.mode => {
                        self.splash.mode = mode;
                        self.changed();
                    }
                    Some(_) => (),
                    None => debug!("plymouth: ignoring unknown mode {name:?}"),
                }
                Reply::Ack
            }
            Request::SystemUpdate(text) => {
                if !self.splash.progress_paused {
                    self.splash.progress = Some(percent(&text));
                    self.changed();
                }
                Reply::Ack
            }
            Request::ShowSplash => {
                self.show_splash();
                Reply::Ack
            }
            Request::HideSplash => {
                if self.splash.shown && !self.splash.frozen {
                    self.splash.frozen = true;
                    self.changed();
                }
                Reply::Ack
            }
            Request::Quit { retain } => {
                if !self.splash.shown {
                    // Nothing to end: a restarted outer, or a second quit.
                    return Some(Reply::Ack);
                }
                self.end_splash();
                self.events.push(Event::Quit { retain });
                self.connections.get_mut(&id)?.waiting = Some(Waiting::Quit);
                return None;
            }
            Request::Password(text) => {
                if !self.splash.shown {
                    return Some(Reply::NoAnswer);
                }
                self.connections.get_mut(&id)?.waiting = Some(Waiting::Password);
                self.password_waiters.push_back((id, text));
                self.update_prompt();
                return None;
            }
            // Never NAK: systemd's agent turns anything but an answer into -EIO.
            Request::CachedPassword | Request::Question(_) | Request::WatchKeystroke => {
                Reply::NoAnswer
            }
            Request::ShowMessage(text) => {
                // systemd clears with an empty message: `m` "does not work" for it.
                if text.is_empty() {
                    if !self.splash.messages.is_empty() {
                        self.splash.messages.clear();
                        self.changed();
                    }
                } else {
                    self.splash.messages.push(text);
                    self.changed();
                }
                Reply::Ack
            }
            Request::HideMessage(text) => {
                let before = self.splash.messages.len();
                self.splash.messages.retain(|m| *m != text);
                if self.splash.messages.len() != before {
                    self.changed();
                }
                Reply::Ack
            }
            Request::PauseProgress => {
                self.splash.progress_paused = true;
                Reply::Ack
            }
            Request::UnpauseProgress => {
                self.splash.progress_paused = false;
                Reply::Ack
            }
            Request::HasActiveVt | Request::Unknown(_) => Reply::Nak,
        };
        Some(reply)
    }

    fn show_splash(&mut self) {
        if !self.splash.shown || self.splash.frozen {
            self.splash.shown = true;
            self.splash.frozen = false;
            self.changed();
            self.sync_pid_file();
        }
    }

    fn end_splash(&mut self) {
        if !self.splash.shown {
            return;
        }
        self.splash.shown = false;
        self.splash.frozen = false;
        self.splash.messages.clear();
        self.splash.progress = None;
        self.cancel_passwords();
        self.changed();
        self.sync_pid_file();
    }

    /// The splash is going away: nobody can type into it, so every waiting `*` asks elsewhere.
    fn cancel_passwords(&mut self) {
        while !self.password_waiters.is_empty() {
            self.answer_password(None);
        }
    }

    /// The splash asks for the oldest waiting password.
    fn update_prompt(&mut self) {
        let prompt = self
            .password_waiters
            .front()
            .map(|(_, text)| Prompt { text: text.clone() });
        if self.splash.password_prompt != prompt {
            self.splash.password_prompt = prompt;
            self.changed();
        }
    }

    fn changed(&mut self) {
        if self.events.last() != Some(&Event::Changed) {
            self.events.push(Event::Changed);
        }
    }

    fn sync_pid_file(&self) {
        let Some(path) = &self.pid_file else { return };
        let result = if self.splash.shown {
            fs::write(path, format!("{}\n", std::process::id()))
        } else {
            fs::remove_file(path).or_else(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(err)
                }
            })
        };
        if let Err(err) = result {
            warn!("plymouth pid file {}: {err}", path.display());
        }
    }

    /// Write what is pending for `id`. Replies are a few bytes, so a full socket means a client
    /// that stopped reading: it is dropped.
    fn flush(&mut self, id: u64) {
        let Some(conn) = self.connections.get_mut(&id) else {
            return;
        };
        while !conn.out.is_empty() {
            // SAFETY: a buffer we own, on a socket we own.
            let n = unsafe {
                libc::send(
                    std::os::fd::AsRawFd::as_raw_fd(&conn.stream),
                    conn.out.as_ptr().cast(),
                    conn.out.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Fire-and-forget clients close before reading: EPIPE is routine.
                conn.out.clear();
                conn.closed = true;
                break;
            }
            conn.out.drain(..n as usize);
        }
    }

    /// Drop `id` once it is closed and owes nothing. Returns whether it is still there.
    fn reap(&mut self, id: u64) -> bool {
        let Some(conn) = self.connections.get(&id) else {
            return false;
        };
        if !conn.closed {
            return true;
        }
        // A waiting request dies with its client (the client's way of cancelling a password).
        if conn.waiting == Some(Waiting::Password) {
            self.password_waiters.retain(|(w, _)| *w != id);
            self.update_prompt();
        }
        if let Some(conn) = self.connections.remove(&id) {
            self.closing.push((id, conn.stream));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn parse_all(bytes: &[u8]) -> Vec<Request> {
        let mut parser = Parser::default();
        parser.feed(bytes);
        std::iter::from_fn(|| parser.next_request()).collect()
    }

    #[test]
    fn requests_parse_from_the_wire() {
        // Every row of boot-splash.md §3.2, as the clients write it.
        let cases: &[(&[u8], Request)] = &[
            (b"P\0", Request::Ping),
            (
                b"U\x02\x0cfoo.service\0",
                Request::Update("foo.service".into()),
            ),
            (b"C\x02\x08updates\0", Request::ChangeMode("updates".into())),
            (b"u\x02\x0342\0", Request::SystemUpdate("42".into())),
            (b"S\0", Request::Sysinit),
            (b"!\0", Request::Error),
            (b"$\0", Request::ShowSplash),
            (b"H\0", Request::HideSplash),
            (b"D\0", Request::Deactivate),
            (b"r\0", Request::Reactivate),
            (b"Q\x02\x02\x01\0", Request::Quit { retain: true }),
            (b"Q\x02\x01\0", Request::Quit { retain: false }),
            (b"l\0", Request::Reload),
            (
                b"*\x02\x0aPassword:\0",
                Request::Password(Some("Password:".into())),
            ),
            (b"*\0", Request::Password(None)),
            (b"c\0", Request::CachedPassword),
            (b"W\x02\x04Go?\0", Request::Question(Some("Go?".into()))),
            (b"M\x02\x03hi\0", Request::ShowMessage("hi".into())),
            (b"m\x02\x03hi\0", Request::HideMessage("hi".into())),
            (b"K\0", Request::WatchKeystroke),
            (b"L\x02\x02y\0", Request::IgnoreKeystroke),
            (b"A\0", Request::PauseProgress),
            (b"a\0", Request::UnpauseProgress),
            (b"R\x02\x09/sysroot\0", Request::NewRoot),
            (b"V\0", Request::HasActiveVt),
            (b"z\0", Request::Unknown(b'z')),
        ];
        for (bytes, request) in cases {
            assert_eq!(parse_all(bytes), std::slice::from_ref(request), "{bytes:?}");
        }
    }

    #[test]
    fn malformed_requests_do_not_crash() {
        // util-linux's argument-less quit, which plymouthd dereferences as NULL.
        assert_eq!(parse_all(b"Q\0"), [Request::Quit { retain: false }]);
        // A 255-byte argument wraps the length to 0, where plymouthd asserts.
        assert_eq!(
            parse_all(b"M\x02\x00P\0"),
            [Request::ShowMessage(String::new()), Request::Ping]
        );
        // No argument where one is expected.
        assert_eq!(parse_all(b"u\0"), [Request::SystemUpdate(String::new())]);
        assert_eq!(parse_all(b"M\0"), [Request::ShowMessage(String::new())]);
    }

    #[test]
    fn requests_arrive_in_any_pieces() {
        // systemd's FIDO2 message and its pause, in one write…
        let both = b"M\x02\x06touch\0A\0";
        assert_eq!(
            parse_all(both),
            [Request::ShowMessage("touch".into()), Request::PauseProgress]
        );
        // …or a byte at a time.
        let mut parser = Parser::default();
        let mut got = Vec::new();
        for byte in both {
            parser.feed(&[*byte]);
            got.extend(std::iter::from_fn(|| parser.next_request()));
        }
        assert_eq!(
            got,
            [Request::ShowMessage("touch".into()), Request::PauseProgress]
        );
    }

    #[test]
    fn progress_reads_like_strtol() {
        assert_eq!(percent("42"), 42);
        assert_eq!(percent(" 7%"), 7);
        assert_eq!(percent("+3"), 3);
        assert_eq!(percent("250"), 100);
        assert_eq!(percent("99999999999999999999999"), 100);
        assert_eq!(percent("-5"), 0);
        assert_eq!(percent("nope"), 0);
        assert_eq!(percent(""), 0);
    }

    #[test]
    fn replies_encode_as_plymouth_reads_them() {
        let mut out = Vec::new();
        for reply in [
            Reply::Ack,
            Reply::Nak,
            Reply::NoAnswer,
            Reply::Answer("pw".into()),
        ] {
            reply.encode(&mut out);
        }
        assert_eq!(out, b"\x06\x15\x05\x02\x02\0\0\0pw");
    }

    /// A server on a name of its own, answering this test's uid, and clients for it.
    struct Harness {
        server: Server,
        name: Vec<u8>,
    }

    impl Harness {
        fn new() -> Self {
            Self::answering(unsafe { libc::getuid() })
        }

        fn answering(uid: u32) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let name = format!(
                "/synoik-test/plymouth/{}/{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
            .into_bytes();
            let server = Server::bind(&name, uid).unwrap();
            Self { server, name }
        }

        fn connect(&mut self) -> UnixStream {
            let addr = SocketAddr::from_abstract_name(&self.name).unwrap();
            let stream = UnixStream::connect_addr(&addr).unwrap();
            stream.set_nonblocking(true).unwrap();
            self.server.dispatch();
            stream
        }

        fn send(&mut self, client: &mut UnixStream, bytes: &[u8]) -> Vec<u8> {
            client.write_all(bytes).unwrap();
            self.server.dispatch();
            read_now(client)
        }
    }

    /// What the server has written to `client` so far.
    fn read_now(client: &mut UnixStream) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("{err}"),
            }
        }
        got
    }

    /// Whether the server closed `client`, waiting for it: a child another test forks holds a copy
    /// of the server's end until it execs, so the end-of-file can trail the close.
    fn closed(client: &mut UnixStream) -> bool {
        client.set_nonblocking(false).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        matches!(client.read(&mut [0u8; 1]), Ok(0))
    }

    #[test]
    fn answers_root_only() {
        let other = unsafe { libc::getuid() } + 1;
        let mut h = Harness::answering(other);
        let mut client = h.connect();
        assert_eq!(h.send(&mut client, b"P\0$\0"), [NAK, NAK]);
        assert!(!h.server.splash().shown);
    }

    #[test]
    fn simple_requests_answer_at_once() {
        let mut h = Harness::new();
        let mut client = h.connect();
        assert_eq!(
            h.send(
                &mut client,
                b"P\0U\x02\x0cfoo.service\0S\0R\x02\x09/sysroot\0"
            ),
            [ACK; 4]
        );
        assert_eq!(h.send(&mut client, b"V\0z\0"), [NAK, NAK]);
        // The prompts the splash never shows send the client elsewhere.
        assert_eq!(h.send(&mut client, b"c\0W\x02\x04Go?\0K\0"), [NO_ANSWER; 3]);
    }

    #[test]
    fn the_splash_follows_mode_messages_and_progress() {
        let mut h = Harness::new();
        let mut client = h.connect();
        let bytes = b"$\0C\x02\x08updates\0M\x02\x06first\0M\x02\x07second\0u\x02\x0342\0";
        assert_eq!(h.send(&mut client, bytes), [ACK; 5]);
        let splash = h.server.splash();
        assert!(splash.shown);
        assert_eq!(splash.mode, Mode::Updates);
        assert_eq!(splash.messages, ["first", "second"]);
        assert_eq!(splash.progress, Some(42));
        assert_eq!(h.server.take_events(), [Event::Changed]);

        // `m` removes the exact text; an unknown mode changes nothing.
        h.send(&mut client, b"m\x02\x06first\0C\x02\x05nope\0");
        assert_eq!(h.server.splash().messages, ["second"]);
        assert_eq!(h.server.splash().mode, Mode::Updates);
        // An empty `M` clears, as systemd uses it.
        h.send(&mut client, b"M\x02\x01\0");
        assert!(h.server.splash().messages.is_empty());
        // Paused progress holds.
        h.send(&mut client, b"A\0u\x02\x0390\0");
        assert_eq!(h.server.splash().progress, Some(42));
        h.send(&mut client, b"a\0u\x02\x0390\0");
        assert_eq!(h.server.splash().progress, Some(90));
    }

    #[test]
    fn a_password_holds_its_connection_until_answered() {
        let mut h = Harness::new();
        let mut client = h.connect();
        h.send(&mut client, b"$\0");
        // A ping behind the password waits for it, whether it came with it or after it: replies
        // keep request order.
        assert_eq!(h.send(&mut client, b"*\x02\x07Disk: \0P\0"), [0u8; 0]);
        assert_eq!(h.send(&mut client, b"P\0"), [0u8; 0]);
        assert_eq!(
            h.server.splash().password_prompt,
            Some(Prompt {
                text: Some("Disk: ".into())
            })
        );
        h.server.answer_password(Some("secret".into()));
        assert_eq!(read_now(&mut client), b"\x02\x06\0\0\0secret\x06\x06");
        assert_eq!(h.server.splash().password_prompt, None);
    }

    #[test]
    fn a_cancelled_password_sends_the_client_elsewhere() {
        let mut h = Harness::new();
        let mut client = h.connect();
        h.send(&mut client, b"$\0");
        h.send(&mut client, b"*\0");
        h.server.answer_password(None);
        assert_eq!(read_now(&mut client), [NO_ANSWER]);
    }

    #[test]
    fn no_splash_no_password() {
        let mut h = Harness::new();
        let mut client = h.connect();
        assert_eq!(h.send(&mut client, b"*\x02\x03pw\0"), [NO_ANSWER]);
    }

    #[test]
    fn a_client_that_leaves_takes_its_password_with_it() {
        let mut h = Harness::new();
        let mut first = h.connect();
        let mut second = h.connect();
        h.send(&mut first, b"$\0");
        h.send(&mut first, b"*\x02\x02a\0");
        h.send(&mut second, b"*\x02\x02b\0");
        drop(first);
        h.server.dispatch();
        // The answer goes to the one still asking, and its prompt is the one on screen.
        assert_eq!(
            h.server.splash().password_prompt,
            Some(Prompt {
                text: Some("b".into())
            })
        );
        h.server.answer_password(Some("b".into()));
        assert_eq!(read_now(&mut second), b"\x02\x01\0\0\0b");
    }

    #[test]
    fn quit_waits_for_the_splash_to_end_then_releases_waiters() {
        let mut h = Harness::new();
        let mut asker = h.connect();
        let mut waiter = h.connect();
        let mut quitter = h.connect();
        h.send(&mut asker, b"$\0");
        h.send(&mut asker, b"*\0");
        // `plymouth --wait` sends nothing at all.
        assert_eq!(h.send(&mut quitter, b"Q\x02\x02\x01\0"), [0u8; 0]);
        h.server.take_events();
        // The splash is going: its password prompt sends the client elsewhere.
        assert_eq!(read_now(&mut asker), [NO_ANSWER]);
        assert!(!h.server.splash().shown);

        h.server.finish_quit();
        assert_eq!(read_now(&mut quitter), [ACK]);
        // Handed out, not dropped: the caller stops watching them first. All three, the quitter
        // too, once answered: nothing is left to say to any of them.
        assert_eq!(h.server.take_closing().len(), 3);
        assert!(closed(&mut waiter));
        assert!(closed(&mut asker));
    }

    #[test]
    fn the_outer_shows_and_ends_the_splash_itself() {
        let dir = std::env::temp_dir().join(format!("synoik-plymouth-end-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pid");
        let mut h = Harness::new();
        h.server.set_pid_file(path.clone());
        h.server.show();
        assert!(h.server.splash().shown);
        assert!(path.exists());
        assert_eq!(h.server.take_events(), [Event::Changed]);

        let mut waiter = h.connect();
        let mut asker = h.connect();
        h.send(&mut asker, b"*\0");
        h.server.end();
        // Nobody asked to quit: no event to act on, only the change to draw.
        assert_eq!(h.server.take_events(), [Event::Changed]);
        assert!(!h.server.splash().shown);
        assert!(!path.exists());
        assert_eq!(read_now(&mut asker), [NO_ANSWER]);
        h.server.take_closing();
        assert!(closed(&mut waiter));
        // The socket stays: a later quit is answered at once.
        let mut late = h.connect();
        assert_eq!(h.send(&mut late, b"Q\0"), [ACK]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_is_answered_before_the_bind() {
        let mut server = Server::new(unsafe { libc::getuid() });
        assert!(server.listener_fd().is_none());
        assert!(server.accept().is_empty());
        // The model works regardless: the splash is drawn before the name is free.
        server.show();
        assert!(server.splash().shown);
    }

    #[test]
    fn quit_reports_whether_the_splash_is_retained() {
        let mut h = Harness::new();
        let mut client = h.connect();
        h.send(&mut client, b"$\0");
        h.server.take_events();
        h.send(&mut client, b"Q\0");
        assert_eq!(
            h.server.take_events(),
            [Event::Changed, Event::Quit { retain: false }]
        );
    }

    #[test]
    fn quit_with_no_splash_is_acknowledged_at_once() {
        // The outer restarted: its unit quits plymouth again, at its own socket.
        let mut h = Harness::new();
        let mut client = h.connect();
        assert_eq!(h.send(&mut client, b"Q\x02\x02\x01\0"), [ACK]);
        assert!(h.server.take_events().is_empty());
    }

    #[test]
    fn the_pid_file_marks_a_splash_on_screen() {
        let dir = std::env::temp_dir().join(format!("synoik-plymouth-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pid");
        let mut h = Harness::new();
        h.server.set_pid_file(path.clone());
        assert!(!path.exists());
        let mut client = h.connect();
        h.send(&mut client, b"$\0");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        h.send(&mut client, b"Q\x02\x02\x01\0");
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// plymouth's own client against the server, inside a user and network namespace of its own:
    /// there it is root, and plymouthd's socket name is free. Re-runs this test binary there.
    #[test]
    fn plymouths_client_talks_to_us() {
        const INSIDE: &str = "SYNOIK_TEST_PLYMOUTH_INSIDE";
        let client = "/usr/bin/plymouth";
        if std::env::var_os(INSIDE).is_some() {
            return inside_namespace(client);
        }
        if !std::path::Path::new(client).exists() {
            eprintln!("skipping: no {client}");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--net", "--"])
            .arg(exe)
            .args([
                "--exact",
                "system_compositor::plymouth::tests::plymouths_client_talks_to_us",
                "--nocapture",
            ])
            .env(INSIDE, "1")
            .status();
        match status {
            Ok(status) => assert!(status.success(), "inside the namespace: {status}"),
            Err(err) => eprintln!("skipping: unshare: {err}"),
        }
    }

    fn inside_namespace(client: &str) {
        use std::process::Command;
        use std::sync::mpsc;

        let mut server = Server::bind(SOCKET_NAME, 0).unwrap();
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        let run = |args: Vec<&'static str>| {
            let tx = tx.clone();
            let client = client.to_owned();
            std::thread::spawn(move || {
                let out = Command::new(client).args(&args).output().unwrap();
                tx.send(vec![
                    args.join(" "),
                    out.status.code().unwrap_or(-1).to_string(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                ])
                .unwrap();
            })
        };
        let wait = |server: &mut Server, done: &dyn Fn(&Server) -> bool| {
            for _ in 0..500 {
                server.dispatch();
                if done(server) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out");
        };
        let finished = |rx: &mpsc::Receiver<Vec<String>>, server: &mut Server| {
            for _ in 0..500 {
                server.dispatch();
                if let Ok(result) = rx.try_recv() {
                    return result;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("the client never finished");
        };

        run(vec!["--ping"]);
        assert_eq!(finished(&rx, &mut server)[1], "0");
        run(vec!["show-splash"]);
        finished(&rx, &mut server);
        run(vec!["change-mode", "--updates"]);
        finished(&rx, &mut server);
        run(vec!["system-update", "--progress=37"]);
        finished(&rx, &mut server);
        run(vec!["display-message", "--text=Installing updates"]);
        finished(&rx, &mut server);
        assert!(server.splash().shown);
        assert_eq!(server.splash().mode, Mode::Updates);
        assert_eq!(server.splash().progress, Some(37));
        assert_eq!(server.splash().messages, ["Installing updates"]);

        // A password, typed on the splash.
        run(vec!["ask-for-password", "--prompt=Disk"]);
        wait(&mut server, &|s| s.splash().password_prompt.is_some());
        server.answer_password(Some("hunter2".into()));
        let asked = finished(&rx, &mut server);
        assert_eq!((asked[1].as_str(), asked[2].as_str()), ("0", "hunter2"));

        // `--wait` returns when the splash quits; the quit itself once it has ended.
        run(vec!["--wait"]);
        run(vec!["quit", "--retain-splash"]);
        wait(&mut server, &|s| !s.splash().shown);
        assert_eq!(
            server.take_events().last(),
            Some(&Event::Quit { retain: true })
        );
        server.finish_quit();
        let mut results = [finished(&rx, &mut server), finished(&rx, &mut server)];
        results.sort();
        assert_eq!(results[0][..2], ["--wait", "0"]);
        assert_eq!(results[1][..2], ["quit --retain-splash", "0"]);

        // Nothing it cannot answer is fatal to it.
        run(vec!["--has-active-vt"]);
        assert_eq!(finished(&rx, &mut server)[1], "1");
    }
}
