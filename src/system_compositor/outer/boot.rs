// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The splash in the outer (`docs/fork/boot-splash.md` §2): plymouthd's socket, answered from the
//! outer's loop, and the splash standing in for the plate — at boot, from the first modeset until a
//! session's screen replaces it, and at shutdown, from `PrepareForShutdown` until the power goes.

use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};
use synoik_splash::handoff;
use synoik_splash::keyboard::{self, Entry, Keyboard};
use synoik_splash::layout::Dialog;

use super::{Outer, Shown};
use crate::system_compositor::plymouth::{self, Event, Server};
use crate::system_compositor::splash;
use crate::utils::get_monotonic_time;

/// How soon to try plymouthd's name again while plymouthd still holds it: it frees it a moment
/// after acknowledging `ExecStartPost=`'s quit (§2.1 step 4).
const BIND_RETRY: Duration = Duration::from_millis(20);

/// The socket side: always there, so the shutdown's `change-mode` reaches the outer too.
pub(super) struct Plymouth {
    pub server: Server,
    retry: Option<RegistrationToken>,
    connections: std::collections::HashMap<u64, RegistrationToken>,
    /// Open, armed and watched while a password prompt is up (§4.2).
    keyboard: Option<(Keyboard, RegistrationToken)>,
    /// It would not open for this prompt; not tried again until the next.
    keyboard_failed: bool,
    entry: Entry,
    /// Inside the keyboard's own callback, whose watch goes by returning `Remove`; set once it
    /// should.
    in_keyboard: Option<bool>,
}

impl Plymouth {
    pub fn new() -> Self {
        Self {
            // plymouthd answers root only (`ply-boot-server.c:265-285`).
            server: Server::new(0),
            retry: None,
            connections: std::collections::HashMap::new(),
            keyboard: None,
            keyboard_failed: false,
            entry: Entry::default(),
            in_keyboard: None,
        }
    }
}

/// The splash while it stands in for the plate, at boot or at shutdown.
pub(super) struct BootSplash {
    /// When the spinner's turn began, on the monotonic clock.
    start: Duration,
    /// The frame a frozen spinner (`H`) keeps.
    frozen: Option<usize>,
    /// Carried on from another drawer's splash, its spinner's clock with it.
    resumed: bool,
    /// Bumped with every change to what the splash shows, so a display knows its frame is stale.
    pub revision: u64,
    timer: Option<RegistrationToken>,
}

/// A connection's fd, for its watch. The stream itself stays with the server, which hands it
/// back only once it is over (`Server::take_closing`); the watch is removed before it is dropped.
struct ConnectionFd(RawFd);

impl AsFd for ConnectionFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the watch is removed before the stream owning the fd is dropped.
        unsafe { BorrowedFd::borrow_raw(self.0) }
    }
}

impl Outer {
    /// Put the splash up in place of the plate: a fresh start with something to draw it with
    /// (a restarted outer adopts the session frame on screen instead), or the machine going down.
    ///
    /// Shown from the first modeset: the frame `plymouth deactivate` leaves on screen has the
    /// spinner in it, so a first frame without one would blink it off (§2.1).
    ///
    /// With `resume`, the splash is the one the initramfs splash had up, its clients' requests
    /// and its spinner's phase carried on (§7.2).
    pub(super) fn start_boot_splash(&mut self, resume: Option<handoff::State>) {
        let (start, frozen, resumed) = match &resume {
            Some(state) => (state.spinner_start, state.frozen_frame, true),
            None => (get_monotonic_time(), None, false),
        };
        self.boot = Some(BootSplash {
            start,
            frozen,
            resumed,
            revision: 0,
            timer: None,
        });
        match resume {
            Some(state) => {
                info!("carrying on the initramfs splash");
                self.plymouth.server.restore(state.splash);
            }
            None => self.plymouth.server.show(),
        }
        // Nothing is on screen yet to redraw: the first modeset draws what this says.
        self.plymouth.server.take_events();
    }

    /// Start the spinner turning, once the first modeset is on screen.
    pub(super) fn begin_spinner(&mut self) {
        let Some(boot) = &mut self.boot else { return };
        if !boot.resumed {
            boot.start = get_monotonic_time();
        }
        // Not the compositor clock of `Synoik::timer_after`: the outer holds no `Synoik`.
        #[allow(clippy::disallowed_methods)]
        let timer = Timer::from_duration(splash::SPINNER_FRAME_INTERVAL);
        let token = self
            .handle
            .insert_source(timer, |_, _, outer: &mut Outer| {
                if outer.boot.is_none() {
                    return TimeoutAction::Drop;
                }
                outer.redraw_splash();
                TimeoutAction::ToDuration(splash::SPINNER_FRAME_INTERVAL)
            })
            .expect("inserting a timer");
        if let Some(boot) = &mut self.boot {
            boot.timer = Some(token);
        }
    }

    /// The spinner frame to draw now.
    pub(super) fn spinner_frame(&mut self) -> usize {
        let Some(boot) = &self.boot else { return 0 };
        if let Some(frame) = boot.frozen {
            return frame;
        }
        let elapsed = get_monotonic_time().saturating_sub(boot.start);
        let frames = self
            .compositor
            .as_mut()
            .map_or(1, |c| c.spinner_frames().max(1));
        splash::spinner_frame(elapsed, frames)
    }

    /// The splash's revision and spinner frame as the screen should show them. A splash not
    /// shown has no spinner, nor has one asking for a password, so its frame stays put: a
    /// turning one would redraw the same picture fifteen times a second.
    pub(super) fn splash_state(&mut self) -> (u64, usize) {
        let revision = self.boot.as_ref().map_or(0, |b| b.revision);
        let splash = self.plymouth.server.splash();
        let frame = if splash.shown && splash.password_prompt.is_none() {
            self.spinner_frame()
        } else {
            0
        };
        (revision, frame)
    }

    /// Redraw the splash on every display that shows it, is idle and is out of date.
    pub(super) fn redraw_splash(&mut self) {
        if self.boot.is_none() {
            return;
        }
        let state = self.splash_state();
        let stale: Vec<String> = self
            .displays
            .iter()
            .filter(|(_, d)| {
                d.in_flight.is_none()
                    && d.queued.is_none()
                    && d.transition.is_none()
                    && !d.switch_pending
                    && !d.relayout_pending
                    && matches!(d.on_screen, Shown::Splash { revision, frame, .. }
                        if (revision, frame) != state)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for d in stale {
            self.flip(&d, super::Next::Plate);
        }
    }

    /// A session's screen replaced the splash: it is over. The socket stays.
    pub(super) fn end_boot_splash(&mut self) {
        let Some(boot) = self.boot.take() else { return };
        if let Some(token) = boot.timer {
            self.handle.remove(token);
        }
        info!("the splash is over");
        // A later restart adopts the session's frame, not this splash.
        if let Err(err) = handoff::remove(handoff::FD_SPLASH) {
            debug!("removing the splash from the fd store: {err}");
        }
        self.plymouth.server.end();
        self.on_plymouth();
    }

    /// Start answering on plymouthd's name: on `inherited`, the listener a previous outer or the
    /// initramfs splash left in the fd store, or else bound here, retrying while plymouthd still
    /// holds the name.
    pub(super) fn bind_plymouth(&mut self, inherited: Option<UnixListener>) {
        if let Some(listener) = inherited {
            match self.plymouth.server.adopt(listener) {
                Ok(()) => return self.answer_plymouth(),
                Err(err) => warn!("not adopting plymouth's listener: {err}"),
            }
        }
        match self.plymouth.server.try_bind(plymouth::SOCKET_NAME) {
            Ok(()) => {
                self.plymouth.retry = None;
                // Kept across a restart, so the name is never free for anyone else to take.
                if let Some(fd) = self.plymouth.server.listener_fd() {
                    if std::env::var_os("NOTIFY_SOCKET").is_some() {
                        if let Err(err) = handoff::store(handoff::FD_PLYMOUTH, fd) {
                            warn!("could not put plymouth's listener in the fd store: {err}");
                        }
                    }
                }
                self.answer_plymouth();
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                #[allow(clippy::disallowed_methods)]
                let timer = Timer::from_duration(BIND_RETRY);
                let token = self
                    .handle
                    .insert_source(timer, |_, _, outer: &mut Outer| {
                        outer.bind_plymouth(None);
                        TimeoutAction::Drop
                    })
                    .expect("inserting a timer");
                self.plymouth.retry = Some(token);
            }
            Err(err) => warn!("not answering plymouth's clients: {err}"),
        }
    }

    fn answer_plymouth(&mut self) {
        self.watch_plymouth_listener();
        // Only now: the pid file is what tells systemd's agent a splash is listening.
        self.plymouth
            .server
            .set_pid_file(PathBuf::from(plymouth::PID_FILE));
        info!("answering plymouth's clients");
    }

    fn watch_plymouth_listener(&mut self) {
        let Some(fd) = self.plymouth.server.listener_fd() else {
            return;
        };
        // The listener lives as long as the server, which lives as long as the outer.
        let fd = ConnectionFd(std::os::fd::AsRawFd::as_raw_fd(&fd));
        self.handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                |_, _, outer: &mut Outer| {
                    for id in outer.plymouth.server.accept() {
                        outer.watch_plymouth_connection(id);
                    }
                    outer.on_plymouth();
                    Ok(PostAction::Continue)
                },
            )
            .expect("inserting the plymouth listener");
    }

    fn watch_plymouth_connection(&mut self, id: u64) {
        let Some(fd) = self.plymouth.server.connection_fd(id) else {
            return;
        };
        let fd = ConnectionFd(std::os::fd::AsRawFd::as_raw_fd(&fd));
        let token = self
            .handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, outer: &mut Outer| {
                    outer.plymouth.server.on_readable(id);
                    // Its own watch goes by returning `Remove`, not through the handle.
                    let over = outer.plymouth.server.is_closing(id);
                    outer.on_plymouth_except(Some(id));
                    Ok(if over {
                        PostAction::Remove
                    } else {
                        PostAction::Continue
                    })
                },
            )
            .expect("inserting a plymouth connection");
        self.plymouth.connections.insert(id, token);
    }

    /// Act on what the server's clients changed.
    pub(super) fn on_plymouth(&mut self) {
        self.on_plymouth_except(None);
    }

    /// [`Self::on_plymouth`] from connection `current`'s own callback, whose watch goes by
    /// returning `Remove` instead of through the handle.
    fn on_plymouth_except(&mut self, current: Option<u64>) {
        loop {
            self.follow_prompt();
            let events = self.plymouth.server.take_events();
            if events.is_empty() {
                break;
            }
            for event in events {
                match event {
                    Event::Changed => self.on_splash_changed(),
                    Event::Quit { retain } => {
                        info!("plymouth quit (retain: {retain})");
                        // The retained frame is what the splash draws once it is not shown, and
                        // there is no text console to give back (§2.4).
                        self.plymouth.server.finish_quit();
                    }
                }
            }
        }
        self.close_plymouth_connections(current);
    }

    /// Read the keyboard while a password prompt is up, and only then. Without a keyboard the
    /// dialog still shows: NO_ANSWER would leave the asking unit waiting unseen (§3.2).
    fn follow_prompt(&mut self) {
        let prompting = self.plymouth.server.splash().password_prompt.is_some();
        if prompting && self.plymouth.keyboard.is_none() && !self.plymouth.keyboard_failed {
            let mut keyboard = match keyboard::open_system() {
                Ok(keyboard) => keyboard,
                Err(err) => {
                    warn!("no keyboard for the password: {err}");
                    self.plymouth.keyboard_failed = true;
                    return;
                }
            };
            keyboard.arm();
            // The keyboard lives in `self.plymouth` until the watch is removed.
            let fd = ConnectionFd(std::os::fd::AsRawFd::as_raw_fd(&keyboard.fd()));
            let token = self
                .handle
                .insert_source(
                    Generic::new(fd, Interest::READ, Mode::Level),
                    |_, _, outer: &mut Outer| {
                        outer.plymouth.in_keyboard = Some(false);
                        outer.on_keyboard();
                        Ok(match outer.plymouth.in_keyboard.take() {
                            Some(true) => PostAction::Remove,
                            _ => PostAction::Continue,
                        })
                    },
                )
                .expect("inserting the keyboard");
            self.plymouth.keyboard = Some((keyboard, token));
            self.dialog_changed();
        } else if !prompting {
            self.plymouth.keyboard_failed = false;
            if let Some((keyboard, token)) = self.plymouth.keyboard.take() {
                match &mut self.plymouth.in_keyboard {
                    Some(remove) => *remove = true,
                    None => self.handle.remove(token),
                }
                // After the watch is gone, as a connection's stream (`close_plymouth_connections`).
                self.handle.insert_idle(move |_| drop::<Keyboard>(keyboard));
                self.plymouth.entry.clear();
                self.dialog_changed();
            }
        }
    }

    /// What the keyboard says about the password being typed.
    pub(super) fn splash_dialog(&self) -> Dialog {
        let keyboard = self.plymouth.keyboard.as_ref().map(|(k, _)| k);
        Dialog {
            bullets: self.plymouth.entry.bullets(),
            caps_lock: keyboard.is_some_and(Keyboard::caps_lock),
            keymap: keyboard.and_then(Keyboard::layout_name),
        }
    }

    fn on_keyboard(&mut self) {
        let Some((keyboard, _)) = &mut self.plymouth.keyboard else {
            return;
        };
        let (keys, mut changed) = keyboard.dispatch();
        for key in keys {
            changed = true;
            if let Some(password) = self.plymouth.entry.apply(key) {
                self.plymouth.server.answer_password(Some(password));
            }
        }
        if changed {
            self.dialog_changed();
        }
        self.on_plymouth();
    }

    fn dialog_changed(&mut self) {
        if let Some(boot) = &mut self.boot {
            boot.revision += 1;
        }
        self.redraw_splash();
    }

    fn on_splash_changed(&mut self) {
        let frozen = self.plymouth.server.splash().frozen;
        let Some(was_frozen) = self.boot.as_ref().map(|b| b.frozen.is_some()) else {
            return;
        };
        let frame = self.spinner_frame();
        let Some(boot) = &mut self.boot else { return };
        boot.revision += 1;
        match (frozen, was_frozen) {
            (true, false) => boot.frozen = Some(frame),
            // `$` after `H` starts the spinner over (`ply_throbber_start`).
            (false, true) => {
                boot.frozen = None;
                boot.start = get_monotonic_time();
            }
            _ => (),
        }
        self.redraw_splash();
    }

    fn close_plymouth_connections(&mut self, current: Option<u64>) {
        for (id, stream) in self.plymouth.server.take_closing() {
            if let Some(token) = self.plymouth.connections.remove(&id) {
                if Some(id) != current {
                    self.handle.remove(token);
                }
            }
            // After the watch is gone: dropped once this dispatch is over, when even the current
            // connection's `Remove` has been applied.
            self.handle.insert_idle(move |_| drop::<UnixStream>(stream));
        }
    }
}
