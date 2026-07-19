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
    /// `None` when there's no battery (desktop / VM without one).
    pub battery: Option<BatteryStatus>,
    /// Airplane (rfkill) mode, from gsd-rfkill (the authoritative source, replacing the old coarse
    /// NM `WirelessEnabled` proxy).
    pub airplane: AirplaneStatus,
    /// Power profile, from power-profiles-daemon (`org.freedesktop.UPower.PowerProfiles`).
    pub power: PowerProfileStatus,
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
#[derive(Debug, Clone, PartialEq)]
pub struct BatteryStatus {
    /// UPower's own `IconName` — already the exact themed symbolic name
    /// (e.g. `battery-level-90-charging-symbolic`), charge state included.
    pub icon_name: String,
    /// 0..=100, for the derived fallback icon and future percentage labels.
    pub percentage: f64,
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

    #[test]
    fn battery_icon_prefers_upower_name_then_derives_a_fallback() {
        let b = BatteryStatus {
            icon_name: "battery-level-90-charging-symbolic".to_string(),
            percentage: 87.0,
        };
        let icons = battery_icon(&b);
        // UPower's exact name is tried first.
        assert_eq!(icons[0], "battery-level-90-charging-symbolic");
        // Then a percentage-bucketed fallback, carrying the charge state.
        assert_eq!(icons[1], "battery-level-90-charging-symbolic");

        let discharging = BatteryStatus {
            icon_name: "battery-level-20-symbolic".to_string(),
            percentage: 23.0,
        };
        assert_eq!(battery_icon(&discharging)[1], "battery-level-20-symbolic");
    }
}
