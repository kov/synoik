// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! The placement seed order — see `layout::placement`.
//!
//! These drive `Layout<TestWindow>` directly, so they pin the resolution chain without a Wayland
//! client. `TestWindow::is_wl_surface` is always false, so the *parent* seed cannot be exercised
//! here; it is covered against real xdg-shell requests in `src/tests/gnome.rs`.

use super::*;
use crate::layout::placement::{PlacementSeeds, PlacementTarget};

/// A layout with `n` outputs named `output1..=outputn`, focused on `output1`.
fn layout_with_outputs(n: usize) -> Layout<TestWindow> {
    let mut layout = Layout::default();
    for id in 1..=n {
        Op::AddOutput(id).apply(&mut layout);
    }
    Op::FocusOutput(1).apply(&mut layout);
    layout
}

fn output_named(layout: &Layout<TestWindow>, name: &str) -> Output {
    layout.outputs().find(|o| o.name() == name).unwrap().clone()
}

fn monitor_name(target: &PlacementTarget<'_, TestWindow>) -> Option<String> {
    target.monitor.map(|mon| mon.output().name())
}

#[test]
fn with_no_seeds_it_falls_back_to_the_active_monitor() {
    let mut layout = layout_with_outputs(2);
    Op::FocusOutput(2).apply(&mut layout);

    let target = layout.resolve_placement(PlacementSeeds::default());

    assert_eq!(monitor_name(&target).as_deref(), Some("output2"));
    assert!(!target.follows_parent);
    assert!(target.workspace.is_some());
}

#[test]
fn an_explicit_output_beats_the_active_monitor() {
    let layout = layout_with_outputs(2);
    let output2 = output_named(&layout, "output2");

    let target = layout.resolve_placement(PlacementSeeds {
        output: Some(&output2),
        ..Default::default()
    });

    assert_eq!(monitor_name(&target).as_deref(), Some("output2"));
    assert!(!target.follows_parent);
}

#[test]
fn a_named_workspace_beats_an_explicit_output() {
    let mut layout = layout_with_outputs(2);
    // Name output2's active workspace "ws2", then come back to output1.
    Op::FocusOutput(2).apply(&mut layout);
    Op::SetWorkspaceName {
        new_ws_name: 2,
        ws_name: None,
    }
    .apply(&mut layout);
    Op::FocusOutput(1).apply(&mut layout);

    let output1 = output_named(&layout, "output1");
    let target = layout.resolve_placement(PlacementSeeds {
        workspace_name: Some("ws2"),
        output: Some(&output1),
        ..Default::default()
    });

    assert_eq!(
        monitor_name(&target).as_deref(),
        Some("output2"),
        "the named workspace pins the monitor, outranking the output seed"
    );
    assert_eq!(
        target
            .workspace
            .and_then(|ws| ws.name())
            .map(String::as_str),
        Some("ws2"),
        "the named workspace itself must come back, not the monitor's active one"
    );
}

#[test]
fn an_explicit_output_beats_the_pointer() {
    let layout = layout_with_outputs(3);
    let output2 = output_named(&layout, "output2");
    let output3 = output_named(&layout, "output3");

    let target = layout.resolve_placement(PlacementSeeds {
        output: Some(&output2),
        pointer_output: Some(&output3),
        ..Default::default()
    });

    assert_eq!(
        monitor_name(&target).as_deref(),
        Some("output2"),
        "an output the window asked for outranks wherever the mouse happens to be"
    );
}

#[test]
fn the_pointer_beats_the_active_monitor() {
    let layout = layout_with_outputs(2);
    let output2 = output_named(&layout, "output2");

    let target = layout.resolve_placement(PlacementSeeds {
        pointer_output: Some(&output2),
        ..Default::default()
    });

    assert_eq!(
        monitor_name(&target).as_deref(),
        Some("output2"),
        "output1 is active, but the pointer is over output2"
    );
}

#[test]
fn a_stale_output_seed_falls_through_to_the_next_seed() {
    let mut layout = layout_with_outputs(2);
    let stale = output_named(&layout, "output2");
    Op::RemoveOutput(2).apply(&mut layout);

    // A window that resolved to output2 before it was unplugged must still land somewhere.
    let target = layout.resolve_placement(PlacementSeeds {
        output: Some(&stale),
        ..Default::default()
    });

    assert_eq!(monitor_name(&target).as_deref(), Some("output1"));
}

#[test]
fn a_named_workspace_that_does_not_exist_yields_no_workspace() {
    let layout = layout_with_outputs(1);

    let target = layout.resolve_placement(PlacementSeeds {
        workspace_name: Some("nonexistent"),
        ..Default::default()
    });

    // The monitor still resolves, through the active-monitor fallback...
    assert_eq!(monitor_name(&target).as_deref(), Some("output1"));
    // ...but we deliberately do not substitute the active workspace for the one that was asked
    // for, because that would size the window against the wrong workspace.
    assert!(
        target.workspace.is_none(),
        "a named workspace that matched no monitor must not fall back to the active workspace"
    );
}

#[test]
fn with_no_outputs_everything_is_none() {
    let layout: Layout<TestWindow> = Layout::default();

    let target = layout.resolve_placement(PlacementSeeds::default());

    assert!(target.monitor.is_none());
    assert!(target.workspace.is_none());
    assert!(!target.follows_parent);
    assert!(target.output_to_store().is_none());
}

#[test]
fn output_to_store_drops_a_monitor_inherited_from_a_parent() {
    let layout = layout_with_outputs(1);

    let inherited = PlacementTarget {
        monitor: layout.active_monitor_ref(),
        follows_parent: true,
        workspace: layout.active_workspace(),
    };
    assert!(
        inherited.output_to_store().is_none(),
        "a dialog must re-fetch its parent's monitor at map time, not pin one now"
    );

    let chosen = PlacementTarget {
        monitor: layout.active_monitor_ref(),
        follows_parent: false,
        workspace: layout.active_workspace(),
    };
    assert!(chosen.output_to_store().is_some());
}
