// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A session's VT, on a seat0 that has them (`docs/fork/greeter.md` §2.4).
//!
//! logind requires every seat0 session on a seat with VTs to sit on one, and makes a session active
//! by switching to its VT; so, as GDM does, the greeter takes the initial VT and every other
//! session a free one, named to `pam_systemd` through `XDG_VTNR` and `PAM_TTY`, and the worker
//! switches to it as the session's program starts (`set_up_for_new_vt`, `jump_to_vt`,
//! `gdm-session-worker.c:1101-1165`, `2271-2333`). The system compositor holds the display either
//! way: a VT here is logind's bookkeeping and the switch it follows, never a framebuffer.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;

use anyhow::Context as _;

use crate::pam::Pam;

/// GDM's `GDM_INITIAL_VT`: the greeter's.
const INITIAL_VT: i32 = 1;

const VT_OPENQRY: libc::c_ulong = 0x5600;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

/// A VT this session holds open for as long as it runs.
pub struct Vt {
    number: i32,
    tty: OwnedFd,
}

/// Whether seat0 has VTs, as `sd_seat_can_tty` reads it.
pub fn seat0_has_vts() -> bool {
    std::fs::read_to_string("/run/systemd/seats/seat0")
        .is_ok_and(|state| state.lines().any(|line| line == "CAN_TTY=1"))
}

fn open_tty(number: i32) -> anyhow::Result<OwnedFd> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(format!("/dev/tty{number}"))
        .with_context(|| format!("opening /dev/tty{number}"))?;
    Ok(file.into())
}

/// Put the session on a VT before it opens, when seat0 has them: the initial one for the greeter,
/// else the first free one. `None` on a seat without VTs.
pub fn prepare(pam: &mut Pam, greeter: bool) -> anyhow::Result<Option<Vt>> {
    if !seat0_has_vts() {
        return Ok(None);
    }
    let initial = open_tty(INITIAL_VT)?;
    let vt = if greeter {
        Vt {
            number: INITIAL_VT,
            tty: initial,
        }
    } else {
        // Holding the initial VT open while asking keeps it out of the answer.
        let mut number: libc::c_int = 0;
        // SAFETY: VT_OPENQRY writes one int.
        if unsafe { libc::ioctl(initial.as_raw_fd(), VT_OPENQRY, &mut number) } < 0 || number <= 0 {
            anyhow::bail!("no free VT: {}", std::io::Error::last_os_error());
        }
        Vt {
            number,
            tty: open_tty(number)?,
        }
    };
    pam.putenv("XDG_VTNR", &vt.number.to_string())?;
    pam.set_tty(&format!("/dev/tty{}", vt.number))?;
    Ok(Some(vt))
}

impl Vt {
    /// Switch to this VT, as the session's program starts: logind makes the session on it the
    /// active one, and the system compositor follows.
    pub fn activate(&self) {
        // A blank VT with no cursor, should anything ever show it.
        let _ = std::fs::File::from(self.tty.try_clone().expect("dup of a tty"))
            .write_all(b"\x1b[H\x1b[2J\x1b[?25l");
        let mut state = VtStat::default();
        // SAFETY: VT_GETSTATE fills a vt_stat.
        let active = (unsafe { libc::ioctl(self.tty.as_raw_fd(), VT_GETSTATE, &mut state) } == 0)
            .then_some(i32::from(state.v_active));
        if active == Some(self.number) {
            return;
        }
        // SAFETY: plain ioctls on our own tty.
        unsafe {
            if libc::ioctl(self.tty.as_raw_fd(), VT_ACTIVATE, self.number) < 0 {
                eprintln!(
                    "synoik-login: switching to VT {}: {}",
                    self.number,
                    std::io::Error::last_os_error()
                );
            } else if libc::ioctl(self.tty.as_raw_fd(), VT_WAITACTIVE, self.number) < 0 {
                eprintln!(
                    "synoik-login: waiting for VT {}: {}",
                    self.number,
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    /// The session's stdin, and its controlling terminal (`gdm-session-worker.c:2104-2136`).
    /// Called in the forked child, after `setsid`.
    pub fn attach_as_stdin(&self) {
        // SAFETY: the child's own descriptors.
        unsafe {
            libc::dup2(self.tty.as_raw_fd(), 0);
            libc::ioctl(0, libc::TIOCSCTTY, 0);
        }
    }
}
