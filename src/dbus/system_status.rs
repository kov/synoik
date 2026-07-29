//! System-bus watcher for the panel status area: network (NetworkManager),
//! battery (UPower), power profile (power-profiles-daemon), and bluetooth
//! (BlueZ, in [`super::bluez`]).
//!
//! Mirrors [`crate::dbus::freedesktop_locale1`]: one `Connection::system()` with
//! a task per source on its executor, each subscribing to a `PropertiesChanged` stream and
//! pushing a fresh [`crate::system_status`] snapshot to the compositor over a
//! calloop channel. The panel turns those into icons ([`crate::system_status::network_icon`]
//! / [`battery_icon`](crate::system_status::battery_icon)). The power-profiles task additionally
//! tracks the daemon's bus-name owner (like [`crate::dbus::rfkill`]) because *visibility is owner
//! presence* and the daemon may start after us or be absent entirely.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::names::InterfaceName;
use zbus::{fdo, zvariant};

use crate::system_status::{
    BatteryStatus, BluetoothStatus, KnownProfile, NetworkStatus, PowerProfileStatus,
};

const POWER_PROFILES_BUS: &str = "org.freedesktop.UPower.PowerProfiles";
const POWER_PROFILES_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

/// A status update from one of the watched services.
pub enum SystemStatusToNiri {
    /// UPower's aggregate battery, or `None` when no battery is present.
    Battery(Option<BatteryStatus>),
    /// NetworkManager's primary-connection state.
    Network(NetworkStatus),
    /// power-profiles-daemon's profile state (hidden when the daemon is absent).
    PowerProfiles(PowerProfileStatus),
    /// BlueZ's adapter + device snapshot (absent default when bluetoothd is gone).
    Bluetooth(BluetoothStatus),
    /// A `Device1.Connect`/`Disconnect` we issued finished (either way) — clears the row's
    /// busy mark ([`super::bluez::connect_device`]).
    BluetoothConnectDone(String),
}

type Props = HashMap<String, zvariant::OwnedValue>;

pub fn start(
    to_niri: calloop::channel::Sender<SystemStatusToNiri>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;
    let async_conn = conn.inner().clone();

    // --- UPower battery (aggregate DisplayDevice) ---
    {
        let to_niri = to_niri.clone();
        let async_conn = async_conn.clone();
        let future = async move {
            let proxy = match fdo::PropertiesProxy::new(
                &async_conn,
                "org.freedesktop.UPower",
                "/org/freedesktop/UPower/devices/DisplayDevice",
            )
            .await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating UPower PropertiesProxy: {err:?}");
                    return;
                }
            };
            let iface = InterfaceName::try_from("org.freedesktop.UPower.Device").unwrap();

            let mut changed = match proxy.receive_properties_changed().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to UPower PropertiesChanged: {err:?}");
                    return;
                }
            };

            let mut last: Option<Option<BatteryStatus>> = None;
            loop {
                if let Ok(props) = proxy.get_all(iface.clone()).await {
                    let battery = read_battery(&props);
                    if last.as_ref() != Some(&battery) {
                        last = Some(battery.clone());
                        if to_niri.send(SystemStatusToNiri::Battery(battery)).is_err() {
                            return;
                        }
                    }
                }
                if changed.next().await.is_none() {
                    return;
                }
            }
        };
        conn.inner()
            .executor()
            .spawn(future, "monitor UPower battery")
            .detach();
    }

    // --- NetworkManager primary connection ---
    {
        let to_niri = to_niri.clone();
        let async_conn = async_conn.clone();
        let future = async move {
            let proxy = match fdo::PropertiesProxy::new(
                &async_conn,
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
            )
            .await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating NetworkManager PropertiesProxy: {err:?}");
                    return;
                }
            };
            let iface = InterfaceName::try_from("org.freedesktop.NetworkManager").unwrap();

            let mut changed = match proxy.receive_properties_changed().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to NetworkManager PropertiesChanged: {err:?}");
                    return;
                }
            };

            let mut last: Option<NetworkStatus> = None;
            loop {
                if let Ok(props) = proxy.get_all(iface.clone()).await {
                    let network = read_network(&props);
                    if last != Some(network) {
                        last = Some(network);
                        if to_niri.send(SystemStatusToNiri::Network(network)).is_err() {
                            return;
                        }
                    }
                }
                if changed.next().await.is_none() {
                    return;
                }
            }
        };
        conn.inner()
            .executor()
            .spawn(future, "monitor NetworkManager state")
            .detach();
    }

    // --- power-profiles-daemon ---
    {
        let to_niri = to_niri.clone();
        let async_conn = async_conn.clone();
        let future = async move {
            let proxy = match fdo::PropertiesProxy::new(
                &async_conn,
                POWER_PROFILES_BUS,
                POWER_PROFILES_PATH,
            )
            .await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating PowerProfiles PropertiesProxy: {err:?}");
                    return;
                }
            };
            let iface = InterfaceName::try_from(POWER_PROFILES_BUS).unwrap();

            let changed = match proxy.receive_properties_changed().await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to PowerProfiles PropertiesChanged: {err:?}");
                    return;
                }
            };

            // power-profiles-daemon may start after us or be absent, and *visibility is owner
            // presence*, so also wake on the bus name gaining/losing an owner and re-read then
            // (the same reason [`crate::dbus::rfkill`] does).
            let dbus = match fdo::DBusProxy::new(&async_conn).await {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating DBusProxy for PowerProfiles name tracking: {err:?}");
                    return;
                }
            };
            let owner_changed = match dbus
                .receive_name_owner_changed_with_args(&[(0, POWER_PROFILES_BUS)])
                .await
            {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("error subscribing to PowerProfiles NameOwnerChanged: {err:?}");
                    return;
                }
            };
            let mut wake =
                futures_util::stream::select(changed.map(|_| ()), owner_changed.map(|_| ()));

            let mut last: Option<PowerProfileStatus> = None;
            loop {
                // A successful `get_all` means the daemon is present (show = true); a failure means
                // it's gone — send the hidden default rather than skipping, so the tile/icon
                // disappear when the daemon dies (NOT rfkill's skip-on-error, which would leave it
                // stuck visible).
                let status = match proxy.get_all(iface.clone()).await {
                    Ok(props) => read_power_profile(&props),
                    Err(_) => PowerProfileStatus::default(),
                };
                if last.as_ref() != Some(&status) {
                    last = Some(status.clone());
                    if to_niri
                        .send(SystemStatusToNiri::PowerProfiles(status))
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
            .spawn(future, "monitor power-profiles-daemon")
            .detach();
    }

    // --- BlueZ (bluetooth adapter + devices) ---
    super::bluez::spawn(&conn, to_niri);

    Ok(conn)
}

/// Set power-profiles-daemon's `ActiveProfile` (a QS click). Fire-and-forget on the connection's
/// executor — a synchronous `Set` on the compositor thread would stall it for the D-Bus timeout.
/// The tile is echo-driven (updates on the daemon's `PropertiesChanged`), not optimistic.
pub fn set_active_profile(conn: &zbus::blocking::Connection, profile: String) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let proxy =
            match fdo::PropertiesProxy::new(&async_conn, POWER_PROFILES_BUS, POWER_PROFILES_PATH)
                .await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating PowerProfiles PropertiesProxy for write: {err:?}");
                    return;
                }
            };
        let iface = InterfaceName::try_from(POWER_PROFILES_BUS).unwrap();
        if let Err(err) = proxy
            .set(iface, "ActiveProfile", zvariant::Value::from(profile))
            .await
        {
            warn!("error setting ActiveProfile: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "set power-profiles-daemon ActiveProfile")
        .detach();
}

fn get_str(props: &Props, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

fn get_u32(props: &Props, key: &str) -> Option<u32> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| u32::try_from(v).ok())
}

fn get_f64(props: &Props, key: &str) -> Option<f64> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| f64::try_from(v).ok())
}

fn get_bool(props: &Props, key: &str) -> Option<bool> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
}

/// Build a [`BatteryStatus`] from UPower's DisplayDevice properties, or `None`
/// when no battery is present.
fn read_battery(props: &Props) -> Option<BatteryStatus> {
    if !get_bool(props, "IsPresent").unwrap_or(false) {
        return None;
    }
    Some(BatteryStatus {
        icon_name: get_str(props, "IconName").unwrap_or_default(),
        percentage: get_f64(props, "Percentage").unwrap_or(0.),
    })
}

/// Map NetworkManager's top-level properties to a [`NetworkStatus`]. Airplane mode is no longer
/// derived here (the old coarse `!WirelessEnabled` proxy) — gsd-rfkill owns it now
/// ([`crate::dbus::rfkill`]); a disconnected primary is simply `Offline`.
fn read_network(props: &Props) -> NetworkStatus {
    // NM_STATE: 70 CONNECTED_GLOBAL, 60 SITE, 50 LOCAL, 40 CONNECTING, 20 DISCONNECTED, 10 ASLEEP.
    let state = get_u32(props, "State").unwrap_or(0);
    let conn_type = get_str(props, "PrimaryConnectionType").unwrap_or_default();

    if state >= 50 {
        if conn_type.starts_with("802-11") {
            // TODO: read the active AP's real strength (PrimaryConnection → Devices
            // → wireless → ActiveAccessPoint.Strength) and subscribe for live
            // updates. Unverifiable on this wired VM, so a fixed "good" bucket for
            // now — the icon still correctly reads "wireless, connected".
            NetworkStatus::Wireless(70)
        } else {
            // 802-3 (wired) and any other connected primary (vpn/tun/bridge).
            NetworkStatus::Wired
        }
    } else {
        NetworkStatus::Offline
    }
}

/// Build a [`PowerProfileStatus`] from power-profiles-daemon's properties. Called only on a
/// successful `get_all`, so the daemon is present → `show = true`. `Profiles` is `aa{sv}` (an array
/// of dicts each carrying a `Profile` string); we keep only the KNOWN ones and **reverse** them
/// (daemon order power-saver→performance → GNOME's performance→power-saver menu order,
/// `powerProfiles.js`).
fn read_power_profile(props: &Props) -> PowerProfileStatus {
    let active = get_str(props, "ActiveProfile").unwrap_or_default();
    let available = props
        .get("Profiles")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<HashMap<String, zvariant::OwnedValue>>::try_from(v).ok())
        .map(|dicts| {
            dicts
                .iter()
                .filter_map(|dict| dict.get("Profile"))
                .filter_map(|v| v.try_clone().ok())
                .filter_map(|v| String::try_from(v).ok())
                .filter_map(|id| KnownProfile::parse(&id))
                .rev()
                .collect()
        })
        .unwrap_or_default();
    PowerProfileStatus {
        active,
        available,
        show: true,
    }
}
