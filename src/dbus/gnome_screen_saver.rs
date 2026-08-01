//! `org.gnome.ScreenSaver` — the interface a GNOME session locks the screen through
//! (`js/ui/shellDBus.js:517-566`, `data/dbus-interfaces/org.gnome.ScreenSaver.xml`).
//!
//! Not to be confused with [`super::freedesktop_screensaver`], which is a *different* name and a
//! different job: `org.freedesktop.ScreenSaver` is `Inhibit`/`UnInhibit`, what a video player
//! calls to stop the screen blanking. Nothing in it can lock. Serving only that one — which is
//! where this fork stood until now — leaves `Lock` unowned, so gsd-power's idle lock, its
//! lock-on-suspend, and `loginctl lock-session` all land on a name nobody answers and the session
//! silently never locks.
//!
//! # Two names, on purpose
//!
//! gnome-shell exports this object on **`org.gnome.Shell.ScreenShield`**, and ships a separate
//! tiny gjs service that owns **`org.gnome.ScreenSaver`** and proxies straight through to it
//! (`js/dbusServices/screensaver/screenSaverService.js`). The split exists so the well-known name
//! is D-Bus-activatable while the shell is down.
//!
//! We are a single process with no such staging, so we own both names on this one connection and
//! export the object once. The object path is `/org/gnome/ScreenSaver` for both, which is what
//! callers use — see the well-known-name placement rule: an object must live on the connection
//! owning the name its callers ask for, or they get `UnknownObject` and a silent no-op.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::interface;
use zbus::object_server::SignalEmitter;

use super::Start;
use crate::utils::get_monotonic_time;

/// gnome-shell's own name for the object; the real implementation lives here.
const SHIELD_NAME: &str = "org.gnome.Shell.ScreenShield";
/// The activatable name callers actually talk to.
const SAVER_NAME: &str = "org.gnome.ScreenSaver";
const PATH: &str = "/org/gnome/ScreenSaver";

/// A call from the session into the compositor.
#[derive(Debug, Clone, Copy)]
pub enum ScreenSaverToNiri {
    /// `Lock` — put the shield down and require authentication (`shellDBus.js:538-546`).
    Lock,
    /// `SetActive(b)` — the screensaver half. GNOME activates *with* animation and deactivates
    /// *without* (`:548-553`).
    SetActive(bool),
}

/// A change the compositor wants on the bus.
#[derive(Debug, Clone, Copy)]
pub enum NiriToScreenSaver {
    ActiveChanged(bool),
    WakeUpScreen,
}

/// The state `GetActive`/`GetActiveTime` read. Written by the compositor, read by the bus task,
/// so it is shared rather than messaged: both are pure reads that must answer immediately.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShieldSnapshot {
    pub active: bool,
    /// Monotonic instant the shield went down, mirroring `_activationTime`.
    pub activation_time: Option<Duration>,
}

#[derive(Clone)]
pub struct GnomeScreenSaver {
    to_niri: calloop::channel::Sender<ScreenSaverToNiri>,
    from_niri: async_channel::Receiver<NiriToScreenSaver>,
    snapshot: Arc<Mutex<ShieldSnapshot>>,
}

#[interface(name = "org.gnome.ScreenSaver")]
impl GnomeScreenSaver {
    /// `Lock`.
    ///
    /// GNOME does not return until the lock screen is actually shown — it defers the reply on
    /// `lock-screen-shown` (`shellDBus.js:538-546`) — so that a caller which locks and then
    /// suspends cannot race the shield onto the screen. We reply immediately, which is a
    /// divergence worth closing when the shield has an on-screen state to wait for; until then
    /// there is no such event to wait on and a deferred reply would simply never arrive.
    async fn lock(&self) {
        let _ = self.to_niri.send(ScreenSaverToNiri::Lock);
    }

    async fn set_active(&self, active: bool) {
        let _ = self.to_niri.send(ScreenSaverToNiri::SetActive(active));
    }

    async fn get_active(&self) -> bool {
        self.snapshot.lock().unwrap().active
    }

    /// Whole seconds since the shield went down, or 0 while it is up (`:558-565`).
    async fn get_active_time(&self) -> u32 {
        let snapshot = *self.snapshot.lock().unwrap();
        let Some(started) = snapshot.activation_time else {
            return 0;
        };
        get_monotonic_time().saturating_sub(started).as_secs() as u32
    }

    /// Emitted by the task in [`Start::start`], never from a method call.
    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, new_value: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn wake_up_screen(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl GnomeScreenSaver {
    pub fn new(
        to_niri: calloop::channel::Sender<ScreenSaverToNiri>,
        from_niri: async_channel::Receiver<NiriToScreenSaver>,
        snapshot: Arc<Mutex<ShieldSnapshot>>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            snapshot,
        }
    }
}

impl Start for GnomeScreenSaver {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let from_niri = self.from_niri.clone();
        let conn = zbus::blocking::Connection::session()?;

        conn.object_server().at(PATH, self)?;

        // `DoNotQueue` for the same reason as the brightness object: being *queued* reads as
        // success but leaves us emitting from a connection nobody is listening to.
        let flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
        for name in [SHIELD_NAME, SAVER_NAME] {
            match conn.request_name_with_flags(name, flags)? {
                RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => (),
                reply => anyhow::bail!(
                    "{name} is owned by another process (request replied {reply:?}); \
                     the screen shield is disabled"
                ),
            }
        }

        let emit_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move {
                    while let Ok(msg) = from_niri.recv().await {
                        let iface = match emit_conn
                            .object_server()
                            .interface::<_, GnomeScreenSaver>(PATH)
                            .await
                        {
                            Ok(iface) => iface,
                            Err(err) => {
                                warn!("screen saver: error resolving our own interface: {err:?}");
                                continue;
                            }
                        };
                        let emitter = iface.signal_emitter();
                        let res = match msg {
                            NiriToScreenSaver::ActiveChanged(active) => {
                                GnomeScreenSaver::active_changed(emitter, active).await
                            }
                            NiriToScreenSaver::WakeUpScreen => {
                                GnomeScreenSaver::wake_up_screen(emitter).await
                            }
                        };
                        if let Err(err) = res {
                            warn!("screen saver: error emitting {msg:?}: {err:?}");
                        }
                    }
                },
                "screen-saver-emit",
            )
            .detach();

        Ok(conn)
    }
}
