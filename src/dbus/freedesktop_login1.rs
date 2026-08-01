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
async fn resolve_session_path(conn: &zbus::Connection) -> anyhow::Result<OwnedObjectPath> {
    let manager =
        zbus::Proxy::new(conn, "org.freedesktop.login1", MANAGER_PATH, MANAGER_IFACE).await?;

    let by_pid = manager
        .call_method("GetSessionByPID", &(std::process::id(),))
        .await
        .and_then(|reply| reply.body().deserialize::<OwnedObjectPath>());
    match by_pid {
        Ok(path) => return Ok(path),
        Err(err) => debug!("logind has no session for our pid ({err}); asking for the user's"),
    }

    let user = zbus::Proxy::new(
        conn,
        "org.freedesktop.login1",
        // SAFETY: `getuid` is always successful and touches no memory.
        format!("/org/freedesktop/login1/user/_{}", unsafe {
            libc::getuid()
        }),
        "org.freedesktop.login1.User",
    )
    .await?;
    let (_id, path): (String, OwnedObjectPath) = user.get_property("Display").await?;
    Ok(path)
}

pub fn start(
    to_niri: calloop::channel::Sender<Login1ToNiri>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    let async_conn = conn.inner().clone();
    let future = async move {
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

        // The session's own `Lock`/`Unlock`, on a different object from the manager properties
        // above — and on the session's *concrete* path, which we have to go and ask for.
        let mut session_signals = match resolve_session_path(&async_conn).await {
            Ok(path) => {
                debug!("watching logind session {path} for Lock/Unlock");
                let path = SESSION_OBJECT.get_or_init(|| path.into());
                let session =
                    zbus::Proxy::new(&async_conn, "org.freedesktop.login1", path, SESSION_IFACE)
                        .await;
                match session {
                    Ok(proxy) => match proxy.receive_all_signals().await {
                        Ok(stream) => Some(stream),
                        Err(err) => {
                            warn!("error subscribing to logind session signals: {err:?}");
                            None
                        }
                    },
                    Err(err) => {
                        warn!("error creating the logind session proxy: {err:?}");
                        None
                    }
                }
            }
            Err(err) => {
                warn!("error resolving our logind session path: {err:?}");
                None
            }
        };

        loop {
            let signal = if let Some(signals) = session_signals.as_mut() {
                futures_util::select! {
                    changed = props_changed.next() => changed,
                    msg = signals.next() => {
                        let Some(msg) = msg else { continue };
                        let member = msg.header().member().map(|m| m.as_str().to_owned());
                        let locked = match member.as_deref() {
                            Some("Lock") => true,
                            Some("Unlock") => false,
                            _ => continue,
                        };
                        if let Err(err) = to_niri.send(Login1ToNiri::SessionLock(locked)) {
                            warn!("error sending the session lock signal to niri: {err:?}");
                            return;
                        }
                        continue;
                    }
                }
            } else {
                props_changed.next().await
            };
            let Some(signal) = signal else { break };

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

    Ok(conn)
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
