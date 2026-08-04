// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Display backlight hardware — the compositor half of GNOME 50.1 brightness.
//!
//! In 50.1 brightness is **not** gsd-power: mutter owns the hardware and the shell's
//! `BrightnessManager` (`js/misc/brightnessManager.js`, ported in [`crate::brightness`]) builds its
//! sliders on `monitor.get_backlight()`. Mutter's `MetaBacklight` is a per-output sysfs backlight
//! device found through udev (`meta-udev.c` `meta_udev_backlight_find`), read from
//! `/sys/class/backlight/<dev>/brightness`, watched for external changes on the udev `backlight`
//! subsystem, and written through logind's `Session.SetBrightness`
//! (`meta-backlight-sysfs.c:152-181`).
//!
//! This module is the **pure** half of that: device selection, the usable-range algebra, the
//! one-write-in-flight serializer and the plain-data snapshot the UI reads. Everything that
//! touches udev or D-Bus lives in the TTY backend ([`crate::backend::tty`]) and
//! [`crate::dbus::freedesktop_login1`], so this compiles and tests headless with no hardware.
//!
//! Divergences from mutter are recorded per item below; the slice-level ones (no pkexec helper
//! fallback, no HDR ref-white backlight, scale-per-output rather than per-logical-monitor) are in
//! `docs/fork/panel-status-port.md` under Q4.

use std::path::{Path, PathBuf};

/// The usable brightness range of a sysfs backlight device.
///
/// Both bounds are raw sysfs values, not a normalized fraction — normalization to the 0..1 the
/// slider speaks is [`crate::brightness`]'s job, exactly as in GNOME (mutter hands out raw
/// min/max/brightness, `MonitorBrightnessScale` divides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacklightRange {
    pub min: i32,
    pub max: i32,
}

impl BacklightRange {
    /// Clamps a raw brightness into the range. Returns `None` when the value was in range, so
    /// callers can warn on out-of-range hardware reads like mutter does
    /// (`meta-backlight-sysfs.c:66-73`).
    pub fn clamp(&self, brightness: i32) -> i32 {
        brightness.clamp(self.min, self.max)
    }
}

/// Mutter's `get_backlight_info` (`meta-backlight-sysfs.c:239-274`), as pure algebra over the two
/// sysfs attributes it reads.
///
/// `min` is one percent of the range (at least 1) so a slider can never drive an internal panel
/// fully dark; the exception is a `raw` interface with fewer than 100 steps, where 0 is assumed
/// not to turn the panel off. A device whose `min >= max` has no steps and is unusable.
pub fn backlight_range(max_brightness: i32, type_attr: Option<&str>) -> Option<BacklightRange> {
    let max = max_brightness;
    let mut min = std::cmp::max(1, max / 100);

    // If the interface has less than 100 possible values, and it is of type raw, then assume that
    // 0 does not turn off the backlight completely.
    if max < 99 && type_attr == Some("raw") {
        min = 0;
    }

    // Ignore a backlight which has no steps.
    if min >= max {
        return None;
    }

    Some(BacklightRange { min, max })
}

/// A `/sys/class/backlight` device as the device-picking algebra needs to see it, so the pick can
/// be tested without udev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklightCandidate {
    /// The udev device name (`acpi_video0`, `intel_backlight`, …) — what logind's `SetBrightness`
    /// takes as its subsystem-relative name.
    pub name: String,
    /// The resolved sysfs path; identity for udev change events and for reading `brightness`.
    pub sysfs_path: PathBuf,
    /// The `type` sysfs attribute: `firmware`, `platform` or `raw`.
    pub type_attr: Option<String>,
    /// The device's `drm_connector`-subsystem parent, when the DRM driver registered it against a
    /// connector. Present only for driver-registered raw interfaces.
    pub drm_connector_parent: Option<DrmConnectorParent>,
    /// `max_brightness`, for [`backlight_range`].
    pub max_brightness: i32,
}

/// The `drm`/`drm_connector` parent of a raw backlight device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmConnectorParent {
    /// The kernel's connector device name, of the form `card<n>-<connector>` (e.g. `card0-eDP-1`).
    pub name: String,
    /// Whether the connector's `enabled` attribute reads `enabled`.
    pub enabled: bool,
}

fn find_type<'a>(
    candidates: &'a [BacklightCandidate],
    type_attr: &str,
) -> Option<&'a BacklightCandidate> {
    candidates
        .iter()
        .find(|c| c.type_attr.as_deref() == Some(type_attr))
}

/// Mutter's `meta_udev_backlight_find_for_connector` (`meta-udev.c:252-294`): a **raw** interface
/// whose `drm_connector` parent is named `card<n>-<connector>` and is currently enabled.
fn find_for_connector<'a>(
    candidates: &'a [BacklightCandidate],
    connector_name: &str,
) -> Option<&'a BacklightCandidate> {
    let suffix = format!("-{connector_name}");
    candidates.iter().find(|c| {
        // Only look for raw backlight interfaces.
        if c.type_attr.as_deref() != Some("raw") {
            return false;
        }

        let Some(parent) = &c.drm_connector_parent else {
            return false;
        };

        parent.name.ends_with(&suffix) && parent.enabled
    })
}

/// Mutter's `meta_udev_backlight_find` (`meta-udev.c:296-333`): pick the backlight device that
/// drives `connector_name`.
///
/// Internal panels prefer `firmware` then `platform` (the ACPI/vendor interfaces, which are
/// system-wide and unconnected to any connector), then fall through to the connector match, then
/// to *any* raw interface. External connectors get the connector match only — an unmatched
/// external monitor has no backlight we can drive (there is no DDC path in mutter 50.1 either).
///
/// `is_internal` is mutter's `meta_output_info_is_builtin`, which we keep in
/// [`crate::utils::is_laptop_panel`] since the display naming needs the same predicate.
pub fn find_backlight<'a>(
    candidates: &'a [BacklightCandidate],
    connector_name: &str,
    is_internal: bool,
) -> Option<&'a BacklightCandidate> {
    if candidates.is_empty() {
        return None;
    }

    if is_internal {
        // For internal monitors, prefer the types firmware -> platform -> raw.
        if let Some(device) = find_type(candidates, "firmware") {
            return Some(device);
        }
        if let Some(device) = find_type(candidates, "platform") {
            return Some(device);
        }
    }

    // Try to find a backlight interface matching the connector.
    if let Some(device) = find_for_connector(candidates, connector_name) {
        return Some(device);
    }

    // For internal monitors, fall back to just picking the first raw backlight interface if no
    // other interface was found.
    if is_internal {
        return find_type(candidates, "raw");
    }

    None
}

/// Reads a device's current `brightness` attribute.
///
/// Mutter reads `brightness`, **not** `actual_brightness` (`meta-backlight-sysfs.c:53`) — the
/// latter is the panel's measured value and lags or quantizes the request.
pub fn read_brightness(sysfs_path: &Path) -> anyhow::Result<i32> {
    let contents = std::fs::read_to_string(sysfs_path.join("brightness"))?;
    parse_brightness(&contents)
}

/// Parses the contents of a sysfs `brightness`/`max_brightness` attribute (mutter uses
/// `g_ascii_strtoll(…, NULL, 0)`, which stops at the trailing newline).
pub fn parse_brightness(contents: &str) -> anyhow::Result<i32> {
    let value = contents.trim();
    value
        .parse::<i32>()
        .map_err(|err| anyhow::anyhow!("invalid sysfs brightness value {value:?}: {err}"))
}

/// The outcome of a completed `SetBrightness` write, as [`BacklightWriter`] needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The write landed, carrying the value that was written.
    Done(i32),
    /// The write failed; the target is left alone and nothing is re-issued (mutter warns and
    /// returns, `meta-backlight.c:123-133`).
    Failed,
}

/// Mutter's write serialization (`meta-backlight.c:107-196`) — **load-bearing**, not an
/// optimization: a slider drag produces a write per motion event, and without this every one of
/// them would become an in-flight D-Bus call to logind.
///
/// At most one write is in flight. A target set while one is pending is stored, not sent; when the
/// in-flight write completes and the target has since moved, one more write is issued. External
/// (udev) changes update the target without ever issuing a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklightWriter {
    range: BacklightRange,
    target: i32,
    pending: bool,
}

impl BacklightWriter {
    /// Starts from the value just read out of sysfs.
    pub fn new(range: BacklightRange, brightness: i32) -> Self {
        Self {
            range,
            target: range.clamp(brightness),
            pending: false,
        }
    }

    pub fn range(&self) -> BacklightRange {
        self.range
    }

    /// The brightness the compositor believes the device is at or heading to — what the UI shows,
    /// so a drag tracks the finger rather than the echo.
    pub fn target(&self) -> i32 {
        self.target
    }

    pub fn has_pending(&self) -> bool {
        self.pending
    }

    /// `meta_backlight_set_brightness` (`:159-196`). Returns the value to write now, or `None`
    /// when nothing needs sending (unchanged target, or a write already in flight).
    pub fn set_target(&mut self, brightness: i32) -> Option<i32> {
        let new_brightness = self.range.clamp(brightness);
        if new_brightness != brightness {
            warn!(
                "trying to set out-of-range backlight brightness {brightness} \
                 (range {}..={})",
                self.range.min, self.range.max
            );
        }

        if self.target == new_brightness {
            return None;
        }

        self.target = new_brightness;

        if self.pending {
            return None;
        }

        self.pending = true;
        Some(self.target)
    }

    /// `on_brightness_set` (`:107-149`). Returns a value to write when the target moved while the
    /// completed write was in flight.
    pub fn write_finished(&mut self, outcome: WriteOutcome) -> Option<i32> {
        self.pending = false;

        let WriteOutcome::Done(written) = outcome else {
            return None;
        };

        // The brightness got updated from the system and we tried to set it at the same time; set
        // the brightness the system was setting to make sure we're in the correct state.
        if self.target == written {
            return None;
        }

        self.pending = true;
        Some(self.target)
    }

    /// `meta_backlight_update_brightness_target` (`:83-105`): an external change observed through
    /// udev. Never issues a write. Returns whether the target moved.
    pub fn update_from_hardware(&mut self, brightness: i32) -> bool {
        if self.target == brightness {
            return false;
        }

        let new_brightness = self.range.clamp(brightness);
        if new_brightness != brightness {
            warn!(
                "backlight value read from sysfs is out of range: {brightness} \
                 (range {}..={})",
                self.range.min, self.range.max
            );
        }

        if self.target == new_brightness {
            return false;
        }

        self.target = new_brightness;
        true
    }
}

/// One output's backlight, as the brightness algebra and the UI see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBacklight {
    /// The connector name (`eDP-1`) — the stable identity per-monitor UI actions key on.
    pub connector: String,
    /// The monitor's display name, for the per-monitor detail card
    /// (`brightnessManager.js:338` names a scale after its first monitor).
    pub display_name: String,
    pub range: BacklightRange,
    /// The current raw brightness target.
    pub brightness: i32,
}

impl OutputBacklight {
    /// The brightness as a 0..1 fraction of the usable range — `MonitorBrightnessScale`'s
    /// `_getRelativeBrightness` (`brightnessManager.js:372-380`).
    pub fn relative_brightness(&self) -> f64 {
        let range = f64::from(self.range.max - self.range.min);
        if range <= 0. {
            return 0.;
        }
        f64::from(self.brightness - self.range.min) / range
    }
}

/// Every output that has a usable backlight, in output order.
///
/// [`Default`] (empty) is the headless / no-hardware case, and the case where the `dbus` feature
/// is off: the brightness slider is simply absent, exactly as in GNOME when no monitor reports a
/// backlight (`brightness.js:59-60`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BacklightSnapshot {
    pub outputs: Vec<OutputBacklight>,
}

impl BacklightSnapshot {
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    pub fn get(&self, connector: &str) -> Option<&OutputBacklight> {
        self.outputs.iter().find(|o| o.connector == connector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, type_attr: &str) -> BacklightCandidate {
        BacklightCandidate {
            name: name.to_owned(),
            sysfs_path: PathBuf::from(format!("/sys/class/backlight/{name}")),
            type_attr: Some(type_attr.to_owned()),
            drm_connector_parent: None,
            max_brightness: 255,
        }
    }

    fn connector_candidate(name: &str, parent: &str, enabled: bool) -> BacklightCandidate {
        BacklightCandidate {
            drm_connector_parent: Some(DrmConnectorParent {
                name: parent.to_owned(),
                enabled,
            }),
            ..candidate(name, "raw")
        }
    }

    #[test]
    fn backlight_range_is_one_percent_of_max() {
        // The common laptop cases: min is 1% of the range, at least 1.
        assert_eq!(
            backlight_range(255, Some("firmware")),
            Some(BacklightRange { min: 2, max: 255 })
        );
        assert_eq!(
            backlight_range(100, Some("raw")),
            Some(BacklightRange { min: 1, max: 100 })
        );
        assert_eq!(
            backlight_range(96000, Some("raw")),
            Some(BacklightRange {
                min: 960,
                max: 96000
            })
        );
    }

    #[test]
    fn small_raw_interfaces_reach_zero() {
        // < 100 steps AND raw: 0 is assumed not to turn the panel off.
        assert_eq!(
            backlight_range(20, Some("raw")),
            Some(BacklightRange { min: 0, max: 20 })
        );
        // ... but only for raw.
        assert_eq!(
            backlight_range(20, Some("firmware")),
            Some(BacklightRange { min: 1, max: 20 })
        );
        assert_eq!(
            backlight_range(20, None),
            Some(BacklightRange { min: 1, max: 20 })
        );
        // 99 is not "less than 99".
        assert_eq!(
            backlight_range(99, Some("raw")),
            Some(BacklightRange { min: 1, max: 99 })
        );
    }

    #[test]
    fn stepless_backlights_are_unusable() {
        // min >= max: no steps at all.
        assert_eq!(backlight_range(1, Some("firmware")), None);
        assert_eq!(backlight_range(0, Some("raw")), None);
        assert_eq!(backlight_range(-5, Some("firmware")), None);
        // A raw device with max 1 still has the 0..1 step.
        assert_eq!(
            backlight_range(1, Some("raw")),
            Some(BacklightRange { min: 0, max: 1 })
        );
    }

    #[test]
    fn internal_prefers_firmware_then_platform() {
        let all = vec![
            candidate("intel_backlight", "raw"),
            candidate("platform_backlight", "platform"),
            candidate("acpi_video0", "firmware"),
        ];
        assert_eq!(
            find_backlight(&all, "eDP-1", true).map(|c| &*c.name),
            Some("acpi_video0")
        );

        let no_firmware = all[..2].to_vec();
        assert_eq!(
            find_backlight(&no_firmware, "eDP-1", true).map(|c| &*c.name),
            Some("platform_backlight")
        );
    }

    #[test]
    fn internal_falls_back_to_first_raw() {
        // No firmware/platform and no connector match: any raw interface will do for a panel.
        let all = vec![
            candidate("raw0", "raw"),
            connector_candidate("raw1", "card0-DP-2", true),
        ];
        assert_eq!(
            find_backlight(&all, "eDP-1", true).map(|c| &*c.name),
            Some("raw0")
        );
    }

    #[test]
    fn connector_match_is_raw_enabled_and_suffixed() {
        let matching = vec![connector_candidate("dp_aux_bl", "card0-DP-2", true)];
        assert_eq!(
            find_backlight(&matching, "DP-2", false).map(|c| &*c.name),
            Some("dp_aux_bl")
        );

        // Disabled connector: not a match.
        let disabled = vec![connector_candidate("dp_aux_bl", "card0-DP-2", false)];
        assert_eq!(find_backlight(&disabled, "DP-2", false), None);

        // Wrong connector.
        assert_eq!(find_backlight(&matching, "DP-1", false), None);

        // A `-DP-2` suffix match must not fire on `DP-2` vs `card0-eDP-2`... it does end with
        // "-DP-2", which is mutter's behavior too; the guard that keeps this honest is that the
        // connector name we pass is the full formatted name.
        let edp = vec![connector_candidate("bl", "card0-eDP-2", true)];
        assert_eq!(
            find_backlight(&edp, "eDP-2", true).map(|c| &*c.name),
            Some("bl")
        );

        // Non-raw types never match a connector.
        let firmware_parent = vec![BacklightCandidate {
            type_attr: Some("firmware".to_owned()),
            ..connector_candidate("fw", "card0-DP-2", true)
        }];
        assert_eq!(find_backlight(&firmware_parent, "DP-2", false), None);
    }

    #[test]
    fn external_unmatched_has_no_backlight() {
        // The firmware/platform preference and the raw fallback are internal-only: an external
        // monitor with no connector-matched device gets nothing.
        let all = vec![
            candidate("acpi_video0", "firmware"),
            candidate("intel_backlight", "raw"),
        ];
        assert_eq!(find_backlight(&all, "HDMI-A-1", false), None);
        assert_eq!(find_backlight(&[], "eDP-1", true), None);
    }

    #[test]
    fn internal_connectors_are_the_panel_interfaces() {
        use crate::utils::is_laptop_panel;

        assert!(is_laptop_panel("eDP-1"));
        assert!(is_laptop_panel("LVDS-1"));
        assert!(is_laptop_panel("DSI-1"));
        // DPI is in mutter's builtin list too, and was missing from the inherited helper.
        assert!(is_laptop_panel("DPI-1"));
        assert!(!is_laptop_panel("DP-1"));
        assert!(!is_laptop_panel("HDMI-A-1"));
        assert!(!is_laptop_panel("Virtual-1"));
        // Not a prefix of a longer interface name.
        assert!(!is_laptop_panel("DSIX-1"));
    }

    #[test]
    fn parses_sysfs_values() {
        assert_eq!(parse_brightness("120\n").unwrap(), 120);
        assert_eq!(parse_brightness("0").unwrap(), 0);
        assert!(parse_brightness("").is_err());
        assert!(parse_brightness("abc\n").is_err());
    }

    fn writer() -> BacklightWriter {
        BacklightWriter::new(BacklightRange { min: 1, max: 100 }, 50)
    }

    #[test]
    fn first_write_goes_out_immediately() {
        let mut w = writer();
        assert_eq!(w.set_target(60), Some(60));
        assert!(w.has_pending());
        assert_eq!(w.target(), 60);
    }

    #[test]
    fn a_drag_collapses_to_one_in_flight_write() {
        let mut w = writer();
        assert_eq!(w.set_target(60), Some(60));
        // Every subsequent motion event only moves the target.
        assert_eq!(w.set_target(70), None);
        assert_eq!(w.set_target(80), None);
        assert_eq!(w.set_target(90), None);
        assert_eq!(w.target(), 90);
        // The completion re-issues exactly once, with the latest target.
        assert_eq!(w.write_finished(WriteOutcome::Done(60)), Some(90));
        assert!(w.has_pending());
        // And that one settles.
        assert_eq!(w.write_finished(WriteOutcome::Done(90)), None);
        assert!(!w.has_pending());
    }

    #[test]
    fn unchanged_target_writes_nothing() {
        let mut w = writer();
        assert_eq!(w.set_target(50), None);
        assert!(!w.has_pending());
    }

    #[test]
    fn set_target_clamps_to_the_usable_range() {
        let mut w = writer();
        assert_eq!(w.set_target(1000), Some(100));
        assert_eq!(w.write_finished(WriteOutcome::Done(100)), None);
        assert_eq!(w.set_target(-5), Some(1));
        assert_eq!(w.target(), 1);
    }

    #[test]
    fn a_failed_write_clears_pending_without_reissuing() {
        let mut w = writer();
        assert_eq!(w.set_target(60), Some(60));
        assert_eq!(w.set_target(70), None);
        assert_eq!(w.write_finished(WriteOutcome::Failed), None);
        assert!(!w.has_pending());
        // The next user move starts a fresh write from the stored target.
        assert_eq!(w.set_target(80), Some(80));
    }

    #[test]
    fn hardware_echo_never_writes() {
        let mut w = writer();
        assert!(w.update_from_hardware(30));
        assert_eq!(w.target(), 30);
        assert!(!w.has_pending());
        // Same value again is not a change.
        assert!(!w.update_from_hardware(30));
        // Out of range gets clamped.
        assert!(w.update_from_hardware(0));
        assert_eq!(w.target(), 1);
    }

    #[test]
    fn relative_brightness_spans_the_usable_range() {
        let bl = OutputBacklight {
            connector: "eDP-1".to_owned(),
            display_name: "Built-in display".to_owned(),
            range: BacklightRange { min: 10, max: 110 },
            brightness: 60,
        };
        assert!((bl.relative_brightness() - 0.5).abs() < 1e-9);

        let at_min = OutputBacklight {
            brightness: 10,
            ..bl.clone()
        };
        assert_eq!(at_min.relative_brightness(), 0.);

        let at_max = OutputBacklight {
            brightness: 110,
            ..bl
        };
        assert_eq!(at_max.relative_brightness(), 1.);
    }
}
