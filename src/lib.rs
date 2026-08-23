// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

#[macro_use]
extern crate tracing;

pub mod a11y;
pub mod animation;
pub mod app_system;
pub mod audio;
pub mod backend;
pub mod backlight;
pub mod brightness;
pub mod calendar_events;
pub mod cli;
pub mod clipboard;
pub mod cursor;
pub mod dbus;
pub mod dbusmenu;
pub mod end_session;
pub mod frame_clock;
pub mod frame_log;
pub mod gnome;
pub mod handlers;
pub mod idle_monitor;
pub mod image_source;
pub mod input;
pub mod input_method;
pub mod ipc;
pub mod keyboard_layout;
pub mod layer;
pub mod layout;
pub mod monitors_xml;
pub mod mpris;
pub mod notifications;
pub mod output_identity;
#[cfg(feature = "pipewire")]
pub mod pipewire_audio;
/// The polkit dialog's state machine, answering the D-Bus authentication agent.
pub mod polkit_dialog;
pub mod protocols;
pub mod recording;
pub mod render_helpers;
pub mod rubber_band;
pub mod screen_shield;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod screencasting;
pub mod session_state;
pub mod status_notifier;
pub mod synoik;
pub mod system_status;
pub mod ui;
pub mod unlock_dialog;
pub mod utils;
pub mod wallpaper;
pub mod window;
pub mod workspace_names;
pub mod world_clocks;

#[cfg(test)]
mod tests;
