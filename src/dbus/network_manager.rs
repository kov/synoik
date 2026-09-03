// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! System-bus watcher for NetworkManager, feeding the quick-settings network tiles.
//!
//! gnome-shell reaches NM through libnm's `NM.Client`, which is itself an `ObjectManager` client
//! with generated proxies; this is the GObject-free equivalent, and the same shape as
//! [`super::bluez`]: one `GetManagedObjects` snapshot of `/org/freedesktop`, rebuilt whole on any
//! wake, plus the two writes the menu needs.
//!
//! # Why the snapshot is coalesced
//!
//! A radio in range of a dozen routers publishes a `PropertiesChanged` for every strength change,
//! several a second. The bluez watcher rebuilds per wake because BlueZ is quiet; here the wakes
//! are drained first, so a burst costs one rebuild.
//!
//! # The one thing the snapshot does not carry
//!
//! A `Settings.Connection` exports no settings as properties — `GetSettings` is a method call. So
//! profiles are read once and cached by object path, and the cache is dropped for a path when the
//! connection announces `Updated` or leaves the tree. Without the cache every strength change
//! would fan out into one call per saved profile.

use std::collections::{HashMap, HashSet};

use futures_util::{FutureExt as _, StreamExt};
use zbus::{fdo, zvariant};

use super::system_status::SystemStatusToSynoik;
use crate::network_model::{
    self, active_state, device_type, AccessPoint, NetworkDevice, NetworkState, SavedConnection,
    VpnConnection, WirelessDevice,
};
use crate::system_status::NetworkStatus;

pub const NM_BUS: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const SETTINGS_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";

type Props = HashMap<String, zvariant::OwnedValue>;
type Objects = HashMap<zvariant::OwnedObjectPath, HashMap<zbus::names::OwnedInterfaceName, Props>>;

fn get_u32(props: &Props, key: &str) -> Option<u32> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| u32::try_from(v).ok())
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

fn get_bytes(props: &Props, key: &str) -> Option<Vec<u8>> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<u8>::try_from(v).ok())
}

fn get_path(props: &Props, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zvariant::OwnedObjectPath::try_from(v).ok())
        .map(|p| p.to_string())
        // NM writes "/" for "no object", which is not a path we can look anything up by.
        .filter(|p| p != "/")
}

fn get_paths(props: &Props, key: &str) -> Vec<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<zvariant::OwnedObjectPath>::try_from(v).ok())
        .map(|paths| paths.iter().map(|p| p.to_string()).collect())
        .unwrap_or_default()
}

fn iface<'a>(
    ifaces: &'a HashMap<zbus::names::OwnedInterfaceName, Props>,
    name: &str,
) -> Option<&'a Props> {
    ifaces
        .iter()
        .find(|(k, _)| k.as_str() == name)
        .map(|(_, v)| v)
}

/// A `Settings.Connection`'s settings, as far as the menu cares. `GetSettings` returns
/// `a{sa{sv}}`: setting name → key → value, with secrets stripped.
fn read_settings(
    path: &str,
    settings: &HashMap<String, HashMap<String, zvariant::OwnedValue>>,
) -> SavedConnection {
    let connection = settings.get("connection");
    let kind = connection
        .and_then(|c| get_str(c, "type"))
        .unwrap_or_default();
    let wireless = settings.get("802-11-wireless");
    SavedConnection {
        path: path.to_string(),
        uuid: connection
            .and_then(|c| get_str(c, "uuid"))
            .unwrap_or_default(),
        id: connection
            .and_then(|c| get_str(c, "id"))
            .unwrap_or_default(),
        ssid: wireless.and_then(|w| get_bytes(w, "ssid")),
        wireless_mode: wireless.and_then(|w| get_str(w, "mode")),
        kind,
    }
}

/// Whether a saved profile is a port of another connection, which the VPN list must skip.
fn is_port(settings: &HashMap<String, HashMap<String, zvariant::OwnedValue>>) -> bool {
    settings.get("connection").is_some_and(|c| {
        get_str(c, "master").is_some_and(|m| !m.is_empty())
            || get_str(c, "controller").is_some_and(|m| !m.is_empty())
    })
}

/// Build the whole model from one `GetManagedObjects` snapshot plus the cached profile settings.
fn read_state(
    objects: &Objects,
    profiles: &HashMap<String, (SavedConnection, bool)>,
) -> NetworkState {
    let manager = objects
        .iter()
        .find(|(p, _)| p.as_str() == NM_PATH)
        .and_then(|(_, ifaces)| iface(ifaces, NM_IFACE));
    let Some(manager) = manager else {
        return NetworkState::default();
    };

    // Active connections, indexed by the Settings.Connection they instantiate — that is the key
    // both a device and a VPN row look themselves up by.
    let mut active_by_profile: HashMap<String, u32> = HashMap::new();
    let mut active_by_path: HashMap<String, (String, u32)> = HashMap::new();
    for (path, ifaces) in objects {
        let Some(props) = iface(ifaces, ACTIVE_IFACE) else {
            continue;
        };
        let Some(profile) = get_path(props, "Connection") else {
            continue;
        };
        let state = get_u32(props, "State").unwrap_or(active_state::UNKNOWN);
        active_by_profile.insert(profile.clone(), state);
        active_by_path.insert(path.to_string(), (profile, state));
    }

    let mut devices = Vec::new();
    for (path, ifaces) in objects {
        let Some(props) = iface(ifaces, DEVICE_IFACE) else {
            continue;
        };
        let path = path.to_string();
        let kind = get_u32(props, "DeviceType").unwrap_or(0);
        let (active_connection, active_state) = get_path(props, "ActiveConnection")
            .and_then(|p| active_by_path.get(&p).cloned())
            .map_or((None, active_state::UNKNOWN), |(profile, state)| {
                (Some(profile), state)
            });
        let available: Vec<String> = get_paths(props, "AvailableConnections");
        let state = get_u32(props, "State").unwrap_or(0);
        let interface = get_str(props, "Interface").unwrap_or_default();

        let wireless = iface(ifaces, WIRELESS_IFACE).map(|w| {
            let ap_paths: HashSet<String> = get_paths(w, "AccessPoints").into_iter().collect();
            let mut access_points: Vec<AccessPoint> = objects
                .iter()
                .filter(|(p, _)| ap_paths.contains(&p.to_string()))
                .filter_map(|(p, ifaces)| {
                    let ap = iface(ifaces, AP_IFACE)?;
                    Some(AccessPoint {
                        path: p.to_string(),
                        ssid: get_bytes(ap, "Ssid").unwrap_or_default(),
                        strength: get_u32(ap, "Strength").unwrap_or(0).min(100) as u8,
                        mode: get_u32(ap, "Mode").unwrap_or(0),
                        flags: get_u32(ap, "Flags").unwrap_or(0),
                        wpa_flags: get_u32(ap, "WpaFlags").unwrap_or(0),
                        rsn_flags: get_u32(ap, "RsnFlags").unwrap_or(0),
                    })
                })
                .collect();
            access_points.sort_by(|a, b| a.path.cmp(&b.path));
            WirelessDevice {
                path: path.clone(),
                interface: interface.clone(),
                state,
                capabilities: get_u32(w, "WirelessCapabilities").unwrap_or(0),
                access_points,
                active_access_point: get_path(w, "ActiveAccessPoint"),
                active_connection: active_connection.clone(),
                available_connections: available.clone(),
                // A hotspot is a profile that shares its IPv4 config; the settings cache is what
                // knows that (`get is_hotspot`, `network.js:1178-1191`).
                hotspot: active_connection
                    .as_ref()
                    .and_then(|p| profiles.get(p))
                    .is_some_and(|(_, shared)| *shared),
            }
        });

        devices.push(NetworkDevice {
            path,
            kind,
            interface,
            state,
            active_connection,
            active_state,
            wireless,
        });
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));

    let mut saved: Vec<SavedConnection> = profiles.values().map(|(c, _)| c.clone()).collect();
    saved.sort_by(|a, b| a.path.cmp(&b.path));

    let mut vpn: Vec<VpnConnection> = saved
        .iter()
        .filter(|c| c.kind == "vpn" || c.kind == "wireguard")
        .map(|c| VpnConnection {
            path: c.path.clone(),
            uuid: c.uuid.clone(),
            id: c.id.clone(),
            kind: c.kind.clone(),
            state: active_by_profile
                .get(&c.path)
                .copied()
                .unwrap_or(active_state::UNKNOWN),
        })
        .collect();
    vpn.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.path.cmp(&b.path)));

    NetworkState {
        running: true,
        networking_enabled: get_bool(manager, "NetworkingEnabled").unwrap_or(false),
        wireless_enabled: get_bool(manager, "WirelessEnabled").unwrap_or(false),
        wireless_hardware_enabled: get_bool(manager, "WirelessHardwareEnabled").unwrap_or(false),
        connectivity: get_u32(manager, "Connectivity").unwrap_or(0),
        devices,
        saved,
        vpn,
    }
}

/// The panel icon's coarse state, derived from the same snapshot that feeds the tiles — so the
/// wireless bars are the live signal rather than a fixed bucket.
pub fn status_of(state: &NetworkState, manager: Option<&Props>) -> NetworkStatus {
    let Some(manager) = manager else {
        return NetworkStatus::Unknown;
    };
    // NM_STATE: 70 CONNECTED_GLOBAL, 60 SITE, 50 LOCAL, 40 CONNECTING, 20 DISCONNECTED.
    if get_u32(manager, "State").unwrap_or(0) < 50 {
        return NetworkStatus::Offline;
    }
    let kind = get_str(manager, "PrimaryConnectionType").unwrap_or_default();
    if !kind.starts_with("802-11") {
        return NetworkStatus::Wired;
    }
    // The strength of the AP the primary wireless device is actually on.
    let strength = state
        .devices
        .iter()
        .filter_map(|d| d.wireless.as_ref())
        .find(|w| w.active_access_point.is_some())
        .and_then(|w| {
            let active = w.active_access_point.as_deref()?;
            w.access_points
                .iter()
                .find(|ap| ap.path == active)
                .map(|ap| ap.strength)
        })
        .unwrap_or(0);
    NetworkStatus::Wireless(strength)
}

/// Spawn the NetworkManager monitor on the shared system-bus connection.
pub(super) fn spawn(
    conn: &zbus::blocking::Connection,
    to_niri: calloop::channel::Sender<SystemStatusToSynoik>,
) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let object_manager = match fdo::ObjectManagerProxy::builder(&async_conn)
            .destination(NM_BUS)
            .and_then(|b| b.path("/org/freedesktop"))
        {
            Ok(builder) => match builder.build().await {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating NetworkManager ObjectManagerProxy: {err:?}");
                    return;
                }
            },
            Err(err) => {
                warn!("error building NetworkManager ObjectManagerProxy: {err:?}");
                return;
            }
        };

        let Ok(added) = object_manager.receive_interfaces_added().await else {
            warn!("error subscribing to NetworkManager InterfacesAdded");
            return;
        };
        let Ok(removed) = object_manager.receive_interfaces_removed().await else {
            warn!("error subscribing to NetworkManager InterfacesRemoved");
            return;
        };

        // One rule for every object under NM: the manager, devices, APs, active connections and
        // the saved profiles alike.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(NM_BUS)
            .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
            .and_then(|b| b.member("PropertiesChanged"))
            .and_then(|b| b.path_namespace("/org/freedesktop/NetworkManager"))
            .map(|b| b.build());
        let Ok(rule) = rule else {
            warn!("error building the NetworkManager PropertiesChanged match rule");
            return;
        };
        let Ok(props_changed) =
            zbus::MessageStream::for_match_rule(rule, &async_conn, Some(64)).await
        else {
            warn!("error subscribing to NetworkManager PropertiesChanged");
            return;
        };

        // A saved profile announces an edit with its own signal, not PropertiesChanged.
        let updated = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(NM_BUS)
            .and_then(|b| b.interface(SETTINGS_CONNECTION_IFACE))
            .and_then(|b| b.member("Updated"))
            .map(|b| b.build());
        let Ok(updated) = updated else {
            warn!("error building the NetworkManager Connection.Updated match rule");
            return;
        };
        let Ok(mut settings_updated) =
            zbus::MessageStream::for_match_rule(updated, &async_conn, Some(16)).await
        else {
            warn!("error subscribing to NetworkManager Connection.Updated");
            return;
        };

        let dbus = match fdo::DBusProxy::new(&async_conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating DBusProxy for NetworkManager name tracking: {err:?}");
                return;
            }
        };
        let Ok(owner_changed) = dbus
            .receive_name_owner_changed_with_args(&[(0, NM_BUS)])
            .await
        else {
            warn!("error subscribing to NetworkManager NameOwnerChanged");
            return;
        };

        /// What woke the loop: an edited profile has to leave the settings cache, everything else
        /// is answered by the snapshot alone.
        enum Wake {
            Snapshot,
            /// An edited or vanished profile, by object path.
            Profile(String),
        }

        let mut wake = futures_util::stream::select(
            futures_util::stream::select(
                added.map(|_| Wake::Snapshot),
                removed.map(|msg| {
                    msg.args()
                        .map(|args| Wake::Profile(args.object_path().to_string()))
                        .unwrap_or(Wake::Snapshot)
                }),
            ),
            futures_util::stream::select(
                props_changed.map(|_| Wake::Snapshot),
                owner_changed.map(|_| Wake::Snapshot),
            ),
        );

        // path → (profile, is_hotspot). Populated lazily; entries die when NM says the profile
        // changed or went away.
        let mut profiles: HashMap<String, (SavedConnection, bool)> = HashMap::new();
        let mut last: Option<NetworkState> = None;
        let mut last_status: Option<NetworkStatus> = None;
        loop {
            let objects = object_manager.get_managed_objects().await.ok();
            let objects = objects.unwrap_or_default();

            // Fill the settings cache for profiles we have not read yet, and evict the ones NM no
            // longer exports.
            let present: HashSet<String> = objects
                .iter()
                .filter(|(_, ifaces)| iface(ifaces, SETTINGS_CONNECTION_IFACE).is_some())
                .map(|(p, _)| p.to_string())
                .collect();
            profiles.retain(|path, _| present.contains(path));
            for path in &present {
                if profiles.contains_key(path) {
                    continue;
                }
                let settings = async_conn
                    .call_method(
                        Some(NM_BUS),
                        path.as_str(),
                        Some(SETTINGS_CONNECTION_IFACE),
                        "GetSettings",
                        &(),
                    )
                    .await
                    .ok()
                    .and_then(|reply| {
                        reply
                            .body()
                            .deserialize::<HashMap<String, HashMap<String, zvariant::OwnedValue>>>()
                            .ok()
                    });
                let Some(settings) = settings else { continue };
                if is_port(&settings) {
                    continue;
                }
                let shared = settings
                    .get("ipv4")
                    .and_then(|ip| get_str(ip, "method"))
                    .is_some_and(|m| m == "shared");
                profiles.insert(path.clone(), (read_settings(path, &settings), shared));
            }

            let manager_props = objects
                .iter()
                .find(|(p, _)| p.as_str() == NM_PATH)
                .and_then(|(_, ifaces)| iface(ifaces, NM_IFACE))
                .cloned();
            let state = read_state(&objects, &profiles);
            let status = status_of(&state, manager_props.as_ref());

            if last_status != Some(status) {
                last_status = Some(status);
                if to_niri.send(SystemStatusToSynoik::Network(status)).is_err() {
                    return;
                }
            }
            if last.as_ref() != Some(&state) {
                last = Some(state.clone());
                if to_niri
                    .send(SystemStatusToSynoik::NetworkModel(Box::new(state)))
                    .is_err()
                {
                    return;
                }
            }

            // Block for the next wake, then take every other one that is already queued: a
            // scanning radio produces a burst, and one rebuild answers all of it.
            let Some(first) = wake.next().await else {
                return;
            };
            let mut wakes = vec![first];
            while let Some(Some(next)) = wake.next().now_or_never() {
                wakes.push(next);
            }
            while let Some(Some(Ok(msg))) = settings_updated.next().now_or_never() {
                if let Some(path) = msg.header().path() {
                    profiles.remove(&path.to_string());
                }
            }
            for w in wakes {
                if let Wake::Profile(path) = w {
                    profiles.remove(&path);
                }
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "monitor NetworkManager")
        .detach();
}

/// Turn the Wi-Fi radio on or off (`client.wireless_enabled = …`, `network.js:1826-1830`). A
/// property write, echo-driven like every other quick-settings write.
pub fn set_wireless_enabled(conn: &zbus::blocking::Connection, enabled: bool) {
    let async_conn = conn.inner().clone();
    let future = async move {
        if let Err(err) = async_conn
            .call_method(
                Some(NM_BUS),
                NM_PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(NM_IFACE, "WirelessEnabled", zvariant::Value::from(enabled)),
            )
            .await
        {
            warn!("error setting NetworkManager WirelessEnabled: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "set NM WirelessEnabled")
        .detach();
}

/// Ask a wireless device to scan (`RequestScan`, driven by the menu opening —
/// `_startScanning`, `network.js:1866-1874`).
pub fn request_scan(conn: &zbus::blocking::Connection, device: String) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let options: HashMap<String, zvariant::Value> = HashMap::new();
        if let Err(err) = async_conn
            .call_method(
                Some(NM_BUS),
                device.as_str(),
                Some(WIRELESS_IFACE),
                "RequestScan",
                &(options,),
            )
            .await
        {
            // Scans are rate-limited by NM and refusing one is routine, not an error worth a
            // warning every 15 s.
            debug!("NetworkManager refused a scan on {device}: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "NM RequestScan")
        .detach();
}

/// Bring a saved profile up on a device (`ActivateConnection`). `device` may be empty for a VPN,
/// which NM places itself.
pub fn activate_connection(conn: &zbus::blocking::Connection, profile: String, device: String) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let device = zvariant::ObjectPath::try_from(if device.is_empty() { "/" } else { &device })
            .unwrap_or_else(|_| zvariant::ObjectPath::from_static_str_unchecked("/"));
        let profile_path = match zvariant::ObjectPath::try_from(profile.as_str()) {
            Ok(path) => path,
            Err(err) => {
                warn!("not a connection path: {profile}: {err:?}");
                return;
            }
        };
        let specific = zvariant::ObjectPath::from_static_str_unchecked("/");
        if let Err(err) = async_conn
            .call_method(
                Some(NM_BUS),
                NM_PATH,
                Some(NM_IFACE),
                "ActivateConnection",
                &(profile_path, device, specific),
            )
            .await
        {
            warn!("error activating {profile}: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "NM ActivateConnection")
        .detach();
}

/// Take a VPN back down (`deactivateConnection`, `network.js:1636-1638`). Takes the *active*
/// connection path, which is what NM's `DeactivateConnection` wants — so it is looked up here
/// from the profile the row knows.
pub fn deactivate_profile(conn: &zbus::blocking::Connection, profile: String) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let active = async_conn
            .call_method(
                Some(NM_BUS),
                NM_PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(NM_IFACE, "ActiveConnections"),
            )
            .await
            .ok()
            .and_then(|reply| reply.body().deserialize::<zvariant::OwnedValue>().ok())
            .and_then(|v| Vec::<zvariant::OwnedObjectPath>::try_from(v).ok())
            .unwrap_or_default();

        for path in active {
            let owner = async_conn
                .call_method(
                    Some(NM_BUS),
                    &path,
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &(ACTIVE_IFACE, "Connection"),
                )
                .await
                .ok()
                .and_then(|reply| reply.body().deserialize::<zvariant::OwnedValue>().ok())
                .and_then(|v| zvariant::OwnedObjectPath::try_from(v).ok());
            if owner.map(|o| o.to_string()).as_deref() != Some(profile.as_str()) {
                continue;
            }
            if let Err(err) = async_conn
                .call_method(
                    Some(NM_BUS),
                    NM_PATH,
                    Some(NM_IFACE),
                    "DeactivateConnection",
                    &(&path,),
                )
                .await
            {
                warn!("error deactivating {profile}: {err:?}");
            }
            return;
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "NM DeactivateConnection")
        .detach();
}

/// Join a network we have no profile for (`AddAndActivateConnection` with an empty settings map —
/// NM fills the rest in from the AP, `network.js:938-950`).
///
/// **Divergence, deliberate.** gnome-shell asks polkit whether the user may write a *system*
/// connection and, when they may not, adds a `connection.permissions = ["user:<name>"]` so the
/// profile is theirs alone. We always add the permission: an unshared profile is the safe answer
/// either way, and it saves a synchronous polkit round trip on the click. The cost is that a user
/// who *could* have made a machine-wide profile gets a personal one instead.
pub fn add_and_activate(conn: &zbus::blocking::Connection, device: String, ap: String) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let user = std::env::var("USER").unwrap_or_default();
        let mut connection: HashMap<&str, zvariant::Value> = HashMap::new();
        if !user.is_empty() {
            connection.insert(
                "permissions",
                zvariant::Value::from(vec![format!("user:{user}:")]),
            );
        }
        let mut settings: HashMap<&str, HashMap<&str, zvariant::Value>> = HashMap::new();
        if !connection.is_empty() {
            settings.insert("connection", connection);
        }

        let device_path = match zvariant::ObjectPath::try_from(device.as_str()) {
            Ok(path) => path,
            Err(err) => {
                warn!("not a device path: {device}: {err:?}");
                return;
            }
        };
        let ap_path = match zvariant::ObjectPath::try_from(ap.as_str()) {
            Ok(path) => path,
            Err(err) => {
                warn!("not an access-point path: {ap}: {err:?}");
                return;
            }
        };
        if let Err(err) = async_conn
            .call_method(
                Some(NM_BUS),
                NM_PATH,
                Some(NM_IFACE),
                "AddAndActivateConnection",
                &(settings, device_path, ap_path),
            )
            .await
        {
            warn!("error joining {ap}: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "NM AddAndActivateConnection")
        .detach();
}

/// The device types that get their own tile, in grid order.
pub const TILE_DEVICE_TYPES: [u32; 2] = [device_type::WIFI, device_type::ETHERNET];

/// Re-exported so callers do not need the model's module path for the one constant they use.
pub use network_model::device_type as kinds;

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the *live* NetworkManager on this machine and prints the model it produces.
    ///
    /// Ignored, like every test that needs a system daemon: run it with
    /// `cargo test -p synoik network_manager -- --ignored --nocapture` to see what a real tree
    /// turns into. It asserts only what must hold on any host with NM running, so it stays honest
    /// on a machine with no radio (which is where this port was written).
    #[test]
    #[ignore = "needs a running NetworkManager on the system bus"]
    fn the_live_tree_reads_as_a_model() {
        let conn = zbus::blocking::Connection::system().expect("system bus");
        let manager = zbus::blocking::fdo::ObjectManagerProxy::builder(&conn)
            .destination(NM_BUS)
            .unwrap()
            .path("/org/freedesktop")
            .unwrap()
            .build()
            .expect("NM ObjectManager");
        let objects = manager.get_managed_objects().expect("GetManagedObjects");

        let mut profiles = HashMap::new();
        for (path, ifaces) in &objects {
            if iface(ifaces, SETTINGS_CONNECTION_IFACE).is_none() {
                continue;
            }
            let path = path.to_string();
            let settings: HashMap<String, HashMap<String, zvariant::OwnedValue>> = conn
                .call_method(
                    Some(NM_BUS),
                    path.as_str(),
                    Some(SETTINGS_CONNECTION_IFACE),
                    "GetSettings",
                    &(),
                )
                .expect("GetSettings")
                .body()
                .deserialize()
                .expect("settings dict");
            if is_port(&settings) {
                continue;
            }
            let shared = settings
                .get("ipv4")
                .and_then(|ip| get_str(ip, "method"))
                .is_some_and(|m| m == "shared");
            profiles.insert(path.clone(), (read_settings(&path, &settings), shared));
        }

        let state = read_state(&objects, &profiles);
        println!(
            "running={} networking={}",
            state.running, state.networking_enabled
        );
        for device in &state.devices {
            println!(
                "device {} kind={} state={} active={:?}",
                device.interface, device.kind, device.state, device.active_connection
            );
            if let Some(wireless) = &device.wireless {
                for network in wireless.networks(&state.saved) {
                    println!(
                        "  {} {:?} strength={} known={} active={}",
                        network.label(),
                        network.security,
                        network.strength(),
                        network.has_connections(),
                        network.active
                    );
                }
            }
        }
        for profile in &state.saved {
            println!("saved {} ({})", profile.id, profile.kind);
        }
        for vpn in &state.vpn {
            println!("vpn {} state={}", vpn.id, vpn.state);
        }

        assert!(
            state.running,
            "NM answered, so the model must read as running"
        );
        assert!(
            !state.devices.is_empty(),
            "a machine on the network has at least one device"
        );
    }
}
