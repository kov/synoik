use fixture::Fixture;
use niri_config::{Config, WindowingMode};

/// Puts a config into niri's scrollable-tiling mode.
///
/// The upstream niri test corpus pins the behavior of the scrolling layer,
/// which this fork keeps as an opt-in behind `windowing-mode "scrolling"`
/// (the default is GNOME-style floating). Wrap the corpus' configs with this
/// so those tests keep testing what they were written against.
fn scrolling(mut config: Config) -> Config {
    config.layout.windowing_mode = WindowingMode::Scrolling;
    config
}

mod client;
pub(crate) mod fixture;
mod server;

mod animations;
mod floating;
mod fullscreen;
mod gnome;
mod layer_shell;
mod producer_sync;
mod remove_output;
mod transactions;
mod vulkan_render;
mod window_opening;
