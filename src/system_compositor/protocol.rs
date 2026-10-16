// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The private wire between the system compositor (the *outer*) and a nested session compositor
//! (the *inner*), `docs/fork/system-compositor.md` §0.
//!
//! Both ends are built from this tree and versioned in lockstep, so this ships our own structs
//! rather than a public protocol: one JSON message per `SOCK_SEQPACKET` packet, with the file
//! descriptors a message carries riding the same packet as `SCM_RIGHTS`. A packet boundary is a
//! message boundary, so there is no framing to get wrong, and an fd can never be paired with the
//! wrong message.
//!
//! The version is checked on `Hello`, and a mismatch is refused rather than negotiated down: the
//! two ends ship together, so a mismatch means a half-finished upgrade, and the honest response to
//! that is a clear error.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};

/// Bumped on any change to the messages below.
pub const VERSION: u32 = 5;

/// Where the outer listens unless told otherwise. The unit's `RuntimeDirectory=` creates it.
pub const DEFAULT_SOCKET: &str = "/run/synoik-system/outer.sock";

/// The variable that makes a session compositor start nested: the path of the outer's socket.
pub const SOCKET_ENV: &str = "SYNOIK_SYSTEM_SOCKET";

/// A packet larger than this is a bug on the sending side; EDIDs are the largest thing we carry.
const MAX_PACKET: usize = 64 * 1024;
/// The most descriptors any message carries.
const MAX_FDS: usize = 4;

/// Inner → outer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToOuter {
    /// The first message on a connection, and the only one before `Welcome`.
    ///
    /// `session` is the logind session id the inner runs in. It is a claim, not a credential: the
    /// outer checks it against the peer's credentials before it fronts anything.
    Hello {
        version: u32,
        session: Option<String>,
    },
    /// The displays this session lights, each at its mode; every other connector is off for it.
    /// The session decides its display configuration and the outer executes it (§3), for the
    /// session on screen: sent before the first `Present`, after every `Connectors`, and whenever
    /// the session's own configuration changes — always before a `Present` drawn for it.
    ///
    /// `active` is false while the session blanks its displays (idle, `PowerSaveMode`): the outer
    /// turns them off and lights them again when it goes back to true.
    Configure {
        outputs: Vec<OutputConfig>,
        active: bool,
    },
    /// Register a scanout buffer. Carries its dmabuf as the one fd.
    ///
    /// Buffers are registered once and presented by id, so the outer imports each one exactly
    /// once and a present is a flip, not an import.
    AddBuffer {
        id: u32,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
    },
    /// Forget a buffer. The outer keeps scanning it out if it is on screen, until it is replaced.
    RemoveBuffer { id: u32 },
    /// Show `buffer` on `output`. When `fenced`, the one fd is a `sync_file` that signals when the
    /// inner's rendering into it is done; the flip must not happen before that.
    ///
    /// `cursor` is where the cursor plane's image goes with this frame, as `CursorPosition`'s
    /// `position`: a frame and its cursor change in one commit, so the pointer neither lags its
    /// frame nor holds it back a vblank.
    Present {
        output: String,
        buffer: u32,
        fenced: bool,
        cursor: Option<(i32, i32)>,
    },
    /// Keep `buffer` as this session's frame for `output` without showing it, answered `Held`: the
    /// frame the session is to come back to, while the one on screen stays there for the switch
    /// away to start from (Switch User leaves the desktop on screen and its lock screen held). It
    /// stays in flight until the outer shows it (`Presented`) or gives it back (`Discarded`); a
    /// `Present` of the same buffer shows it now. The fd is as `Present`'s.
    Hold {
        output: String,
        buffer: u32,
        fenced: bool,
    },
    /// The pointer's image on `output`, for the outer's cursor plane (§3): `width`×`height` — the
    /// connector's `cursor_size` — premultiplied ARGB8888, rows of `width * 4` bytes, in the one
    /// fd (a memfd). `hotspot` is where in it the pointer points. Sent when the image changes, and
    /// only kept: the next `Present` or `CursorPosition` for `output` puts it on screen, with its
    /// position, so a new image never shows at the old one's place.
    CursorImage {
        output: String,
        width: u32,
        height: u32,
        hotspot: (i32, i32),
    },
    /// Where the cursor image's top-left corner sits on `output`, in its mode's pixels; `None`
    /// takes the cursor off the display. For a pointer that moved with no frame to carry it (a
    /// frame carries its own, `Present`): the outer commits it alone. The outer shows a session's
    /// cursor over that session's frames only.
    CursorPosition {
        output: String,
        position: Option<(i32, i32)>,
    },
}

/// Outer → inner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToInner {
    /// The answer to `Hello`.
    Welcome {
        version: u32,
        /// The render node of the device the outer scans out from, as `(major, minor)`. The inner
        /// must allocate its scanout buffers there, or the outer cannot import them.
        render_node: Option<(u64, u64)>,
        connectors: Vec<ConnectorInfo>,
        /// How switches look (`SYNOIK_SYSTEM_TRANSITION`), for what the session brings up for one
        /// (the end-session dialog) to move the same way.
        transition: super::Style,
    },
    /// The connected displays changed: one was plugged or unplugged, or its mode list or identity
    /// changed (a resized VM window is a new preferred mode). The whole set, as in `Welcome`.
    Connectors { connectors: Vec<ConnectorInfo> },
    /// A `Present` reached the screen.
    ///
    /// `time_ns` is the flip's `CLOCK_MONOTONIC` timestamp from the kernel, passed through
    /// untouched (§3.1): the inner paces off real hardware time, never a clock the outer made up.
    /// Zero means the driver gave none.
    Presented {
        output: String,
        buffer: u32,
        sequence: u32,
        time_ns: u64,
        refresh_ns: u64,
    },
    /// A `Present` that will never reach the screen (the flip was refused). The inner owes a
    /// fresh frame, and `buffer` is free again.
    Discarded { output: String, buffer: u32 },
    /// The outer keeps the `Hold`'s `buffer` as the session's frame for `output`. Still in
    /// flight: `Presented` or `Discarded` comes when it is shown or given back.
    Held { output: String, buffer: u32 },
}

/// One connected display, as the inner needs it to name and configure the output the way the tty
/// backend would: the connector name is `monitors.xml`'s identity key (§3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorInfo {
    pub connector: String,
    pub edid: Option<Vec<u8>>,
    pub physical_size_mm: (u32, u32),
    /// The size of its cursor plane's buffer, when its CRTC has one: the session then sends its
    /// pointer as `CursorImage`/`CursorPosition` instead of drawing it into its frames.
    pub cursor_size: Option<(u32, u32)>,
    /// What it advertises, interlaced modes left out (they do not work, as on the tty).
    pub modes: Vec<ModeInfo>,
}

/// A display a session lights, and its mode — one of the connector's `modes`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputConfig {
    pub connector: String,
    pub mode: ModeInfo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModeInfo {
    pub width: u16,
    pub height: u16,
    pub refresh_mhz: u32,
    pub preferred: bool,
}

impl ToOuter {
    /// How many descriptors this message carries. Checked on both ends, so a sender that forgets
    /// one and a receiver that loses one fail the same way.
    pub fn fd_count(&self) -> usize {
        match self {
            ToOuter::AddBuffer { .. } => 1,
            ToOuter::Present { fenced: true, .. }
            | ToOuter::Hold { fenced: true, .. }
            | ToOuter::CursorImage { .. } => 1,
            ToOuter::Hello { .. }
            | ToOuter::Configure { .. }
            | ToOuter::RemoveBuffer { .. }
            | ToOuter::Present { .. }
            | ToOuter::Hold { .. }
            | ToOuter::CursorPosition { .. } => 0,
        }
    }
}

impl ToInner {
    pub fn fd_count(&self) -> usize {
        0
    }
}

/// What one `recv` produced.
#[derive(Debug)]
pub enum Received<T> {
    Message(T, Vec<OwnedFd>),
    /// Nothing queued; the socket is non-blocking.
    WouldBlock,
    /// The peer is gone.
    Closed,
}

/// Send `msg` with `fds` as one packet.
pub fn send<T: Serialize>(sock: BorrowedFd<'_>, msg: &T, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    assert!(
        fds.len() <= MAX_FDS,
        "a message carries at most {MAX_FDS} fds"
    );
    let bytes = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if bytes.len() > MAX_PACKET {
        return Err(io::Error::other(format!(
            "message of {} bytes exceeds the {MAX_PACKET}-byte packet limit",
            bytes.len()
        )));
    }

    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * size_of::<RawFd>()) as u32) } as usize;
    let mut cmsg = vec![0u8; space];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if !fds.is_empty() {
        hdr.msg_control = cmsg.as_mut_ptr().cast();
        hdr.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * size_of::<RawFd>()) as u32) as _;
            let data = libc::CMSG_DATA(c).cast::<RawFd>();
            for (i, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(data.add(i), fd.as_raw_fd());
            }
        }
    }

    loop {
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Receive one packet. Descriptors come back `CLOEXEC`.
pub fn recv<T: for<'de> Deserialize<'de>>(sock: BorrowedFd<'_>) -> io::Result<Received<T>> {
    let mut buf = vec![0u8; MAX_PACKET];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) } as usize;
    let mut cmsg = vec![0u8; space];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg.as_mut_ptr().cast();
    hdr.msg_controllen = space as _;

    let n = loop {
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(Received::WouldBlock),
            io::ErrorKind::ConnectionReset => return Ok(Received::Closed),
            _ => return Err(err),
        }
    };

    // Take ownership of every descriptor before anything can fail, so an error path cannot leak
    // one into this process for good.
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }

    if n == 0 && fds.is_empty() {
        return Ok(Received::Closed);
    }
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::other(
            "a packet or its descriptors were truncated",
        ));
    }

    let msg = serde_json::from_slice(&buf[..n]).map_err(io::Error::other)?;
    Ok(Received::Message(msg, fds))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixDatagram;

    use super::*;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        let rv = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rv, 0, "socketpair: {}", io::Error::last_os_error());
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_message_and_its_fd_arrive_together() {
        let (a, b) = pair();
        let (payload, _keep) = UnixDatagram::pair().unwrap();
        let msg = ToOuter::Present {
            output: "Virtual-1".into(),
            buffer: 3,
            fenced: true,
            cursor: Some((-3, 12)),
        };
        send(a.as_fd(), &msg, &[payload.as_fd()]).unwrap();

        match recv::<ToOuter>(b.as_fd()).unwrap() {
            Received::Message(got, fds) => {
                assert_eq!(got, msg);
                assert_eq!(fds.len(), msg.fd_count());
            }
            other => panic!("expected a message, got {other:?}"),
        }
        assert!(matches!(
            recv::<ToOuter>(b.as_fd()).unwrap(),
            Received::WouldBlock
        ));
    }

    #[test]
    fn each_packet_is_one_message() {
        let (a, b) = pair();
        for id in 0..3 {
            send(a.as_fd(), &ToOuter::RemoveBuffer { id }, &[]).unwrap();
        }
        for id in 0..3 {
            match recv::<ToOuter>(b.as_fd()).unwrap() {
                Received::Message(ToOuter::RemoveBuffer { id: got }, fds) => {
                    assert_eq!(got, id);
                    assert!(fds.is_empty());
                }
                other => panic!("expected RemoveBuffer {id}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_cursor_image_carries_its_pixels_and_a_move_none() {
        let image = ToOuter::CursorImage {
            output: "Virtual-1".into(),
            width: 64,
            height: 64,
            hotspot: (3, 2),
        };
        let position = ToOuter::CursorPosition {
            output: "Virtual-1".into(),
            position: Some((-3, 700)),
        };
        assert_eq!(image.fd_count(), 1);
        assert_eq!(position.fd_count(), 0);

        let (a, b) = pair();
        let (payload, _keep) = UnixDatagram::pair().unwrap();
        send(a.as_fd(), &image, &[payload.as_fd()]).unwrap();
        send(a.as_fd(), &position, &[]).unwrap();
        for want in [image, position] {
            match recv::<ToOuter>(b.as_fd()).unwrap() {
                Received::Message(got, fds) => {
                    assert_eq!(fds.len(), want.fd_count());
                    assert_eq!(got, want);
                }
                other => panic!("expected {want:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_closed_peer_reads_as_closed() {
        let (a, b) = pair();
        drop(a);
        assert!(matches!(
            recv::<ToInner>(b.as_fd()).unwrap(),
            Received::Closed
        ));
    }
}
