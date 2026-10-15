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

/// Where a splash with no systemd in its initramfs (initramfs-tools) waits for the real root's
/// `synoik-splash --hand-over`, an abstract name: it asks for the fd store's contents over it
/// (`docs/fork/boot-splash.md` §7.6).
pub const HANDOVER_SOCKET: &str = "synoik-splash-handover";

/// The most descriptors one handover carries: the card, the listener and the splash.
const MAX_FDS: usize = 3;

/// The uid of the process at the other end of `socket`.
pub fn peer_uid(socket: BorrowedFd<'_>) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a ucred of ours, its size beside it.
    let r = unsafe {
        libc::getsockopt(
            std::os::fd::AsRawFd::as_raw_fd(&socket),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Send `fds` with their names over `socket`, in one message: the names, newline-separated, as
/// its bytes, the descriptors as its `SCM_RIGHTS`.
pub fn send_fds(socket: BorrowedFd<'_>, fds: &[(&str, BorrowedFd<'_>)]) -> io::Result<()> {
    assert!(fds.len() <= MAX_FDS && !fds.is_empty());
    let names = fds
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join("\n");
    let raw: Vec<i32> = fds
        .iter()
        .map(|(_, fd)| std::os::fd::AsRawFd::as_raw_fd(fd))
        .collect();
    let mut iov = libc::iovec {
        iov_base: names.as_ptr().cast_mut().cast(),
        iov_len: names.len(),
    };
    let payload = std::mem::size_of_val(raw.as_slice()) as u32;
    // SAFETY: CMSG_SPACE is arithmetic.
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(payload) } as usize];
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    // SAFETY: `control` has room for one header and `raw`, as CMSG_SPACE sized it.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(payload) as _;
        std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(cmsg).cast(), raw.len());
    }
    // SAFETY: the msghdr built above.
    let sent = unsafe {
        libc::sendmsg(
            std::os::fd::AsRawFd::as_raw_fd(&socket),
            &raw const msg,
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The descriptors and names [`send_fds`] sent over `socket`.
pub fn receive_fds(socket: BorrowedFd<'_>) -> io::Result<Vec<(String, OwnedFd)>> {
    let mut bytes = [0u8; 256];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let payload = (MAX_FDS * std::mem::size_of::<i32>()) as u32;
    // SAFETY: CMSG_SPACE is arithmetic.
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(payload) } as usize];
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    // SAFETY: the msghdr built above.
    let n = unsafe {
        libc::recvmsg(
            std::os::fd::AsRawFd::as_raw_fd(&socket),
            &raw mut msg,
            libc::MSG_CMSG_CLOEXEC,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control buffer recvmsg filled, as the CMSG macros do.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<i32>();
                for i in 0..len / std::mem::size_of::<i32>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("more descriptors than a handover carries"));
    }
    let names = std::str::from_utf8(&bytes[..n as usize]).map_err(io::Error::other)?;
    let names: Vec<&str> = names.split('\n').collect();
    if names.len() != fds.len() {
        return Err(io::Error::other(format!(
            "{} names for {} descriptors",
            names.len(),
            fds.len()
        )));
    }
    Ok(names.into_iter().map(String::from).zip(fds).collect())
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

    /// A handover's descriptors arrive in order, under their names, as the same open files.
    #[test]
    fn descriptors_cross_with_their_names() {
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(a.as_fd()).unwrap(), unsafe { libc::getuid() });
        let memfd = State {
            splash: Splash::default(),
            spinner_start: Duration::from_secs(3),
            frozen_frame: None,
        }
        .to_memfd()
        .unwrap();
        let (pipe_r, pipe_w) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        send_fds(
            a.as_fd(),
            &[(FD_PLYMOUTH, pipe_w.as_fd()), (FD_SPLASH, memfd.as_fd())],
        )
        .unwrap();
        let got = receive_fds(b.as_fd()).unwrap();
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [FD_PLYMOUTH, FD_SPLASH]);
        // The same pipe: what goes in through the received end comes out of ours.
        let mut w = File::from(got[0].1.try_clone().unwrap());
        w.write_all(b"x").unwrap();
        drop(w);
        drop(got);
        drop(pipe_w);
        let mut out = String::new();
        File::from(pipe_r).read_to_string(&mut out).unwrap();
        assert_eq!(out, "x");
    }
}
