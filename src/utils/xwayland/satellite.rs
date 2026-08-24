// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use calloop::channel::Sender;
use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction, RegistrationToken};
use smithay::reexports::rustix::io::{fcntl_setfd, FdFlags};
use smithay::reexports::wayland_server::Client;
use smithay::wayland::selection::data_device::set_selection_excluded_client;

use crate::synoik::State;
use crate::utils::xwayland::{selection, X11Connection};
use crate::utils::{expand_home, get_credentials_for_client};

pub struct Satellite {
    x11: X11Connection,
    abstract_token: Option<RegistrationToken>,
    unix_token: Option<RegistrationToken>,
    to_main: Sender<ToMain>,
}

enum ToMain {
    SetupWatch,
}

impl Satellite {
    pub fn display_name(&self) -> &str {
        &self.x11.display_name
    }
}

/// Bar satellite from taking part in selections.
///
/// The compositor bridges X11 selections itself (see [`crate::utils::xwayland::selection`]); a
/// satellite that also bridged them would re-export every selection back the way it came, and the
/// second export cancels the first source.
///
/// Matching on the pid is safe *because* the spawn happens on the main thread: satellite cannot
/// reach the Wayland socket until we return to the event loop, so its pid is always recorded
/// before its client exists. Handing it a socket we made and `WAYLAND_SOCKET` would name the
/// client outright, but satellite spawns Xwayland before it connects (`src/lib.rs:96` vs
/// `src/server/mod.rs:499`) and Xwayland would inherit the variable and steal the connection --
/// two clients on one socket, which hangs the display.
pub fn exclude_from_selections_if_satellite(state: &mut State, client: &Client) {
    let Some(pid) = state.synoik.satellite_pid else {
        return;
    };
    let dh = state.synoik.display_handle.clone();
    if get_credentials_for_client(&dh, client).map(|c| c.pid) != Some(pid) {
        return;
    }
    debug!("barring xwayland-satellite (pid {pid}) from selections");
    set_selection_excluded_client(&state.synoik.seat, Some(client.clone()));
}

pub fn setup(state: &mut State) {
    if state.synoik.satellite.is_some() {
        return;
    }

    let config = state.synoik.config.borrow();
    let xwls_config = &config.xwayland_satellite;
    if xwls_config.off {
        return;
    }

    if !test_ondemand(&xwls_config.path) {
        return;
    }
    drop(config);

    let x11 = match super::setup_connection() {
        Ok(x11) => x11,
        Err(err) => {
            warn!("error opening X11 sockets, disabling xwayland-satellite integration: {err:?}");
            return;
        }
    };

    let event_loop = &state.synoik.event_loop;
    let (to_main, rx) = calloop::channel::channel();
    event_loop
        .insert_source(rx, move |event, _, state| match event {
            calloop::channel::Event::Msg(msg) => match msg {
                ToMain::SetupWatch => setup_watch(state),
            },
            calloop::channel::Event::Closed => (),
        })
        .unwrap();

    state.synoik.satellite = Some(Satellite {
        x11,
        abstract_token: None,
        unix_token: None,
        to_main,
    });

    setup_watch(state);
}

fn test_ondemand(path: &str) -> bool {
    let _span = tracy_client::span!("satellite::test_ondemand");

    // Expand `~` at the start.
    let mut path = Path::new(path);
    let expanded = expand_home(path);
    match &expanded {
        Ok(Some(expanded)) => path = expanded.as_ref(),
        Ok(None) => (),
        Err(err) => {
            warn!("error expanding ~: {err:?}");
        }
    }

    let mut process = Command::new(path);
    process
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("error spawning xwayland-satellite at {path:?}, disabling integration: {err}");
            return false;
        }
    };

    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            warn!("error waiting for xwayland-satellite, disabling integration: {err}");
            return false;
        }
    };

    if !status.success() {
        warn!("xwayland-satellite doesn't support on-demand activation, disabling integration");
        return false;
    }

    true
}

// When xwayland-satellite fails to start and accept a connection on the socket, the socket will
// keep triggering our event source, even after the X11 client quits, resulting in a busyloop of
// trying to start xwayland-satellite. This function will clear out (accept and drop) all pending
// connections on the socket before registering a new event source, working around this problem.
// When the problem happens, it's very likely that xwayland-satellite won't be able to accept the
// pending client (since it had just failed to do so), so it's fine to drop the connections.
fn clear_out_pending_connections(fd: OwnedFd) -> OwnedFd {
    let listener = UnixListener::from(fd);

    if let Err(err) = listener.set_nonblocking(true) {
        warn!("error setting X11 socket to nonblocking: {err:?}");
        return OwnedFd::from(listener);
    }

    while listener.accept().is_ok() {}

    if let Err(err) = listener.set_nonblocking(false) {
        warn!("error setting X11 socket to blocking: {err:?}");
    }

    OwnedFd::from(listener)
}

fn setup_watch(state: &mut State) {
    if state.synoik.satellite.is_none() {
        return;
    }

    // We only get here with no satellite running -- at startup, or after one died. Neither the
    // bridge (its display went with it) nor the selection bar (its client did) outlives that.
    stop_selection_bridge(state);
    set_selection_excluded_client(&state.synoik.seat, None);
    state.synoik.satellite_pid = None;

    let satellite = state.synoik.satellite.as_mut().unwrap();

    let event_loop = &state.synoik.event_loop;

    if let Some(token) = satellite.abstract_token.take() {
        error!("abstract_token must be None in setup_watch()");
        event_loop.remove(token);
    }
    if let Some(token) = satellite.unix_token.take() {
        error!("unix_token must be None in setup_watch()");
        event_loop.remove(token);
    }

    if let Some(fd) = &satellite.x11.abstract_fd {
        let fd = fd.try_clone().unwrap();
        let fd = clear_out_pending_connections(fd);
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(source, move |_, _, state| {
                if let Some(satellite) = &mut state.synoik.satellite {
                    // Remove the other source.
                    if let Some(token) = satellite.unix_token.take() {
                        state.synoik.event_loop.remove(token);
                    }
                    // Clear this source.
                    satellite.abstract_token = None;

                    debug!("connection to X11 abstract socket; spawning xwayland-satellite");
                }
                spawn(state);
                Ok(PostAction::Remove)
            })
            .unwrap();
        satellite.abstract_token = Some(token);
    }

    let fd = satellite.x11.unix_fd.try_clone().unwrap();
    let fd = clear_out_pending_connections(fd);
    let source = Generic::new(fd, Interest::READ, Mode::Level);
    let token = event_loop
        .insert_source(source, move |_, _, state| {
            if let Some(satellite) = &mut state.synoik.satellite {
                // Remove the other source.
                if let Some(token) = satellite.abstract_token.take() {
                    state.synoik.event_loop.remove(token);
                }
                // Clear this source.
                satellite.unix_token = None;

                debug!("connection to X11 unix socket; spawning xwayland-satellite");
            }
            spawn(state);
            Ok(PostAction::Remove)
        })
        .unwrap();
    satellite.unix_token = Some(token);
}

/// Start xwayland-satellite, and the compositor's own X11 selection bridge alongside it.
fn spawn(state: &mut State) {
    let _span = tracy_client::span!("satellite::spawn");

    let Some(xwl) = &state.synoik.satellite else {
        return;
    };

    let abstract_fd = xwl
        .x11
        .abstract_fd
        .as_ref()
        .map(|fd| fd.try_clone().unwrap());
    let unix_fd = xwl.x11.unix_fd.try_clone().unwrap();
    let to_main = xwl.to_main.clone();
    let display_name = xwl.x11.display_name.clone();

    // Expand `~` at the start.
    let mut path = PathBuf::from(state.synoik.config.borrow().xwayland_satellite.path.clone());
    let expanded = expand_home(&path);
    match expanded {
        Ok(Some(expanded)) => path = expanded,
        Ok(None) => (),
        Err(err) => {
            warn!("error expanding ~: {err:?}");
        }
    }

    let mut process = Command::new(&path);
    process.arg(&display_name).env_remove("DISPLAY");

    // We don't want it spamming the synoik output.
    process
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");
    process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe { process.pre_exec(crate::utils::signals::unblock_all) };

    start_selection_bridge(state, display_name);

    // Fork here rather than on the waiter thread, even though it costs a few milliseconds of the
    // frame this lands in: we are inside the event loop, so no client can be inserted until we
    // return, which makes the pid we record below strictly earlier than satellite's connection.
    // That is what `exclude_from_selections_if_satellite` relies on.
    let child = match spawn_child(&path, process, abstract_fd, unix_fd) {
        Some(child) => child,
        None => {
            let _ = to_main.send(ToMain::SetupWatch);
            return;
        }
    };
    state.synoik.satellite_pid = Some(child.id() as i32);

    // Reaping is the part that takes a whole session, so that does go on a thread.
    let res = thread::Builder::new()
        .name("Xwl-s Reaper".to_owned())
        .spawn(move || {
            wait_for_child(child);

            // Once xwayland-satellite crashes or fails to spawn, re-establish our X11 socket watch
            // to try again next time.
            let _ = to_main.send(ToMain::SetupWatch);
        });

    if let Err(err) = res {
        warn!("error spawning a thread to reap xwayland-satellite: {err:?}");
    }
}

/// Tear down the X11 selection bridge, if there is one.
///
/// Dropping the [`Bridge`] closes the channel its thread is watching, which is how the thread
/// learns to exit; the event source has to come out by hand or every satellite restart leaks one.
fn stop_selection_bridge(state: &mut State) {
    let Some(bridge) = state.synoik.x_selection.take() else {
        return;
    };
    if let Some(token) = bridge.token {
        state.synoik.event_loop.remove(token);
    }
}

/// Bring up the compositor-side X11 selection bridge for `display_name`.
///
/// Started here rather than at compositor startup on purpose: opening an X connection is what
/// *triggers* satellite's on-demand spawn, so dialling the display before there is a satellite
/// would defeat the whole point of the lazy start.
fn start_selection_bridge(state: &mut State, display_name: String) {
    stop_selection_bridge(state);

    let (bridge, channel) = match selection::start(display_name) {
        Ok(pair) => pair,
        Err(err) => {
            warn!("error starting the X11 selection bridge: {err}");
            return;
        }
    };

    let res = state
        .synoik
        .event_loop
        .insert_source(channel, move |event, _, state| match event {
            calloop::channel::Event::Msg(msg) => state.on_x_selection_event(msg),
            // The X thread only ends when its connection does, which means the display we were
            // bridging is gone. Anything still routed there would hang a paste.
            calloop::channel::Event::Closed => stop_selection_bridge(state),
        });
    let token = match res {
        Ok(token) => token,
        Err(err) => {
            warn!("error hooking up the X11 selection bridge: {err}");
            return;
        }
    };

    let mut bridge = bridge;
    bridge.token = Some(token);
    state.synoik.x_selection = Some(bridge);
}

fn spawn_child(
    path: &Path,
    mut process: Command,
    abstract_fd: Option<OwnedFd>,
    unix_fd: OwnedFd,
) -> Option<std::process::Child> {
    let abstract_raw = abstract_fd.as_ref().map(|fd| fd.as_raw_fd());
    let unix_raw = unix_fd.as_raw_fd();

    process.arg("-listenfd").arg(unix_raw.to_string());

    if let Some(abstract_raw) = abstract_raw {
        process.arg("-listenfd").arg(abstract_raw.to_string());
    }

    unsafe {
        process.pre_exec(move || {
            // We're about to exec xwl-s; perfect time to clear CLOEXEC on the file descriptors
            // that we want to pass it.

            // We're not dropping these until after spawn().
            let unix_fd = BorrowedFd::borrow_raw(unix_raw);
            fcntl_setfd(unix_fd, FdFlags::empty())?;

            if let Some(abstract_raw) = abstract_raw {
                let abstract_fd = BorrowedFd::borrow_raw(abstract_raw);
                fcntl_setfd(abstract_fd, FdFlags::empty())?;
            }

            Ok(())
        })
    };

    let child = {
        let _span = tracy_client::span!();
        match process.spawn() {
            Ok(child) => child,
            Err(err) => {
                warn!("error spawning {path:?}: {err:?}");
                return None;
            }
        }
    };

    // The process spawned, we can drop our fds.
    drop(abstract_fd);
    drop(unix_fd);

    Some(child)
}

fn wait_for_child(mut child: std::process::Child) {
    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            warn!("error waiting for xwayland-satellite: {err:?}");
            return;
        }
    };

    // This is most likely a crash, hence warn!().
    warn!("xwayland-satellite exited with: {status}");
}
