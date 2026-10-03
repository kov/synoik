// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Idle inhibition, GNOME's way: the compositor is a *client* of it, never its owner.
//!
//! A Wayland `zwp_idle_inhibitor_v1` on an unobscured surface becomes an
//! `org.freedesktop.ScreenSaver.Inhibit(<compositor>, "idle-inhibit")` call, dropped again with
//! `UnInhibit` when the surface is hidden or the inhibitor goes away
//! (mutter `src/wayland/meta-wayland-idle-inhibit.c:130-197`). The name is served by
//! gsd-screensaver-proxy, which forwards to `org.gnome.SessionManager.Inhibit(.., IDLE)` and
//! releases every cookie of a sender that drops off the bus — so a compositor that dies holding
//! inhibits leaves none behind. D-Bus clients (a video player calling the fdo interface itself)
//! reach gsd-screensaver-proxy directly; we are not on that path at all.
//!
//! What the session ends up inhibiting comes back as gnome-session's `InhibitedActions`, which
//! [`watch_session_inhibited`] relays so ext-idle-notify honours every source, not just ours.

use std::collections::HashMap;

use futures_util::StreamExt;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

const SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";
const SCREENSAVER_PATH: &str = "/org/freedesktop/ScreenSaver";

const SESSION_NAME: &str = "org.gnome.SessionManager";
const SESSION_PATH: &str = "/org/gnome/SessionManager";

/// `GSM_INHIBITOR_FLAG_IDLE` (gnome-session `gsm-inhibitor-flag.h`).
const INHIBIT_IDLE: u32 = 1 << 3;

/// The surfaces currently holding an idle inhibit on the bus.
///
/// Each held surface owns one task that takes the inhibit and gives it back when the surface's
/// release handle is dropped. Every hold has its own cookie, so a surface that is hidden and shown
/// again while a call is still in flight just runs a second, independent hold: the first one's
/// `UnInhibit` names the first cookie and cannot touch the second. (Mutter serializes per surface
/// with an `INHIBITING`/`UNINHIBITING` state machine for the same reason — and wedges for good when
/// a call fails. Here a failed `Inhibit` only means that hold has nothing to give back.)
#[derive(Default)]
pub struct IdleInhibitForwarder {
    conn: Option<zbus::Connection>,
    held: HashMap<WlSurface, Option<async_channel::Sender<()>>>,
}

impl IdleInhibitForwarder {
    /// Start forwarding over `conn`. Before this, holds are tracked but nothing is called, which is
    /// what an instance without a session bus (the test fixture) wants.
    pub fn connect(&mut self, conn: zbus::Connection) {
        self.conn = Some(conn);
    }

    pub fn is_held(&self, surface: &WlSurface) -> bool {
        self.held.contains_key(surface)
    }

    pub fn is_any_held(&self) -> bool {
        !self.held.is_empty()
    }

    /// Make the held set exactly `wanted`: take an inhibit for each newcomer, give back the rest.
    /// Called every refresh, so it only touches the bus on a change.
    pub fn sync<'a>(&mut self, wanted: impl IntoIterator<Item = &'a WlSurface> + Clone) {
        // Dropping a release handle closes its channel, which is the hold task's cue to give back.
        self.held
            .retain(|held, _| wanted.clone().into_iter().any(|w| w == held));

        for surface in wanted {
            if !self.held.contains_key(surface) {
                let release = self.conn.as_ref().map(hold);
                self.held.insert(surface.clone(), release);
            }
        }
    }
}

/// Take one inhibit, hold it until the returned sender is dropped, then give it back.
fn hold(conn: &zbus::Connection) -> async_channel::Sender<()> {
    let (release, released) = async_channel::bounded::<()>(1);

    let task_conn = conn.clone();
    let future = async move {
        let dest = Some(SCREENSAVER_NAME);
        let iface = Some(SCREENSAVER_NAME);
        // Mutter's arguments, with our name for its own: the inhibit is the compositor's, so the
        // session's inhibitor list names the compositor, not the client that asked.
        let reply = task_conn
            .call_method(
                dest,
                SCREENSAVER_PATH,
                iface,
                "Inhibit",
                &("synoik", "idle-inhibit"),
            )
            .await;
        let cookie = match reply.and_then(|reply| reply.body().deserialize::<u32>()) {
            Ok(cookie) => cookie,
            Err(err) => {
                warn!("error taking an idle inhibit from {SCREENSAVER_NAME}: {err:?}");
                return;
            }
        };

        // Nothing is ever sent; this returns when the release handle is dropped.
        let _ = released.recv().await;

        if let Err(err) = task_conn
            .call_method(dest, SCREENSAVER_PATH, iface, "UnInhibit", &(cookie,))
            .await
        {
            warn!("error releasing idle inhibit {cookie} on {SCREENSAVER_NAME}: {err:?}");
        }
    };
    conn.executor()
        .spawn(future, "hold an idle inhibit")
        .detach();

    release
}

pub enum SessionInhibitToSynoik {
    /// Whether gnome-session currently has the idle action inhibited, by anyone.
    IdleInhibited(bool),
}

/// Open the connection idle inhibits are forwarded over, and relay gnome-session's
/// `InhibitedActions` idle bit to the compositor.
///
/// The connection must live as long as the compositor: gsd-screensaver-proxy releases a sender's
/// inhibits when its unique name vanishes, and this connection is that sender.
pub fn start(
    to_niri: calloop::channel::Sender<SessionInhibitToSynoik>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;

    let async_conn = conn.inner().clone();
    let future = async move {
        let session =
            match zbus::Proxy::new(&async_conn, SESSION_NAME, SESSION_PATH, SESSION_NAME).await {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating the gnome-session manager proxy: {err:?}");
                    return;
                }
            };

        // The stream yields the current value first, then every change; gnome-session publishes
        // `InhibitedActions` through its skeleton setter, which emits `PropertiesChanged`.
        let mut changes = session
            .receive_property_changed::<u32>("InhibitedActions")
            .await;
        while let Some(change) = changes.next().await {
            let actions = match change.get().await {
                Ok(actions) => actions,
                Err(err) => {
                    warn!("error reading gnome-session InhibitedActions: {err:?}");
                    continue;
                }
            };
            let idle = actions & INHIBIT_IDLE != 0;
            if to_niri
                .send(SessionInhibitToSynoik::IdleInhibited(idle))
                .is_err()
            {
                break;
            }
        }
    };

    conn.inner()
        .executor()
        .spawn(future, "monitor gnome-session inhibited actions")
        .detach();

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;

    /// gsd-screensaver-proxy's side of the conversation, recording what it was asked.
    struct FakeScreenSaver {
        calls: Arc<Mutex<Vec<String>>>,
        refuse: bool,
    }

    #[zbus::interface(name = "org.freedesktop.ScreenSaver")]
    impl FakeScreenSaver {
        fn inhibit(&self, application_name: &str, reason: &str) -> zbus::fdo::Result<u32> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(format!("Inhibit({application_name}, {reason})"));
            if self.refuse {
                return Err(zbus::fdo::Error::Failed("refused".to_owned()));
            }
            Ok(42)
        }

        fn un_inhibit(&self, cookie: u32) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("UnInhibit({cookie})"));
        }
    }

    /// A peer-to-peer connection to a fake ScreenSaver: (our end, its end, its call log).
    fn connect(
        refuse: bool,
    ) -> (
        zbus::blocking::Connection,
        zbus::blocking::Connection,
        Arc<Mutex<Vec<String>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeScreenSaver {
            calls: calls.clone(),
            refuse,
        };
        let (ours, theirs) = UnixStream::pair().unwrap();
        // The server's handshake waits for the client's, so one of them has to run elsewhere.
        let server = std::thread::spawn(move || {
            zbus::blocking::connection::Builder::unix_stream(theirs)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .serve_at(SCREENSAVER_PATH, fake)
                .unwrap()
                .build()
                .unwrap()
        });
        let client = zbus::blocking::connection::Builder::unix_stream(ours)
            .p2p()
            .build()
            .unwrap();
        (client, server.join().unwrap(), calls)
    }

    /// The calls are made on the connection's executor, so wait for them in wall-clock time.
    fn wait_for(calls: &Mutex<Vec<String>>, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.lock().unwrap().len() < n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        calls.lock().unwrap().clone()
    }

    #[test]
    fn a_hold_inhibits_and_gives_its_own_cookie_back_on_release() {
        let (conn, _server, calls) = connect(false);

        let release = hold(conn.inner());
        assert_eq!(wait_for(&calls, 1), ["Inhibit(synoik, idle-inhibit)"]);

        drop(release);
        assert_eq!(
            wait_for(&calls, 2),
            ["Inhibit(synoik, idle-inhibit)", "UnInhibit(42)"]
        );
    }

    #[test]
    fn a_refused_inhibit_has_nothing_to_give_back() {
        let (conn, _server, calls) = connect(true);

        let release = hold(conn.inner());
        assert_eq!(wait_for(&calls, 1), ["Inhibit(synoik, idle-inhibit)"]);

        drop(release);
        // Give a stray `UnInhibit` the time it would need to arrive.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            *calls.lock().unwrap(),
            ["Inhibit(synoik, idle-inhibit)"],
            "a hold that never got a cookie must not release one"
        );
    }
}
