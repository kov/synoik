// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A worker: one PAM transaction that opens a session and parents it until it ends
//! (`docs/fork/greeter.md` §2.3-§2.4).
//!
//! It reports `opened <logind session id>` on fd 3 once the session exists, then closes it.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::FromRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{bail, Context as _};

use crate::config;
use crate::ipc::Frame;
use crate::pam::{Pam, PAM_DELETE_CRED, PAM_ESTABLISH_CRED};

/// Where a worker talks to the daemon: a pipe (session workers) or a socket (verification).
pub const STATUS_FD: i32 = 3;

/// Start `synoik-login ARGS...` as root, with `fd` as its [`STATUS_FD`]. Returns its pid; the
/// daemon's SIGCHLD handler reaps it.
pub fn spawn_worker(args: &[&str], fd: libc::c_int) -> anyhow::Result<libc::pid_t> {
    let mut command = Command::new(std::env::current_exe().context("finding our binary")?);
    command.args(args).stdin(Stdio::null());
    // SAFETY: only async-signal-safe calls. The child keeps `fd` on STATUS_FD, and gets back the
    // signals the daemon takes through its signalfd: an exec keeps the mask.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, STATUS_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut none = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = command.spawn().context("spawning a worker")?;
    Ok(child.id() as libc::pid_t)
}

const PATH: &str = "/usr/local/bin:/usr/bin";

pub struct Request {
    pub service: String,
    pub user: String,
    pub class: String,
    pub program: Vec<String>,
}

impl Request {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        match args {
            [service, user, class, sep, program @ ..] if sep == "--" && !program.is_empty() => {
                Ok(Self {
                    service: service.clone(),
                    user: user.clone(),
                    class: class.clone(),
                    program: program.to_vec(),
                })
            }
            _ => bail!("usage: synoik-login session SERVICE USER CLASS -- PROGRAM [ARG...]"),
        }
    }

    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "session".to_owned(),
            self.service.clone(),
            self.user.clone(),
            self.class.clone(),
            "--".to_owned(),
        ];
        args.extend(self.program.iter().cloned());
        args
    }
}

struct Account {
    name: CString,
    uid: libc::uid_t,
    gid: libc::gid_t,
    home: String,
    shell: String,
}

fn account(user: &str) -> anyhow::Result<Account> {
    let name = CString::new(user)?;
    // SAFETY: getpwnam's result is copied out before any other passwd call.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        bail!("no such user: {user}");
    }
    let field =
        |p: *const libc::c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: non-null passwd entry.
    let pw = unsafe { &*pw };
    Ok(Account {
        name,
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        home: field(pw.pw_dir),
        shell: field(pw.pw_shell),
    })
}

static CHILD: AtomicI32 = AtomicI32::new(0);

/// How long a session gets to end on its own after SIGTERM before it is killed.
const TERM_GRACE_SECS: libc::c_uint = 5;

extern "C" fn forward(signal: libc::c_int) {
    let child = CHILD.load(Ordering::Relaxed);
    if child > 0 {
        // SAFETY: kill and alarm are async-signal-safe.
        unsafe {
            libc::kill(-child, signal);
            if signal == libc::SIGTERM {
                libc::alarm(TERM_GRACE_SECS);
            }
        }
    }
}

extern "C" fn kill_session(_: libc::c_int) {
    let child = CHILD.load(Ordering::Relaxed);
    if child > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(-child, libc::SIGKILL) };
    }
}

pub fn run(request: &Request) -> anyhow::Result<i32> {
    let account = account(&request.user)?;
    let mut pam = Pam::start(&request.service, &request.user)?;

    // Everything pam_systemd reads goes in before the session opens (greeter.md §2.4).
    pam.putenv("XDG_SEAT", "seat0")?;
    pam.putenv("XDG_SESSION_TYPE", "wayland")?;
    pam.putenv("XDG_SESSION_CLASS", &request.class)?;
    pam.putenv("XDG_SESSION_DESKTOP", "synoik")?;

    pam.authenticate()?;
    pam.acct_mgmt()?;

    // SAFETY: plain syscalls; the uid stays 0 until the child drops it.
    unsafe {
        if libc::setgid(account.gid) != 0
            || libc::initgroups(account.name.as_ptr(), account.gid) != 0
        {
            bail!("setting groups: {}", std::io::Error::last_os_error());
        }
    }
    pam.setcred(PAM_ESTABLISH_CRED)?;
    pam.open_session()?;

    let id = pam.getenv("XDG_SESSION_ID").unwrap_or_default();
    eprintln!(
        "synoik-login: opened session {id} for {} ({})",
        request.user, request.service
    );
    // SAFETY: the daemon put the write end of a pipe on this fd; nothing else owns it.
    let mut status = unsafe { File::from_raw_fd(STATUS_FD) };
    let _ = Frame::Opened(id.clone()).write_to(&mut status);
    drop(status);

    let env = session_environment(&pam, &account)?;
    let argv: Vec<CString> = request
        .program
        .iter()
        .map(|a| CString::new(a.as_str()))
        .collect::<Result<_, _>>()?;
    let home = CString::new(account.home.as_str())?;

    // SAFETY: this process is single-threaded, so the child may run ordinary code before exec.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => bail!("fork: {}", std::io::Error::last_os_error()),
        0 => exec_session(pam, account.uid, &home, &argv, &env),
        _ => (),
    }

    CHILD.store(pid, Ordering::Relaxed);
    // SAFETY: installing handlers that only call kill and alarm.
    unsafe {
        libc::signal(libc::SIGTERM, forward as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, forward as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGALRM,
            kill_session as *const () as libc::sighandler_t,
        );
    }

    let mut wstatus = 0;
    loop {
        // SAFETY: waiting on our own child.
        let ret = unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        if ret == pid {
            break;
        }
        if ret == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    eprintln!("synoik-login: session {id} for {} ended", request.user);

    if let Err(err) = pam.close_session() {
        eprintln!("synoik-login: {err:#}");
    }
    if let Err(err) = pam.setcred(PAM_DELETE_CRED) {
        eprintln!("synoik-login: {err:#}");
    }
    drop(pam);
    // Whatever the session left behind in its process group goes with it.
    // SAFETY: signalling the session's own process group.
    unsafe { libc::kill(-pid, libc::SIGHUP) };

    Ok(if libc::WIFEXITED(wstatus) {
        libc::WEXITSTATUS(wstatus)
    } else {
        1
    })
}

fn session_environment(pam: &Pam, account: &Account) -> anyhow::Result<Vec<CString>> {
    let mut env = config::environment_d();
    env.insert("HOME".into(), account.home.clone());
    env.insert("USER".into(), account.name.to_string_lossy().into_owned());
    env.insert(
        "LOGNAME".into(),
        account.name.to_string_lossy().into_owned(),
    );
    env.insert("SHELL".into(), account.shell.clone());
    env.entry("PATH".into()).or_insert_with(|| PATH.to_owned());
    if let Ok(socket) = std::env::var("SYNOIK_SYSTEM_SOCKET") {
        env.insert("SYNOIK_SYSTEM_SOCKET".into(), socket);
    }
    // PAM's own (XDG_SESSION_ID, XDG_RUNTIME_DIR, ...) win.
    for pair in pam.envlist().context("reading the PAM environment")? {
        if let Some((k, v)) = pair.to_string_lossy().split_once('=') {
            env.insert(k.to_owned(), v.to_owned());
        }
    }
    env.into_iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(Into::into))
        .collect()
}

fn exec_session(pam: Pam, uid: libc::uid_t, home: &CStr, argv: &[CString], env: &[CString]) -> ! {
    // The parent owns the session: end this copy of the transaction without tearing it down.
    pam.end_silently();
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());
    // SAFETY: the child of a single-threaded process; every pointer outlives the exec.
    unsafe {
        libc::setsid();
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::close(devnull);
        }
        if libc::setuid(uid) != 0 || libc::getuid() != uid {
            eprintln!("synoik-login: setuid: {}", std::io::Error::last_os_error());
            libc::_exit(1);
        }
        if libc::chdir(home.as_ptr()) != 0 {
            libc::chdir(c"/".as_ptr());
        }
        libc::execvpe(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        eprintln!(
            "synoik-login: exec {}: {}",
            argv[0].to_string_lossy(),
            std::io::Error::last_os_error()
        );
        libc::_exit(127);
    }
}
