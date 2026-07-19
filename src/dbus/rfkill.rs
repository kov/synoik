//! Session-bus watcher for airplane (rfkill) mode, from gnome-settings-daemon.
//!
//! Mirrors [`crate::dbus::system_status`] but on the **session** bus: one
//! `Connection::session()` with a single task that re-reads gsd-rfkill's properties on either its
//! `PropertiesChanged` or the well-known name gaining an owner (gsd isn't dbus-activatable and
//! starts after us), pushing a fresh [`AirplaneStatus`] to the compositor over a calloop channel.
//! The same connection doubles as the writer for the QS toggle ([`set_airplane_mode`]). Ports
//! gnome-shell's `RfkillManager` (`js/ui/status/rfkill.js`).

use futures_util::StreamExt;
use zbus::names::InterfaceName;
use zbus::{fdo, zvariant};

use crate::system_status::AirplaneStatus;

const BUS_NAME: &str = "org.gnome.SettingsDaemon.Rfkill";
const OBJECT_PATH: &str = "/org/gnome/SettingsDaemon/Rfkill";
const IFACE: &str = "org.gnome.SettingsDaemon.Rfkill";

type Props = std::collections::HashMap<String, zvariant::OwnedValue>;

pub fn start(
    to_niri: calloop::channel::Sender<AirplaneStatus>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;
    let async_conn = conn.inner().clone();

    let future = async move {
        let proxy = match fdo::PropertiesProxy::new(&async_conn, BUS_NAME, OBJECT_PATH).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating gsd-rfkill PropertiesProxy: {err:?}");
                return;
            }
        };
        let iface = InterfaceName::try_from(IFACE).unwrap();

        let changed = match proxy.receive_properties_changed().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to gsd-rfkill PropertiesChanged: {err:?}");
                return;
            }
        };

        // gsd-rfkill is NOT dbus-activatable and the compositor starts before gnome-session spawns
        // it, so the initial `get_all` routinely fails and gsd may never emit a `PropertiesChanged`
        // after settling its state pre-export. Also wake on the well-known name gaining an owner
        // and re-read then — this is what `Gio.DBusProxy` does for gnome-shell (re-runs
        // GetAll on owner appearance). Without it the feature silently stays hidden on real
        // rfkill hardware.
        let dbus = match fdo::DBusProxy::new(&async_conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating DBusProxy for gsd-rfkill name tracking: {err:?}");
                return;
            }
        };
        let owner_changed = match dbus
            .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to gsd-rfkill NameOwnerChanged: {err:?}");
                return;
            }
        };

        // Either signal is a reason to re-read the full property set.
        let mut wake = futures_util::stream::select(changed.map(|_| ()), owner_changed.map(|_| ()));

        let mut last: Option<AirplaneStatus> = None;
        loop {
            if let Ok(props) = proxy.get_all(iface.clone()).await {
                let status = read_airplane(&props);
                if last != Some(status) {
                    last = Some(status);
                    if to_niri.send(status).is_err() {
                        return;
                    }
                }
            }
            if wake.next().await.is_none() {
                return;
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "monitor gsd-rfkill airplane mode")
        .detach();

    Ok(conn)
}

/// Set gsd-rfkill's `AirplaneMode` property (the QS toggle click). Fire-and-forget on the
/// connection's executor — a synchronous `Set` on the compositor thread would stall it for the
/// D-Bus timeout if gsd is slow/wedged (the hazard `GnomeSettingsWriter` avoids). The tile is
/// echo-driven, so it updates when gsd emits `PropertiesChanged` back, not optimistically.
pub fn set_airplane_mode(conn: &zbus::blocking::Connection, active: bool) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let proxy = match fdo::PropertiesProxy::new(&async_conn, BUS_NAME, OBJECT_PATH).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating gsd-rfkill PropertiesProxy for write: {err:?}");
                return;
            }
        };
        let iface = InterfaceName::try_from(IFACE).unwrap();
        if let Err(err) = proxy
            .set(iface, "AirplaneMode", zvariant::Value::from(active))
            .await
        {
            warn!("error setting AirplaneMode: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "set gsd-rfkill AirplaneMode")
        .detach();
}

fn get_bool(props: &Props, key: &str) -> Option<bool> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
}

/// Build an [`AirplaneStatus`] from gsd-rfkill's properties. `show = HasAirplaneMode &&
/// ShouldShowAirplaneMode` (gnome-shell's `show_airplane_mode` getter, `rfkill.js:59-61`).
fn read_airplane(props: &Props) -> AirplaneStatus {
    AirplaneStatus {
        active: get_bool(props, "AirplaneMode").unwrap_or(false),
        show: get_bool(props, "HasAirplaneMode").unwrap_or(false)
            && get_bool(props, "ShouldShowAirplaneMode").unwrap_or(false),
    }
}
