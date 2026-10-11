// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The outer's chord matcher: its own evdev fds, read for one chord and nothing else (§2, §2.1).
//!
//! The outer opens every event node itself (the `input` group) rather than taking them from
//! logind, so logind's revocation of a background session's devices never reaches these fds and
//! the chord works against a hung session. Nothing is grabbed: every event still reaches the
//! fronted session through its own fds.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Context as _;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

const INPUT_DIR: &str = "/dev/input";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_DROPPED: u16 = 3;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
/// `KEY_F1`..`KEY_F10` are contiguous; F11 and F12 sit elsewhere.
const KEY_F1: u16 = 59;
const KEY_F10: u16 = 68;
const KEY_F11: u16 = 87;
const KEY_F12: u16 = 88;

/// Whether the outer should match chords itself: only where nothing else turns Ctrl+Alt+F<n>
/// into a session switch. With VTs the session compositor does (it asks logind for VT n), and
/// matching here too would act on every chord twice. `SYNOIK_SYSTEM_CHORDS=1`/`0` overrides the
/// detection.
pub(super) fn wanted() -> bool {
    match std::env::var("SYNOIK_SYSTEM_CHORDS").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => !Path::new("/sys/class/tty/tty0").exists(),
    }
}

/// Start matching. `on_chord` is called with `n` for Ctrl+Alt+F<n>.
pub(super) fn watch<D: 'static>(
    handle: &LoopHandle<'static, D>,
    on_chord: fn(&mut D, u32),
) -> anyhow::Result<()> {
    let devices = Rc::new(RefCell::new(Devices {
        handle: handle.clone(),
        open: HashMap::new(),
        on_chord,
    }));

    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error()).context("inotify_init1");
    }
    // SAFETY: just created.
    let inotify = unsafe { OwnedFd::from_raw_fd(raw) };
    let dir = CString::new(INPUT_DIR).expect("no NUL");
    // `IN_ATTRIB` too: udev creates the node first and opens its permissions up after.
    let mask = libc::IN_CREATE | libc::IN_ATTRIB;
    if unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), dir.as_ptr(), mask) } < 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("watching {INPUT_DIR}"));
    }
    let watched = devices.clone();
    handle
        .insert_source(
            Generic::new(inotify, Interest::READ, Mode::Level),
            move |_, inotify, _| {
                for name in read_inotify_names(inotify.as_fd()) {
                    Devices::open(&watched, &Path::new(INPUT_DIR).join(name));
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("inserting the input watch: {err}"))?;

    for entry in std::fs::read_dir(INPUT_DIR).with_context(|| format!("listing {INPUT_DIR}"))? {
        Devices::open(&devices, &entry?.path());
    }
    info!(
        "matching Ctrl+Alt+F<n> on {} input devices",
        devices.borrow().open.len()
    );
    Ok(())
}

struct Devices<D: 'static> {
    handle: LoopHandle<'static, D>,
    open: HashMap<PathBuf, Device>,
    on_chord: fn(&mut D, u32),
}

struct Device {
    /// Keys this device reports held, so a release on another keyboard does not drop them.
    held: HashSet<u16>,
}

impl<D: 'static> Devices<D> {
    fn open(this: &Rc<RefCell<Self>>, path: &Path) {
        let is_event = path
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(b"event"));
        if !is_event || this.borrow().open.contains_key(path) {
            return;
        }
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let raw = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            // Not ours yet (udev has not opened it up), or already gone; `IN_ATTRIB` retries.
            debug!("{}: {}", path.display(), io::Error::last_os_error());
            return;
        }
        // SAFETY: just opened.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let devices = this.clone();
        let key = path.to_owned();
        let token = this.borrow().handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, fd, data: &mut D| {
                let (chord, gone) = Self::read(&devices, &key, fd.as_fd());
                if let Some(n) = chord {
                    let on_chord = devices.borrow().on_chord;
                    on_chord(data, n);
                }
                if gone {
                    debug!("{} went away", key.display());
                    devices.borrow_mut().open.remove(&key);
                    return Ok(PostAction::Remove);
                }
                Ok(PostAction::Continue)
            },
        );
        match token {
            Ok(_) => {
                debug!("watching {} for chords", path.display());
                this.borrow_mut().open.insert(
                    path.to_owned(),
                    Device {
                        held: HashSet::new(),
                    },
                );
            }
            Err(err) => warn!("watching {}: {err}", path.display()),
        }
    }

    /// Drain a device. Returns the chord it completed, if any, and whether the device is gone.
    fn read(this: &Rc<RefCell<Self>>, key: &Path, fd: BorrowedFd<'_>) -> (Option<u32>, bool) {
        let mut chord = None;
        let mut events = [libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: 0,
            code: 0,
            value: 0,
        }; 64];
        let size = std::mem::size_of::<libc::input_event>();
        loop {
            let n = unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    events.as_mut_ptr().cast(),
                    size * events.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                let gone = err.raw_os_error() != Some(libc::EAGAIN);
                return (chord, gone);
            }
            if n == 0 {
                return (chord, true);
            }
            let mut this = this.borrow_mut();
            for event in &events[..n as usize / size] {
                match (event.type_, event.code, event.value) {
                    // Events were lost, releases among them: forget what this device held rather
                    // than leave a modifier stuck down.
                    (EV_SYN, SYN_DROPPED, _) => {
                        if let Some(device) = this.open.get_mut(key) {
                            device.held.clear();
                        }
                    }
                    (EV_KEY, code, 1) => {
                        if let Some(f) = function_key(code) {
                            let held = |a, b| {
                                this.open
                                    .values()
                                    .any(|d| d.held.contains(&a) || d.held.contains(&b))
                            };
                            if held(KEY_LEFTCTRL, KEY_RIGHTCTRL) && held(KEY_LEFTALT, KEY_RIGHTALT)
                            {
                                chord = Some(f);
                            }
                        }
                        if let Some(device) = this.open.get_mut(key) {
                            device.held.insert(code);
                        }
                    }
                    (EV_KEY, code, 0) => {
                        if let Some(device) = this.open.get_mut(key) {
                            device.held.remove(&code);
                        }
                    }
                    _ => (),
                }
            }
        }
    }
}

/// `n` for `KEY_F<n>`.
fn function_key(code: u16) -> Option<u32> {
    match code {
        KEY_F1..=KEY_F10 => Some(u32::from(code - KEY_F1) + 1),
        KEY_F11 => Some(11),
        KEY_F12 => Some(12),
        _ => None,
    }
}

/// The names in the inotify events waiting on `fd`.
fn read_inotify_names(fd: BorrowedFd<'_>) -> Vec<std::ffi::OsString> {
    let mut names = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return names;
        }
        let mut at = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while at + header <= n as usize {
            // SAFETY: the kernel writes whole events; the header fits per the check above.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            let name = &buf[at + header..at + header + event.len as usize];
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            if !name.is_empty() {
                names.push(std::ffi::OsStr::from_bytes(name).to_owned());
            }
            at += header + event.len as usize;
        }
    }
}
