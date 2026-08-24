// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::path::PathBuf;

use crate::utils::{Flag, MergeWith};

#[derive(Debug, Default, PartialEq)]
pub struct Debug {
    pub preview_render: Option<PreviewRender>,
    pub dbus_interfaces_in_non_session_instances: bool,
    pub wait_for_frame_completion_before_queueing: bool,
    pub enable_overlay_planes: bool,
    pub disable_cursor_plane: bool,
    pub disable_direct_scanout: bool,
    pub restrict_primary_scanout_to_matching_format: bool,
    pub force_disable_connectors_on_resume: bool,
    pub render_drm_device: Option<PathBuf>,
    pub ignored_drm_devices: Vec<PathBuf>,
    pub emulate_zero_presentation_time: bool,
    pub disable_resize_throttling: bool,
    pub disable_transactions: bool,
    pub keep_laptop_panel_on_when_lid_is_closed: bool,
    pub disable_monitor_names: bool,
    pub strict_new_window_focus_policy: bool,
    pub honor_xdg_activation_with_invalid_serial: bool,
    pub deactivate_unfocused_windows: bool,
    pub skip_cursor_only_updates_during_vrr: bool,
    /// Connect the PipeWire audio watcher in `--headless` runs, which normally skip it. The only
    /// way to exercise the volume path against a live daemon without a seat.
    pub audio_in_headless: bool,
    /// Start xwayland-satellite in `--headless` runs, which normally skip it. The only way to
    /// exercise the X11 selection bridge without a seat -- at the cost the skip exists to avoid:
    /// the X11 sockets live in the shared `/tmp/.X11-unix`, so two instances with this on compete
    /// for display numbers. One manual run at a time, never the test suite.
    pub xwayland_in_headless: bool,
}

impl Debug {
    /// Read the debug toggles out of the environment.
    ///
    /// These used to live in the config file's `debug {}` block; with the file gone they come
    /// from `SYNOIK_DEBUG_<SCREAMING_SNAKE_FIELD_NAME>`, the same idiom as `SYNOIK_VK_VALIDATION`.
    /// That is deliberate: an env var reaches the *live session* through a systemd drop-in
    /// (`Environment=SYNOIK_DEBUG_DISABLE_TRANSACTIONS=1`), which is where these are actually
    /// used — a debug knob you can only set in a test harness is a knob you cannot debug with.
    ///
    /// A flag is on when its variable is set to anything other than `0` or the empty string.
    pub fn from_env() -> Self {
        fn flag(name: &str) -> bool {
            match std::env::var_os(name) {
                Some(v) => !v.is_empty() && v != "0",
                None => false,
            }
        }
        fn path(name: &str) -> Option<PathBuf> {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }

        Self {
            preview_render: match std::env::var("SYNOIK_DEBUG_PREVIEW_RENDER").as_deref() {
                Ok("screencast") => Some(PreviewRender::Screencast),
                Ok("screen-capture") => Some(PreviewRender::ScreenCapture),
                Ok(other) if !other.is_empty() => {
                    warn!(
                        "ignoring SYNOIK_DEBUG_PREVIEW_RENDER={other:?}: \
                         expected \"screencast\" or \"screen-capture\""
                    );
                    None
                }
                _ => None,
            },
            dbus_interfaces_in_non_session_instances: flag(
                "SYNOIK_DEBUG_DBUS_INTERFACES_IN_NON_SESSION_INSTANCES",
            ),
            wait_for_frame_completion_before_queueing: flag(
                "SYNOIK_DEBUG_WAIT_FOR_FRAME_COMPLETION_BEFORE_QUEUEING",
            ),
            enable_overlay_planes: flag("SYNOIK_DEBUG_ENABLE_OVERLAY_PLANES"),
            disable_cursor_plane: flag("SYNOIK_DEBUG_DISABLE_CURSOR_PLANE"),
            disable_direct_scanout: flag("SYNOIK_DEBUG_DISABLE_DIRECT_SCANOUT"),
            restrict_primary_scanout_to_matching_format: flag(
                "SYNOIK_DEBUG_RESTRICT_PRIMARY_SCANOUT_TO_MATCHING_FORMAT",
            ),
            force_disable_connectors_on_resume: flag(
                "SYNOIK_DEBUG_FORCE_DISABLE_CONNECTORS_ON_RESUME",
            ),
            render_drm_device: path("SYNOIK_DEBUG_RENDER_DRM_DEVICE"),
            // Colon-separated, like a PATH — a device path cannot contain one.
            ignored_drm_devices: std::env::var_os("SYNOIK_DEBUG_IGNORE_DRM_DEVICES")
                .map(|v| {
                    std::env::split_paths(&v)
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            emulate_zero_presentation_time: flag("SYNOIK_DEBUG_EMULATE_ZERO_PRESENTATION_TIME"),
            disable_resize_throttling: flag("SYNOIK_DEBUG_DISABLE_RESIZE_THROTTLING"),
            disable_transactions: flag("SYNOIK_DEBUG_DISABLE_TRANSACTIONS"),
            keep_laptop_panel_on_when_lid_is_closed: flag(
                "SYNOIK_DEBUG_KEEP_LAPTOP_PANEL_ON_WHEN_LID_IS_CLOSED",
            ),
            disable_monitor_names: flag("SYNOIK_DEBUG_DISABLE_MONITOR_NAMES"),
            strict_new_window_focus_policy: flag("SYNOIK_DEBUG_STRICT_NEW_WINDOW_FOCUS_POLICY"),
            honor_xdg_activation_with_invalid_serial: flag(
                "SYNOIK_DEBUG_HONOR_XDG_ACTIVATION_WITH_INVALID_SERIAL",
            ),
            deactivate_unfocused_windows: flag("SYNOIK_DEBUG_DEACTIVATE_UNFOCUSED_WINDOWS"),
            skip_cursor_only_updates_during_vrr: flag(
                "SYNOIK_DEBUG_SKIP_CURSOR_ONLY_UPDATES_DURING_VRR",
            ),
            audio_in_headless: flag("SYNOIK_DEBUG_AUDIO_IN_HEADLESS"),
            xwayland_in_headless: flag("SYNOIK_DEBUG_XWAYLAND_IN_HEADLESS"),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct DebugPart {
    pub preview_render: Option<PreviewRender>,
    pub dbus_interfaces_in_non_session_instances: Option<Flag>,
    pub wait_for_frame_completion_before_queueing: Option<Flag>,
    pub enable_overlay_planes: Option<Flag>,
    pub disable_cursor_plane: Option<Flag>,
    pub disable_direct_scanout: Option<Flag>,
    pub restrict_primary_scanout_to_matching_format: Option<Flag>,
    pub force_disable_connectors_on_resume: Option<Flag>,
    pub render_drm_device: Option<PathBuf>,
    pub ignored_drm_devices: Vec<PathBuf>,
    pub emulate_zero_presentation_time: Option<Flag>,
    pub disable_resize_throttling: Option<Flag>,
    pub disable_transactions: Option<Flag>,
    pub keep_laptop_panel_on_when_lid_is_closed: Option<Flag>,
    pub disable_monitor_names: Option<Flag>,
    pub strict_new_window_focus_policy: Option<Flag>,
    pub honor_xdg_activation_with_invalid_serial: Option<Flag>,
    pub deactivate_unfocused_windows: Option<Flag>,
    pub skip_cursor_only_updates_during_vrr: Option<Flag>,
    pub audio_in_headless: Option<Flag>,
    pub xwayland_in_headless: Option<Flag>,
}

impl MergeWith<DebugPart> for Debug {
    fn merge_with(&mut self, part: &DebugPart) {
        merge!(
            (self, part),
            dbus_interfaces_in_non_session_instances,
            wait_for_frame_completion_before_queueing,
            enable_overlay_planes,
            disable_cursor_plane,
            disable_direct_scanout,
            restrict_primary_scanout_to_matching_format,
            force_disable_connectors_on_resume,
            emulate_zero_presentation_time,
            disable_resize_throttling,
            disable_transactions,
            keep_laptop_panel_on_when_lid_is_closed,
            disable_monitor_names,
            strict_new_window_focus_policy,
            honor_xdg_activation_with_invalid_serial,
            deactivate_unfocused_windows,
            skip_cursor_only_updates_during_vrr,
            audio_in_headless,
            xwayland_in_headless,
        );

        merge_clone_opt!((self, part), preview_render, render_drm_device);

        self.ignored_drm_devices
            .extend(part.ignored_drm_devices.iter().cloned());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRender {
    Screencast,
    ScreenCapture,
}
