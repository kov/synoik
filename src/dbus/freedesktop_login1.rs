use std::sync::OnceLock;

use futures_util::StreamExt;
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::backlight::WriteOutcome;

/// logind resolves this path to the caller's own session, so calls never have to look one up.
///
/// Only good for *calls*. Signals are broadcast from the session's concrete path, and logind never
/// emits anything on `auto` — see [`resolve_session_path`].
const SESSION_PATH: &str = "/org/freedesktop/login1/session/auto";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

pub enum Login1ToNiri {
    LidClosedChanged(bool),
    /// `Session.Lock` / `Session.Unlock` — logind asking the session to raise or lower its shield.
    ///
    /// `Unlock` is how **gdm's own login screen unlocks you**: you switch to gdm's VT,
    /// authenticate there, and gdm tells logind, which signals the session. Without this the
    /// VT switches back and the shield is still up, with no way to tell it what just happened.
    ///
    /// `loginctl lock-session` / `unlock-session` are the same two signals by hand.
    SessionLock(bool),
    /// `Session.Active` — whether our VT is the one on screen. Gates the sleep inhibitor: a
    /// session sitting on a background VT must not hold up everyone else's suspend.
    SessionActive(bool),
    /// `Manager.PrepareForSleep` — `true` while logind waits on the delay inhibitors before
    /// suspending, `false` on resume. The `true` edge is the last chance to lock the screen.
    PrepareForSleep(bool),
    /// A [`set_brightness`] call finished, carrying the connector it was for. Drives the
    /// per-device write serializer ([`crate::backlight::BacklightWriter::write_finished`]), which
    /// is what keeps a slider drag down to one in-flight D-Bus call.
    BrightnessWriteDone {
        connector: String,
        outcome: WriteOutcome,
    },
}

/// Our own session's concrete object path, resolved once by [`start`].
static SESSION_OBJECT: OnceLock<ObjectPath<'static>> = OnceLock::new();

/// The object path of our logind session, or `None` if it could not be resolved.
///
/// Every `Session` call and subscription should go through this rather than [`SESSION_PATH`].
pub fn session_path() -> Option<&'static ObjectPath<'static>> {
    SESSION_OBJECT.get()
}

/// Ask logind for the object path of *our own* session.
///
/// [`SESSION_PATH`] (`.../session/auto`) is a per-caller alias logind resolves from the caller's
/// pid; it is not an object that exists on the bus. Two things follow, and we were wrong about
/// both:
///
/// - **Nothing is ever emitted from it.** The session's signals are broadcast from its escaped
///   concrete path (session `116` is `/org/freedesktop/login1/session/_3116`), so a match rule on
///   `auto` silently never fires: the subscription succeeds and the stream stays empty forever.
/// - **It does not resolve for us at all.** A GNOME session runs the shell as a *user service*
///   (`user@1002.service/session.slice/org.gnome.Shell@user.service`), outside the session scope,
///   so logind's pid lookup answers `NoSessionForPID` and every call on `auto` fails.
///
/// Hence the round trip, and hence the fallback: `GetSessionByPID` is the precise answer when we
/// *are* in a session scope (started from a TTY), and the user object's `Display` — logind's own
/// "this user's graphical session" — is the one that works when we are not. Deriving the path from
/// an id ourselves would mean reimplementing systemd's `bus_label_escape`; both of these hand us
/// the escaping already done.
fn resolve_session_path(conn: &zbus::blocking::Connection) -> anyhow::Result<OwnedObjectPath> {
    let manager =
        zbus::blocking::Proxy::new(conn, "org.freedesktop.login1", MANAGER_PATH, MANAGER_IFACE)?;

    let by_pid = manager
        .call_method("GetSessionByPID", &(std::process::id(),))
        .and_then(|reply| reply.body().deserialize::<OwnedObjectPath>());
    match by_pid {
        Ok(path) => return Ok(path),
        Err(err) => debug!("logind has no session for our pid ({err}); asking for the user's"),
    }

    let user = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.login1",
        // SAFETY: `getuid` is always successful and touches no memory.
        format!("/org/freedesktop/login1/user/_{}", unsafe {
            libc::getuid()
        }),
        "org.freedesktop.login1.User",
    )?;
    let (_id, path): (String, OwnedObjectPath) = user.get_property("Display")?;
    Ok(path)
}

pub fn start(
    to_niri: calloop::channel::Sender<Login1ToNiri>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    // Synchronously, before anything subscribes or calls: `session_path()` has to be valid by the
    // time `start` returns, because the compositor thread reads it from `update_locked_hint` and a
    // task that has not been polled yet would leave it `None` for the first lock.
    match resolve_session_path(&conn) {
        Ok(path) => {
            debug!("our logind session is {path}");
            let _ = SESSION_OBJECT.set(path.into());
        }
        Err(err) => warn!("error resolving our logind session path: {err:?}"),
    }

    let async_conn = conn.inner().clone();
    let lid_to_niri = to_niri.clone();
    let future = async move {
        let to_niri = lid_to_niri;
        let proxy = fdo::PropertiesProxy::new(
            &async_conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
        )
        .await;
        let proxy = match proxy {
            Ok(x) => x,
            Err(err) => {
                warn!("error creating PropertiesProxy: {err:?}");
                return;
            }
        };

        let mut props_changed = match proxy.receive_properties_changed().await {
            Ok(x) => x,
            Err(err) => {
                warn!("error subscribing to PropertiesChanged: {err:?}");
                return;
            }
        };

        let props = proxy
            .get_all(InterfaceName::try_from("org.freedesktop.login1.Manager").unwrap())
            .await;
        let mut props = match props {
            Ok(x) => x,
            Err(err) => {
                warn!("error receiving initial properties: {err:?}");
                return;
            }
        };

        trace!("initial properties: {props:?}");

        let mut lid_closed = props
            .remove("LidClosed")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or_default();

        if let Err(err) = to_niri.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
            warn!("error sending initial lid state to niri: {err:?}");
            return;
        };

        while let Some(signal) = props_changed.next().await {
            let args = match signal.args() {
                Ok(args) => args,
                Err(err) => {
                    warn!("error parsing PropertiesChanged args: {err:?}");
                    return;
                }
            };

            let mut new_lid_closed = lid_closed;
            let mut changed = false;
            for (name, value) in args.changed_properties() {
                trace!("changed property: {name} => {value:?}");
                if *name != "LidClosed" {
                    continue;
                }

                new_lid_closed = bool::try_from(value).unwrap_or(new_lid_closed);
                changed = true;
            }

            if !changed {
                continue;
            }

            if new_lid_closed == lid_closed {
                continue;
            }

            lid_closed = new_lid_closed;
            if let Err(err) = to_niri.send(Login1ToNiri::LidClosedChanged(lid_closed)) {
                warn!("error sending message to niri: {err:?}");
                return;
            };
        }
    };

    let task = conn
        .inner()
        .executor()
        .spawn(future, "monitor login1 property changes");
    task.detach();

    // One task per signal source rather than one `select!` over all of them: they have unrelated
    // lifetimes (the session ones need a path that may not resolve at all), and folding them
    // together is how the `Lock`/`Unlock` stream ended up buried inside the lid loop.
    if let Some(path) = session_path() {
        let async_conn = conn.inner().clone();
        let to_niri = to_niri.clone();
        let path = path.to_owned();
        let future = async move {
            let session =
                match zbus::Proxy::new(&async_conn, "org.freedesktop.login1", &path, SESSION_IFACE)
                    .await
                {
                    Ok(proxy) => proxy,
                    Err(err) => {
                        warn!("error creating the logind session proxy: {err:?}");
                        return;
                    }
                };

            let mut signals = match session.receive_all_signals().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to logind session signals: {err:?}");
                    return;
                }
            };

            // `Active` says whether we own the VT. A session on someone else's VT must not hold
            // logind's sleep inhibitor, so the shield needs to hear this change.
            let props = match fdo::PropertiesProxy::new(
                &async_conn,
                "org.freedesktop.login1",
                path.clone(),
            )
            .await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating the logind session properties proxy: {err:?}");
                    return;
                }
            };
            let session_iface = InterfaceName::try_from(SESSION_IFACE).unwrap();
            if let Ok(mut all) = props.get_all(session_iface.clone()).await {
                if let Some(active) = all.remove("Active").and_then(|v| bool::try_from(v).ok()) {
                    let _ = to_niri.send(Login1ToNiri::SessionActive(active));
                }
            }
            let mut props_changed = match props.receive_properties_changed().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to logind session properties: {err:?}");
                    return;
                }
            };

            loop {
                let msg = futures_util::select! {
                    signal = signals.next() => {
                        let Some(signal) = signal else { break };
                        let member = signal.header().member().map(|m| m.as_str().to_owned());
                        match member.as_deref() {
                            Some("Lock") => Login1ToNiri::SessionLock(true),
                            Some("Unlock") => Login1ToNiri::SessionLock(false),
                            _ => continue,
                        }
                    }
                    changed = props_changed.next() => {
                        let Some(changed) = changed else { break };
                        let Ok(args) = changed.args() else { continue };
                        let active = args
                            .changed_properties()
                            .iter()
                            .find(|(name, _)| **name == "Active")
                            .and_then(|(_, value)| bool::try_from(value).ok());
                        let Some(active) = active else { continue };
                        Login1ToNiri::SessionActive(active)
                    }
                };
                if to_niri.send(msg).is_err() {
                    break;
                }
            }
        };
        conn.inner()
            .executor()
            .spawn(future, "monitor login1 session")
            .detach();
    }

    let async_conn = conn.inner().clone();
    let future = async move {
        let manager = match zbus::Proxy::new(
            &async_conn,
            "org.freedesktop.login1",
            MANAGER_PATH,
            MANAGER_IFACE,
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating the logind manager proxy: {err:?}");
                return;
            }
        };
        let mut signals = match manager.receive_all_signals().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to logind manager signals: {err:?}");
                return;
            }
        };

        while let Some(signal) = signals.next().await {
            if signal
                .header()
                .member()
                .map(|m| m.as_str().to_owned())
                .as_deref()
                != Some("PrepareForSleep")
            {
                continue;
            }
            let Ok(about_to_suspend) = signal.body().deserialize::<bool>() else {
                continue;
            };
            if to_niri
                .send(Login1ToNiri::PrepareForSleep(about_to_suspend))
                .is_err()
            {
                break;
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "monitor login1 PrepareForSleep")
        .detach();

    Ok(conn)
}

/// Take logind's `delay` sleep inhibitor, or `None` if it could not be had.
///
/// Holding the returned fd is what makes logind emit `PrepareForSleep(true)` and then **wait** for
/// us before suspending; dropping it is how we say we are done. That handshake is the only reason
/// there is time to lock the screen before the machine goes down, so a failure here is not
/// cosmetic — it silently turns suspend-locks into a race.
///
/// Blocking, unlike [`set_brightness`]: this is called only when the shield's state or its settings
/// change (a handful of times in a session), never per frame or per input event, and the fd must be
/// in hand before we can claim to be inhibiting.
pub fn take_sleep_inhibitor(conn: &zbus::blocking::Connection) -> Option<zbus::zvariant::OwnedFd> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.login1"),
            MANAGER_PATH,
            Some(MANAGER_IFACE),
            "Inhibit",
            // `sleep` only. GNOME asks for exactly this (`_syncInhibitor`, `:219-221`); adding
            // `shutdown` or `idle` here would delay things we have nothing to do about.
            &(
                "sleep",
                "gnome-shell-rs",
                "GNOME needs to lock the screen",
                "delay",
            ),
        )
        .inspect_err(|err| warn!("failed to inhibit suspend: {err:?}"))
        .ok()?;

    reply
        .body()
        .deserialize::<zbus::zvariant::OwnedFd>()
        .inspect_err(|err| warn!("logind's sleep inhibitor reply had no fd: {err:?}"))
        .ok()
}

/// Write a backlight brightness through logind: `Session.SetBrightness("backlight", <device>, v)`,
/// which is how mutter drives the panel without being root (`meta-backlight-sysfs.c:167-173`).
///
/// Spawned on the connection's executor rather than called synchronously — a slider drag would
/// otherwise stall the compositor thread for the D-Bus timeout whenever logind is slow. The
/// completion comes back as [`Login1ToNiri::BrightnessWriteDone`] because the write serializer
/// needs it: it is what releases the next write of a drag.
///
/// **Divergence (D1):** mutter falls back to a `pkexec mutter-backlight-helper` subprocess when
/// logind has no `SetBrightness` (old logind, or seatd instead of logind). We ship no such helper,
/// so a failing write is warned about and dropped.
pub fn set_brightness(
    conn: &zbus::blocking::Connection,
    done: calloop::channel::Sender<Login1ToNiri>,
    connector: String,
    device_name: String,
    brightness: i32,
) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let value = u32::try_from(brightness).unwrap_or(0);
        // `auto` is a last resort: it only resolves when we happen to be inside the session scope.
        let path = session_path()
            .cloned()
            .unwrap_or_else(|| ObjectPath::from_static_str_unchecked(SESSION_PATH));
        let result = async_conn
            .call_method(
                Some("org.freedesktop.login1"),
                &path,
                Some(SESSION_IFACE),
                "SetBrightness",
                &("backlight", device_name.as_str(), value),
            )
            .await;

        let outcome = match result {
            Ok(_) => WriteOutcome::Done(brightness),
            Err(err) => {
                warn!("error setting backlight brightness on {device_name}: {err:?}");
                WriteOutcome::Failed
            }
        };

        let _ = done.send(Login1ToNiri::BrightnessWriteDone { connector, outcome });
    };

    conn.inner()
        .executor()
        .spawn(future, "set login1 backlight brightness")
        .detach();
}
