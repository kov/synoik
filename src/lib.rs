#[macro_use]
extern crate tracing;

#[cfg(feature = "dbus")]
pub mod a11y;
pub mod animation;
pub mod app_system;
pub mod audio;
pub mod backend;
pub mod backlight;
pub mod brightness;
pub mod calendar_events;
pub mod cli;
pub mod cursor;
#[cfg(feature = "dbus")]
pub mod dbus;
pub mod end_session;
pub mod frame_clock;
pub mod frame_log;
pub mod gnome;
pub mod handlers;
pub mod idle_monitor;
pub mod input;
pub mod ipc;
pub mod keyboard_layout;
pub mod layer;
pub mod layout;
pub mod monitors_xml;
pub mod mpris;
pub mod niri;
pub mod notifications;
#[cfg(feature = "pipewire")]
pub mod pipewire_audio;
pub mod protocols;
pub mod recording;
pub mod render_helpers;
pub mod rubber_band;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod screencasting;
pub mod system_status;
pub mod ui;
pub mod utils;
pub mod wallpaper;
pub mod window;
pub mod world_clocks;

#[cfg(test)]
mod tests;
