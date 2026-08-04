// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

//! The compositor's compiled-in configuration.
//!
//! There is no config *file*: a session runs on `Config::default()` plus GSettings, so the
//! `Default` impls here *are* the shipped configuration rather than a starting point for a
//! parse. Per-window and per-layer overrides still layer on top at runtime, which is why many
//! types are split in two — `Layout` and `LayoutPart`, say — with `MergeWith` folding the part
//! into the whole.

#[macro_use]
extern crate tracing;

#[macro_use]
pub mod macros;

pub mod animations;
pub mod appearance;
pub mod binds;
pub mod debug;
pub mod gestures;
pub mod input;
pub mod layer_rule;
pub mod layout;
pub mod misc;
pub mod output;
pub mod utils;
pub mod window_rule;
pub mod workspace;

pub use crate::animations::{Animation, Animations};
pub use crate::appearance::*;
pub use crate::binds::*;
pub use crate::debug::Debug;
pub use crate::gestures::Gestures;
pub use crate::input::{Input, ModKey, ScrollMethod, TrackLayout, WarpMouseToFocusMode, Xkb};
pub use crate::layer_rule::LayerRule;
pub use crate::layout::*;
pub use crate::misc::*;
pub use crate::output::{Output, OutputName, Outputs, Position, Vrr};
pub use crate::utils::FloatOrInt;
pub use crate::window_rule::{
    FloatingPosition, PopupsRule, RelativeTo, ResolvedPopupsRules, WindowRule,
};
pub use crate::workspace::{Workspace, WorkspaceLayoutPart};

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub input: Input,
    pub outputs: Outputs,
    pub spawn_at_startup: Vec<SpawnAtStartup>,
    pub spawn_sh_at_startup: Vec<SpawnShAtStartup>,
    pub layout: Layout,
    pub prefer_no_csd: bool,
    pub cursor: Cursor,
    pub screenshot_path: ScreenshotPath,
    pub clipboard: Clipboard,
    pub hotkey_overlay: HotkeyOverlay,
    pub config_notification: ConfigNotification,
    pub animations: Animations,
    pub blur: Blur,
    pub gestures: Gestures,
    pub overview: Overview,
    pub environment: Environment,
    pub xwayland_satellite: XwaylandSatellite,
    pub window_rules: Vec<WindowRule>,
    pub layer_rules: Vec<LayerRule>,
    pub switch_events: SwitchBinds,
    pub debug: Debug,
    pub workspaces: Vec<Workspace>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled-in defaults are what every session runs on now that there is no config
    /// file, so the shape they used to be checked against — `default-config.kdl` — is gone
    /// with it. What is left worth pinning is that the input defaults are GNOME's schema
    /// defaults; `peripherals_defaults_match_a_pristine_gnome_store` checks the whole set
    /// against a real GSettings store.
    #[test]
    fn the_input_defaults_are_gnomes() {
        let config = Config::default();
        assert!(config.input.touchpad.tap);
        assert!(config.input.touchpad.natural_scroll);
        assert!(!config.input.keyboard.numlock);
        assert_eq!(config.input.keyboard.repeat_delay, 500);
        assert_eq!(config.input.keyboard.repeat_rate, 33);
    }
}
