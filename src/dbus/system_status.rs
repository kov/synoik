//! System-bus watcher for the panel status area: network (NetworkManager) and
//! battery (UPower).
//!
//! Mirrors [`crate::dbus::freedesktop_locale1`]: one `Connection::system()` with
//! two tasks on its executor, each subscribing to a `PropertiesChanged` stream and
//! pushing a fresh [`crate::system_status`] snapshot to the compositor over a
//! calloop channel. The panel turns those into icons ([`crate::system_status::network_icon`]
//! / [`battery_icon`](crate::system_status::battery_icon)).

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::names::InterfaceName;
use zbus::{fdo, zvariant};

use crate::system_status::{BatteryStatus, NetworkStatus};

/// A status update from one of the two watched services.
pub enum SystemStatusToNiri {
    /// UPower's aggregate battery, or `None` when no battery is present.
    Battery(Option<BatteryStatus>),
    /// NetworkManager's primary-connection state.
    Network(NetworkStatus),
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

    Ok(conn)
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

/// Map NetworkManager's top-level properties to a [`NetworkStatus`].
fn read_network(props: &Props) -> NetworkStatus {
    // NM_STATE: 70 CONNECTED_GLOBAL, 60 SITE, 50 LOCAL, 40 CONNECTING, 20 DISCONNECTED, 10 ASLEEP.
    let state = get_u32(props, "State").unwrap_or(0);
    let conn_type = get_str(props, "PrimaryConnectionType").unwrap_or_default();
    let wireless_enabled = get_bool(props, "WirelessEnabled").unwrap_or(true);

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
    } else if !wireless_enabled {
        NetworkStatus::Airplane
    } else {
        NetworkStatus::Offline
    }
}
