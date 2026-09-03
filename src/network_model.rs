// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The quick-settings network model: NetworkManager's objects, grouped the way the shell's menu
//! shows them.
//!
//! Pure data. The watcher ([`crate::dbus::network_manager`]) fills these in from the bus and a
//! `debug-*` action can fill them in from nothing, which is the only way to exercise Wi-Fi on a
//! machine with no radio.
//!
//! # What a row is
//!
//! **A row is a network, never an access point.** GNOME groups every AP that shares an
//! SSID + mode + security type into one [`WirelessNetwork`] (`WirelessNetwork.checkAccessPoint`,
//! `js/ui/status/network.js:829-841`), because a router with three radios is one thing to join.
//! The strongest AP in the group ("best AP") supplies the signal icon.
//!
//! # Security
//!
//! The security type is not a field NM hands out — it is *derived* by testing each type against
//! the AP's beacon flags and the device's capabilities and keeping the highest that fits
//! (`_getApSecurityType`, `:976-984`, over `nm_utils_security_valid`,
//! `NetworkManager/src/libnm-core-impl/nm-utils.c:1023-1180`). "Highest" is the enum's own order,
//! which runs weakest-to-strongest, so WPA2 beats WPA on an AP advertising both.

use std::collections::BTreeMap;

/// `NM80211ApFlags` (`nm-dbus-interface.h:366-370`).
pub mod ap_flags {
    pub const PRIVACY: u32 = 0x1;
    pub const WPS: u32 = 0x2;
    pub const WPS_PBC: u32 = 0x4;
}

/// `NM80211ApSecurityFlags` (`nm-dbus-interface.h:407-421`).
pub mod ap_sec {
    pub const PAIR_WEP40: u32 = 0x1;
    pub const PAIR_WEP104: u32 = 0x2;
    pub const PAIR_TKIP: u32 = 0x4;
    pub const PAIR_CCMP: u32 = 0x8;
    pub const GROUP_WEP40: u32 = 0x10;
    pub const GROUP_WEP104: u32 = 0x20;
    pub const GROUP_TKIP: u32 = 0x40;
    pub const GROUP_CCMP: u32 = 0x80;
    pub const KEY_MGMT_PSK: u32 = 0x100;
    pub const KEY_MGMT_802_1X: u32 = 0x200;
    pub const KEY_MGMT_SAE: u32 = 0x400;
    pub const KEY_MGMT_OWE: u32 = 0x800;
    pub const KEY_MGMT_OWE_TM: u32 = 0x1000;
    pub const KEY_MGMT_EAP_SUITE_B_192: u32 = 0x2000;
}

/// `NMDeviceWifiCapabilities` (`nm-dbus-interface.h:337-351`).
pub mod wifi_caps {
    pub const CIPHER_WEP40: u32 = 0x1;
    pub const CIPHER_WEP104: u32 = 0x2;
    pub const CIPHER_TKIP: u32 = 0x4;
    pub const CIPHER_CCMP: u32 = 0x8;
    pub const WPA: u32 = 0x10;
    pub const RSN: u32 = 0x20;
    pub const IBSS_RSN: u32 = 0x2000;
}

/// `NM80211Mode` (`nm-dbus-interface.h:441-445`).
pub mod mode {
    pub const UNKNOWN: u32 = 0;
    pub const ADHOC: u32 = 1;
    pub const INFRA: u32 = 2;
    pub const AP: u32 = 3;
    pub const MESH: u32 = 4;
}

/// `NMDeviceType` — only the ones that get a quick-settings tile (`nm-dbus-interface.h:261-268`).
pub mod device_type {
    pub const ETHERNET: u32 = 1;
    pub const WIFI: u32 = 2;
    pub const BT: u32 = 5;
    pub const MODEM: u32 = 8;
}

/// `NMDeviceState` (`nm-dbus-interface.h:546-558`).
pub mod device_state {
    pub const UNKNOWN: u32 = 0;
    pub const UNMANAGED: u32 = 10;
    pub const UNAVAILABLE: u32 = 20;
    pub const DISCONNECTED: u32 = 30;
    pub const PREPARE: u32 = 40;
    pub const CONFIG: u32 = 50;
    pub const NEED_AUTH: u32 = 60;
    pub const IP_CONFIG: u32 = 70;
    pub const IP_CHECK: u32 = 80;
    pub const SECONDARIES: u32 = 90;
    pub const ACTIVATED: u32 = 100;
    pub const DEACTIVATING: u32 = 110;
    pub const FAILED: u32 = 120;
}

/// `NMUtilsSecurityType` (`nm-utils.h:60-73`). The **declaration order is the strength order** —
/// `_getApSecurityType` sorts the values descending and takes the first that validates, so a
/// variant added in the middle changes which security an AP reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SecurityType {
    /// Nothing validated: the AP is not joinable by this device at all.
    #[default]
    Invalid,
    None,
    StaticWep,
    Leap,
    DynamicWep,
    WpaPsk,
    WpaEnterprise,
    Wpa2Psk,
    Wpa2Enterprise,
    Sae,
    Owe,
    Wpa3SuiteB192,
}

/// Strongest first, as `WirelessNetwork._securityTypes` (`network.js:763-764`) has them.
const SECURITY_TYPES: [SecurityType; 11] = [
    SecurityType::Wpa3SuiteB192,
    SecurityType::Owe,
    SecurityType::Sae,
    SecurityType::Wpa2Enterprise,
    SecurityType::Wpa2Psk,
    SecurityType::WpaEnterprise,
    SecurityType::WpaPsk,
    SecurityType::DynamicWep,
    SecurityType::Leap,
    SecurityType::StaticWep,
    SecurityType::None,
];

/// `device_supports_ap_ciphers` (`nm-utils.c:912-954`): the device needs at least one pairwise and
/// one group cipher in common with the AP. Static WEP uses group ciphers only, so its pairwise
/// half is a free pass.
fn supports_ap_ciphers(caps: u32, ap: u32, static_wep: bool) -> bool {
    let pair = static_wep
        || (caps & wifi_caps::CIPHER_WEP40 != 0 && ap & ap_sec::PAIR_WEP40 != 0)
        || (caps & wifi_caps::CIPHER_WEP104 != 0 && ap & ap_sec::PAIR_WEP104 != 0)
        || (caps & wifi_caps::CIPHER_TKIP != 0 && ap & ap_sec::PAIR_TKIP != 0)
        || (caps & wifi_caps::CIPHER_CCMP != 0 && ap & ap_sec::PAIR_CCMP != 0);
    let group = (caps & wifi_caps::CIPHER_WEP40 != 0 && ap & ap_sec::GROUP_WEP40 != 0)
        || (caps & wifi_caps::CIPHER_WEP104 != 0 && ap & ap_sec::GROUP_WEP104 != 0)
        || (!static_wep
            && ((caps & wifi_caps::CIPHER_TKIP != 0 && ap & ap_sec::GROUP_TKIP != 0)
                || (caps & wifi_caps::CIPHER_CCMP != 0 && ap & ap_sec::GROUP_CCMP != 0)));
    pair && group
}

impl SecurityType {
    /// `nm_utils_security_valid` (`nm-utils.c:1023-1180`), with `have_ap` always true — we only
    /// ever ask about a beacon we have seen.
    pub fn valid(self, caps: u32, adhoc: bool, flags: u32, wpa: u32, rsn: u32) -> bool {
        let privacy = flags & ap_flags::PRIVACY != 0;
        match self {
            SecurityType::Invalid => false,
            SecurityType::None => !privacy && wpa == 0 && rsn == 0,
            SecurityType::Leap if adhoc => false,
            SecurityType::Leap | SecurityType::StaticWep => {
                if !privacy {
                    return false;
                }
                if wpa != 0 || rsn != 0 {
                    return supports_ap_ciphers(caps, wpa, true)
                        || supports_ap_ciphers(caps, rsn, true);
                }
                true
            }
            SecurityType::DynamicWep => {
                if adhoc || rsn != 0 || !privacy {
                    return false;
                }
                // Some APs broadcast minimal WPA-enabled beacons that must be handled.
                if wpa != 0 {
                    return wpa & ap_sec::KEY_MGMT_802_1X != 0
                        && supports_ap_ciphers(caps, wpa, false);
                }
                true
            }
            SecurityType::WpaPsk => {
                !adhoc
                    && caps & wifi_caps::WPA != 0
                    && wpa & ap_sec::KEY_MGMT_PSK != 0
                    && psk_cipher_ok(caps, wpa)
            }
            SecurityType::Wpa2Psk => {
                if caps & wifi_caps::RSN == 0 {
                    return false;
                }
                if adhoc {
                    return caps & wifi_caps::IBSS_RSN != 0
                        && rsn & ap_sec::PAIR_CCMP != 0
                        && caps & wifi_caps::CIPHER_CCMP != 0;
                }
                rsn & ap_sec::KEY_MGMT_PSK != 0 && psk_cipher_ok(caps, rsn)
            }
            SecurityType::WpaEnterprise => {
                !adhoc
                    && caps & wifi_caps::WPA != 0
                    && wpa & ap_sec::KEY_MGMT_802_1X != 0
                    && supports_ap_ciphers(caps, wpa, false)
            }
            SecurityType::Wpa2Enterprise => {
                !adhoc
                    && caps & wifi_caps::RSN != 0
                    && rsn & ap_sec::KEY_MGMT_802_1X != 0
                    && supports_ap_ciphers(caps, rsn, false)
            }
            SecurityType::Sae => {
                !adhoc
                    && caps & wifi_caps::RSN != 0
                    && rsn & ap_sec::KEY_MGMT_SAE != 0
                    && rsn & ap_sec::PAIR_CCMP != 0
                    && caps & wifi_caps::CIPHER_CCMP != 0
            }
            SecurityType::Owe => {
                !adhoc
                    && caps & wifi_caps::RSN != 0
                    && rsn & (ap_sec::KEY_MGMT_OWE | ap_sec::KEY_MGMT_OWE_TM) != 0
            }
            SecurityType::Wpa3SuiteB192 => {
                !adhoc && caps & wifi_caps::RSN != 0 && rsn & ap_sec::KEY_MGMT_EAP_SUITE_B_192 != 0
            }
        }
    }

    /// Whether the row draws the padlock. OWE is encrypted but joins without a secret, so GNOME
    /// deliberately shows it as open (`get secure`, `network.js:802-806`).
    pub fn secure(self) -> bool {
        !matches!(
            self,
            SecurityType::Invalid | SecurityType::None | SecurityType::Owe
        )
    }

    /// Whether joining needs nothing but a password we could ask for. Enterprise needs a whole
    /// 802.1x profile, which is Settings' job (`canAutoconnect`, `network.js:921-926`).
    pub fn can_autoconnect(self) -> bool {
        !matches!(
            self,
            SecurityType::WpaEnterprise | SecurityType::Wpa2Enterprise
        )
    }
}

/// The WPA/WPA2-PSK cipher test, identical in both branches of `nm_utils_security_valid`.
fn psk_cipher_ok(caps: u32, ap: u32) -> bool {
    (ap & ap_sec::PAIR_TKIP != 0 && caps & wifi_caps::CIPHER_TKIP != 0)
        || (ap & ap_sec::PAIR_CCMP != 0 && caps & wifi_caps::CIPHER_CCMP != 0)
}

/// One beacon, as NM's `AccessPoint` exports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPoint {
    pub path: String,
    /// **Bytes, not a string.** An SSID is 32 arbitrary octets; two different SSIDs can share a
    /// lossy UTF-8 rendering, so grouping keys off the bytes and only the label is decoded.
    pub ssid: Vec<u8>,
    /// 0..=100.
    pub strength: u8,
    pub mode: u32,
    pub flags: u32,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
}

impl AccessPoint {
    /// The strongest security this device can use with this beacon (`_getApSecurityType`).
    pub fn security(&self, caps: u32) -> SecurityType {
        let adhoc = self.mode == mode::ADHOC;
        SECURITY_TYPES
            .into_iter()
            .find(|t| t.valid(caps, adhoc, self.flags, self.wpa_flags, self.rsn_flags))
            .unwrap_or(SecurityType::Invalid)
    }
}

/// A saved profile, from NM's `Settings.Connection` objects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SavedConnection {
    pub path: String,
    pub uuid: String,
    pub id: String,
    /// `connection.type`, e.g. `802-11-wireless`, `802-3-ethernet`, `vpn`, `wireguard`.
    pub kind: String,
    /// `802-11-wireless.ssid`, when this is a Wi-Fi profile.
    pub ssid: Option<Vec<u8>>,
    /// `802-11-wireless.mode` (`infrastructure` / `adhoc` / `ap`), when set.
    pub wireless_mode: Option<String>,
}

impl SavedConnection {
    /// A cut-down `nm_access_point_connection_valid`
    /// (`NetworkManager/src/libnm-client-impl/nm-access-point.c:278-365`): the SSID must match
    /// exactly and the declared mode must not contradict the beacon's.
    ///
    /// **Simplification.** Upstream also filters on BSSID, band, channel and full security
    /// compatibility. Those only ever *narrow* the match, and NM has already narrowed the input:
    /// we only ever test a device's `AvailableConnections`, which NM filters per device. The
    /// visible cost of over-matching is a "known network" mark on a profile that would not in fact
    /// come up here — not a wrong join, since activating a saved profile hands NM the profile and
    /// NM decides.
    pub fn matches_ap(&self, ap: &AccessPoint) -> bool {
        if self.kind != "802-11-wireless" {
            return false;
        }
        if self.ssid.as_deref() != Some(ap.ssid.as_slice()) {
            return false;
        }
        if ap.mode == mode::UNKNOWN {
            return false;
        }
        match self.wireless_mode.as_deref() {
            Some("infrastructure") => ap.mode == mode::INFRA,
            Some("adhoc") => ap.mode == mode::ADHOC,
            // A hotspot profile is device-specific and never matches a scanned AP.
            Some("ap") => false,
            _ => true,
        }
    }
}

/// One row of the Wi-Fi list: every AP sharing an SSID, mode and security type, plus the saved
/// profiles that would join it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessNetwork {
    pub ssid: Vec<u8>,
    pub mode: u32,
    pub security: SecurityType,
    /// Sorted strongest-first, so the head is the "best AP".
    pub access_points: Vec<AccessPoint>,
    /// Object paths of the saved profiles that match, in device order.
    pub connections: Vec<String>,
    /// Whether this is the network the device is on.
    pub active: bool,
}

impl WirelessNetwork {
    /// The best AP's strength — what the row's signal icon and the sort read
    /// (`get signal_strength`, `network.js:786-788`).
    pub fn strength(&self) -> u8 {
        self.access_points.first().map_or(0, |ap| ap.strength)
    }

    /// The label: the SSID decoded as UTF-8, or `<unknown>` when it is not text
    /// (`ssidToLabel`/`NM.utils_ssid_to_utf8`, `network.js:59-64`).
    pub fn label(&self) -> String {
        ssid_to_label(&self.ssid)
    }

    pub fn has_connections(&self) -> bool {
        !self.connections.is_empty()
    }

    /// The signal icon, or the ad-hoc icon (`get icon_name`, `network.js:794-800`).
    pub fn icon_name(&self) -> String {
        if self.mode == mode::ADHOC {
            return "network-workgroup-symbolic".to_string();
        }
        format!(
            "network-wireless-signal-{}-symbolic",
            signal_to_icon(self.strength())
        )
    }

    /// `WirelessNetwork.compare` (`network.js:902-925`), exactly: known profiles first, then
    /// networks that still have a beacon, then stronger, then secure, then by name.
    ///
    /// **`active` is not in this order** — being connected only draws the check mark.
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        other
            .has_connections()
            .cmp(&self.has_connections())
            .then_with(|| {
                other
                    .access_points
                    .is_empty()
                    .cmp(&self.access_points.is_empty())
            })
            .then_with(|| other.strength().cmp(&self.strength()))
            .then_with(|| other.security.secure().cmp(&self.security.secure()))
            .then_with(|| self.label().cmp(&other.label()))
    }
}

/// `signalToIcon` (`network.js:46-57`).
pub fn signal_to_icon(strength: u8) -> &'static str {
    match strength {
        0..=19 => "none",
        20..=39 => "weak",
        40..=49 => "ok",
        50..=79 => "good",
        _ => "excellent",
    }
}

/// `nm_utils_ssid_to_utf8`'s observable half: valid UTF-8 passes through, anything else falls back
/// to the placeholder rather than being mangled into replacement characters.
pub fn ssid_to_label(ssid: &[u8]) -> String {
    match std::str::from_utf8(ssid) {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => "<unknown>".to_string(),
    }
}

/// A Wi-Fi device and everything the menu needs off it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WirelessDevice {
    pub path: String,
    /// The kernel name (`wlp3s0`), the fallback tile subtitle.
    pub interface: String,
    pub state: u32,
    pub capabilities: u32,
    pub access_points: Vec<AccessPoint>,
    /// The AP path the device is associated with, if any.
    pub active_access_point: Option<String>,
    /// The `Settings.Connection` path behind the device's active connection, if any.
    pub active_connection: Option<String>,
    /// `Settings.Connection` paths NM says could be used here.
    pub available_connections: Vec<String>,
    /// Whether the active connection shares its IPv4 config (`ipv4.method == shared`) — a hotspot
    /// (`get is_hotspot`, `network.js:1178-1191`).
    pub hotspot: bool,
}

impl WirelessDevice {
    /// Group this device's beacons into rows, sorted as the menu shows them.
    pub fn networks(&self, saved: &[SavedConnection]) -> Vec<WirelessNetwork> {
        // Keyed by exactly what `checkAccessPoint` compares: SSID bytes, mode, security type. A
        // BTreeMap only to make the pre-sort grouping order deterministic; `compare` decides the
        // order that is shown.
        let mut groups: BTreeMap<(Vec<u8>, u32, SecurityType), Vec<AccessPoint>> = BTreeMap::new();
        for ap in &self.access_points {
            if ap.ssid.is_empty() {
                // "not visible yet": NM publishes the AP before its SSID arrives.
                continue;
            }
            let security = ap.security(self.capabilities);
            if security == SecurityType::Invalid {
                continue;
            }
            groups
                .entry((ap.ssid.clone(), ap.mode, security))
                .or_default()
                .push(ap.clone());
        }

        let active_saved = self.active_connection.as_deref();
        let mut networks: Vec<WirelessNetwork> = groups
            .into_iter()
            .map(|((ssid, mode, security), mut aps)| {
                aps.sort_by(|a, b| {
                    b.strength
                        .cmp(&a.strength)
                        .then_with(|| a.path.cmp(&b.path))
                });
                let connections: Vec<String> = self
                    .available_connections
                    .iter()
                    .filter(|path| {
                        saved
                            .iter()
                            .find(|c| &&c.path == path)
                            .is_some_and(|c| aps.iter().any(|ap| c.matches_ap(ap)))
                    })
                    .cloned()
                    .collect();
                // Active either by beacon or, for a hidden AP, by the profile that is up
                // (`get is_active`, `network.js:809-819`).
                let active = self
                    .active_access_point
                    .as_ref()
                    .is_some_and(|p| aps.iter().any(|ap| &ap.path == p))
                    || active_saved.is_some_and(|p| connections.iter().any(|c| c == p));
                WirelessNetwork {
                    ssid,
                    mode,
                    security,
                    access_points: aps,
                    connections,
                    active,
                }
            })
            .collect();
        networks.sort_by(|a, b| a.compare(b));
        networks
    }
}

/// The most network rows the menu shows (`MAX_VISIBLE_NETWORKS`, `network.js:31`). Larger than the
/// device pickers' cap: a scan in a city finds dozens and the list is the point of the menu.
pub const MAX_VISIBLE_NETWORKS: usize = 8;

/// Which of the sorted networks are shown (`_updateItemsVisibility`, `network.js:1277-1288`).
///
/// `has_windows` is the session mode's: on the lock screen only networks we could rejoin without a
/// dialog appear, so an unattended machine cannot be walked onto a new network.
pub fn visible_networks(networks: &[WirelessNetwork], has_windows: bool) -> Vec<&WirelessNetwork> {
    networks
        .iter()
        .filter(|n| has_windows || n.has_connections() || n.security.can_autoconnect())
        .take(MAX_VISIBLE_NETWORKS)
        .collect()
}

/// Whether a device gets a menu item at all (`_shouldShowDevice`, `network.js:1675-1704`).
///
/// `wifi` relaxes the rule for `UNAVAILABLE`, which is the state a radio sits in while Wi-Fi is
/// switched off — the tile has to survive its own off switch (`NMWirelessToggle`, `:1893-1898`).
pub fn should_show_device(state: u32, wifi: bool) -> bool {
    match state {
        device_state::UNAVAILABLE => wifi,
        device_state::DISCONNECTED
        | device_state::PREPARE
        | device_state::CONFIG
        | device_state::NEED_AUTH
        | device_state::IP_CONFIG
        | device_state::IP_CHECK
        | device_state::SECONDARIES
        | device_state::ACTIVATED
        | device_state::DEACTIVATING
        | device_state::FAILED => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device that supports everything, so a test that is not about capabilities never trips
    /// over them.
    const FULL_CAPS: u32 = wifi_caps::CIPHER_WEP40
        | wifi_caps::CIPHER_WEP104
        | wifi_caps::CIPHER_TKIP
        | wifi_caps::CIPHER_CCMP
        | wifi_caps::WPA
        | wifi_caps::RSN;

    fn ap(path: &str, ssid: &str, strength: u8) -> AccessPoint {
        AccessPoint {
            path: path.to_string(),
            ssid: ssid.as_bytes().to_vec(),
            strength,
            mode: mode::INFRA,
            flags: 0,
            wpa_flags: 0,
            rsn_flags: 0,
        }
    }

    fn wpa2_psk(mut ap: AccessPoint) -> AccessPoint {
        ap.flags = ap_flags::PRIVACY;
        ap.rsn_flags = ap_sec::KEY_MGMT_PSK | ap_sec::PAIR_CCMP | ap_sec::GROUP_CCMP;
        ap
    }

    fn wpa2_enterprise(mut ap: AccessPoint) -> AccessPoint {
        ap.flags = ap_flags::PRIVACY;
        ap.rsn_flags = ap_sec::KEY_MGMT_802_1X | ap_sec::PAIR_CCMP | ap_sec::GROUP_CCMP;
        ap
    }

    fn device(aps: Vec<AccessPoint>) -> WirelessDevice {
        WirelessDevice {
            path: "/dev/wlan0".to_string(),
            interface: "wlp3s0".to_string(),
            state: device_state::DISCONNECTED,
            capabilities: FULL_CAPS,
            access_points: aps,
            ..Default::default()
        }
    }

    fn saved(path: &str, ssid: &str) -> SavedConnection {
        SavedConnection {
            path: path.to_string(),
            uuid: "uuid".to_string(),
            id: ssid.to_string(),
            kind: "802-11-wireless".to_string(),
            ssid: Some(ssid.as_bytes().to_vec()),
            wireless_mode: None,
        }
    }

    #[test]
    fn an_open_ap_is_not_secure_and_a_wpa2_one_is() {
        assert_eq!(
            ap("/ap/1", "open", 50).security(FULL_CAPS),
            SecurityType::None
        );
        let secured = wpa2_psk(ap("/ap/2", "home", 50));
        assert_eq!(secured.security(FULL_CAPS), SecurityType::Wpa2Psk);
        assert!(secured.security(FULL_CAPS).secure());
        assert!(!SecurityType::None.secure());
    }

    #[test]
    fn the_strongest_security_the_ap_offers_wins() {
        // An AP advertising both WPA and RSN with PSK reads as WPA2, never WPA.
        let mut both = wpa2_psk(ap("/ap/1", "home", 50));
        both.wpa_flags = ap_sec::KEY_MGMT_PSK | ap_sec::PAIR_TKIP | ap_sec::GROUP_TKIP;
        assert_eq!(both.security(FULL_CAPS), SecurityType::Wpa2Psk);
    }

    #[test]
    fn a_device_that_cannot_do_rsn_falls_back_or_gives_up() {
        // WPA2-only AP, WPA-only device: nothing validates, so the AP is not joinable.
        let wpa2_only = wpa2_psk(ap("/ap/1", "home", 50));
        let wpa_device = wifi_caps::WPA | wifi_caps::CIPHER_TKIP | wifi_caps::CIPHER_CCMP;
        assert_eq!(wpa2_only.security(wpa_device), SecurityType::Invalid);
    }

    #[test]
    fn owe_is_encrypted_but_shows_no_padlock() {
        let mut owe = ap("/ap/1", "cafe", 50);
        owe.rsn_flags = ap_sec::KEY_MGMT_OWE;
        assert_eq!(owe.security(FULL_CAPS), SecurityType::Owe);
        assert!(!SecurityType::Owe.secure());
        assert!(SecurityType::Owe.can_autoconnect());
    }

    #[test]
    fn enterprise_cannot_be_joined_from_the_menu() {
        assert!(!SecurityType::Wpa2Enterprise.can_autoconnect());
        assert!(!SecurityType::WpaEnterprise.can_autoconnect());
        assert!(SecurityType::Wpa2Psk.can_autoconnect());
    }

    #[test]
    fn aps_sharing_ssid_mode_and_security_are_one_row() {
        let dev = device(vec![
            wpa2_psk(ap("/ap/1", "home", 40)),
            wpa2_psk(ap("/ap/2", "home", 90)),
            wpa2_psk(ap("/ap/3", "other", 60)),
        ]);
        let networks = dev.networks(&[]);
        assert_eq!(networks.len(), 2);
        let home = networks.iter().find(|n| n.label() == "home").unwrap();
        assert_eq!(home.access_points.len(), 2);
        // The best AP leads, and it is the one the row's strength comes from.
        assert_eq!(home.strength(), 90);
        assert_eq!(home.access_points[0].path, "/ap/2");
    }

    #[test]
    fn one_ssid_with_two_security_types_is_two_rows() {
        let dev = device(vec![
            wpa2_psk(ap("/ap/1", "guest", 50)),
            ap("/ap/2", "guest", 50),
        ]);
        assert_eq!(dev.networks(&[]).len(), 2);
    }

    #[test]
    fn an_ap_with_no_ssid_yet_is_not_a_row() {
        let dev = device(vec![ap("/ap/1", "", 50)]);
        assert!(dev.networks(&[]).is_empty());
    }

    #[test]
    fn known_networks_sort_above_stronger_unknown_ones() {
        let mut dev = device(vec![
            wpa2_psk(ap("/ap/1", "known", 20)),
            wpa2_psk(ap("/ap/2", "strong", 95)),
        ]);
        dev.available_connections = vec!["/conn/1".to_string()];
        let networks = dev.networks(&[saved("/conn/1", "known")]);
        assert_eq!(networks[0].label(), "known");
        assert!(networks[0].has_connections());
        assert_eq!(networks[1].label(), "strong");
    }

    #[test]
    fn equal_networks_sort_by_strength_then_security_then_name() {
        let dev = device(vec![
            ap("/ap/1", "bbb", 50),
            wpa2_psk(ap("/ap/2", "aaa", 50)),
            ap("/ap/3", "zzz", 80),
        ]);
        let labels: Vec<String> = dev.networks(&[]).iter().map(|n| n.label()).collect();
        // strength first (zzz), then secure before open at equal strength, then the collation.
        assert_eq!(labels, vec!["zzz", "aaa", "bbb"]);
    }

    #[test]
    fn the_row_is_active_when_the_device_is_on_one_of_its_aps() {
        let mut dev = device(vec![wpa2_psk(ap("/ap/1", "home", 50))]);
        dev.active_access_point = Some("/ap/1".to_string());
        assert!(dev.networks(&[])[0].active);
    }

    #[test]
    fn a_hidden_ap_is_active_through_its_profile() {
        let mut dev = device(vec![wpa2_psk(ap("/ap/1", "home", 50))]);
        // No active AP reported (the hidden case), but the profile that is up matches the row.
        dev.available_connections = vec!["/conn/1".to_string()];
        dev.active_connection = Some("/conn/1".to_string());
        assert!(dev.networks(&[saved("/conn/1", "home")])[0].active);
    }

    #[test]
    fn a_hotspot_profile_never_matches_a_scanned_ap() {
        let mut profile = saved("/conn/1", "home");
        profile.wireless_mode = Some("ap".to_string());
        assert!(!profile.matches_ap(&ap("/ap/1", "home", 50)));

        let mut adhoc_profile = saved("/conn/2", "home");
        adhoc_profile.wireless_mode = Some("adhoc".to_string());
        assert!(!adhoc_profile.matches_ap(&ap("/ap/1", "home", 50)));
    }

    #[test]
    fn the_list_stops_at_eight_rows() {
        let aps: Vec<AccessPoint> = (0..12)
            .map(|i| wpa2_psk(ap(&format!("/ap/{i}"), &format!("net{i}"), 50)))
            .collect();
        let dev = device(aps);
        let networks = dev.networks(&[]);
        assert_eq!(networks.len(), 12);
        assert_eq!(
            visible_networks(&networks, true).len(),
            MAX_VISIBLE_NETWORKS
        );
    }

    #[test]
    fn the_lock_screen_shows_only_networks_it_could_rejoin_unattended() {
        let mut dev = device(vec![
            wpa2_enterprise(ap("/ap/1", "campus", 90)),
            wpa2_psk(ap("/ap/2", "home", 50)),
        ]);
        dev.available_connections = vec!["/conn/1".to_string()];
        let saved_profiles = [saved("/conn/1", "campus")];

        let networks = dev.networks(&saved_profiles);
        // Unlocked: both rows. Locked: the enterprise one only because it has a saved profile.
        assert_eq!(visible_networks(&networks, true).len(), 2);
        let locked = visible_networks(&networks, false);
        assert_eq!(locked.len(), 2);

        // Drop the profile and the enterprise row goes with it.
        let mut bare = dev.clone();
        bare.available_connections.clear();
        let networks = bare.networks(&[]);
        let locked: Vec<String> = visible_networks(&networks, false)
            .iter()
            .map(|n| n.label())
            .collect();
        assert_eq!(locked, vec!["home"]);
    }

    #[test]
    fn an_ssid_that_is_not_text_reads_as_unknown() {
        assert_eq!(ssid_to_label(&[0xff, 0xfe]), "<unknown>");
        assert_eq!(ssid_to_label(b""), "<unknown>");
        assert_eq!(ssid_to_label("caf\u{e9}".as_bytes()), "caf\u{e9}");
    }

    #[test]
    fn the_signal_icon_buckets_match_gnomes() {
        assert_eq!(signal_to_icon(0), "none");
        assert_eq!(signal_to_icon(19), "none");
        assert_eq!(signal_to_icon(20), "weak");
        assert_eq!(signal_to_icon(39), "weak");
        assert_eq!(signal_to_icon(40), "ok");
        assert_eq!(signal_to_icon(49), "ok");
        assert_eq!(signal_to_icon(50), "good");
        assert_eq!(signal_to_icon(79), "good");
        assert_eq!(signal_to_icon(80), "excellent");
        assert_eq!(signal_to_icon(100), "excellent");
    }

    #[test]
    fn an_adhoc_network_draws_the_workgroup_icon() {
        let mut adhoc = ap("/ap/1", "peer", 50);
        adhoc.mode = mode::ADHOC;
        let dev = device(vec![adhoc]);
        assert_eq!(
            dev.networks(&[])[0].icon_name(),
            "network-workgroup-symbolic"
        );
    }

    #[test]
    fn only_wifi_devices_survive_being_unavailable() {
        assert!(should_show_device(device_state::UNAVAILABLE, true));
        assert!(!should_show_device(device_state::UNAVAILABLE, false));
        assert!(!should_show_device(device_state::UNMANAGED, true));
        assert!(should_show_device(device_state::ACTIVATED, false));
        assert!(should_show_device(device_state::FAILED, false));
    }
}
