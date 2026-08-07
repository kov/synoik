// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! Resolving which monitor and workspace a not-yet-mapped window belongs to.
//!
//! A window that has been initially configured but has not yet mapped still has to answer "which
//! monitor am I on?" every time the client changes its mind — a `set_maximized` before the first
//! buffer has to be configured against *some* workspace to pick a size. Before this module, that
//! chain was open-coded at five sites in `handlers::xdg_shell` (initial configure plus the
//! maximize/unmaximize/fullscreen/unfullscreen requests), four of which carried a
//! `FIXME: deduplicate`.
//!
//! The chain is the same everywhere; only the *seeds* differ, so those are what callers pass:
//!
//! 1. an explicitly named workspace,
//! 2. an explicitly chosen output,
//! 3. the window's parent's monitor (a dialog follows its parent),
//! 4. the monitor under the pointer,
//! 5. the active monitor.
//!
//! **Step 4 is only ever seeded by the initial configure.** That is a ported GNOME behavior —
//! mutter seeds `window->monitor` from the pointer for a window that gave no position hint
//! (`window.c:1245-1259`) — and it is deliberately a *once* decision: re-consulting the pointer
//! from a later request would let a window hop monitors because the mouse moved. Post-configure
//! callers pass the output they already resolved instead.

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::monitor::Monitor;
use super::workspace::Workspace;
use super::{Layout, LayoutElement};

/// What a caller knows about where a window wants to go.
///
/// Every field is optional and they are consulted in the order given by
/// [`Layout::resolve_placement`]. A caller that has no opinion on a seed leaves it `None`, which is
/// how the differences between call sites are expressed.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlacementSeeds<'a> {
    /// A workspace requested by name, e.g. from the `open-on-workspace` window rule.
    pub workspace_name: Option<&'a str>,

    /// An output the window asked for, or the one we resolved for it previously.
    pub output: Option<&'a Output>,

    /// The window's parent surface, if it has one; a dialog opens on its parent's monitor.
    pub parent: Option<&'a WlSurface>,

    /// The output under the pointer.
    ///
    /// Only the initial configure seeds this — see the module docs. Passing it from a
    /// post-configure request would make windows follow the mouse between monitors.
    pub pointer_output: Option<&'a Output>,
}

/// The monitor and workspace a window should be configured against.
#[derive(Debug)]
pub struct PlacementTarget<'a, W: LayoutElement> {
    /// The resolved monitor. `None` only when there are no monitors at all.
    pub monitor: Option<&'a Monitor<W>>,

    /// Whether the monitor came from the window's parent rather than from a choice of its own.
    ///
    /// Callers use this to decide *not* to remember the output on the window: a dialog should
    /// re-fetch its parent's monitor when it maps, in case the parent moved in between. See
    /// [`PlacementTarget::output_to_store`].
    pub follows_parent: bool,

    /// The workspace to configure against.
    ///
    /// `None` when there are no monitors, and also when a workspace was requested by name but no
    /// workspace on the resolved monitor has that name — in that case the caller skips
    /// configuring rather than silently falling back to the active workspace.
    pub workspace: Option<&'a Workspace<W>>,
}

impl<W: LayoutElement> PlacementTarget<'_, W> {
    /// The output to remember on the window, or `None` if it should be re-resolved at map time.
    pub fn output_to_store(&self) -> Option<Output> {
        if self.follows_parent {
            return None;
        }
        self.monitor.map(|mon| mon.output().clone())
    }
}

impl<W: LayoutElement> Layout<W> {
    /// Resolves where a not-yet-mapped window should be configured.
    ///
    /// See the module docs for the seed order and why the pointer seed is special.
    pub fn resolve_placement(&self, seeds: PlacementSeeds<'_>) -> PlacementTarget<'_, W> {
        // A named workspace wins: it pins the monitor too, since a workspace lives on one.
        let mon = seeds
            .workspace_name
            .and_then(|name| self.monitor_for_workspace(name))
            .map(|mon| (mon, false));

        // Then an explicitly chosen output.
        let mon = mon.or_else(|| {
            seeds
                .output
                .and_then(|o| self.monitor_for_output(o))
                .map(|mon| (mon, false))
        });

        // Then the parent's monitor, flagged so the caller knows not to pin it.
        let mon = mon.or_else(|| {
            seeds
                .parent
                .and_then(|parent| self.find_window_and_output(parent))
                .and_then(|(_window, output)| output)
                .and_then(|o| self.monitor_for_output(o))
                .map(|mon| (mon, true))
        });

        // Then the pointer's monitor — initial configure only.
        let mon = mon.or_else(|| {
            seeds
                .pointer_output
                .and_then(|o| self.monitor_for_output(o))
                .map(|mon| (mon, false))
        });

        // Finally the active monitor.
        let mon = mon.or_else(|| self.active_monitor_ref().map(|mon| (mon, false)));

        let follows_parent = mon.is_some_and(|(_, from_parent)| from_parent);
        let monitor = mon.map(|(mon, _)| mon);

        // A named workspace resolves against the monitor we landed on. If that monitor has no
        // workspace by that name we deliberately yield `None` rather than falling back to the
        // active workspace — the caller asked for a specific workspace, and quietly configuring
        // against a different one would size the window wrong.
        let workspace = seeds
            .workspace_name
            .and_then(|name| monitor.map(|mon| mon.find_named_workspace(name)))
            .unwrap_or_else(|| {
                monitor
                    .map(|mon| mon.active_workspace_ref())
                    .or_else(|| self.active_workspace())
            });

        PlacementTarget {
            monitor,
            follows_parent,
            workspace,
        }
    }
}
