//! GNOME-behavior conformance corpus.
//!
//! This module is the executable "conformance corpus" from the fork strategy
//! (`docs/fork/STRATEGY.md` §8): instead of treating GNOME's source as the spec, each
//! GNOME behavior we port is pinned here as a headless, deterministic test that drives
//! the real compositor through `State::do_action` (and, later, synthetic input) and
//! asserts on observable state via the same surfaces a client/IPC sees.
//!
//! The first entries are *characterization* tests that pin the inherited niri overview
//! contract before we reshape it toward GNOME's Activities overview (Experiment 1).

use niri_config::Action;

use super::*;

/// The overview opens and closes through the action-dispatch path, and
/// `is_overview_open()` reflects every transition.
///
/// This is the template every later GNOME-behavior test follows: build a headless
/// fixture, drive a real action, settle animations, assert observable state.
#[test]
fn overview_opens_and_closes_via_action() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(
        !f.niri().layout.is_overview_open(),
        "overview must start closed"
    );

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "OpenOverview must open the overview"
    );

    // Opening an already-open overview is a no-op, not a toggle.
    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "OpenOverview must be idempotent"
    );

    f.niri_state().do_action(Action::CloseOverview, false);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "CloseOverview must close the overview"
    );
}

/// `ToggleOverview` flips the open state on each invocation.
#[test]
fn toggle_overview_flips_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(!f.niri().layout.is_overview_open());

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri_complete_animations();
    assert!(f.niri().layout.is_overview_open(), "first toggle opens");

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri_complete_animations();
    assert!(!f.niri().layout.is_overview_open(), "second toggle closes");
}
