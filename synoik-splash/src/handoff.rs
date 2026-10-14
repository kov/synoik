// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! What crosses from the initramfs splash to the outer in `synoik-system.service`'s fd store
//! (`docs/fork/boot-splash.md` §7.2): the card, plymouthd's listener, and the splash as it stood.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd as _, OwnedFd};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::plymouth::Splash;

/// The card: the open file that is DRM master.
pub const FD_DRM: &str = "drm";
/// plymouthd's listening socket, bound.
pub const FD_PLYMOUTH: &str = "plymouth";
/// A memfd holding a [`State`].
pub const FD_SPLASH: &str = "splash";

/// The splash on screen when its drawer stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub splash: Splash,
    /// When the spinner's turn began, on the monotonic clock, which both processes share.
    pub spinner_start: Duration,
    /// The frame a frozen spinner keeps.
    pub frozen_frame: Option<usize>,
}

impl State {
    /// `self` in a sealed-size memfd, for the fd store.
    pub fn to_memfd(&self) -> io::Result<OwnedFd> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        // SAFETY: a static name; the flags are memfd_create's.
        let raw = unsafe { libc::memfd_create(c"synoik-splash-state".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just created, and ours.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut file = File::from(fd);
        file.write_all(&bytes)?;
        Ok(OwnedFd::from(file))
    }

    /// The state in a memfd from [`Self::to_memfd`].
    pub fn from_fd(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let mut file = File::from(fd.try_clone_to_owned()?);
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }
}

/// Put `fd` in the unit's fd store as `name`, replacing what was there under that name.
pub fn store(name: &str, fd: BorrowedFd<'_>) -> io::Result<()> {
    remove(name)?;
    sd_notify::notify_with_fds(
        &[
            sd_notify::NotifyState::FdStore,
            sd_notify::NotifyState::FdName(name),
        ],
        &[fd],
    )
}

/// Take `name` out of the unit's fd store.
pub fn remove(name: &str) -> io::Result<()> {
    sd_notify::notify(&[
        sd_notify::NotifyState::FdStoreRemove,
        sd_notify::NotifyState::FdName(name),
    ])
}

/// Wait until systemd has processed every notification sent before: one about to exit must not
/// leave its last `FDSTORE=1` racing its own death (`sd_notify_barrier(3)`).
pub fn barrier(timeout: Duration) -> io::Result<()> {
    let mut fds = [0; 2];
    // SAFETY: two ints for the pipe's ends.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created, and ours.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    sd_notify::notify_with_fds(
        &[sd_notify::NotifyState::Custom("BARRIER=1")],
        &[write.as_fd()],
    )?;
    drop(write);
    // systemd closes its copy of the write end once it has got this far: the read end hangs up.
    let mut pfd = libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(&read),
        events: 0,
        revents: 0,
    };
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one pollfd, ours.
    match unsafe { libc::poll(&raw mut pfd, 1, ms) } {
        n if n < 0 => Err(io::Error::last_os_error()),
        0 => Err(io::ErrorKind::TimedOut.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plymouth::Mode;

    #[test]
    fn the_state_survives_a_memfd() {
        let state = State {
            splash: Splash {
                mode: Mode::Reboot,
                shown: true,
                frozen: true,
                messages: vec!["one".into(), "two\nlines".into()],
                progress: Some(42),
                progress_paused: true,
                password_prompt: None,
            },
            spinner_start: Duration::from_nanos(1_234_567_891),
            frozen_frame: Some(7),
        };
        let fd = state.to_memfd().unwrap();
        assert_eq!(State::from_fd(fd.as_fd()).unwrap(), state);
        // Read twice: the outer may read it again after a crash restart.
        assert_eq!(State::from_fd(fd.as_fd()).unwrap(), state);
    }
}
