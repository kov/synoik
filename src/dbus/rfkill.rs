//! Session-bus watcher for rfkill state, from gnome-settings-daemon.
//!
//! Mirrors [`crate::dbus::system_status`] but on the **session** bus: one
//! `Connection::session()` with a single task that re-reads gsd-rfkill's properties on either its
//! `PropertiesChanged` or the well-known name gaining an owner (gsd isn't dbus-activatable and
//! starts after us), pushing a fresh [`RfkillStatus`] to the compositor over a calloop channel:
//! airplane mode (gnome-shell's `RfkillManager`, `js/ui/status/rfkill.js`) plus the Bluetooth
//! kill-switch properties `BtClient` reads from the same service
//! (`js/ui/status/bluetooth.js:74-80,103-108`). The same connection doubles as the writer for
//! the QS toggles ([`set_airplane_mode`], [`set_bluetooth_airplane_mode`]).

use futures_util::StreamExt;
use zbus::names::InterfaceName;
use zbus::{fdo, zvariant};

use crate::system_status::{AirplaneStatus, BluetoothRfkill};

/// Everything gsd-rfkill tells us in one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RfkillStatus {
    pub airplane: AirplaneStatus,
    pub bluetooth: BluetoothRfkill,
}

const BUS_NAME: &str = "org.gnome.SettingsDaemon.Rfkill";
const OBJECT_PATH: &str = "/org/gnome/SettingsDaemon/Rfkill";
const IFACE: &str = "org.gnome.SettingsDaemon.Rfkill";

type Props = std::collections::HashMap<String, zvariant::OwnedValue>;

pub fn start(
    to_niri: calloop::channel::Sender<RfkillStatus>,
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

        let mut last: Option<RfkillStatus> = None;
        loop {
            if let Ok(props) = proxy.get_all(iface.clone()).await {
                let status = read_rfkill(&props);
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

/// Set gsd-rfkill's `BluetoothAirplaneMode` — half of gnome-shell's Bluetooth `toggleActive`
/// (`bluetooth.js:138`; the other half powers the adapter, [`super::bluez::set_adapter_powered`]).
/// Same fire-and-forget, echo-driven shape as [`set_airplane_mode`].
pub fn set_bluetooth_airplane_mode(conn: &zbus::blocking::Connection, active: bool) {
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
            .set(
                iface,
                "BluetoothAirplaneMode",
                zvariant::Value::from(active),
            )
            .await
        {
            warn!("error setting BluetoothAirplaneMode: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "set gsd-rfkill BluetoothAirplaneMode")
        .detach();
}

fn get_bool(props: &Props, key: &str) -> Option<bool> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
}

/// Build an [`RfkillStatus`] from gsd-rfkill's properties. Airplane `show = HasAirplaneMode &&
/// ShouldShowAirplaneMode` (gnome-shell's `show_airplane_mode` getter, `rfkill.js:59-61`); the
/// Bluetooth trio is what `BtClient` reads (`bluetooth.js:74-80,103-108`).
pub(crate) fn read_rfkill(props: &Props) -> RfkillStatus {
    RfkillStatus {
        airplane: AirplaneStatus {
            active: get_bool(props, "AirplaneMode").unwrap_or(false),
            show: get_bool(props, "HasAirplaneMode").unwrap_or(false)
                && get_bool(props, "ShouldShowAirplaneMode").unwrap_or(false),
        },
        bluetooth: BluetoothRfkill {
            airplane: get_bool(props, "BluetoothAirplaneMode").unwrap_or(false),
            has_airplane: get_bool(props, "BluetoothHasAirplaneMode").unwrap_or(false),
            hardware_airplane: get_bool(props, "BluetoothHardwareAirplaneMode").unwrap_or(false),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, bool)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    zvariant::Value::from(*v).try_to_owned().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn read_rfkill_maps_airplane_and_bluetooth_properties() {
        let all = props(&[
            ("AirplaneMode", true),
            ("HasAirplaneMode", true),
            ("ShouldShowAirplaneMode", true),
            ("BluetoothAirplaneMode", true),
            ("BluetoothHasAirplaneMode", true),
            ("BluetoothHardwareAirplaneMode", false),
        ]);
        let status = read_rfkill(&all);
        assert!(status.airplane.active && status.airplane.show);
        assert!(status.bluetooth.airplane);
        assert!(status.bluetooth.available());

        // A hardware Bluetooth switch kills the tile even with the soft one present
        // (`bluetooth.js:104-107`).
        let hw = props(&[
            ("BluetoothHasAirplaneMode", true),
            ("BluetoothHardwareAirplaneMode", true),
        ]);
        assert!(!read_rfkill(&hw).bluetooth.available());

        // Absent properties default off/hidden.
        let empty = read_rfkill(&Props::default());
        assert_eq!(empty, RfkillStatus::default());
    }
}
