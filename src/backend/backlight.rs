// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! udev-facing half of the backlight subsystem: enumeration, the per-output device match, and the
//! change watch.
//!
//! The algebra all lives in [`crate::backlight`]; this is the part that has to touch libudev, so
//! it is only ever driven from the TTY backend. Mutter does the same split — `meta-udev.c` finds
//! the device, `meta-backlight-sysfs.c` reads/writes it, and both are native-backend-only.
//!
//! Smithay's `UdevBackend` is DRM-only, so we open our own monitor on the `backlight` subsystem;
//! mutter's `MetaUdev` watches exactly the two subsystems `{drm, backlight}`
//! (`meta-udev.c:357-367,419`).

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

use crate::backlight::{
    backlight_range, find_backlight, read_brightness, BacklightCandidate, BacklightSnapshot,
    BacklightWriter, DrmConnectorParent, OutputBacklight, WriteOutcome,
};
use crate::utils::is_laptop_panel;

/// A `backlight`-subsystem udev monitor, as a calloop-registerable source.
///
/// The newtype exists only because `udev`'s `AsFd` is `io_lifetimes`', not `std`'s, which
/// `calloop::generic::Generic` requires.
pub struct BacklightMonitor(udev::MonitorSocket);

impl BacklightMonitor {
    pub fn new() -> anyhow::Result<Self> {
        let socket = udev::MonitorBuilder::new()?
            .match_subsystem("backlight")?
            .listen()?;
        Ok(Self(socket))
    }

    /// Drains the pending uevents. The socket is level-triggered, so this must consume everything
    /// readable or calloop will spin.
    pub fn drain(&self) -> Vec<BacklightUevent> {
        self.0
            .iter()
            .map(|event| BacklightUevent {
                syspath: canonicalize(event.syspath()),
                kind: match event.event_type() {
                    udev::EventType::Add => UeventKind::Add,
                    udev::EventType::Remove => UeventKind::Remove,
                    _ => UeventKind::Change,
                },
            })
            .collect()
    }
}

impl AsFd for BacklightMonitor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the fd is owned by the socket, which outlives the borrow and is only closed
        // after the event source is removed from the loop.
        unsafe { BorrowedFd::borrow_raw(self.0.as_raw_fd()) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UeventKind {
    Add,
    Remove,
    /// Everything else, including the `change` a brightness write generates.
    Change,
}

#[derive(Debug, Clone)]
pub struct BacklightUevent {
    pub syspath: PathBuf,
    pub kind: UeventKind,
}

/// A write the caller must actually send (through logind), produced by the serializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    /// The connector the completion has to be reported back against.
    pub connector: String,
    /// The udev device name logind's `SetBrightness` takes.
    pub device_name: String,
    pub brightness: i32,
}

struct Device {
    /// udev device name (`intel_backlight`), for `SetBrightness`.
    name: String,
    sysfs_path: PathBuf,
    display_name: String,
    writer: BacklightWriter,
}

/// Every output's backlight, matched and tracked.
///
/// One device can back several outputs (the internal first-raw fallback is not connector-specific),
/// exactly as in mutter, where each `MetaOutput` gets its own `MetaBacklight` regardless.
#[derive(Default)]
pub struct Backlights {
    /// The `backlight` subsystem as last enumerated; re-scanned when a device comes or goes.
    candidates: Vec<BacklightCandidate>,
    /// `(connector, display name)` for the outputs currently connected, cached so a udev
    /// add/remove can redo the match without the backend re-deriving it.
    outputs: Vec<(String, String)>,
    devices: HashMap<String, Device>,
}

impl Backlights {
    pub fn new() -> Self {
        Self {
            candidates: enumerate_candidates(),
            ..Default::default()
        }
    }

    /// The same manager over an injected device list, so everything but libudev itself can be
    /// tested on a machine with no backlight (this VM, and the CI box).
    #[cfg(test)]
    fn with_candidates(candidates: Vec<BacklightCandidate>) -> Self {
        Self {
            candidates,
            ..Default::default()
        }
    }

    /// Re-match every output against the enumerated devices. Call whenever the set of connected
    /// outputs changes — a connector's `enabled` attribute flips on mode-set, and that is part of
    /// the match (`meta-udev.c:286-292`).
    ///
    /// Returns whether the resulting snapshot changed.
    pub fn set_outputs(&mut self, outputs: Vec<(String, String)>) -> bool {
        self.outputs = outputs;
        self.rematch()
    }

    fn rematch(&mut self) -> bool {
        let before = self.snapshot();

        let mut devices = HashMap::new();
        for (connector, display_name) in &self.outputs {
            let is_internal = is_laptop_panel(connector);
            let Some(candidate) = find_backlight(&self.candidates, connector, is_internal) else {
                continue;
            };
            let Some(range) =
                backlight_range(candidate.max_brightness, candidate.type_attr.as_deref())
            else {
                debug!(
                    "backlight {} has no usable steps; ignoring it for {connector}",
                    candidate.name
                );
                continue;
            };

            // Keep the existing writer when the same device is still driving this connector, so a
            // drag in flight across a hotplug is not restarted.
            let writer = match self.devices.remove(connector) {
                Some(old)
                    if old.sysfs_path == candidate.sysfs_path && old.writer.range() == range =>
                {
                    old.writer
                }
                _ => {
                    let brightness = match read_brightness(&candidate.sysfs_path) {
                        Ok(brightness) => brightness,
                        Err(err) => {
                            warn!(
                                "error reading brightness of {}: {err:?}",
                                candidate.sysfs_path.display()
                            );
                            continue;
                        }
                    };
                    BacklightWriter::new(range, brightness)
                }
            };

            devices.insert(
                connector.clone(),
                Device {
                    name: candidate.name.clone(),
                    sysfs_path: candidate.sysfs_path.clone(),
                    display_name: display_name.clone(),
                    writer,
                },
            );
        }
        self.devices = devices;

        self.snapshot() != before
    }

    /// Handle one uevent. Returns whether the snapshot changed.
    ///
    /// A `change` on a tracked device is an external brightness change (another seat tool, a
    /// firmware hotkey, or the echo of our own write) and re-reads sysfs
    /// (`meta-backlight-sysfs.c:226-237`). Add/remove re-enumerates and re-matches.
    pub fn handle_uevent(&mut self, event: &BacklightUevent) -> bool {
        if event.kind != UeventKind::Change {
            self.candidates = enumerate_candidates();
            return self.rematch();
        }

        let mut changed = false;
        for device in self.devices.values_mut() {
            if device.sysfs_path != event.syspath {
                continue;
            }

            match read_brightness(&device.sysfs_path) {
                Ok(brightness) => changed |= device.writer.update_from_hardware(brightness),
                Err(err) => warn!(
                    "error reading brightness of {}: {err:?}",
                    device.sysfs_path.display()
                ),
            }
        }
        changed
    }

    pub fn snapshot(&self) -> BacklightSnapshot {
        // Snapshot in output order, not HashMap order, so the detail card's rows are stable.
        let outputs = self
            .outputs
            .iter()
            .filter_map(|(connector, _)| {
                let device = self.devices.get(connector)?;
                Some(OutputBacklight {
                    connector: connector.clone(),
                    display_name: device.display_name.clone(),
                    range: device.writer.range(),
                    brightness: device.writer.target(),
                })
            })
            .collect();
        BacklightSnapshot { outputs }
    }

    /// A UI-requested brightness for one output. Returns the write to send now, if any — the
    /// serializer holds everything else back until the in-flight write completes.
    pub fn request(&mut self, connector: &str, brightness: i32) -> Option<PendingWrite> {
        let device = self.devices.get_mut(connector)?;
        let brightness = device.writer.set_target(brightness)?;
        Some(PendingWrite {
            connector: connector.to_owned(),
            device_name: device.name.clone(),
            brightness,
        })
    }

    /// Report a completed write; returns the follow-up write when the target moved meanwhile.
    pub fn write_finished(
        &mut self,
        connector: &str,
        outcome: WriteOutcome,
    ) -> Option<PendingWrite> {
        let device = self.devices.get_mut(connector)?;
        let brightness = device.writer.write_finished(outcome)?;
        Some(PendingWrite {
            connector: connector.to_owned(),
            device_name: device.name.clone(),
            brightness,
        })
    }
}

/// Enumerate `/sys/class/backlight`, mutter's `g_udev_client_query_by_subsystem (…, "backlight")`.
fn enumerate_candidates() -> Vec<BacklightCandidate> {
    let mut enumerator = match udev::Enumerator::new() {
        Ok(enumerator) => enumerator,
        Err(err) => {
            warn!("error creating a udev enumerator for backlights: {err:?}");
            return Vec::new();
        }
    };
    if let Err(err) = enumerator.match_subsystem("backlight") {
        warn!("error matching the backlight subsystem: {err:?}");
        return Vec::new();
    }
    let devices = match enumerator.scan_devices() {
        Ok(devices) => devices,
        Err(err) => {
            warn!("error scanning backlight devices: {err:?}");
            return Vec::new();
        }
    };

    devices
        .filter_map(|device| candidate_from(&device))
        .collect()
}

fn candidate_from(device: &udev::Device) -> Option<BacklightCandidate> {
    let name = device.sysname().to_str()?.to_owned();

    // Mutter realpaths the sysfs path (`meta-backlight-sysfs.c:333-334`) so the uevent comparison
    // is against a canonical path; udev's syspath already is one, but symlinked class paths are
    // cheap to defend against.
    let sysfs_path = canonicalize(device.syspath());

    let attr = |name: &str| {
        device
            .attribute_value(name)
            .and_then(|v| v.to_str())
            .map(str::to_owned)
    };

    let max_brightness = crate::backlight::parse_brightness(&attr("max_brightness")?).ok()?;

    let drm_connector_parent = device
        .parent_with_subsystem_devtype("drm", "drm_connector")
        .ok()
        .flatten()
        .and_then(|parent| {
            Some(DrmConnectorParent {
                name: parent.sysname().to_str()?.to_owned(),
                enabled: parent
                    .attribute_value("enabled")
                    .and_then(|v| v.to_str())
                    .map(|v| v.trim() == "enabled")
                    .unwrap_or(false),
            })
        });

    Some(BacklightCandidate {
        name,
        sysfs_path,
        type_attr: attr("type").map(|v| v.trim().to_owned()),
        drm_connector_parent,
        max_brightness,
    })
}

/// Both the enumerated devices and the uevents have to be compared as the same path, so both go
/// through here (mutter realpaths at construction, `meta-backlight-sysfs.c:333-334`).
fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake sysfs device directory holding a writable `brightness` file, so the manager's reads
    /// exercise the real path.
    struct FakeDevice {
        dir: PathBuf,
    }

    impl FakeDevice {
        fn new(name: &str, brightness: i32) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gsrs-backlight-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id(),
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let device = Self { dir };
            device.set(brightness);
            device
        }

        fn set(&self, brightness: i32) {
            std::fs::write(self.dir.join("brightness"), format!("{brightness}\n")).unwrap();
        }

        fn candidate(&self, name: &str, max_brightness: i32) -> BacklightCandidate {
            BacklightCandidate {
                name: name.to_owned(),
                sysfs_path: self.dir.clone(),
                type_attr: Some("firmware".to_owned()),
                drm_connector_parent: None,
                max_brightness,
            }
        }

        fn uevent(&self, kind: UeventKind) -> BacklightUevent {
            BacklightUevent {
                syspath: self.dir.clone(),
                kind,
            }
        }
    }

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn matching_an_internal_panel_reads_its_current_brightness() {
        let device = FakeDevice::new("panel", 120);
        let mut backlights =
            Backlights::with_candidates(vec![device.candidate("acpi_video0", 255)]);

        assert!(backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned(),)]));

        let snapshot = backlights.snapshot();
        let output = snapshot.get("eDP-1").unwrap();
        assert_eq!(output.display_name, "Built-in display");
        assert_eq!(output.brightness, 120);
        // min = max(1, 255/100).
        assert_eq!(output.range.min, 2);
        assert_eq!(output.range.max, 255);

        // An external monitor with only a firmware device gets nothing, so the snapshot is
        // unchanged -- still the one output.
        assert!(!backlights.set_outputs(vec![
            ("eDP-1".to_owned(), "Built-in display".to_owned()),
            ("HDMI-A-1".to_owned(), "Dell 24″".to_owned()),
        ]));
        assert_eq!(backlights.snapshot().outputs.len(), 1);
    }

    #[test]
    fn an_external_change_is_picked_up_from_sysfs() {
        let device = FakeDevice::new("echo", 120);
        let mut backlights =
            Backlights::with_candidates(vec![device.candidate("acpi_video0", 255)]);
        backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned())]);

        // A firmware hotkey moved it behind our back.
        device.set(200);
        assert!(backlights.handle_uevent(&device.uevent(UeventKind::Change)));
        assert_eq!(backlights.snapshot().get("eDP-1").unwrap().brightness, 200);

        // The same value again is not a change, so nothing downstream is woken.
        assert!(!backlights.handle_uevent(&device.uevent(UeventKind::Change)));

        // A change for some other device is ignored.
        let other = BacklightUevent {
            syspath: PathBuf::from("/sys/class/backlight/nope"),
            kind: UeventKind::Change,
        };
        assert!(!backlights.handle_uevent(&other));
    }

    #[test]
    fn writes_carry_the_device_name_and_serialize() {
        let device = FakeDevice::new("write", 120);
        let mut backlights =
            Backlights::with_candidates(vec![device.candidate("intel_backlight", 255)]);
        backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned())]);

        let write = backlights.request("eDP-1", 200).unwrap();
        assert_eq!(write.connector, "eDP-1");
        assert_eq!(write.device_name, "intel_backlight");
        assert_eq!(write.brightness, 200);
        // The UI follows the request, not the (not yet arrived) hardware echo.
        assert_eq!(backlights.snapshot().get("eDP-1").unwrap().brightness, 200);

        // Mid-drag: held back until the in-flight write completes.
        assert_eq!(backlights.request("eDP-1", 210), None);
        assert_eq!(backlights.request("eDP-1", 220), None);
        let followup = backlights
            .write_finished("eDP-1", WriteOutcome::Done(200))
            .unwrap();
        assert_eq!(followup.brightness, 220);
        assert_eq!(
            backlights.write_finished("eDP-1", WriteOutcome::Done(220)),
            None
        );

        // An output with no backlight, and an unknown one, are silent no-ops.
        assert_eq!(backlights.request("HDMI-A-1", 100), None);
        assert_eq!(
            backlights.write_finished("HDMI-A-1", WriteOutcome::Done(100)),
            None
        );
    }

    #[test]
    fn a_rematch_keeps_the_in_flight_write_state() {
        let device = FakeDevice::new("rematch", 120);
        let mut backlights =
            Backlights::with_candidates(vec![device.candidate("acpi_video0", 255)]);
        backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned())]);

        backlights.request("eDP-1", 200).unwrap();
        // A second monitor plugs in mid-drag. The panel's device is unchanged, so its writer (and
        // with it the pending write) must survive -- otherwise the completion would be orphaned and
        // the drag would restart from the hardware value.
        backlights.set_outputs(vec![
            ("eDP-1".to_owned(), "Built-in display".to_owned()),
            ("HDMI-A-1".to_owned(), "Dell 24″".to_owned()),
        ]);
        assert_eq!(backlights.snapshot().get("eDP-1").unwrap().brightness, 200);
        assert_eq!(backlights.request("eDP-1", 220), None);
        assert_eq!(
            backlights
                .write_finished("eDP-1", WriteOutcome::Done(200))
                .unwrap()
                .brightness,
            220
        );
    }

    #[test]
    fn losing_the_device_empties_the_snapshot() {
        let device = FakeDevice::new("gone", 120);
        let mut backlights =
            Backlights::with_candidates(vec![device.candidate("acpi_video0", 255)]);
        backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned())]);
        assert!(!backlights.snapshot().is_empty());

        // A remove uevent re-enumerates -- and on a machine with no real backlight (this VM), that
        // enumeration comes back empty, which is exactly the case being asserted.
        backlights.candidates.clear();
        assert!(backlights.set_outputs(vec![("eDP-1".to_owned(), "Built-in display".to_owned(),)]));
        assert!(backlights.snapshot().is_empty());
    }
}
