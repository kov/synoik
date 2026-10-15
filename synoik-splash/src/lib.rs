// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The boot splash both splash drawers share (`docs/fork/boot-splash.md`): plymouthd's protocol
//! and the splash's layout. The outer draws it on the GPU; `synoik-splash`, the initramfs program,
//! on the CPU (§7).

#[macro_use]
extern crate tracing;

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd};

pub mod handoff;
pub mod layout;
pub mod paint;
pub mod plymouth;

/// A unix socket peer's credentials.
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
}

pub fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let rv = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rv < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
    })
}
