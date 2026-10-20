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

use super::{End, Motion, Outer, Picture, Shown, Target, Transition, Way};
use crate::render_helpers::vulkan::VkTexture;
use crate::system_compositor::plymouth::{self, Event, Server, Splash};
use crate::system_compositor::splash;
use crate::utils::get_monotonic_time;

/// How soon to try plymouthd's name again while plymouthd still holds it: it frees it a moment
/// after acknowledging `ExecStartPost=`'s quit (§2.1 step 4).
const BIND_RETRY: Duration = Duration::from_millis(20);

/// How long the update bar takes to fill the rest of the way when an update ends short of
/// 100 % (§4.3). Ours: plymouth cuts to the next mode wherever the bar stands.
const FILL: Duration = Duration::from_millis(150);

/// How long a mode change may keep its client waiting for the screen to show it (§4.3): the
/// fill and the fade, with room to spare. The reboot after an update waits on that client.
const MODE_CHANGE_CAP: Duration = Duration::from_secs(1);

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
    /// Armed while a held `C` waits for the screen (`MODE_CHANGE_CAP`); once it fires, the
    /// change is answered whatever the screen shows.
    mode_change_cap: Option<RegistrationToken>,
    mode_change_overdue: bool,
}

impl Plymouth {
    pub fn new() -> Self {
        // plymouthd answers root only (`ply-boot-server.c:265-285`).
        let mut server = Server::new(0);
        server.hold_mode_changes();
        Self {
            server,
            retry: None,
            connections: std::collections::HashMap::new(),
            keyboard: None,
            keyboard_failed: false,
            entry: Entry::default(),
            in_keyboard: None,
            mode_change_cap: None,
            mode_change_overdue: false,
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
    /// The splash as of the last change, for the screen a mode change leaves.
    last: Splash,
}

/// The screen a mode change leaves, as `from` of the crossfade into the new one (§4.3). Drawn
/// live, so its spinner keeps turning, and leaving an update its bar first fills to 100 %.
pub(super) struct Leaving {
    /// The splash as it stood before the change.
    splash: Splash,
    /// Where the bar fills from: leaving an update for a mode without a bar.
    fill_from: Option<u8>,
    /// Drawn into in place, and the progress and spinner frame it shows.
    texture: Option<(VkTexture, (u8, usize))>,
}

impl Leaving {
    /// How long the old screen stays before the crossfade starts: the bar's fill, if any.
    pub fn lead(&self) -> Duration {
        match self.fill_from {
            Some(from) if from < 100 => FILL,
            _ => Duration::ZERO,
        }
    }
}

/// The bar's percentage `elapsed` into a fill from `from` lasting `lead`: eased out like the
/// crossfade that follows, so the bar slows into 100 %.
fn fill_progress(from: u8, elapsed: Duration, lead: Duration) -> u8 {
    if lead.is_zero() {
        return 100;
    }
    let t = elapsed.as_secs_f32() / lead.as_secs_f32();
    let eased = Motion::FADE.ease(t);
    (f32::from(from) + f32::from(100 - from.min(100)) * eased).round() as u8
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
            last: Splash::default(),
        });
        match resume {
            Some(state) => {
                info!("carrying on the initramfs splash");
                self.plymouth.server.restore(state.splash);
            }
            None => self.plymouth.server.show(),
        }
        let splash = self.plymouth.server.splash().clone();
        if let Some(boot) = &mut self.boot {
            boot.last = splash;
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
    /// shown has no spinner, nor has one asking for a password or one in an update mode (its
    /// bar takes the spinner's place), so its frame stays put: a turning one would redraw the
    /// same picture fifteen times a second.
    pub(super) fn splash_state(&mut self) -> (u64, usize) {
        let revision = self.boot.as_ref().map_or(0, |b| b.revision);
        let splash = self.plymouth.server.splash();
        let frame = if splash.shown
            && splash.password_prompt.is_none()
            && splash::titles(splash.mode).is_none()
        {
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
                !d.busy()
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

    /// Keep the listener in the fd store across a restart, so the name is never free for anyone
    /// else to take.
    fn store_plymouth(&self) {
        let Some(fd) = self.plymouth.server.listener_fd() else {
            return;
        };
        if std::env::var_os("NOTIFY_SOCKET").is_some() {
            if let Err(err) = handoff::store(handoff::FD_PLYMOUTH, fd) {
                warn!("could not put plymouth's listener in the fd store: {err}");
            }
        }
    }

    /// Start answering on plymouthd's name: on `inherited`, the listener a previous outer left in
    /// the fd store or the initramfs splash handed over, or else bound here, retrying while
    /// plymouthd still holds the name.
    pub(super) fn bind_plymouth(&mut self, inherited: Option<UnixListener>) {
        if let Some(listener) = inherited {
            match self.plymouth.server.adopt(listener) {
                Ok(()) => {
                    // From the initramfs splash's hand, not the fd store: into it, so a restart
                    // keeps the name too.
                    self.store_plymouth();
                    return self.answer_plymouth();
                }
                Err(err) => warn!("not adopting plymouth's listener: {err}"),
            }
        }
        match self.plymouth.server.try_bind(plymouth::SOCKET_NAME) {
            Ok(()) => {
                self.plymouth.retry = None;
                self.store_plymouth();
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
            let mut events = self.plymouth.server.take_events();
            if events.is_empty() {
                // Only now: a change's fade starts as its event is handled.
                self.release_mode_changes();
                events = self.plymouth.server.take_events();
                if events.is_empty() {
                    break;
                }
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
            if let Some((mut keyboard, token)) = self.plymouth.keyboard.take() {
                // Now, not with the drop: a keyboard opened for the next prompt within this
                // dispatch must find the VT as it was.
                keyboard.disarm();
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
        let splash = self.plymouth.server.splash().clone();
        let frozen = splash.frozen;
        let Some(was_frozen) = self.boot.as_ref().map(|b| b.frozen.is_some()) else {
            return;
        };
        let frame = self.spinner_frame();
        let Some(boot) = &mut self.boot else { return };
        boot.revision += 1;
        let old = std::mem::replace(&mut boot.last, splash.clone());
        // Into an update or out of one the screen changes; between boot, shutdown and reboot, or
        // between two update modes but for the title, it hardly does.
        if old.shown && splash.shown && old.mode != splash.mode {
            let leaving_update = splash::titles(old.mode).is_some();
            if leaving_update != splash::titles(splash.mode).is_some() {
                self.change_screen(old, leaving_update);
            }
        }
        let Some(boot) = &mut self.boot else { return };
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

    /// Whether a display is fading from a mode change.
    fn mode_change_showing(&self) -> bool {
        self.displays
            .values()
            .any(|d| d.transition.as_ref().is_some_and(|t| t.leaving.is_some()))
    }

    /// Answer a held `C` once no display is still showing a mode change, or once it has waited
    /// `MODE_CHANGE_CAP`. Answering lets its client go on, and it may be what the reboot waits
    /// for (`plymouth-reboot.service`, PackageKit).
    fn release_mode_changes(&mut self) {
        let waiting = self.plymouth.server.mode_change_waiting();
        if waiting && !self.plymouth.mode_change_overdue && self.mode_change_showing() {
            if self.plymouth.mode_change_cap.is_none() {
                // Not the compositor clock of `Synoik::timer_after`: the outer holds no `Synoik`.
                #[allow(clippy::disallowed_methods)]
                let timer = Timer::from_duration(MODE_CHANGE_CAP.mul_f32(self.slow));
                let token = self
                    .handle
                    .insert_source(timer, |_, _, outer: &mut Outer| {
                        outer.plymouth.mode_change_cap = None;
                        warn!("a mode change waited too long for the screen; answering it");
                        outer.plymouth.mode_change_overdue = true;
                        outer.on_plymouth();
                        TimeoutAction::Drop
                    })
                    .expect("inserting a timer");
                self.plymouth.mode_change_cap = Some(token);
            }
            return;
        }
        if waiting {
            self.plymouth.server.finish_mode_change();
        }
        self.plymouth.mode_change_overdue = false;
        if let Some(token) = self.plymouth.mode_change_cap.take() {
            self.handle.remove(token);
        }
    }

    /// A fade from a mode change on some display ended: answer the change if it was the last,
    /// from the loop rather than from inside whatever ended it.
    pub(super) fn mode_change_shown(&mut self) {
        self.handle.insert_idle(|outer| outer.on_plymouth());
    }

    /// The splash changed mode from `old`: crossfade every display showing it into the new
    /// mode, and leaving an update fill its bar to 100 % first (§4.3). A display already fading
    /// shows the change as it is: its splash end is drawn live.
    fn change_screen(&mut self, old: Splash, leaving_update: bool) {
        if self.compositor.is_none() {
            return;
        }
        debug!(
            target: super::FLIGHT,
            "the splash changes mode: {:?} to {:?}",
            old.mode,
            self.plymouth.server.splash().mode
        );
        let fill_from = leaving_update.then(|| old.progress.unwrap_or(0));
        // The update screen moves as the greeter does, the front picture over the boot screen:
        // in, it arrives; out, it leaves. Over black, the splash's own background, not the
        // plate.
        let way = if leaving_update {
            Way::Leaves
        } else {
            Way::Arrives
        };
        let motion = Motion {
            backdrop: [0; 3],
            ..Motion::new(self.style, way)
        };
        let names: Vec<String> = self.displays.keys().cloned().collect();
        for d in names {
            let idle = self
                .displays
                .get(&d)
                .is_some_and(|d| d.transition.is_none() && !d.switch_pending);
            if !idle || !self.plate_showing(&d) {
                continue;
            }
            let leaving = Leaving {
                splash: old.clone(),
                fill_from,
                texture: None,
            };
            let to = self.base_picture(&d);
            let Some(display) = self.displays.get_mut(&d) else {
                continue;
            };
            display.transition = Some(Transition {
                // Drawn before the first frame (`refresh_live_pictures`).
                from: to.clone(),
                motion,
                to: None,
                pending: None,
                pending_watch: None,
                start: get_monotonic_time(),
                wait: None,
                from_live: None,
                from_session: None,
                from_frame: None,
                from_pending: None,
                from_splash: false,
                leaving: Some(leaving),
            });
            self.set_target(
                &d,
                Target {
                    picture: to,
                    end: End::Plate,
                },
            );
        }
    }

    /// Draw the screen a mode change leaves on `d` as it stands `elapsed` into the change, into
    /// its fade's `from`.
    pub(super) fn draw_leaving(&mut self, d: &str, elapsed: Duration) {
        let frame = self.spinner_frame();
        let dialog = self.splash_dialog();
        let panel = self.splash_panel();
        let slow = self.slow;
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        let size = display.size();
        let Some(transition) = display.transition.as_mut() else {
            return;
        };
        let Some(leaving) = transition.leaving.as_mut() else {
            return;
        };
        let mut splash = leaving.splash.clone();
        if let Some(from) = leaving.fill_from {
            splash.progress = Some(fill_progress(from, elapsed, leaving.lead().mul_f32(slow)));
        }
        // An update's bar has the spinner's place.
        let frame = if splash::titles(splash.mode).is_none() {
            frame
        } else {
            0
        };
        let state = (splash.progress.unwrap_or(0), frame);
        let reuse = match leaving.texture.take() {
            Some((texture, drawn)) if drawn == state => {
                leaving.texture = Some((texture, drawn));
                return;
            }
            Some((texture, _)) => Some(texture),
            None => None,
        };
        let Some(compositor) = self.compositor.as_mut() else {
            return;
        };
        match compositor.splash_picture(size, &splash, 1., panel, frame, &dialog, reuse) {
            Ok(texture) => {
                transition.from = Picture::Texture(texture.clone());
                leaving.texture = Some((texture, state));
            }
            Err(err) => warn!("drawing the splash a mode change leaves on {d}: {err:#}"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_fills_from_where_it_stood_to_full() {
        let at = |ms| fill_progress(94, Duration::from_millis(ms), FILL);
        assert_eq!(at(0), 94);
        assert_eq!(at(FILL.as_millis() as u64), 100);
        assert_eq!(at(10_000), 100);
        let mut last = 94;
        for ms in (0..=FILL.as_millis() as u64).step_by(10) {
            let p = at(ms);
            assert!(p >= last, "{p} after {last}");
            last = p;
        }
        // Eased out: most of the way early.
        assert!(fill_progress(0, FILL / 4, FILL) > 50);
        // Nothing to fill.
        assert_eq!(fill_progress(100, Duration::ZERO, FILL), 100);
        assert_eq!(fill_progress(42, Duration::ZERO, Duration::ZERO), 100);
    }
}
