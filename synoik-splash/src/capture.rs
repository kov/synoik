// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Where the details view's text comes from in the initramfs (`docs/fork/boot-splash.md` §8.2), as
//! plymouthd gathers it: the kernel's records from `/dev/kmsg` at the console's level
//! (`ply-kmsg-reader.c`), and every write to `/dev/console`, redirected into a pty of ours with
//! `TIOCCONS` (`ply-terminal-session.c`), with pid 1 told to print its status there
//! (`tell_systemd_to_print_details`).
//!
//! Everything is opened at start: past switch_root the splash's root is the emptied initramfs,
//! whose `/dev` and `/proc` have moved to the real root.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::time::Duration;

use synoik_splash::details::{self, Console};

/// How long a reading of the console's level stands (`ply_get_kmsg_log_levels`).
const LEVELS_FOR: Duration = Duration::from_secs(1);

pub struct Capture {
    kmsg: Option<File>,
    printk: Option<File>,
    levels: (u8, u8),
    levels_read: Option<Duration>,
    /// The pty's controller, the redirected console's writes readable on it; its other end, kept
    /// open so the controller never reads a hangup.
    pty: Option<(File, OwnedFd)>,
    /// `/dev/console`, to undo the redirect through.
    console: Option<OwnedFd>,
    /// Whether pid 1 was told to print its status.
    told_init: bool,
}

impl Capture {
    /// Open the sources and redirect the console. What fails is left out, and said so.
    pub fn start() -> Self {
        let open = |path: &str| {
            File::open(path)
                .inspect_err(|err| warn!("details: {path}: {err}"))
                .ok()
        };
        let mut capture = Self {
            kmsg: open("/dev/kmsg"),
            printk: open("/proc/sys/kernel/printk"),
            levels: (4, 4),
            levels_read: None,
            pty: None,
            console: None,
            told_init: false,
        };
        if let Some(kmsg) = &capture.kmsg {
            set_nonblocking(kmsg.as_fd());
        }
        match redirect_console() {
            Ok((pty, console)) => {
                capture.pty = Some(pty);
                capture.console = Some(console);
                // systemd prints its status only to a console that shows it (`ShowStatus=auto`
                // under `quiet`); it is ours now. An init that is not systemd is not told.
                if init_is_systemd() {
                    capture.told_init = signal_init(20);
                }
            }
            Err(err) => warn!("details: not redirecting the console: {err}"),
        }
        capture
    }

    /// The descriptors to poll for text.
    pub fn fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
        self.kmsg
            .as_ref()
            .map(AsFd::as_fd)
            .into_iter()
            .chain(self.pty.as_ref().map(|(p, _)| p.as_fd()))
    }

    /// Move what is waiting into `console`: whether anything came.
    pub fn read(&mut self, console: &mut Console, now: Duration) -> bool {
        let mut any = false;
        if self.kmsg.is_some() {
            if self
                .levels_read
                .is_none_or(|t| now.saturating_sub(t) >= LEVELS_FOR)
            {
                self.levels_read = Some(now);
                if let Some(levels) = self.read_levels() {
                    self.levels = levels;
                }
            }
            let mut record = [0u8; 8192];
            while let Some(kmsg) = &mut self.kmsg {
                match kmsg.read(&mut record) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&record[..n]);
                        if let Some(text) = details::kmsg_text(&text, self.levels.0, self.levels.1)
                        {
                            console.write_str(&text);
                            any = true;
                        }
                    }
                    // Overwritten before we read it: the next read goes on from the oldest kept.
                    Err(err) if err.raw_os_error() == Some(libc::EPIPE) => (),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        warn!("details: /dev/kmsg: {err}");
                        self.kmsg = None;
                    }
                }
            }
        }
        if let Some((pty, _)) = &mut self.pty {
            let mut buf = [0u8; 4096];
            loop {
                match pty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        console.write(&buf[..n]);
                        any = true;
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        warn!("details: the console's pty: {err}");
                        break;
                    }
                }
            }
        }
        any
    }

    fn read_levels(&self) -> Option<(u8, u8)> {
        let mut buf = [0u8; 64];
        let n = self.printk.as_ref()?.read_at(&mut buf, 0).ok()?;
        details::printk_levels(std::str::from_utf8(&buf[..n]).ok()?)
    }

    /// Give the console back and tell pid 1 to stop printing its status, before the splash goes.
    /// The kernel drops the redirect itself once our pty hangs up, but pid 1 would print on.
    pub fn stop(&mut self) {
        if let Some(console) = self.console.take() {
            // `TIOCCONS` on the console itself undoes the redirect (`tioccons`).
            // SAFETY: an ioctl without argument on a descriptor of ours.
            if unsafe { libc::ioctl(console.as_raw_fd(), libc::TIOCCONS) } < 0 {
                warn!(
                    "details: undoing the console redirect: {}",
                    io::Error::last_os_error()
                );
            }
        }
        self.pty = None;
        if std::mem::take(&mut self.told_init) {
            signal_init(21);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn set_nonblocking(fd: BorrowedFd<'_>) {
    // SAFETY: fcntl on a descriptor of ours.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// A pty, `/dev/console` redirected to its other end, and `/dev/console` itself.
fn redirect_console() -> io::Result<((File, OwnedFd), OwnedFd)> {
    let check = |rv: libc::c_int| {
        if rv < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rv)
        }
    };
    // SAFETY: plain syscalls; every descriptor is owned at once.
    unsafe {
        let controller = OwnedFd::from_raw_fd(check(libc::posix_openpt(
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        ))?);
        check(libc::grantpt(controller.as_raw_fd()))?;
        check(libc::unlockpt(controller.as_raw_fd()))?;
        let mut name = [0 as libc::c_char; 64];
        let rv = libc::ptsname_r(controller.as_raw_fd(), name.as_mut_ptr(), name.len());
        if rv != 0 {
            return Err(io::Error::from_raw_os_error(rv));
        }
        let name = CStr::from_ptr(name.as_ptr());
        let other = OwnedFd::from_raw_fd(check(libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        ))?);
        // The text as written: no "\n" turned into "\r\n", no echo.
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(other.as_raw_fd(), &raw mut tio) == 0 {
            libc::cfmakeraw(&raw mut tio);
            libc::tcsetattr(other.as_raw_fd(), libc::TCSANOW, &raw const tio);
        }
        let console = OwnedFd::from_raw_fd(check(libc::open(
            c"/dev/console".as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        ))?);
        check(libc::ioctl(other.as_raw_fd(), libc::TIOCCONS))?;
        Ok(((File::from(controller), other), console))
    }
}

fn init_is_systemd() -> bool {
    std::fs::read_to_string("/proc/1/comm").is_ok_and(|comm| comm.trim() == "systemd")
}

/// `SIGRTMIN + n` to pid 1: 20 turns its status output on, 21 back to its setting.
fn signal_init(n: libc::c_int) -> bool {
    // SAFETY: a signal to pid 1, which systemd handles.
    let rv = unsafe { libc::kill(1, libc::SIGRTMIN() + n) };
    if rv < 0 {
        warn!("details: telling pid 1: {}", io::Error::last_os_error());
    }
    rv == 0
}
