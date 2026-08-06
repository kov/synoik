// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use fixture::Fixture;
use synoik_config::{Config, WindowingMode};

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
mod background_effect;
mod floating;
mod fullscreen;
mod gnome;
mod layer_shell;
mod perf_probe;
mod producer_sync;
mod remove_output;
mod transactions;
mod vulkan_render;
mod window_opening;
