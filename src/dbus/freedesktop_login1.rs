use futures_util::StreamExt;
use zbus::fdo;
use zbus::names::InterfaceName;

use crate::backlight::WriteOutcome;

/// logind resolves this path to the caller's own session, so we never have to look one up.
const SESSION_PATH: &str = "/org/freedesktop/login1/session/auto";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

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
        // above. `.../session/auto` resolves to our own session, so there is nothing to look up.
        let session = zbus::Proxy::new(
            &async_conn,
            "org.freedesktop.login1",
            SESSION_PATH,
            SESSION_IFACE,
        )
        .await;
        let mut session_signals = match &session {
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
        let result = async_conn
            .call_method(
                Some("org.freedesktop.login1"),
                SESSION_PATH,
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
