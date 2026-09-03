// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The panel status area's live system state (network + battery).
//!
//! GNOME's top-right cluster shows the current network and power state as
//! symbolic icons, fed from NetworkManager and UPower. This is the fork-owned
//! model those icons resolve from: a plain data snapshot updated by the system-bus
//! watcher (`src/dbus/system_status.rs`) over a calloop channel — the same
//! Command→model→channel shape as the gsettings model in [`crate::gnome`]. The
//! panel maps it to icon names ([`network_icon`] / [`battery_icon`]); the model
//! itself carries no rendering or D-Bus dependency (it compiles without the
//! `dbus` feature, where it simply stays at its `Unknown`/absent default).

/// A snapshot of the system state the panel status area reflects.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SystemStatus {
    pub network: NetworkStatus,
    /// The full NetworkManager model the quick-settings network tiles read: devices, the grouped
    /// Wi-Fi networks and the saved VPN profiles.
    pub network_state: crate::network_model::NetworkState,
    /// `None` when there's no battery (desktop / VM without one).
    pub battery: Option<BatteryStatus>,
    /// Airplane (rfkill) mode, from gsd-rfkill (the authoritative source, replacing the old coarse
    /// NM `WirelessEnabled` proxy).
    pub airplane: AirplaneStatus,
    /// Power profile, from power-profiles-daemon (`org.freedesktop.UPower.PowerProfiles`).
    pub power: PowerProfileStatus,
    /// Bluetooth adapter + devices, from the BlueZ system-bus watcher.
    pub bluetooth: BluetoothStatus,
    /// The Bluetooth rfkill switch state, from gsd-rfkill (gates the QS tile's visibility and
    /// backs its toggle, `js/ui/status/bluetooth.js`).
    pub bluetooth_rfkill: BluetoothRfkill,
}

/// GnomeBluetooth's `AdapterState`, derived from BlueZ `Adapter1.PowerState`
/// (`bluetooth-client.c` `adapter_get_state`: `on`→On, `off`/`off-blocked`→Off,
/// `off-enabling`→TurningOn, `on-disabling`→TurningOff; `Powered` fallback when the property is
/// absent — older/non-experimental BlueZ doesn't expose it, so the Turning states may only ever
/// come from the predicted-state override on toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BtAdapterState {
    #[default]
    Absent,
    On,
    TurningOn,
    Off,
    TurningOff,
}

/// One BlueZ device as the QS Bluetooth menu needs it (`org.bluez.Device1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothDevice {
    /// The D-Bus object path — the stable identity rows/actions key on.
    pub path: String,
    /// `Alias` (falls back to `Name`/`Address` inside BlueZ itself).
    pub alias: String,
    /// `Icon`, when BlueZ derived one from the device class.
    pub icon: Option<String>,
    /// gnome-bluetooth's derived `connectable`: the device advertises a service gnome-shell can
    /// ask BlueZ to connect (audio/HID/MIDI UUIDs, `bluetooth-device.c` `update_connectable`).
    /// Gates row visibility (`bluetooth.js:233-235`).
    pub connectable: bool,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
}

impl BluetoothDevice {
    /// Icon candidates for the device row: BlueZ's class-derived `Icon` first, then the generic
    /// fallbacks (gnome-bluetooth's default is the plain `bluetooth` icon, not the active one).
    pub fn icon_candidates(&self) -> Vec<String> {
        let mut icons = Vec::new();
        if let Some(icon) = &self.icon {
            icons.push(format!("{icon}-symbolic"));
            icons.push(icon.clone());
        }
        icons.push("bluetooth-symbolic".to_string());
        icons.push("bluetooth-active-symbolic".to_string());
        icons
    }
}

/// Bluetooth adapter + device snapshot from the BlueZ watcher, mirroring what gnome-shell's
/// `BtClient` (`js/ui/status/bluetooth.js`) reduces GnomeBluetooth to. [`Default`] (adapter
/// absent, no devices) where the `dbus` feature / bluetoothd is absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BluetoothStatus {
    /// The default adapter's object path (the lexicographically largest `Adapter1`,
    /// gnome-bluetooth's `should_be_default_adapter`) — the target of the `Powered` write; `None`
    /// when BlueZ is absent or has no adapter.
    pub adapter: Option<String>,
    /// A default adapter exists (BlueZ present and exposes an `Adapter1`).
    pub adapter_present: bool,
    /// The default adapter's `Powered` — gnome-shell's `active` (`bluetooth.js:110-112`),
    /// the tile's `checked`.
    pub powered: bool,
    /// The default adapter's power state, for the tile icon (`bluetooth.js:424-439`).
    pub state: BtAdapterState,
    /// Every `Device1` on the default adapter (unfiltered; see [`Self::menu_devices`]).
    pub devices: Vec<BluetoothDevice>,
}

impl BluetoothStatus {
    /// The devices the menu iterates: none while the adapter is off ("ignore any lingering device
    /// references when turned off"), else paired-or-trusted (`bluetooth.js:161-174`).
    pub fn menu_devices(&self) -> impl Iterator<Item = &BluetoothDevice> {
        self.devices
            .iter()
            .filter(move |d| self.powered && (d.paired || d.trusted))
    }

    /// [`Self::menu_devices`] that actually get a visible row (`connectable`,
    /// `bluetooth.js:233-235`), sorted connected-first then by alias
    /// (`bluetooth.js:369-375`; plain Unicode order stands in for `localeCompare`).
    pub fn visible_devices(&self) -> Vec<&BluetoothDevice> {
        let mut devices: Vec<_> = self.menu_devices().filter(|d| d.connectable).collect();
        devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| a.alias.cmp(&b.alias))
        });
        devices
    }

    /// Connected devices among the menu set — drives the panel icon (visible iff > 0,
    /// `bluetooth.js:458-464`) and the subtitle.
    pub fn connected_count(&self) -> usize {
        self.menu_devices().filter(|d| d.connected).count()
    }

    /// The tile subtitle: >1 → "N Connected", ==1 → that device's alias, 0 → none
    /// (`bluetooth.js:410-419`).
    pub fn subtitle(&self) -> Option<String> {
        let connected: Vec<_> = self.menu_devices().filter(|d| d.connected).collect();
        match connected.len() {
            0 => None,
            1 => Some(connected[0].alias.clone()),
            n => Some(format!("{n} Connected")),
        }
    }

    /// The tile icon for an adapter state (`bluetooth.js:424-439`).
    pub fn icon_for(state: BtAdapterState) -> &'static str {
        match state {
            BtAdapterState::On => "bluetooth-active-symbolic",
            BtAdapterState::Off | BtAdapterState::Absent => "bluetooth-disabled-symbolic",
            BtAdapterState::TurningOn | BtAdapterState::TurningOff => {
                "bluetooth-acquiring-symbolic"
            }
        }
    }
}

/// The Bluetooth-specific gsd-rfkill properties gnome-shell's `BtClient` reads
/// (`bluetooth.js:74-80,103-108`). [`Default`] (absent → tile hidden) without the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BluetoothRfkill {
    /// `BluetoothAirplaneMode` — the soft kill switch the QS toggle writes.
    pub airplane: bool,
    /// `BluetoothHasAirplaneMode` — a Bluetooth rfkill switch exists at all.
    pub has_airplane: bool,
    /// `BluetoothHardwareAirplaneMode` — a hardware switch we can't leave in software.
    pub hardware_airplane: bool,
}

impl BluetoothRfkill {
    /// gnome-shell's `available` — gates the QS tile's visibility (`bluetooth.js:103-108`).
    pub fn available(&self) -> bool {
        self.has_airplane && !self.hardware_airplane
    }
}

/// A power profile gnome-shell knows how to render, mirroring `PROFILE_PARAMS`
/// (`js/ui/status/powerProfiles.js`). The daemon may expose others (custom vendor profiles); those
/// are kept only as the raw [`PowerProfileStatus::active`] string and rendered via the "Custom"
/// fallback, never listed in the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProfile {
    Performance,
    Balanced,
    PowerSaver,
}

impl KnownProfile {
    /// The daemon's profile id (`ActiveProfile` / the `Profile` key), the value written back.
    pub fn id(self) -> &'static str {
        match self {
            KnownProfile::Performance => "performance",
            KnownProfile::Balanced => "balanced",
            KnownProfile::PowerSaver => "power-saver",
        }
    }

    /// Parse a daemon profile id; `None` for anything gnome-shell doesn't have params for.
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "performance" => Some(KnownProfile::Performance),
            "balanced" => Some(KnownProfile::Balanced),
            "power-saver" => Some(KnownProfile::PowerSaver),
            _ => None,
        }
    }

    /// The display name (`PROFILE_PARAMS[*].name`).
    pub fn name(self) -> &'static str {
        match self {
            KnownProfile::Performance => "Performance",
            KnownProfile::Balanced => "Balanced",
            KnownProfile::PowerSaver => "Power Saver",
        }
    }

    /// The symbolic icon (`PROFILE_PARAMS[*].iconName`).
    pub fn icon(self) -> &'static str {
        match self {
            KnownProfile::Performance => "power-profile-performance-symbolic",
            KnownProfile::Balanced => "power-profile-balanced-symbolic",
            KnownProfile::PowerSaver => "power-profile-power-saver-symbolic",
        }
    }
}

/// Power-profile state, mirroring gnome-shell's `PowerProfilesToggle`
/// (`js/ui/status/powerProfiles.js`), fed by the power-profiles-daemon system-bus watcher. `show`
/// gates the QS tile + panel icon (the daemon has a bus-name owner); `active` is the raw
/// `ActiveProfile`; `available` is the KNOWN profiles the daemon exposes, **already filtered and
/// reversed** (daemon order power-saver→performance → reversed = performance→power-saver, GNOME's
/// menu order) so the picker's rows and its geometry agree by construction. [`Default`] (hidden)
/// where the `dbus` feature / the daemon is absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PowerProfileStatus {
    /// The raw `ActiveProfile` (kept raw so a vendor "custom" profile still renders as Custom).
    pub active: String,
    /// The known profiles the daemon exposes, filtered + reversed (top = highest performance).
    pub available: Vec<KnownProfile>,
    /// Whether power-profiles-daemon is present (has a bus-name owner).
    pub show: bool,
}

impl PowerProfileStatus {
    /// gnome-shell's `checked`: the active profile is not Balanced (`powerProfiles.js`). Also the
    /// panel-icon and tile "on" gate.
    pub fn is_active(&self) -> bool {
        self.active != KnownProfile::Balanced.id()
    }

    /// The active profile parsed to a [`KnownProfile`], or `None` for a custom/unknown one.
    pub fn active_known(&self) -> Option<KnownProfile> {
        KnownProfile::parse(&self.active)
    }

    /// The tile/panel icon for the active profile: its known icon, or the "Custom" fallback
    /// (`FALLBACK_PARAMS.iconName`, `powerProfiles.js`).
    pub fn icon(&self) -> &'static str {
        self.active_known()
            .map(KnownProfile::icon)
            .unwrap_or("gnome-power-manager-symbolic")
    }

    /// The active profile's display name, or "Custom" for an unknown one.
    pub fn name(&self) -> &'static str {
        self.active_known()
            .map(KnownProfile::name)
            .unwrap_or("Custom")
    }
}

/// Airplane mode, mirroring gnome-shell's `RfkillManager` (`js/ui/status/rfkill.js`), fed by the
/// gsd-rfkill session-bus watcher. `show` gates both the QS toggle tile and the panel icon
/// (`HasAirplaneMode && ShouldShowAirplaneMode`); `active` is `AirplaneMode`. [`Default`] (hidden,
/// off) where the `dbus` feature / gsd-rfkill is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AirplaneStatus {
    /// `AirplaneMode` — networking radios are killed.
    pub active: bool,
    /// Whether to surface airplane mode at all (`HasAirplaneMode && ShouldShowAirplaneMode`):
    /// false on hardware with no rfkill switches (desktops, this VM).
    pub show: bool,
}

/// The primary connection's state, mirroring what gnome-shell's
/// `js/ui/status/network.js` reduces NetworkManager down to for the panel icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkStatus {
    /// NetworkManager not yet read / unavailable — the panel shows no network icon.
    #[default]
    Unknown,
    /// A wired connection is primary.
    Wired,
    /// A wireless connection is primary, with signal strength 0..=100.
    Wireless(u8),
    /// Not connected (but networking is enabled).
    Offline,
}

/// The battery state, from UPower's aggregate `DisplayDevice`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatteryStatus {
    /// UPower's own `IconName` — already the exact themed symbolic name
    /// (e.g. `battery-level-90-charging-symbolic`), charge state included.
    pub icon_name: String,
    /// 0..=100, for the derived fallback icon and future percentage labels.
    pub percentage: f64,
    /// UPower's `State` ([`BatteryState`]).
    pub state: BatteryState,
    /// UPower's `WarningLevel` ([`BatteryWarning`]).
    pub warning: BatteryWarning,
}

/// UPower's `State` enum (`UP_DEVICE_STATE_*`, `libupower-glib/up-types.h`).
///
/// **Charging is not the only on-AC state**, which is the trap: a machine that is plugged in but
/// not currently drawing reports `PendingCharge`, and one sitting at 100% reports `FullyCharged`.
/// Testing `state == Charging` alone paints a plugged-in laptop as discharging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl BatteryState {
    /// Map UPower's wire value; anything unrecognised reads as `Unknown`.
    pub fn from_upower(raw: u32) -> Self {
        match raw {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }

    /// On external power — charging, topped up, or plugged in and idle. The battery is not the
    /// thing being spent, so a warning level (which UPower keeps reporting) must not colour it.
    pub fn on_ac(self) -> bool {
        matches!(
            self,
            Self::Charging | Self::FullyCharged | Self::PendingCharge
        )
    }
}

/// UPower's `WarningLevel` enum (`UP_DEVICE_LEVEL_*`, `libupower-glib/up-types.h`).
///
/// The thresholds behind these are UPower's policy and are configurable per device, so the shell
/// reads the level and never re-derives it from a percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryWarning {
    /// `Unknown`, `None`, `Discharging`, and the above-normal levels (`Normal`/`High`/`Full`):
    /// nothing to say.
    #[default]
    None,
    Low,
    Critical,
    /// The level at which UPower's configured critical action (suspend/hibernate) fires.
    Action,
}

impl BatteryWarning {
    /// Map UPower's wire value. 0 Unknown, 1 None, 2 Discharging, 6 Normal, 7 High and 8 Full all
    /// mean "no warning" for our purposes; only 3/4/5 carry one.
    pub fn from_upower(raw: u32) -> Self {
        match raw {
            3 => Self::Low,
            4 => Self::Critical,
            5 => Self::Action,
            _ => Self::None,
        }
    }
}

impl BatteryState {
    /// Parse the debug-override spelling of a UPower state. `auto` is not a state — it means
    /// "hand the battery back to UPower" — so it is `None` here and handled by the caller.
    pub fn parse_debug(s: &str) -> Option<Self> {
        Some(match s {
            "charging" => Self::Charging,
            "discharging" => Self::Discharging,
            "empty" => Self::Empty,
            "fully-charged" => Self::FullyCharged,
            "pending-charge" => Self::PendingCharge,
            "pending-discharge" => Self::PendingDischarge,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

impl BatteryWarning {
    /// Parse the debug-override spelling of a UPower warning level.
    pub fn parse_debug(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "low" => Self::Low,
            "critical" => Self::Critical,
            "action" => Self::Action,
            _ => return None,
        })
    }
}

/// The themed battery icon name a debug override should carry, so the icon view and the drawn
/// indicator agree about what state is being faked.
pub fn debug_icon_name(percentage: f64, state: BatteryState) -> String {
    derived_battery_icon(percentage, state == BatteryState::Charging)
}

/// The symbolic-icon candidate list for a network state (first that resolves in
/// the theme wins), or `None` when nothing should be drawn (`Unknown`).
pub fn network_icon(status: NetworkStatus) -> Option<&'static [&'static str]> {
    match status {
        NetworkStatus::Unknown => None,
        NetworkStatus::Wired => Some(&["network-wired-symbolic"]),
        NetworkStatus::Wireless(strength) => Some(wireless_icon(strength)),
        NetworkStatus::Offline => Some(&["network-offline-symbolic"]),
    }
}

/// The airplane-mode panel icon (gnome-shell's rfkill `Indicator`, `js/ui/status/rfkill.js`).
pub fn airplane_icon() -> &'static [&'static str] {
    &["airplane-mode-symbolic"]
}

/// GNOME's five wireless-signal buckets (`network-wireless-signal-*-symbolic`).
fn wireless_icon(strength: u8) -> &'static [&'static str] {
    // Buckets mirror gnome-shell's `_getSignalIcon` thresholds (0/20/40/60/80).
    match strength {
        0..=4 => &["network-wireless-signal-none-symbolic"],
        5..=39 => &["network-wireless-signal-weak-symbolic"],
        40..=59 => &["network-wireless-signal-ok-symbolic"],
        60..=79 => &["network-wireless-signal-good-symbolic"],
        _ => &["network-wireless-signal-excellent-symbolic"],
    }
}

/// The symbolic-icon candidate list for a battery: UPower's own `icon_name`
/// first (already themed), then a level-bucketed fallback derived from the
/// percentage and charge state (in case the exact name is missing from a theme).
pub fn battery_icon(battery: &BatteryStatus) -> Vec<String> {
    let charging = battery.icon_name.contains("charging");
    vec![
        battery.icon_name.clone(),
        derived_battery_icon(battery.percentage, charging),
    ]
}

/// How the dynamic battery indicator should read, derived from UPower's state and warning level
/// (`docs/fork/battery-indicator-design.md`). Kept here, next to the state it reads, so it is a
/// pure function the corpus can table-test without a renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryLook {
    /// The shell (housing) tint. Normally the panel foreground, like every neighbouring icon —
    /// the housing is not what carries the reading. It only leaves white when the whole indicator
    /// goes red, which the mockup reserves for critical.
    pub body: BatteryTint,
    /// The fill bar's tint. **This** is the channel that carries state: colour appears in the
    /// charge, not around it.
    pub fill: BatteryTint,
    /// The glyph drawn over the body, if any.
    pub overlay: BatteryOverlay,
}

/// What is drawn over the battery body.
///
/// `Bolt` and `Cord` are the two halves of "on external power", and telling them apart is the
/// point: a laptop that stops charging at 80% to spare the cell sits in
/// [`BatteryState::PendingCharge`] indefinitely, so a bolt there claims a current that is not
/// flowing. Same for a battery that has reached 100%.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryOverlay {
    #[default]
    None,
    /// Charging: current is flowing in.
    Bolt,
    /// Plugged in and *not* charging — held below full, or already full.
    Cord,
    /// Critical. A shape, because colour alone is not a channel a colour-blind reader has.
    Alert,
}

/// The indicator's colour role. Resolved to pixels by the panel, which owns the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryTint {
    /// Panel foreground — the same light grey the wifi and volume glyphs use.
    #[default]
    Normal,
    /// Actively charging.
    Charging,
    Low,
    Critical,
}

/// The indicator's appearance for a battery state.
///
/// **State decides first, warning level second.** UPower keeps reporting a warning level while
/// plugged in, so a machine charging from 3% must read as charging, not critical — and
/// [`BatteryState::Charging`] is not the only on-AC state.
///
/// Colour is spent narrowly and on purpose. Being on mains is the *least* eventful thing a desktop
/// does, and it is permanent there, so it gets a glyph rather than a tint: only an actively
/// flowing charge is green, and plugged-but-idle looks exactly like a healthy battery with a plug
/// beside it. Yellow and red then mean what they say.
pub fn battery_look(battery: &BatteryStatus) -> BatteryLook {
    use BatteryOverlay as O;
    use BatteryTint as T;
    let look = |body, fill, overlay| BatteryLook {
        body,
        fill,
        overlay,
    };
    match battery.state {
        // Current flowing in: the charge itself is green, the housing stays panel foreground.
        BatteryState::Charging => look(T::Normal, T::Charging, O::Bolt),
        // On mains, not charging — held below full by a charge limit, or already full.
        BatteryState::FullyCharged | BatteryState::PendingCharge => {
            look(T::Normal, T::Normal, O::Cord)
        }
        BatteryState::Empty => look(T::Critical, T::Critical, O::Alert),
        BatteryState::Unknown => look(T::Normal, T::Normal, O::None),
        // Discharging (or pending discharge): the warning level decides.
        _ => match battery.warning {
            BatteryWarning::None => look(T::Normal, T::Normal, O::None),
            // Low tints the charge only; the housing is still just a housing.
            BatteryWarning::Low => look(T::Normal, T::Low, O::None),
            // Critical is the one state that turns the *whole* indicator red.
            BatteryWarning::Critical | BatteryWarning::Action => {
                look(T::Critical, T::Critical, O::Alert)
            }
        },
    }
}

/// The `battery-level-{0,10,..,100}[-charging]-symbolic` name for a percentage.
fn derived_battery_icon(percentage: f64, charging: bool) -> String {
    let level = ((percentage / 10.0).round() as i64 * 10).clamp(0, 100);
    let suffix = if charging { "-charging" } else { "" };
    format!("battery-level-{level}{suffix}-symbolic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wireless_buckets_climb_with_strength() {
        let name = |s| wireless_icon(s)[0];
        assert_eq!(name(0), "network-wireless-signal-none-symbolic");
        assert_eq!(name(10), "network-wireless-signal-weak-symbolic");
        assert_eq!(name(50), "network-wireless-signal-ok-symbolic");
        assert_eq!(name(70), "network-wireless-signal-good-symbolic");
        assert_eq!(name(100), "network-wireless-signal-excellent-symbolic");
    }

    #[test]
    fn network_icon_covers_every_state() {
        assert!(network_icon(NetworkStatus::Unknown).is_none());
        assert_eq!(
            network_icon(NetworkStatus::Wired).unwrap()[0],
            "network-wired-symbolic"
        );
        assert_eq!(
            network_icon(NetworkStatus::Offline).unwrap()[0],
            "network-offline-symbolic"
        );
        assert_eq!(airplane_icon()[0], "airplane-mode-symbolic");
        assert_eq!(
            network_icon(NetworkStatus::Wireless(85)).unwrap()[0],
            "network-wireless-signal-excellent-symbolic"
        );
    }

    #[test]
    fn known_profile_ids_round_trip_and_unknowns_fall_back() {
        for p in [
            KnownProfile::Performance,
            KnownProfile::Balanced,
            KnownProfile::PowerSaver,
        ] {
            assert_eq!(KnownProfile::parse(p.id()), Some(p));
        }
        assert_eq!(KnownProfile::parse("cool-vendor-mode"), None);
    }

    #[test]
    fn power_profile_status_reflects_active_profile() {
        let perf = PowerProfileStatus {
            active: "performance".to_string(),
            available: vec![
                KnownProfile::Performance,
                KnownProfile::Balanced,
                KnownProfile::PowerSaver,
            ],
            show: true,
        };
        assert!(perf.is_active(), "performance is not balanced → checked");
        assert_eq!(perf.icon(), "power-profile-performance-symbolic");
        assert_eq!(perf.name(), "Performance");

        let balanced = PowerProfileStatus {
            active: "balanced".to_string(),
            ..perf.clone()
        };
        assert!(!balanced.is_active(), "balanced → not checked");
        assert_eq!(balanced.icon(), "power-profile-balanced-symbolic");

        // A vendor/custom profile: checked (not balanced), but rendered via the Custom fallback.
        let custom = PowerProfileStatus {
            active: "cool-vendor-mode".to_string(),
            ..perf
        };
        assert!(custom.is_active());
        assert_eq!(custom.icon(), "gnome-power-manager-symbolic");
        assert_eq!(custom.name(), "Custom");
    }

    fn bt_device(alias: &str, connected: bool) -> BluetoothDevice {
        BluetoothDevice {
            path: format!("/org/bluez/hci0/dev_{alias}"),
            alias: alias.to_string(),
            icon: None,
            connectable: true,
            paired: true,
            trusted: false,
            connected,
        }
    }

    #[test]
    fn bluetooth_menu_devices_gate_on_power_and_pairing() {
        let mut status = BluetoothStatus {
            adapter: Some("/org/bluez/hci0".to_string()),
            adapter_present: true,
            powered: true,
            state: BtAdapterState::On,
            devices: vec![
                bt_device("Keyboard", true),
                BluetoothDevice {
                    paired: false,
                    trusted: false,
                    ..bt_device("Stranger", false)
                },
                BluetoothDevice {
                    paired: false,
                    trusted: true,
                    ..bt_device("Trusted", false)
                },
            ],
        };
        // Paired or trusted only (bluetooth.js:171).
        let aliases: Vec<_> = status.menu_devices().map(|d| d.alias.as_str()).collect();
        assert_eq!(aliases, ["Keyboard", "Trusted"]);

        // Nothing while the adapter is off (bluetooth.js:161-164).
        status.powered = false;
        assert_eq!(status.menu_devices().count(), 0);
        assert_eq!(status.connected_count(), 0);
        assert_eq!(status.subtitle(), None);
    }

    #[test]
    fn bluetooth_visible_devices_sort_connected_first_then_alias() {
        let status = BluetoothStatus {
            adapter: Some("/org/bluez/hci0".to_string()),
            adapter_present: true,
            powered: true,
            state: BtAdapterState::On,
            devices: vec![
                bt_device("Zeta", false),
                bt_device("Mouse", true),
                BluetoothDevice {
                    connectable: false,
                    ..bt_device("Camera", false)
                },
                bt_device("Alpha", false),
                bt_device("Buds", true),
            ],
        };
        let aliases: Vec<_> = status
            .visible_devices()
            .iter()
            .map(|d| d.alias.as_str())
            .collect();
        // Connected first, then alias; the non-connectable Camera has no row.
        assert_eq!(aliases, ["Buds", "Mouse", "Alpha", "Zeta"]);
    }

    #[test]
    fn bluetooth_subtitle_counts_connected() {
        let mut status = BluetoothStatus {
            adapter: Some("/org/bluez/hci0".to_string()),
            adapter_present: true,
            powered: true,
            state: BtAdapterState::On,
            devices: vec![bt_device("Buds", true)],
        };
        assert_eq!(status.subtitle().as_deref(), Some("Buds"));

        status.devices.push(bt_device("Mouse", true));
        assert_eq!(status.subtitle().as_deref(), Some("2 Connected"));
        assert_eq!(status.connected_count(), 2);

        status.devices.clear();
        assert_eq!(status.subtitle(), None);
    }

    #[test]
    fn bluetooth_icon_covers_every_adapter_state() {
        use BtAdapterState::*;
        assert_eq!(BluetoothStatus::icon_for(On), "bluetooth-active-symbolic");
        for s in [Off, Absent] {
            assert_eq!(BluetoothStatus::icon_for(s), "bluetooth-disabled-symbolic");
        }
        for s in [TurningOn, TurningOff] {
            assert_eq!(BluetoothStatus::icon_for(s), "bluetooth-acquiring-symbolic");
        }
    }

    #[test]
    fn bluetooth_rfkill_available_requires_soft_switch() {
        let mut rfkill = BluetoothRfkill::default();
        assert!(!rfkill.available(), "no switch at all → hidden");
        rfkill.has_airplane = true;
        assert!(rfkill.available());
        rfkill.hardware_airplane = true;
        assert!(
            !rfkill.available(),
            "a hardware switch can't be left in software (bluetooth.js:104-107)"
        );
    }

    #[test]
    fn bluetooth_device_icons_fall_back_to_generic() {
        let mut dev = bt_device("Buds", true);
        assert_eq!(
            dev.icon_candidates(),
            ["bluetooth-symbolic", "bluetooth-active-symbolic"]
        );
        dev.icon = Some("audio-headset".to_string());
        assert_eq!(
            dev.icon_candidates(),
            [
                "audio-headset-symbolic",
                "audio-headset",
                "bluetooth-symbolic",
                "bluetooth-active-symbolic"
            ]
        );
    }

    /// The on-AC states, and the distinction that matters most on real hardware: a laptop held at
    /// ~80% by a charge limit reports `PendingCharge` *indefinitely*, so a bolt there would claim
    /// a current that is not flowing. It gets a cord instead, and no colour at all — being on
    /// mains is the least eventful thing a machine does, and on a desktop it is permanent.
    #[test]
    fn plugged_in_but_not_charging_shows_a_cord_and_no_colour() {
        let at = |state, warning| {
            battery_look(&BatteryStatus {
                state,
                warning,
                ..Default::default()
            })
        };

        // Current actually flowing: green, in the charge only, with a bolt.
        let charging = at(BatteryState::Charging, BatteryWarning::None);
        assert_eq!(charging.fill, BatteryTint::Charging);
        assert_eq!(
            charging.body,
            BatteryTint::Normal,
            "the housing stays neutral"
        );
        assert_eq!(charging.overlay, BatteryOverlay::Bolt);

        // Held below full, or already full: a cord, and nothing tinted.
        for state in [BatteryState::PendingCharge, BatteryState::FullyCharged] {
            let look = at(state, BatteryWarning::None);
            assert_eq!(
                look.overlay,
                BatteryOverlay::Cord,
                "{state:?} is plugged in but not charging"
            );
            assert_eq!(
                look.fill,
                BatteryTint::Normal,
                "{state:?} must not read as charging"
            );
            assert_eq!(look.body, BatteryTint::Normal);
        }

        // On AC the warning level is stale news: charging out of a critical charge is not critical.
        let from_empty = at(BatteryState::Charging, BatteryWarning::Action);
        assert_eq!(from_empty.fill, BatteryTint::Charging);
        assert_eq!(from_empty.overlay, BatteryOverlay::Bolt);
    }

    /// Discharging is where the warning level decides. Low tints the charge only; critical is the
    /// one state that turns the whole indicator red, and the only one that earns a glyph.
    #[test]
    fn a_discharging_battery_takes_its_tint_from_the_warning_level() {
        let at = |state, warning| {
            battery_look(&BatteryStatus {
                state,
                warning,
                ..Default::default()
            })
        };

        for state in [BatteryState::Discharging, BatteryState::PendingDischarge] {
            let plain = at(state, BatteryWarning::None);
            assert_eq!(
                plain.fill,
                BatteryTint::Normal,
                "{state:?} with no warning is plain"
            );
            assert_eq!(plain.overlay, BatteryOverlay::None);

            let low = at(state, BatteryWarning::Low);
            assert_eq!(low.fill, BatteryTint::Low);
            assert_eq!(
                low.body,
                BatteryTint::Normal,
                "low does not tint the housing"
            );
            assert_eq!(
                low.overlay,
                BatteryOverlay::None,
                "low is a colour change only; the glyph is reserved for critical"
            );

            for warning in [BatteryWarning::Critical, BatteryWarning::Action] {
                let look = at(state, warning);
                assert_eq!(look.fill, BatteryTint::Critical);
                assert_eq!(
                    look.body,
                    BatteryTint::Critical,
                    "critical reddens the whole indicator"
                );
                assert_eq!(
                    look.overlay,
                    BatteryOverlay::Alert,
                    "{warning:?} needs a non-colour channel for colour-blind readers"
                );
            }
        }

        // Empty alerts regardless of what the warning level says; Unknown never does.
        assert_eq!(
            at(BatteryState::Empty, BatteryWarning::None).overlay,
            BatteryOverlay::Alert
        );
        let unknown = at(BatteryState::Unknown, BatteryWarning::Critical);
        assert_eq!(unknown.fill, BatteryTint::Normal);
        assert_eq!(
            unknown.overlay,
            BatteryOverlay::None,
            "an unknown state must not cry wolf"
        );
    }

    /// UPower's above-normal levels (6 Normal, 7 High, 8 Full) and 2 Discharging all mean "no
    /// warning"; only 3/4/5 carry one. Mapping them by `raw >= 3` would make a full battery read
    /// as an alarm.
    #[test]
    fn upower_warning_levels_above_action_are_not_warnings() {
        assert_eq!(BatteryWarning::from_upower(0), BatteryWarning::None);
        assert_eq!(BatteryWarning::from_upower(1), BatteryWarning::None);
        assert_eq!(BatteryWarning::from_upower(2), BatteryWarning::None);
        assert_eq!(BatteryWarning::from_upower(3), BatteryWarning::Low);
        assert_eq!(BatteryWarning::from_upower(4), BatteryWarning::Critical);
        assert_eq!(BatteryWarning::from_upower(5), BatteryWarning::Action);
        for full in [6, 7, 8] {
            assert_eq!(BatteryWarning::from_upower(full), BatteryWarning::None);
        }
        assert_eq!(BatteryState::from_upower(5), BatteryState::PendingCharge);
        assert_eq!(BatteryState::from_upower(99), BatteryState::Unknown);
    }

    #[test]
    fn battery_icon_prefers_upower_name_then_derives_a_fallback() {
        let b = BatteryStatus {
            icon_name: "battery-level-90-charging-symbolic".to_string(),
            percentage: 87.0,
            ..Default::default()
        };
        let icons = battery_icon(&b);
        // UPower's exact name is tried first.
        assert_eq!(icons[0], "battery-level-90-charging-symbolic");
        // Then a percentage-bucketed fallback, carrying the charge state.
        assert_eq!(icons[1], "battery-level-90-charging-symbolic");

        let discharging = BatteryStatus {
            icon_name: "battery-level-20-symbolic".to_string(),
            percentage: 23.0,
            ..Default::default()
        };
        assert_eq!(battery_icon(&discharging)[1], "battery-level-20-symbolic");
    }
}
