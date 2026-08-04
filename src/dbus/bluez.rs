//! System-bus watcher for BlueZ (`org.bluez`), feeding the QS Bluetooth toggle.
//!
//! A fourth task on the shared [`super::system_status`] connection. gnome-shell reaches BlueZ
//! through libgnome-bluetooth's `GnomeBluetooth.Client` (`js/ui/status/bluetooth.js` `BtClient`);
//! this is the GObject-free equivalent: one `ObjectManager` snapshot of `/`, rebuilt whole on any
//! wake (interfaces added/removed, any `PropertiesChanged` under `/org/bluez`, or the bus name
//! changing owner — bluetoothd may start after us or restart). The rfkill half of `BtClient`
//! (availability + the airplane-mode write) lives in [`super::rfkill`], like upstream's split.
//!
//! Semantics ported from gnome-bluetooth (`lib/bluetooth-client.c`, master):
//! - default adapter = the lexicographically **largest** `Adapter1` object path
//!   (`should_be_default_adapter`);
//! - adapter state from `Adapter1.PowerState` with a `Powered` fallback (`adapter_get_state`) —
//!   `PowerState` is absent on older/non-experimental BlueZ;
//! - a device is *connectable* iff its `UUIDs` contain a service gnome-shell can ask BlueZ to
//!   connect (`bluetooth-device.c` `update_connectable`'s audio/HID/MIDI list);
//! - connect/disconnect = plain `Device1.Connect`/`Disconnect` (`bluetooth_client_connect_service`
//!   — no profile selection, BlueZ picks).
//!
//! Open question (recorded at review): we scope devices to the default adapter
//! (`Device1.Adapter == default`); whether gnome-bluetooth's device store does the same on a
//! multi-adapter machine is unverified. Rare configuration — revisit if a second adapter's
//! devices ever need to show.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::{fdo, zvariant};

use super::system_status::SystemStatusToSynoik;
use crate::system_status::{BluetoothDevice, BluetoothStatus, BtAdapterState};

const BLUEZ_BUS: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";

/// gnome-bluetooth's `connectable_uuids` (`bluetooth-device.c`), as the raw 128-bit strings BlueZ
/// reports (SIG base UUID for the 16-bit ones; values from gnome-bluetooth's
/// `bluetooth_uuid_to_string` switch).
const CONNECTABLE_UUIDS: &[&str] = &[
    "00001108-0000-1000-8000-00805f9b34fb", // HSP
    "0000110a-0000-1000-8000-00805f9b34fb", // AudioSource (A2DP)
    "0000110b-0000-1000-8000-00805f9b34fb", // AudioSink (A2DP)
    "0000110c-0000-1000-8000-00805f9b34fb", // A/V_RemoteControlTarget
    "0000110e-0000-1000-8000-00805f9b34fb", // A/V_RemoteControl
    "00001112-0000-1000-8000-00805f9b34fb", // Headset AG
    "0000111e-0000-1000-8000-00805f9b34fb", // Handsfree
    "0000111f-0000-1000-8000-00805f9b34fb", // Handsfree AG
    "00001124-0000-1000-8000-00805f9b34fb", // HID
    "00001812-0000-1000-8000-00805f9b34fb", // HID over GATT (LE)
    "03b80e5a-ede8-4b33-a751-6ce34ec4c700", // MIDI
];

type Props = HashMap<String, zvariant::OwnedValue>;

/// Spawn the BlueZ monitor task on the shared system-bus connection.
pub(super) fn spawn(
    conn: &zbus::blocking::Connection,
    to_niri: calloop::channel::Sender<SystemStatusToSynoik>,
) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let object_manager = match fdo::ObjectManagerProxy::builder(&async_conn)
            .destination(BLUEZ_BUS)
            .and_then(|b| b.path("/"))
        {
            Ok(builder) => match builder.build().await {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating BlueZ ObjectManagerProxy: {err:?}");
                    return;
                }
            },
            Err(err) => {
                warn!("error building BlueZ ObjectManagerProxy: {err:?}");
                return;
            }
        };

        let added = match object_manager.receive_interfaces_added().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to BlueZ InterfacesAdded: {err:?}");
                return;
            }
        };
        let removed = match object_manager.receive_interfaces_removed().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to BlueZ InterfacesRemoved: {err:?}");
                return;
            }
        };

        // One match rule covers Adapter1 *and* every Device1's PropertiesChanged — no per-device
        // proxy churn (the bus resolves the well-known sender to bluetoothd's unique name).
        let rule = (|| -> zbus::Result<zbus::MatchRule<'static>> {
            Ok(zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(BLUEZ_BUS)?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")?
                .path_namespace("/org/bluez")?
                .build())
        })();
        let rule = match rule {
            Ok(rule) => rule,
            Err(err) => {
                warn!("error building BlueZ PropertiesChanged match rule: {err:?}");
                return;
            }
        };
        let props_changed =
            match zbus::MessageStream::for_match_rule(rule, &async_conn, Some(64)).await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to BlueZ PropertiesChanged: {err:?}");
                    return;
                }
            };

        // bluetoothd may start after us or restart; wake on its name changing owner too.
        let dbus = match fdo::DBusProxy::new(&async_conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating DBusProxy for BlueZ name tracking: {err:?}");
                return;
            }
        };
        let owner_changed = match dbus
            .receive_name_owner_changed_with_args(&[(0, BLUEZ_BUS)])
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to BlueZ NameOwnerChanged: {err:?}");
                return;
            }
        };

        let mut wake = futures_util::stream::select(
            futures_util::stream::select(added.map(|_| ()), removed.map(|_| ())),
            futures_util::stream::select(props_changed.map(|_| ()), owner_changed.map(|_| ())),
        );

        let mut last: Option<BluetoothStatus> = None;
        loop {
            // A failed GetManagedObjects means bluetoothd is gone: send the absent default so
            // the tile state degrades rather than sticking (the power-profiles shape, not
            // rfkill's skip-on-error).
            let status = match object_manager.get_managed_objects().await {
                Ok(objects) => read_bluetooth(&objects),
                Err(_) => BluetoothStatus::default(),
            };
            if last.as_ref() != Some(&status) {
                last = Some(status.clone());
                if to_niri
                    .send(SystemStatusToSynoik::Bluetooth(status))
                    .is_err()
                {
                    return;
                }
            }
            if wake.next().await.is_none() {
                return;
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "monitor BlueZ")
        .detach();
}

/// Set the default adapter's `Powered` (half of gnome-shell's `toggleActive`,
/// `bluetooth.js:139-140`). Fire-and-forget on the connection's executor; echo-driven like every
/// other QS write.
pub fn set_adapter_powered(conn: &zbus::blocking::Connection, path: String, powered: bool) {
    let async_conn = conn.inner().clone();
    let future = async move {
        if let Err(err) = async_conn
            .call_method(
                Some(BLUEZ_BUS),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(ADAPTER_IFACE, "Powered", zvariant::Value::from(powered)),
            )
            .await
        {
            warn!("error setting BlueZ Adapter1.Powered on {path}: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "set BlueZ adapter Powered")
        .detach();
}

/// Connect or disconnect a device (`Device1.Connect`/`Disconnect`, gnome-bluetooth's
/// `bluetooth_client_connect_service`). The call blocks until BlueZ finishes either way, so the
/// spawned future awaits it and reports [`SystemStatusToSynoik::BluetoothConnectDone`] back — the
/// GObject-free stand-in for the `await` around gnome-shell's row spinner
/// (`bluetooth.js:257-261`).
pub fn connect_device(
    conn: &zbus::blocking::Connection,
    path: String,
    connect: bool,
    done: calloop::channel::Sender<SystemStatusToSynoik>,
) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let method = if connect { "Connect" } else { "Disconnect" };
        if let Err(err) = async_conn
            .call_method(
                Some(BLUEZ_BUS),
                path.as_str(),
                Some(DEVICE_IFACE),
                method,
                &(),
            )
            .await
        {
            warn!("error calling BlueZ Device1.{method} on {path}: {err:?}");
        }
        let _ = done.send(SystemStatusToSynoik::BluetoothConnectDone(path));
    };
    conn.inner()
        .executor()
        .spawn(future, "BlueZ device connect/disconnect")
        .detach();
}

fn iface_props<'a>(
    ifaces: &'a HashMap<zbus::names::OwnedInterfaceName, Props>,
    name: &str,
) -> Option<&'a Props> {
    ifaces
        .iter()
        .find_map(|(k, v)| (k.as_str() == name).then_some(v))
}

fn get_bool(props: &Props, key: &str) -> Option<bool> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
}

fn get_str(props: &Props, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

fn get_str_array(props: &Props, key: &str) -> Vec<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
}

/// gnome-bluetooth's `update_connectable`: any advertised service UUID in the connectable list.
/// BlueZ reports lowercase, but compare case-insensitively to be safe.
fn is_connectable(uuids: &[String]) -> bool {
    uuids.iter().any(|uuid| {
        CONNECTABLE_UUIDS
            .iter()
            .any(|c| c.eq_ignore_ascii_case(uuid))
    })
}

/// `Adapter1.PowerState` → [`BtAdapterState`] (`bluetooth-client.c` `adapter_get_state`), with
/// the `Powered` fallback when the property is missing.
fn adapter_state(props: &Props) -> BtAdapterState {
    match get_str(props, "PowerState").as_deref() {
        Some("on") => BtAdapterState::On,
        Some("off") | Some("off-blocked") => BtAdapterState::Off,
        Some("off-enabling") => BtAdapterState::TurningOn,
        Some("on-disabling") => BtAdapterState::TurningOff,
        _ => {
            if get_bool(props, "Powered").unwrap_or(false) {
                BtAdapterState::On
            } else {
                BtAdapterState::Off
            }
        }
    }
}

/// Reduce a full `GetManagedObjects` snapshot to a [`BluetoothStatus`]. Pure, so it's
/// unit-testable without a bus.
fn read_bluetooth(objects: &fdo::ManagedObjects) -> BluetoothStatus {
    let Some((adapter_path, adapter_props)) = objects
        .iter()
        .filter_map(|(path, ifaces)| iface_props(ifaces, ADAPTER_IFACE).map(|props| (path, props)))
        // The lexicographically largest path is the default adapter
        // (gnome-bluetooth `should_be_default_adapter`).
        .max_by(|a, b| a.0.as_str().cmp(b.0.as_str()))
    else {
        return BluetoothStatus::default();
    };

    let mut devices: Vec<BluetoothDevice> = objects
        .iter()
        .filter_map(|(path, ifaces)| {
            let props = iface_props(ifaces, DEVICE_IFACE)?;
            // Only devices hanging off the default adapter.
            let device_adapter = props
                .get("Adapter")
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| zvariant::OwnedObjectPath::try_from(v).ok())?;
            if device_adapter.as_str() != adapter_path.as_str() {
                return None;
            }
            Some(BluetoothDevice {
                path: path.to_string(),
                alias: get_str(props, "Alias")
                    .or_else(|| get_str(props, "Address"))
                    .unwrap_or_default(),
                icon: get_str(props, "Icon"),
                connectable: is_connectable(&get_str_array(props, "UUIDs")),
                paired: get_bool(props, "Paired").unwrap_or(false),
                trusted: get_bool(props, "Trusted").unwrap_or(false),
                connected: get_bool(props, "Connected").unwrap_or(false),
            })
        })
        .collect();
    // A stable order so snapshot comparison (`last != status`) doesn't churn on HashMap order.
    devices.sort_by(|a, b| a.path.cmp(&b.path));

    BluetoothStatus {
        adapter: Some(adapter_path.to_string()),
        adapter_present: true,
        powered: get_bool(adapter_props, "Powered").unwrap_or(false),
        state: adapter_state(adapter_props),
        devices,
    }
}

#[cfg(test)]
mod tests {
    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

    use super::*;

    fn props(pairs: Vec<(&str, Value<'static>)>) -> Props {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.try_to_owned().unwrap()))
            .collect()
    }

    fn object(objects: &mut fdo::ManagedObjects, path: &str, iface: &str, properties: Props) {
        let path = OwnedObjectPath::try_from(path).unwrap();
        let iface = OwnedInterfaceName::try_from(iface.to_string()).unwrap();
        objects.entry(path).or_default().insert(iface, properties);
    }

    fn device_props(adapter: &str, alias: &str, connected: bool, uuids: &[&str]) -> Props {
        props(vec![
            (
                "Adapter",
                Value::from(ObjectPath::try_from(adapter.to_string()).unwrap()),
            ),
            ("Alias", Value::from(alias.to_string())),
            (
                "UUIDs",
                Value::from(uuids.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            ),
            ("Paired", Value::from(true)),
            ("Trusted", Value::from(false)),
            ("Connected", Value::from(connected)),
        ])
    }

    const HID: &str = "00001124-0000-1000-8000-00805f9b34fb";
    const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
    const SPP: &str = "00001101-0000-1000-8000-00805f9b34fb"; // serial port — NOT connectable

    #[test]
    fn read_bluetooth_picks_largest_adapter_and_its_devices() {
        let mut objects = fdo::ManagedObjects::default();
        object(
            &mut objects,
            "/org/bluez/hci0",
            ADAPTER_IFACE,
            props(vec![("Powered", Value::from(false))]),
        );
        object(
            &mut objects,
            "/org/bluez/hci1",
            ADAPTER_IFACE,
            props(vec![
                ("Powered", Value::from(true)),
                ("PowerState", Value::from("on")),
            ]),
        );
        // On the default (largest-path) adapter.
        object(
            &mut objects,
            "/org/bluez/hci1/dev_AA",
            DEVICE_IFACE,
            device_props("/org/bluez/hci1", "Keyboard", true, &[HID]),
        );
        // On the other adapter — must be excluded.
        object(
            &mut objects,
            "/org/bluez/hci0/dev_BB",
            DEVICE_IFACE,
            device_props("/org/bluez/hci0", "Elsewhere", true, &[A2DP_SINK]),
        );

        let status = read_bluetooth(&objects);
        assert!(status.adapter_present);
        assert_eq!(status.adapter.as_deref(), Some("/org/bluez/hci1"));
        assert!(status.powered);
        assert_eq!(status.state, BtAdapterState::On);
        assert_eq!(status.devices.len(), 1);
        assert_eq!(status.devices[0].alias, "Keyboard");
        assert!(status.devices[0].connectable);
        assert!(status.devices[0].connected);
    }

    #[test]
    fn read_bluetooth_absent_without_an_adapter() {
        let objects = fdo::ManagedObjects::default();
        assert_eq!(read_bluetooth(&objects), BluetoothStatus::default());
    }

    #[test]
    fn power_state_maps_and_falls_back_to_powered() {
        for (power_state, expected) in [
            ("on", BtAdapterState::On),
            ("off", BtAdapterState::Off),
            ("off-blocked", BtAdapterState::Off),
            ("off-enabling", BtAdapterState::TurningOn),
            ("on-disabling", BtAdapterState::TurningOff),
        ] {
            let p = props(vec![("PowerState", Value::from(power_state))]);
            assert_eq!(adapter_state(&p), expected, "PowerState {power_state:?}");
        }
        // No PowerState (older BlueZ): Powered decides.
        let p = props(vec![("Powered", Value::from(true))]);
        assert_eq!(adapter_state(&p), BtAdapterState::On);
        assert_eq!(adapter_state(&props(vec![])), BtAdapterState::Off);
    }

    #[test]
    fn connectable_follows_gnome_bluetooth_uuid_list() {
        assert!(is_connectable(&[SPP.to_string(), HID.to_string()]));
        assert!(is_connectable(&[A2DP_SINK.to_uppercase()]));
        assert!(!is_connectable(&[SPP.to_string()]));
        assert!(!is_connectable(&[]));
    }
}
