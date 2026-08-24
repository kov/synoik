// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use smithay::utils::Size;
use zbus::fdo::RequestNameFlags;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, OwnedValue, Type};
use zbus::{fdo, interface};

use super::Start;
use crate::backend::IpcOutputMap;
use crate::utils::scale::supported_scales;
use crate::utils::{is_laptop_panel, make_display_name};

pub struct DisplayConfig {
    to_niri: calloop::channel::Sender<AppliedConfig>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
}

/// One `ApplyMonitorsConfig` on its way to the compositor.
///
/// Primary rides along rather than only reaching the compositor through the `monitors.xml` write,
/// because that write only happens on a PERSISTENT apply — a TEMPORARY one (GNOME Settings'
/// confirmation countdown) would otherwise leave primary behind.
pub struct AppliedConfig {
    /// Per connector: the requested configuration, or `None` to disable it.
    pub outputs: HashMap<String, Option<synoik_config::Output>>,
    /// Connector of the logical monitor flagged primary, if the request named one.
    pub primary: Option<String>,
}

#[derive(Serialize, Type)]
pub struct Monitor {
    names: (String, String, String, String),
    modes: Vec<Mode>,
    properties: HashMap<String, OwnedValue>,
}

#[derive(Serialize, Type)]
pub struct Mode {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: HashMap<String, OwnedValue>,
}

#[derive(Serialize, Type)]
pub struct LogicalMonitor {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    is_primary: bool,
    monitors: Vec<(String, String, String, String)>,
    properties: HashMap<String, OwnedValue>,
}

// ApplyMonitorsConfig
#[derive(Deserialize, Type)]
pub struct LogicalMonitorConfiguration {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    is_primary: bool,
    monitors: Vec<(String, String, HashMap<String, OwnedValue>)>,
}

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfig {
    async fn get_current_state(
        &self,
    ) -> fdo::Result<(
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        HashMap<String, OwnedValue>,
    )> {
        // Construct the DBus response.
        let mut monitors = Vec::new();
        let mut logical_monitors = Vec::new();

        for output in self.ipc_outputs.lock().unwrap().values() {
            // Loosely matches the check in Mutter.
            let c = &output.name;
            let is_laptop_panel = is_laptop_panel(c);
            let display_name = make_display_name(output, is_laptop_panel);

            let mut properties = HashMap::new();
            properties.insert(
                String::from("display-name"),
                OwnedValue::from(zvariant::Str::from(display_name)),
            );
            properties.insert(
                String::from("is-builtin"),
                OwnedValue::from(is_laptop_panel),
            );

            let mut modes: Vec<Mode> = output
                .modes
                .iter()
                .map(|m| {
                    let synoik_ipc::Mode {
                        width,
                        height,
                        refresh_rate,
                        is_preferred,
                    } = *m;
                    let width = i32::from(width);
                    let height = i32::from(height);
                    let refresh_rate = refresh_rate as f64 / 1000.;

                    Mode {
                        id: format!("{width}x{height}@{refresh_rate:.3}"),
                        width,
                        height,
                        refresh_rate,
                        preferred_scale: 1.,
                        supported_scales: supported_scales(Size::from((width, height))).collect(),
                        properties: HashMap::from([(
                            String::from("is-preferred"),
                            OwnedValue::from(is_preferred),
                        )]),
                    }
                })
                .collect();
            if let Some(mode) = output.current_mode {
                modes[mode]
                    .properties
                    .insert(String::from("is-current"), OwnedValue::from(true));
            }

            let connector = c.clone();
            let model = output.model.clone();
            // mutter's monitorspec `vendor` is the raw EDID manufacturer code, not the decoded
            // name (`set_output_details_from_edid`, meta-output.c:318): the decoded one is only
            // ever shown, via the `display-name` property above.
            let vendor = output.vendor.clone().unwrap_or_default();

            // Serial is used for session restore, so fall back to the connector name if it's
            // not available.
            let serial = output.serial.as_ref().unwrap_or(&connector).clone();

            let names = (connector, vendor, model, serial);

            if let Some(logical) = output.logical.as_ref() {
                let transform = match logical.transform {
                    synoik_ipc::Transform::Normal => 0,
                    synoik_ipc::Transform::_90 => 1,
                    synoik_ipc::Transform::_180 => 2,
                    synoik_ipc::Transform::_270 => 3,
                    synoik_ipc::Transform::Flipped => 4,
                    synoik_ipc::Transform::Flipped90 => 5,
                    synoik_ipc::Transform::Flipped180 => 6,
                    synoik_ipc::Transform::Flipped270 => 7,
                };

                logical_monitors.push(LogicalMonitor {
                    x: logical.x,
                    y: logical.y,
                    scale: logical.scale,
                    transform,
                    is_primary: logical.is_primary,
                    monitors: vec![names.clone()],
                    properties: HashMap::new(),
                });
            }

            monitors.push(Monitor {
                names,
                modes,
                properties,
            });
        }

        // Sort by connector.
        monitors.sort_unstable_by(|a, b| a.names.0.cmp(&b.names.0));
        logical_monitors.sort_unstable_by(|a, b| a.monitors[0].0.cmp(&b.monitors[0].0));

        let properties = HashMap::from([(String::from("layout-mode"), OwnedValue::from(1u32))]);
        Ok((0, monitors, logical_monitors, properties))
    }

    async fn apply_monitors_config(
        &self,
        _serial: u32,
        method: u32,
        logical_monitor_configs: Vec<LogicalMonitorConfiguration>,
        _properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        let current_conf = self.ipc_outputs.lock().unwrap();
        let mut new_conf = HashMap::new();
        let mut primary = None;
        // Collected in parallel for persistence (method == PERSISTENT): the full monitorspec + mode
        // per logical monitor, so we can write `monitors.xml` the way mutter does.
        let mut write_lms: Vec<crate::monitors_xml::WriteLogicalMonitor> = Vec::new();
        for requested_config in logical_monitor_configs {
            if requested_config.monitors.len() > 1 {
                return Err(zbus::fdo::Error::Failed(
                    "Mirroring is not yet supported".to_owned(),
                ));
            }
            let mut write_monitors = Vec::new();
            for (connector, mode, _props) in requested_config.monitors {
                if requested_config.is_primary {
                    primary = Some(connector.clone());
                }
                let Some(output) = current_conf.values().find(|o| o.name == connector) else {
                    return Err(zbus::fdo::Error::Failed(format!(
                        "Connector '{connector}' not found",
                    )));
                };
                // Resolve the monitorspec + chosen mode dims for persistence: `<vendor>` is the raw
                // EDID manufacturer code, as mutter writes it, so a file written here and one
                // written by mutter describe the same monitor with the same bytes.
                let (mw, mh, mr) = parse_mode_id(&mode);
                write_monitors.push(crate::monitors_xml::WriteMonitor {
                    connector: connector.clone(),
                    vendor: output.vendor.clone().unwrap_or_default(),
                    product: output.model.clone(),
                    serial: output.serial.clone().unwrap_or_else(|| connector.clone()),
                    width: mw,
                    height: mh,
                    rate: mr,
                });
                new_conf.insert(
                    connector.clone(),
                    Some(synoik_config::Output {
                        off: false,
                        name: connector,
                        scale: Some(synoik_config::FloatOrInt(requested_config.scale)),
                        transform: match requested_config.transform {
                            0 => synoik_ipc::Transform::Normal,
                            1 => synoik_ipc::Transform::_90,
                            2 => synoik_ipc::Transform::_180,
                            3 => synoik_ipc::Transform::_270,
                            4 => synoik_ipc::Transform::Flipped,
                            5 => synoik_ipc::Transform::Flipped90,
                            6 => synoik_ipc::Transform::Flipped180,
                            7 => synoik_ipc::Transform::Flipped270,
                            x => {
                                return Err(zbus::fdo::Error::Failed(format!(
                                    "Unknown transform {x}",
                                )))
                            }
                        },
                        position: Some(synoik_config::Position {
                            x: requested_config.x,
                            y: requested_config.y,
                        }),
                        mode: Some(synoik_config::output::Mode {
                            custom: false,
                            mode: synoik_ipc::ConfiguredMode::from_str(&mode).map_err(|e| {
                                zbus::fdo::Error::Failed(format!(
                                    "Could not parse mode '{mode}': {e}"
                                ))
                            })?,
                        }),
                        // FIXME: VRR
                        ..Default::default()
                    }),
                );
            }
            write_lms.push(crate::monitors_xml::WriteLogicalMonitor {
                x: requested_config.x,
                y: requested_config.y,
                scale: requested_config.scale,
                primary: requested_config.is_primary,
                transform: requested_config.transform,
                monitors: write_monitors,
            });
        }
        if new_conf.is_empty() {
            return Err(zbus::fdo::Error::Failed(
                "At least one output must be enabled".to_owned(),
            ));
        }
        for output in current_conf.values() {
            if !new_conf.contains_key(&output.name) {
                new_conf.insert(output.name.clone(), None);
            }
        }
        if method == 0 {
            // 0 means "verify", so don't actually apply here
            return Ok(());
        }
        if let Err(err) = self.to_niri.send(AppliedConfig {
            outputs: new_conf,
            primary,
        }) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }
        // PERSISTENT (2): write GNOME's monitors.xml so the choice survives logout, the way mutter
        // does. TEMPORARY (1) applies for the session only. `reload_output_config` then re-reads
        // the file, so the in-memory KDL config (set above) and the store stay consistent.
        if method == 2 {
            match crate::monitors_xml::write(&write_lms) {
                Ok(path) => tracing::info!("persisted display config to {}", path.display()),
                Err(err) => warn!("could not persist monitors.xml: {err}"),
            }
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn monitors_changed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn power_save_mode(&self) -> i32 {
        -1
    }

    #[zbus(property)]
    fn set_power_save_mode(&self, _mode: i32) -> zbus::Result<()> {
        Err(zbus::Error::Unsupported)
    }

    #[zbus(property)]
    fn panel_orientation_managed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn night_light_supported(&self) -> bool {
        false
    }
}

impl DisplayConfig {
    pub fn new(
        to_niri: calloop::channel::Sender<AppliedConfig>,
        ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    ) -> Self {
        Self {
            to_niri,
            ipc_outputs,
        }
    }
}

impl Start for DisplayConfig {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/DisplayConfig", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.DisplayConfig", flags)?;

        Ok(conn)
    }
}

/// Parse a mode id as minted by `get_current_state` — `"{width}x{height}@{rate:.3}"` — back into
/// `(width, height, rate)` for persistence. Falls back to `(0, 0, 60.0)` on a malformed id (the
/// ids we hand out always parse; the fallback just keeps a rogue client from writing garbage).
fn parse_mode_id(mode: &str) -> (i32, i32, f64) {
    (|| {
        let (dims, rate) = mode.split_once('@')?;
        let (w, h) = dims.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?, rate.parse().ok()?))
    })()
    .unwrap_or((0, 0, 60.0))
}
