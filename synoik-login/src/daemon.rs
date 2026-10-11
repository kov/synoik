// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The daemon: opens the greeter's session at boot and keeps it, runs autologin once per boot,
//! and brings the greeter forward when seat0 has nothing active (`docs/fork/greeter.md` §2.6, §5).

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::config::Config;
use crate::ipc::Frame;
use crate::worker::{spawn_worker, Request};

const GREETER_USER: &str = "synoik-greeter";
/// Autologin runs once per boot: after that user logs out, the greeter is what comes up.
const AUTOLOGIN_DONE: &str = "/run/synoik-login/autologin-done";
const SEAT_DIR: &str = "/run/systemd/seats";
/// A greeter that dies sooner than this after starting is restarted after a pause, not at once.
const RESPAWN_PAUSE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Greeter,
    User,
}

struct Worker {
    kind: Kind,
    status: Option<OwnedFd>,
    session: Option<String>,
    started: Instant,
}

struct Daemon {
    workers: HashMap<libc::pid_t, Worker>,
}

pub fn run() -> anyhow::Result<()> {
    let config = Config::load();
    let signals = block_signals()?;
    // GDM's name, for the lock screen's reauthentication (§2.1). Without it sessions still open,
    // but nothing can unlock one.
    let _bus = match crate::bus::serve() {
        Ok(bus) => Some(bus),
        Err(err) => {
            eprintln!("synoik-login: not serving org.gnome.DisplayManager: {err:#}");
            None
        }
    };
    let seats = watch_seats()?;
    let mut daemon = Daemon {
        workers: HashMap::new(),
    };

    // The greeter first, so it is seat0's earliest session (Ctrl+Alt+F1, greeter.md §5); a
    // session opened after it becomes active on its own.
    daemon.open_greeter()?;

    if let Some(user) = config
        .autologin
        .filter(|_| !Path::new(AUTOLOGIN_DONE).exists())
    {
        let _ = std::fs::write(AUTOLOGIN_DONE, "");
        let request = Request {
            service: "synoik-autologin".into(),
            user,
            class: "user".into(),
            program: vec!["synoik-session".into()],
        };
        daemon.spawn(Kind::User, request)?;
    }

    loop {
        let mut fds = vec![
            libc::pollfd {
                fd: signals.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: seats.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let reporting: Vec<libc::pid_t> = daemon
            .workers
            .iter()
            .filter(|(_, w)| w.status.is_some())
            .map(|(&pid, _)| pid)
            .collect();
        for pid in &reporting {
            fds.push(libc::pollfd {
                fd: daemon.workers[pid].status.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: a valid array of pollfds.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err).context("poll");
        }

        for (pid, fd) in reporting.iter().zip(&fds[2..]) {
            if fd.revents != 0 {
                daemon.read_status(*pid);
            }
        }
        if fds[1].revents != 0 {
            drain(&seats);
            daemon.front_greeter_if_idle();
        }
        if fds[0].revents != 0 {
            match read_signal(&signals) {
                libc::SIGCHLD => daemon.reap()?,
                signal => {
                    eprintln!("synoik-login: signal {signal}, stopping the greeter");
                    daemon.stop_greeter();
                    return Ok(());
                }
            }
        }
    }
}

fn greeter_request() -> Request {
    Request {
        service: "synoik-greeter".into(),
        user: GREETER_USER.into(),
        class: "greeter".into(),
        program: vec!["synoik".into(), "--greeter".into()],
    }
}

impl Daemon {
    /// Open the greeter's session and wait for it. A new session becomes active on its own, so
    /// one that was active before — a user's, when the greeter is reopened after a crash or a
    /// restart of this daemon — is given the seat back.
    fn open_greeter(&mut self) -> anyhow::Result<()> {
        let previous = read_seat_active();
        let pid = self.spawn(Kind::Greeter, greeter_request())?;
        self.await_session(pid, Duration::from_secs(15));
        let greeter = self.workers.get(&pid).and_then(|w| w.session.clone());
        if let Some(previous) = previous.filter(|p| greeter.as_ref() != Some(p)) {
            eprintln!("synoik-login: giving seat0 back to session {previous}");
            if let Err(err) = activate_session(&previous) {
                eprintln!("synoik-login: activating session {previous}: {err:#}");
            }
        }
        Ok(())
    }

    fn spawn(&mut self, kind: Kind, request: Request) -> anyhow::Result<libc::pid_t> {
        let (read, write) = pipe()?;
        let args = request.to_args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let pid = spawn_worker(&args, write.as_raw_fd())?;
        drop(write);
        eprintln!(
            "synoik-login: worker {pid}: {} for {}",
            request.service, request.user
        );
        self.workers.insert(
            pid,
            Worker {
                kind,
                status: Some(read),
                session: None,
                started: Instant::now(),
            },
        );
        Ok(pid)
    }

    /// Block until `pid` reports its session (or gives up), so what opens next opens after it.
    fn await_session(&mut self, pid: libc::pid_t, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while let Some(fd) = self.workers.get(&pid).and_then(|w| w.status.as_ref()) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                eprintln!("synoik-login: worker {pid} did not report a session");
                return;
            }
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) } > 0 {
                self.read_status(pid);
            }
        }
    }

    fn read_status(&mut self, pid: libc::pid_t) {
        let Some(worker) = self.workers.get_mut(&pid) else {
            return;
        };
        let Some(fd) = worker.status.take() else {
            return;
        };
        // The worker reports once and closes.
        if let Ok(Some(Frame::Opened(id))) = Frame::read_from(&mut std::fs::File::from(fd)) {
            eprintln!("synoik-login: worker {pid} opened session {id}");
            worker.session = Some(id);
        }
    }

    fn reap(&mut self) -> anyhow::Result<()> {
        loop {
            let mut status = 0;
            // SAFETY: reaping our own children.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                return Ok(());
            }
            let Some(worker) = self.workers.remove(&pid) else {
                continue;
            };
            eprintln!(
                "synoik-login: worker {pid} ({:?}, session {:?}) exited, status {status:#x}",
                worker.kind, worker.session
            );
            if worker.kind == Kind::Greeter {
                if worker.started.elapsed() < RESPAWN_PAUSE {
                    std::thread::sleep(RESPAWN_PAUSE);
                }
                self.open_greeter()?;
                self.front_greeter_if_idle();
            }
        }
    }

    /// logind leaves seat0 with no active session when the active one closes (measured on a
    /// VT-less seat0); the greeter is what comes forward.
    fn front_greeter_if_idle(&self) {
        if read_seat_active().is_some() {
            return;
        }
        let Some(id) = self
            .workers
            .values()
            .find(|w| w.kind == Kind::Greeter)
            .and_then(|w| w.session.clone())
        else {
            return;
        };
        eprintln!("synoik-login: seat0 has no active session, activating the greeter ({id})");
        if let Err(err) = activate_session(&id) {
            eprintln!("synoik-login: activating session {id}: {err:#}");
        }
    }

    /// End the greeter and wait for it, so a restarted daemon does not open a second one. Users'
    /// sessions are left running.
    fn stop_greeter(&self) {
        let greeters: Vec<libc::pid_t> = self
            .workers
            .iter()
            .filter(|(_, w)| w.kind == Kind::Greeter)
            .map(|(&pid, _)| pid)
            .collect();
        for &pid in &greeters {
            // SAFETY: signalling our own child; it forwards to the session and kills it if it
            // has not ended within its grace period.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        for pid in greeters {
            // SAFETY: waiting on our own child.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        }
    }
}

fn activate_session(id: &str) -> anyhow::Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "ActivateSession",
        &(id,),
    )?;
    Ok(())
}

fn read_seat_active() -> Option<String> {
    let text = std::fs::read_to_string(format!("{SEAT_DIR}/seat0")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("ACTIVE="))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn pipe() -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: fds has room for two.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2");
    }
    // SAFETY: just created.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn block_signals() -> anyhow::Result<OwnedFd> {
    // SAFETY: plain signal-mask calls; `Daemon::spawn` clears the mask again in each worker.
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut set, signal);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("signalfd");
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn read_signal(fd: &OwnedFd) -> i32 {
    // SAFETY: a zeroed siginfo buffer of the size signalfd writes.
    let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    // SAFETY: reading one record into a buffer of its size.
    let n = unsafe { libc::read(fd.as_raw_fd(), (&raw mut info).cast(), size) };
    if n as usize == size {
        info.ssi_signo as i32
    } else {
        0
    }
}

/// A pipe that becomes readable whenever seat0's properties change. logind's signals rather
/// than inotify on its run directory: as a display manager (`xdm_t`) the daemon may read that
/// directory but not watch it.
fn watch_seats() -> anyhow::Result<OwnedFd> {
    let (read, write) = pipe()?;
    // SAFETY: setting a flag on a descriptor we own.
    unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let connection =
        zbus::blocking::Connection::system().context("connecting to the system bus")?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.login1")?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path("/org/freedesktop/login1/seat/seat0")?
        .build();
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)
        .context("subscribing to seat0")?;
    std::thread::spawn(move || {
        for _ in messages {
            // SAFETY: writing one byte to a pipe we own.
            unsafe { libc::write(write.as_raw_fd(), [0u8].as_ptr().cast(), 1) };
        }
    });
    Ok(read)
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 4096];
    // SAFETY: reading into a buffer we own until the non-blocking fd is empty.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}
