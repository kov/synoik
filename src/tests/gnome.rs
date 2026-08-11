// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use insta::assert_snapshot;
use smithay::backend::input::ButtonState;
use smithay::input::keyboard::Keysym;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel;
use smithay::utils::user_data::UserDataMap;
use smithay::wayland::xdg_activation::XdgActivationTokenData;
use synoik_config::{Action, Config};
use wayland_client::protocol::wl_keyboard::KeyState as WlKeyState;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::{ClientId, SessionEvent, TextInputEvent as ClientEv};
use super::*;
use crate::gnome::{
    Accel, AccelMods, AccelTrigger, FocusNewWindows, GnomeKeyAction, KeybindingAction,
};
use crate::layout::SizingMode;
use crate::protocols::raw::xdg_session_management::v1::client::xdg_session_manager_v1::Reason;
use crate::protocols::raw::xdg_session_management::v1::client::xdg_session_v1::XdgSessionV1;
use crate::protocols::raw::xdg_session_management::v1::client::xdg_toplevel_session_v1::XdgToplevelSessionV1;
use crate::session_state::{ToplevelRecord, WindowState};
use crate::status_notifier::ItemProps;
use crate::ui::osd::OsdLevel;
use crate::utils::get_monotonic_time;

/// Linux evdev codes (`input-event-codes.h`) for the inputs these tests inject.
const KEY_ESC: u32 = 1;
const KEY_APOSTROPHE: u32 = 40;
const KEY_1: u32 = 2;
const KEY_2: u32 = 3;
const KEY_TAB: u32 = 15;
const KEY_W: u32 = 17;
const KEY_E: u32 = 18;
const KEY_R: u32 = 19;
const KEY_T: u32 = 20;
const KEY_U: u32 = 22;
const KEY_ENTER: u32 = 28;
const KEY_BACKSPACE: u32 = 14;
const KEY_LEFTCTRL: u32 = 29;
const KEY_A: u32 = 30;
const KEY_CAPSLOCK: u32 = 58;
const KEY_SPACE: u32 = 57;
const KEY_RIGHT: u32 = 106;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_Z: u32 = 44;
const KEY_LEFTALT: u32 = 56;
const KEY_F2: u32 = 60;
const KEY_F4: u32 = 62;
const KEY_F6: u32 = 64;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_DOWN: u32 = 108;
const KEY_PAGEUP: u32 = 104;
const KEY_HOME: u32 = 102;
const KEY_END: u32 = 107;
const KEY_PAGEDOWN: u32 = 109;
const KEY_O: u32 = 24;
const KEY_S: u32 = 31;
const KEY_F10: u32 = 68;
const KEY_J: u32 = 36;
const KEY_K: u32 = 37;
const KEY_V: u32 = 47;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;
pub(super) const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Tap a key: press and release.
fn tap(f: &mut Fixture, key: u32) {
    f.key_press(key);
    f.key_release(key);
}

/// Map a client window; as the only (newly mapped) window it takes keyboard
/// focus, which per-focused-surface state like keyboard-shortcuts-inhibit
/// applies to.
fn map_focused_window(f: &mut Fixture, id: ClientId) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

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
        !f.synoik().layout.is_overview_open(),
        "overview must start closed"
    );

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "OpenOverview must open the overview"
    );

    // Opening an already-open overview is a no-op, not a toggle.
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "OpenOverview must be idempotent"
    );

    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "CloseOverview must close the overview"
    );
}

/// `ToggleOverview` flips the open state on each invocation.
#[test]
fn toggle_overview_flips_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(!f.synoik().layout.is_overview_open());

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik_complete_animations();
    assert!(f.synoik().layout.is_overview_open(), "first toggle opens");

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "second toggle closes"
    );
}

/// Shove the pointer into the top-left corner `times` times, `px` px of overshoot each,
/// starting from wherever it is. Every event past the first is entirely discarded by the
/// output clamp, which is the pressure a hot corner accumulates.
fn push_into_corner(f: &mut Fixture, times: usize, px: f64) {
    for _ in 0..times {
        f.pointer_motion(-px, -px);
    }
}

/// GNOME's hot corner is a *pressure* barrier: brushing the corner does nothing, and the
/// overview only opens once the pointer has pushed 100 px into it inside a second
/// (`HOT_CORNER_PRESSURE_THRESHOLD`/`TIMEOUT`, `layout.js:24-25`).
#[test]
fn hot_corner_needs_pressure_to_open_the_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Arriving at the corner is not pushing into it: the motion that lands there is spent
    // travelling, and nothing is discarded.
    pointer_motion_to(&mut f, 0., 0.);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "merely touching the corner must not open the overview"
    );

    // Nor is a nudge: 5 px per event, capped by nothing, is 25 px of the 100 px budget.
    push_into_corner(&mut f, 5, 5.);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a nudge below the threshold must not open the overview"
    );

    // Sustained pushing does. The per-event cap is 15 px (`layout.js:1401`), so this is
    // 5 more events at 15 px on top of the 25 already banked.
    push_into_corner(&mut f, 5, 20.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "pushing past the threshold must open the overview"
    );
}

/// Once it has fired, the barrier latches: resting in the corner and pushing on doesn't
/// toggle the overview shut again (`layout.js:1375-1377`). It re-arms when the pointer
/// leaves the corner's region.
#[test]
fn hot_corner_latches_until_the_pointer_leaves() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    pointer_motion_to(&mut f, 0., 0.);
    push_into_corner(&mut f, 10, 20.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "the corner fires once"
    );

    push_into_corner(&mut f, 20, 20.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "pushing on must not toggle the overview back shut"
    );

    // Leave the corner's L entirely, then push again.
    pointer_motion_to(&mut f, 960., 540.);
    pointer_motion_to(&mut f, 0., 0.);
    push_into_corner(&mut f, 10, 20.);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "leaving re-arms the corner, so the next push toggles"
    );
}

/// The barrier listens along an L of `panelBox.height` down the left edge and the same
/// across the top (`HotCorner.setBarrierSize`, `layout.js:1195-1233`) — not on a single
/// pixel. Pushing into the left edge just below the panel builds the same pressure.
#[test]
fn hot_corner_listens_along_both_edges() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let size = crate::ui::panel::panel_height();

    // Inside the vertical segment: on the left edge, above `size`.
    pointer_motion_to(&mut f, 0., size / 2.);
    for _ in 0..10 {
        f.pointer_motion(-20., 0.);
    }
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "the left edge within panel height of the corner is part of the hot corner"
    );

    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();

    // Past the segment's end: the same push does nothing.
    pointer_motion_to(&mut f, 0., size + 100.);
    for _ in 0..10 {
        f.pointer_motion(-20., 0.);
    }
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "the left edge below the barrier must not trigger"
    );
}

/// Sliding *along* an edge is not pushing *into* it: crossing the top of the screen on the
/// way somewhere else must not arrive with the corner half-triggered (`layout.js:1393-1396`).
#[test]
fn sliding_along_the_top_edge_builds_no_pressure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    pointer_motion_to(&mut f, 400., 0.);
    // Each event pushes 4 px up (all discarded, the pointer is already at y = 0) while
    // travelling 40 px sideways.
    for _ in 0..40 {
        f.pointer_motion(-10., -4.);
    }
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "travelling along the edge must not open the overview"
    );
}

/// `org.gnome.desktop.interface enable-hot-corners` turns the corner off entirely
/// (`layout.js:440-443`).
#[test]
fn hot_corner_honors_enable_hot_corners() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().synoik.gnome_settings.enable_hot_corners = false;

    pointer_motion_to(&mut f, 0., 0.);
    push_into_corner(&mut f, 20, 20.);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a disabled hot corner must not open the overview"
    );
}

/// The dock — **our divergence**, not GNOME (`docs/fork/dock-divergence.md`). Pushing into the
/// bottom edge slides the dash out; touching the edge does not.
#[test]
fn the_dock_needs_pressure_on_the_bottom_edge() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Arrive at the bottom edge without pushing into it.
    pointer_motion_to(&mut f, 960., 1079.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().dock.area(&output).is_none(),
        "touching the bottom edge must not summon the dock"
    );

    // Travelling *along* it doesn't either, however far.
    for _ in 0..40 {
        f.pointer_motion(-20., 4.);
    }
    f.synoik_complete_animations();
    assert!(
        f.synoik().dock.area(&output).is_none(),
        "sliding along the bottom edge must not summon the dock"
    );

    // Pushing does.
    for _ in 0..10 {
        f.pointer_motion(0., 20.);
    }
    assert!(
        f.synoik().dock.area(&output).is_some(),
        "pushing into the bottom edge must slide the dock out"
    );
}

/// The dock is the same dash the overview draws: it hit-tests, hovers and activates through the
/// very same paths, just at a different place on screen.
#[test]
fn the_dock_hit_tests_the_same_dash_as_the_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    seed_favorites(&mut f, &["a.desktop", "b.desktop"]);

    pointer_motion_to(&mut f, 960., 1079.);
    for _ in 0..10 {
        f.pointer_motion(0., 20.);
    }
    f.synoik_complete_animations();

    let area = f.synoik().dock.area(&output).expect("the dock is out");
    assert!(
        f.synoik().dock_owns_dash(&output),
        "with the overview shut, the dock owns the dash"
    );

    // The first favorite's tile, asked of the dash rather than guessed at.
    let center = f
        .synoik()
        .dash
        .tile_center(0, area)
        .expect("a favorite to aim at");
    pointer_motion_to(&mut f, center.x, center.y);
    assert!(
        f.synoik().dash.hovered_for_test().is_some(),
        "a pointer over a dock icon must light it up, as in the overview"
    );

    // And leaving the dock's area hides it again, after the grace period.
    pointer_motion_to(&mut f, 960., 300.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().dash.hovered_for_test().is_none(),
        "leaving drops the hover"
    );
}

/// Right-clicking a dock icon opens its context menu and leaves it up.
///
/// Two things were wrong, both from the dock reusing overview code with no overview behind it.
/// The menu anchored to `controls.dash` — the overview's slot, not where the dock draws — and
/// the "an app menu cannot outlive its overview" rule (`appDisplay.js:3039-3040`) closed it on
/// the very next frame, so nothing was ever seen.
#[test]
fn the_dock_opens_an_icons_context_menu_and_keeps_it_up() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    seed_favorites(&mut f, &["a.desktop", "b.desktop"]);

    pointer_motion_to(&mut f, 960., 1079.);
    for _ in 0..10 {
        f.pointer_motion(0., 20.);
    }
    f.synoik_complete_animations();
    assert!(f.synoik().dock_owns_dash(&output), "the dock is out");

    let area = f.synoik().dock.area(&output).expect("the dock is out");
    let center = f
        .synoik()
        .dash
        .tile_center(0, area)
        .expect("a favorite to aim at");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    assert!(
        f.synoik().panel_popover.is_app_menu(),
        "a right-click on a dock icon must open its context menu"
    );
    // It hangs off the icon the dock drew: a dash menu's arrow is on the bottom
    // (`popupMenuSide: St.Side.BOTTOM`, `dash.js:27`), so the box sits above the tile.
    let tile = f.synoik().dash.tile_rect(0, area).expect("tile 0");
    let box_origin = f.synoik().panel_popover.content_location(&output);
    assert!(
        box_origin.y < tile.loc.y,
        "the menu opens upward out of the dock, got {box_origin:?} for a tile at {tile:?}"
    );

    // The frame-by-frame sync is where it used to die: the overview is shut, so the
    // outlive-the-overview rule fired and closed it before anyone saw it. Run the fade out
    // too — `is_app_menu` stays true while a popover closes, so asserting on the frame after
    // the sync alone would not notice.
    f.synoik().advance_animations();
    f.synoik_complete_animations();
    assert!(
        f.synoik().panel_popover.is_app_menu(),
        "the menu must survive the frame — there is no overview for it to outlive"
    );

    // And the dock must not slide out from under it once the pointer moves up to the menu.
    pointer_motion_to(&mut f, 960., 300.);
    f.synoik_complete_animations();
    assert!(
        f.synoik().dock.area(&output).is_some(),
        "the dock is held open while one of its icons has a menu up"
    );

    assert!(
        f.synoik().dock.next_wakeup().is_none(),
        "and no hide timer is armed while it is held"
    );

    // Closing the menu releases the hold: the hide deadline is armed again, and the dock goes
    // away on its own the way it always did. (`Dock::a_menu_holds_the_dock_open` runs out that
    // clock; here the point is that the popover state drives it.)
    f.synoik().panel_popover.close_immediately();
    f.synoik().advance_animations();
    assert!(
        f.synoik().dock.next_wakeup().is_some(),
        "with the menu gone the dock must be free to hide again"
    );
}

/// The dock's show-apps button opens the overview *at the app grid*, the same as `<Super>A`.
///
/// gnome-shell binds one handler (`_toggleAppsPage`, `overviewControls.js:660-667,481`) to both
/// the button and `toggle-application-view`, and its overview-shut branch is
/// `Main.overview.show(ControlsState.APP_GRID)`. Ours toggled the grid only, which is a no-op
/// with the overview down — so on the dock the button did nothing at all.
#[test]
fn the_dock_show_apps_button_opens_the_app_grid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    seed_favorites(&mut f, &["a.desktop", "b.desktop"]);

    pointer_motion_to(&mut f, 960., 1079.);
    for _ in 0..10 {
        f.pointer_motion(0., 20.);
    }
    f.synoik_complete_animations();
    assert!(
        f.synoik().dock_owns_dash(&output),
        "the dock is out with the overview shut"
    );

    // Click the show-apps button, asked of the dash rather than guessed at.
    let area = f.synoik().dock.area(&output).expect("the dock is out");
    let i = f.synoik().dash.show_apps_index();
    let center = f
        .synoik()
        .dash
        .tile_center(i, area)
        .expect("the show-apps button");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "it must open the overview"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "and land on the app grid, not the window picker"
    );
}

/// GNOME's "overlay key": tapping Super on its own — pressed and released with
/// nothing in between — toggles the Activities overview. This is the first
/// genuinely GNOME-distinct behavior (niri has no overlay key), and it pins
/// mutter's `process_special_modifier_key` semantics: arm on a bare Super press,
/// fire on the matching release.
#[test]
fn super_tap_toggles_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(
        !f.synoik().layout.is_overview_open(),
        "overview must start closed"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "a lone Super tap opens the overview"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a second Super tap closes it"
    );
}

/// Two quick Super taps land in the app grid: the second tap "shifts a state up"
/// (window picker → app grid) instead of toggling the overview back shut
/// (`overviewControls.js:419-438`). With animations on, "quick" is not a timer —
/// gnome-shell asks whether its state adjustment is still transitioning upward,
/// so the escalation window is exactly the open animation.
#[test]
fn double_super_tap_opens_the_app_grid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Deliberately do NOT settle: the overview must still be animating open.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    assert!(
        f.synoik().layout.is_overview_open(),
        "the first tap opens the overview"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "a second tap during the open animation must not close the overview"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "it must shift a state up, into the app grid"
    );

    // A third tap, now that nothing is transitioning, toggles as always — the
    // shift is clamped at APP_GRID, so it never toggles the grid back down.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a tap after the animation settles closes the overview"
    );
}

/// The other half of the same branch: a second tap that arrives after the open
/// animation has settled is a plain toggle, and must leave the app grid alone.
#[test]
fn slow_second_super_tap_closes_the_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a second tap after the open animation closes the overview"
    );
    assert!(
        !f.synoik().layout.is_app_grid_open(),
        "and must not have shifted into the app grid"
    );
}

/// With animations off there is no transition to catch, so gnome-shell falls back
/// to a real timer: the overview is up and the previous overlay-key tap fired less
/// than `Overview.ANIMATION_TIME` (250 ms) ago (`overviewControls.js:431-433`).
#[test]
fn double_super_tap_opens_the_app_grid_without_animations() {
    let mut config = Config::default();
    config.animations.off = true;
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "two taps within 250 ms must reach the app grid with animations off"
    );

    // Past the window, the same tap is a plain toggle again.
    f.advance_input_time(300);
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a tap more than 250 ms later closes the overview"
    );
}

/// GNOME's default overlay-key is `"Super"`, meaning *either* Super. So a lone
/// right-Super tap opens the overview too, with no setting change.
#[test]
fn right_super_tap_toggles_overview_by_default() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_RIGHTMETA);
    f.key_release(KEY_RIGHTMETA);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "a lone right Super tap opens the overview by default"
    );
}

/// Using Super as a modifier (Super+key) must *not* trigger the overlay key:
/// once another key participates, the press is no longer a lone tap.
///
/// The second key has to be one nothing binds, or this stops measuring the
/// overlay key: `<Super>a` used to stand in for "any Super+key" until it became
/// `toggle-application-view`, whose whole job is to open the overview.
#[test]
fn super_plus_key_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_Z);
    f.key_release(KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "Super+key must not trigger the overlay key"
    );
}

/// Pointer activity between the Super press and release cancels the tap, so the
/// overview must not open (mutter clears `overlay_key_only_pressed` on a click).
#[test]
fn super_then_click_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Click mid-screen, clear of the panel's Activities button (which itself
    // toggles the overview) so this exercises only the tap-cancel.
    f.pointer_motion(960., 540.);

    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "a click between Super press and release must cancel the tap"
    );
}

/// The overlay key's client-visible wire contract, as mutter implements it:
/// the arming Super *press* is delivered to the focused client (mutter
/// propagates it), but the *release* that fires the overlay key is not
/// (mutter returns CLUTTER_EVENT_STOP for it). A canceled tap delivers both.
/// Clients cope with the missing release because focus moves to the overview,
/// and a keyboard leave releases all keys client-side.
#[test]
fn overlay_key_firing_release_is_not_sent_to_the_client() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).get_keyboard();
    f.roundtrip(id);
    let _ = f.client(id).take_key_events();

    // A canceled tap (Super+Z — a chord nothing binds, so the events reach the
    // client instead of being eaten by the binding): all four key events arrive.
    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_Z);
    f.key_release(KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).take_key_events(),
        vec![
            (KEY_LEFTMETA, WlKeyState::Pressed),
            (KEY_Z, WlKeyState::Pressed),
            (KEY_Z, WlKeyState::Released),
            (KEY_LEFTMETA, WlKeyState::Released),
        ],
        "a canceled tap must deliver both Super key events to the client"
    );

    // A firing tap: the press is delivered, the release is swallowed.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert!(
        f.synoik().layout.is_overview_open(),
        "the lone tap must have fired"
    );
    assert_eq!(
        f.client(id).take_key_events(),
        vec![(KEY_LEFTMETA, WlKeyState::Pressed)],
        "a firing tap must deliver the Super press but swallow the release"
    );
}

/// The overlay key honors `org.gnome.mutter overlay-key`: setting it to `None`
/// (mutter's empty-string "disabled") means a Super tap does nothing.
#[test]
fn overlay_key_setting_can_disable() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().gnome_settings.overlay_keys.clear();

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "a disabled overlay key must not open the overview"
    );
}

/// Pointer *motion* between the Super press and release does NOT cancel the
/// tap. This is deliberate in mutter: `meta_keybindings_process_event` resets
/// the pending tap on button, scroll, and touch begin/end events only — so
/// wiggling the mouse while tapping Super still opens the overview.
#[test]
fn super_tap_with_pointer_motion_still_toggles() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.pointer_motion(5.0, 5.0);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "pointer motion must not cancel a pending Super tap"
    );
}

/// A scroll between the Super press and release cancels the tap (mutter's
/// CLUTTER_SCROLL arm).
#[test]
fn super_then_scroll_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.scroll_wheel();
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "a scroll between Super press and release must cancel the tap"
    );
}

/// A touch tap between the Super press and release cancels the tap (mutter's
/// CLUTTER_TOUCH_BEGIN/END arms).
#[test]
fn super_then_touch_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.touch_down(100.0, 100.0);
    f.touch_up();
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "a touch between Super press and release must cancel the tap"
    );
}

/// Super tapped while a real modifier is held is not a lone tap: mutter only
/// arms when no non-ignored modifier is active in the press's modifier state
/// (`process_special_modifier_key`), so Shift+Super does nothing.
#[test]
fn shift_held_super_tap_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTSHIFT);
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.key_release(KEY_LEFTSHIFT);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "Super tapped with Shift held must not trigger the overlay key"
    );
}

/// A client holding an active keyboard-shortcuts-inhibit on the focused window
/// disables the overlay key: mutter checks the inhibitor when arming
/// (`process_overlay_key`), so Super passes through to e.g. a VM viewer or
/// remote-desktop client instead of opening the overview. Releasing the
/// inhibitor restores the overlay key.
#[test]
fn shortcuts_inhibit_disables_overlay_key() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.client(id).inhibit_shortcuts(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "the overlay key must be inert while the focused window inhibits shortcuts"
    );

    f.client(id).release_shortcuts_inhibitor(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "releasing the inhibitor must restore the overlay key"
    );
}

/// The GNOME `close` keybinding (`org.gnome.desktop.wm.keybindings`, default
/// `<Alt>F4`) asks the focused window to close, through the same xdg-toplevel
/// close event any close request uses.
#[test]
fn alt_f4_requests_close_on_the_focused_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);

    assert!(
        f.client(id).window(&surface).close_requested,
        "<Alt>F4 must request the focused window to close"
    );
}

/// The numbered workspace switches (`switch-to-workspace-N`) focus that
/// workspace. Only workspace 1 has an accelerator out of the box, `<Super>Home`
/// — `<Super>N` belongs to `switch-to-application-N`, so 2..12 start unbound
/// and only work once the user binds them.
#[test]
fn numbered_workspace_switches_follow_the_settings() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Mapping a window gives the monitor an occupied workspace plus the
    // trailing empty one, so there is a workspace 2 to go to.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_2);
    f.key_release(KEY_2);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0,
        "<Super>2 is unbound by default and must not switch workspaces"
    );

    let switch_2 = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action.gnome() == Some(GnomeKeyAction::SwitchToWorkspace(2)))
        .unwrap();
    switch_2.accels = vec![Accel {
        trigger: AccelTrigger::Keysym(Keysym::_2),
        mods: AccelMods::SUPER,
    }];

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_2);
    f.key_release(KEY_2);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "the bound <Super>2 must focus the second workspace"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "using Super as a modifier must not have fired the overlay key"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_1);
    f.key_release(KEY_1);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "<Super>1 belongs to switch-to-application-1, not workspace 1"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_HOME);
    f.key_release(KEY_HOME);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0,
        "<Super>Home must focus the first workspace again"
    );
}

/// The directional workspace switches: GNOME's horizontal row and the legacy
/// vertical axis both map onto niri's workspace column, so `<Control><Alt>Down`
/// (switch-to-workspace-down) goes to the next workspace and `<Control><Alt>Up`
/// back to the previous one.
#[test]
fn ctrl_alt_arrows_switch_workspaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);

    f.key_press(KEY_LEFTCTRL);
    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_DOWN);
    f.key_release(KEY_DOWN);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "<Control><Alt>Down must focus the next workspace"
    );

    f.key_press(KEY_UP);
    f.key_release(KEY_UP);
    f.key_release(KEY_LEFTALT);
    f.key_release(KEY_LEFTCTRL);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0,
        "<Control><Alt>Up must focus the previous workspace"
    );
}

/// `move-to-workspace-right` (default `<Super><Shift>Page_Down`) carries the
/// focused window along to the next workspace. Two windows are needed to
/// observe it: with just one, niri's dynamic workspaces garbage-collect the
/// emptied origin, putting the window back at index 0.
#[test]
fn super_shift_page_down_moves_window_to_next_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _first = map_focused_window(&mut f, id);
    let _second = map_focused_window(&mut f, id);

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTSHIFT);
    f.key_press(KEY_PAGEDOWN);
    f.key_release(KEY_PAGEDOWN);
    f.key_release(KEY_LEFTSHIFT);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    let monitor = f.synoik().layout.active_monitor_ref().unwrap();
    assert_eq!(
        monitor.active_workspace_idx(),
        1,
        "the move must follow the window to the next workspace"
    );
    assert_eq!(
        monitor.active_workspace_ref().windows().count(),
        1,
        "the moved window must be on the next workspace"
    );
}

/// The keybindings honor the settings model: rebinding `close` makes the old
/// accelerator inert and the new one live, with no compositor restart. This
/// pins the seam the live GSettings subscription feeds.
#[test]
fn keybindings_follow_the_settings_model() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    let close = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action.gnome() == Some(GnomeKeyAction::Close))
        .unwrap();
    close.accels = vec![Accel {
        trigger: AccelTrigger::Keysym(Keysym::w),
        mods: AccelMods::SUPER,
    }];

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        !f.client(id).window(&surface).close_requested,
        "the default accelerator must be inert after a rebind"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_W);
    f.key_release(KEY_W);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    assert!(
        f.client(id).window(&surface).close_requested,
        "the rebound accelerator must request the close"
    );
}

/// A keyboard-shortcuts-inhibitor on the focused window masks the GNOME
/// keybindings, like every mutter binding not flagged NON_MASKABLE; releasing
/// it restores them.
#[test]
fn shortcuts_inhibit_masks_gnome_keybindings() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.client(id).inhibit_shortcuts(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        !f.client(id).window(&surface).close_requested,
        "<Alt>F4 must reach the client, not close it, while shortcuts are inhibited"
    );

    f.client(id).release_shortcuts_inhibitor(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        f.client(id).window(&surface).close_requested,
        "releasing the inhibitor must restore the keybinding"
    );
}

/// The scroll bindings live in the settings model too, not only in the config
/// file: `<Super>` plus a wheel notch walks the workspaces, and rebinding it in
/// the model takes effect like any other key.
///
/// mutter's accelerators are keys only, so the trigger names are our extension —
/// but they run through the same table, the same modifier matching and the same
/// `handle_bind` as everything else.
#[test]
fn a_wheel_notch_resolves_through_the_settings_model() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A mapped window gives the monitor a second workspace to scroll to. The
    // pointer goes over it, away from the panel and the hot corner, so this
    // measures the binding and not some other scroll consumer.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.pointer_motion(960., 540.);
    let active = |f: &mut Fixture| {
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx()
    };
    assert_eq!(active(&mut f), 0);

    // Without the modifier it is an ordinary scroll, not a binding.
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        active(&mut f),
        0,
        "a bare wheel notch must not switch workspaces"
    );

    f.key_press(KEY_LEFTMETA);
    f.scroll_wheel();
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        active(&mut f),
        1,
        "<Super> + a wheel notch down must go to the next workspace"
    );
}

/// Our own schema's bindings resolve through the same path as GNOME's: they are
/// entries in one keybinding model, differing only in which action they carry.
///
/// `<Super>k`/`<Super>j` walk the windows in a column — the arrows can't, since
/// `<Super>` plus an arrow is GNOME's four times over (tiling, maximize,
/// unmaximize, move-to-monitor), which is exactly the sort of collision
/// `synoik_accels_do_not_collide_with_gnome` exists to keep out.
#[test]
fn our_own_keybindings_resolve_too() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let left = f.add_client();
    let _left_surface = map_focused_window(&mut f, left);
    let right = f.add_client();
    let right_surface = map_focused_window(&mut f, right);

    let focused = |f: &mut Fixture| f.synoik().layout.focus().map(|m| m.id());
    let newest = f.synoik().layout.focus().map(|m| m.id());

    // <Super><Alt>, not bare <Super>: bare <Super>h is GNOME's `minimize` and <Super>l is
    // gnome-settings-daemon's lock key, and ours would win both.
    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_K);
    f.key_release(KEY_LEFTALT);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_ne!(
        focused(&mut f),
        newest,
        "<Super><Alt>k must focus the window above"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_J);
    f.key_release(KEY_LEFTALT);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(focused(&mut f), newest, "<Super><Alt>j must come back down");

    // And rebinding one in the model takes effect, like any other key.
    let binding = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action == KeybindingAction::Synoik(Action::FocusWindowUp))
        .expect("our schema's keys are in the model");
    binding.accels = vec![Accel {
        trigger: AccelTrigger::Keysym(Keysym::z),
        mods: AccelMods::SUPER,
    }];

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_ne!(
        focused(&mut f),
        newest,
        "the rebound accelerator must focus up"
    );

    let _ = right_surface;
}

/// A gnome-settings-daemon grab outranks a key that only *we* have, at the keypress and
/// not merely at grab time.
///
/// gsd owns lock, logout and the media keys, and it cannot connect to D-Bus until after our
/// keybindings exist — so "first grabber wins", ported straight from mutter, would hand it
/// every contest against us. Keys GNOME itself names still outrank a grab, as in mutter;
/// only the scrolling-layout extras yield. `<Super>l` was `focus-column-right` until it
/// turned out to be gsd's `screensaver`, which is the shape of bug this pins.
#[test]
fn a_grab_outranks_our_own_keybinding() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let left = f.add_client();
    let _left_surface = map_focused_window(&mut f, left);
    let right = f.add_client();
    let _right_surface = map_focused_window(&mut f, right);

    // Point one of our own keys at a free chord, then let a "gsd" grab take the same one.
    let binding = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action == KeybindingAction::Synoik(Action::FocusWindowUp))
        .expect("our schema's keys are in the model");
    binding.accels = vec![Accel {
        trigger: AccelTrigger::Keysym(Keysym::z),
        mods: AccelMods::SUPER,
    }];

    let action = f
        .synoik_state()
        .grab_accelerator("<Super>z", 1, 0, ":1.10".to_owned());
    assert_ne!(action, 0, "ours must not block the grab");

    let newest = f.synoik().layout.focus().map(|m| m.id());
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert_eq!(
        f.synoik().layout.focus().map(|m| m.id()),
        newest,
        "the grab wins the keypress, so our focus-window-up must not have run"
    );

    // And with the grab gone, our key works again — the chord was yielded, not lost.
    assert!(f.synoik_state().ungrab_accelerator(action, ":1.10"));
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_ne!(
        f.synoik().layout.focus().map(|m| m.id()),
        newest,
        "after the ungrab our key resolves again"
    );
}

/// `<Super>N` is `switch-to-application-N`: it activates the Nth *dash favourite*
/// — not workspace N, which GNOME leaves unbound. A stopped app launches, and the
/// overview closes on the way, since `_switchToApplication` calls
/// `Main.overview.hide()` before `app.activate()`.
#[test]
fn super_number_activates_the_nth_favorite() {
    use crate::app_system::ResolvedLaunch;

    let (mut f, recorder) = dash_fixture(&["a.desktop", "b.desktop"]);

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_2);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one favorite launched");
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "<Super>2 must activate the second favorite"
    );
    assert_eq!(calls[0].1, ResolvedLaunch::Default);
    drop(calls);

    assert!(
        !f.synoik().layout.is_overview_open(),
        "_switchToApplication hides the overview before activating"
    );
}

/// The index is into the *resolved* favourites, the same list the dash draws
/// (`AppFavorites.getFavorites()`): a stored id whose app isn't installed drops
/// out of it, and `<Super>N` has to keep pointing at the Nth tile rather than the
/// Nth stored string.
#[test]
fn super_number_counts_the_favorites_the_dash_shows() {
    let (mut f, recorder) = dash_fixture(&["a.desktop", "b.desktop"]);

    // A stored favorite for an app that isn't installed: the dash skips it, so the
    // second *tile* is the third stored id.
    f.synoik().app_system.set_favorites(vec![
        "a.desktop".to_owned(),
        "ghost.desktop".to_owned(),
        "b.desktop".to_owned(),
    ]);
    f.synoik().sync_dash_favorites();

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_2);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one favorite launched");
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "<Super>2 must follow the dash, not the raw favorite-apps list"
    );
}

/// Every `<Super>N` activates the Nth favourite, not just the low digits.
///
/// Nine favourites, every digit, one assertion each. Written to chase a seat report of `<Super>8`
/// launching the *seventh* favourite, which turned out to be the instrument rather than the
/// compositor: `synoik msg input key Super+8` read a bare number as a decimal evdev keycode, and
/// keycode 8 is `KEY_7`. Bare digits now mean the digit key (`input::synthetic::resolve_key`; raw
/// keycodes moved behind `code:`), so the command means what it reads. The test stays because the
/// shape it rules out is real: a mapping that is right at 2 and wrong at 8 is what a per-digit slip
/// looks like, and the one spot-check we had could not have seen it.
#[test]
fn every_super_digit_activates_that_favorite() {
    const DIGIT_KEYS: [u32; 9] = [2, 3, 4, 5, 6, 7, 8, 9, 10];

    let favorites: Vec<String> = (1..=9).map(|n| format!("app{n}.desktop")).collect();
    let refs: Vec<&str> = favorites.iter().map(String::as_str).collect();
    let (mut f, recorder) = dash_fixture(&refs);

    for (i, key) in DIGIT_KEYS.iter().enumerate() {
        f.key_press(KEY_LEFTMETA);
        tap(&mut f, *key);
        f.key_release(KEY_LEFTMETA);
        f.synoik_complete_animations();

        let calls = recorder.calls.borrow();
        let n = i + 1;
        assert_eq!(calls.len(), n, "<Super>{n} must have activated something");
        assert_eq!(
            calls[i].0.id,
            format!("app{n}.desktop"),
            "<Super>{n} must activate favourite {n}"
        );
    }
}

/// `<Super><Ctrl>N` is `open-new-window-application-N`, which asks for another
/// window rather than raising the one the app has — and, unlike its plain
/// counterpart, leaves the overview up (`_openNewApplicationWindow` has no
/// `Main.overview.hide()`).
#[test]
fn super_ctrl_number_asks_for_a_new_window() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTCTRL);
    tap(&mut f, KEY_1);
    f.key_release(KEY_LEFTCTRL);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert_eq!(
        recorder.calls.borrow().len(),
        1,
        "<Super><Ctrl>1 must reach the launcher"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "opening a new window must not close the overview"
    );
}

/// `toggle-maximized` (`<Alt>F10`) is the one key that goes both ways:
/// `handle_toggle_maximized` unmaximizes a maximized window and maximizes
/// anything else, where `maximize`/`unmaximize` (`<Super>Up`/`<Super>Down`) are
/// one-directional.
#[test]
fn alt_f10_toggles_maximized() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let _ = f.client(id).window(&surface).recent_configures();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F10);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized"),
        "<Alt>F10 must maximize an unmaximized window, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F10);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("Maximized"),
        "pressing it again must unmaximize, got: {configures}"
    );
}

/// `switch-to-workspace-last` (`<Super>End`) goes to the last workspace on the
/// monitor — `get_workspace_by_index(n_workspaces - 1)` in
/// `_showWorkspaceSwitcher`, which under dynamic workspaces means the trailing
/// empty one counts.
#[test]
fn super_end_focuses_the_last_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A mapped window gives an occupied workspace plus the trailing empty one.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    let last = f
        .synoik()
        .layout
        .active_monitor_ref()
        .unwrap()
        .n_workspaces()
        - 1;
    assert!(last > 0, "there must be a workspace to go to");

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_END);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        last,
        "<Super>End must focus the last workspace"
    );
}

/// `move-to-monitor-{left,right,up,down}` (`<Super><Shift>` + arrows) sends the
/// focused window to the neighbouring monitor, and the focus follows it.
#[test]
fn super_shift_right_moves_the_window_to_the_next_monitor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    let first = f.synoik().layout.active_output().unwrap().clone();

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_RIGHT);
    f.key_release(KEY_LEFTSHIFT);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    let now = f.synoik().layout.active_output().unwrap().clone();
    assert_ne!(
        now, first,
        "<Super><Shift>Right must move the window to the monitor on the right, focus following"
    );
    // Focus landing on the other monitor is not enough — a *focus*-monitor action
    // would do that too, leaving the window behind on an now-unfocused output.
    assert!(
        f.synoik().layout.focus().is_some(),
        "the window must have come along, not just the focus"
    );
}

/// `switch-input-source` (`<Super>space`) steps through the configured keyboard
/// layouts, `-backward` steps the other way.
///
/// **DIVERGENCE:** gnome-shell shows an input-source switcher popup while the
/// modifier is held; we switch immediately. The popup belongs with the alt-tab
/// switchers it is built from.
#[test]
fn super_space_switches_the_input_source() {
    let mut config = Config::default();
    config.input.keyboard.xkb.layout = "us,de,fr".to_owned();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let layout_index = |f: &mut Fixture| {
        let keyboard = f.synoik_state().synoik.seat.get_keyboard().unwrap();
        keyboard.with_xkb_state(f.synoik_state(), |context| {
            context.xkb().lock().unwrap().active_layout().0
        })
    };
    assert_eq!(layout_index(&mut f), 0);

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_SPACE);
    f.key_release(KEY_LEFTMETA);
    assert_eq!(layout_index(&mut f), 1, "<Super>space must step forward");

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_SPACE);
    f.key_release(KEY_LEFTSHIFT);
    f.key_release(KEY_LEFTMETA);
    assert_eq!(
        layout_index(&mut f),
        0,
        "<Shift><Super>space must step back"
    );
}

/// `toggle-application-view` (`<Super>a`) is the show-apps button as a key:
/// from a closed overview it opens straight into the app grid, and with the
/// overview already up it flips between the window picker and the grid
/// (`_toggleAppsPage`, `overviewControls.js:660-667`).
#[test]
fn super_a_toggles_the_app_grid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_A);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "from closed, <Super>a must open the overview at the app grid"
    );

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_A);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_app_grid_open(),
        "pressing it again must fall back to the window picker"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "...without leaving the overview — that is what the show-apps button does"
    );
}

/// `toggle-message-tray` (`<Super>v`, and `<Super>m` as its second accelerator)
/// and `toggle-quick-settings` (`<Super>s`) open the panel menus, the same
/// `Panel.toggleCalendar` / `toggleQuickSettings` the pointer reaches by
/// clicking the clock and the indicators (`js/ui/panel.js:603-609`).
#[test]
fn super_v_and_super_s_open_the_panel_menus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_V);
    f.key_release(KEY_LEFTMETA);
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_DATE_MENU),
        "<Super>v must open the date menu"
    );

    // Toggling: the same key closes it again.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_V);
    f.key_release(KEY_LEFTMETA);
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        None,
        "<Super>v must toggle the date menu back closed"
    );

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_S);
    f.key_release(KEY_LEFTMETA);
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        "<Super>s must open quick settings"
    );
}

/// `restore-shortcuts` (`<Super>Escape`) is one of mutter's two NON_MASKABLE
/// bindings, so it resolves *through* an inhibitor — otherwise a client that
/// grabbed the keyboard could keep it — and hands the shortcuts back.
///
/// It restores and only restores: mutter's handler bails when the focus isn't
/// inhibiting (`meta_wayland_compositor_restore_shortcuts`), so pressing it
/// again must not toggle inhibition back on.
#[test]
fn restore_shortcuts_beats_the_inhibitor_and_never_arms_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.client(id).inhibit_shortcuts(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        !f.client(id).window(&surface).close_requested,
        "the inhibitor must mask <Alt>F4 to begin with"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        f.client(id).window(&surface).close_requested,
        "<Super>Escape must resolve despite the inhibitor and give the shortcuts back"
    );

    // A second press has nothing to restore. A toggle would re-arm the inhibitor
    // here, which is exactly what a recovery key must never do.
    f.client(id).window(&surface).close_requested = false;
    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert!(
        f.client(id).window(&surface).close_requested,
        "pressing it again must not put the inhibitor back on"
    );
}

/// `switch-to-session-N` is mutter's other NON_MASKABLE binding, so it changes
/// VT even while a client is inhibiting the shortcuts.
///
/// It is bound here rather than driven through its default `<Ctrl><Alt>F3`:
/// on this keymap that chord already arrives as `XF86Switch_VT_3` and is caught
/// by the hardcoded path in `find_bind` before any settings are consulted (see
/// `vt_switch_works_from_the_lock_screen`). This exercises the settings path
/// itself, which is what covers keymaps carrying no VT-switch mapping.
#[test]
fn switch_to_session_beats_the_inhibitor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    let session_3 = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action.gnome() == Some(GnomeKeyAction::SwitchToSession(3)))
        .expect("the wayland keybindings table must be loaded");
    session_3.accels = vec![Accel {
        trigger: AccelTrigger::Keysym(Keysym::F6),
        mods: AccelMods::SUPER,
    }];

    f.client(id).inhibit_shortcuts(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_F6);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik_state().backend.headless().last_vt(),
        Some(3),
        "an inhibitor must not be able to swallow the VT switch"
    );
}

/// GNOME keybindings take precedence over binds from the niri config file:
/// the GSettings store is the keybinding config of a GNOME session, so a
/// conflicting config bind must lose.
#[test]
fn gnome_keybindings_are_the_only_keybindings() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.client(id).window(&surface).close_requested,
        "<Alt>F4 is GNOME's `close` default and there is no other source of bindings"
    );
}

/// GNOME's `switch-windows` (default `<Alt>Tab`) cycles windows: holding Alt and tapping Tab
/// opens the switcher, releasing Alt commits, and focus lands on the previously-used window. A
/// second Alt+Tab returns to the first.
///
/// This is the GNOME `WindowSwitcherPopup` now, not niri's MRU switcher — one item per window,
/// with a live preview. `<Super>Tab` raises a *different* popup (`AppSwitcherPopup`, one item per
/// app), which is the divergence this test used to carry a note about.
#[test]
fn alt_tab_switches_to_previous_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _first = map_focused_window(&mut f, id);
    let _second = map_focused_window(&mut f, id);
    let second_focused = f.synoik().layout.focus().unwrap().id();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(
        f.synoik().switcher.is_open(),
        "Alt+Tab must open the window switcher"
    );
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    // Let the focus change go through a refresh cycle, like in a real event
    // loop iteration, so the MRU bookkeeping sees it.
    f.double_roundtrip(id);

    assert!(
        !f.synoik().switcher.is_open(),
        "releasing Alt must commit and close the switcher"
    );
    let now_focused = f.synoik().layout.focus().unwrap().id();
    assert_ne!(
        now_focused, second_focused,
        "Alt+Tab must move focus to the previous window"
    );

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        second_focused,
        "a second Alt+Tab must switch back"
    );
}

/// `panel-run-dialog` (default `<Alt>F2`) opens the run dialog, and it is
/// modal: keys typed into it never reach the focused client (gnome-shell
/// holds a modal grab while it's up).
#[test]
fn alt_f2_opens_run_dialog_and_swallows_keys() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).get_keyboard();
    f.roundtrip(id);
    let _ = f.client(id).take_key_events();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F2);
    f.key_release(KEY_LEFTALT);
    assert!(
        f.synoik().run_dialog.is_open(),
        "<Alt>F2 must open the run dialog"
    );

    tap(&mut f, KEY_Z);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "z",
        "typing must edit the entry"
    );

    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).take_key_events(),
        vec![
            (KEY_LEFTALT, WlKeyState::Pressed),
            (KEY_LEFTALT, WlKeyState::Released),
        ],
        "only the bare Alt of the opening chord may reach the client"
    );
}

/// Escape pressed *and* released on the dialog closes it (gnome-shell pairs
/// the press and release before closing).
#[test]
fn run_dialog_escape_closes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    assert!(f.synoik().run_dialog.is_open());

    tap(&mut f, KEY_ESC);
    assert!(
        !f.synoik().run_dialog.is_open(),
        "an Escape tap must close the run dialog"
    );
}

/// Ctrl+Enter runs, like plain Enter. gnome-shell reads `CONTROL_MASK` off the activate event
/// to decide whether to run *in a terminal* (`runDialog.js:113-114`, `_run(input, inTerminal)`
/// `:204,218`) — so it is a run either way, and letting the shared entry's plain-only Activate
/// arm swallow it made Ctrl+Enter silently do nothing.
#[test]
fn run_dialog_ctrl_enter_still_runs() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    tap(&mut f, KEY_Z);
    tap(&mut f, KEY_Z);
    f.key_press(KEY_LEFTCTRL);
    tap(&mut f, KEY_ENTER);
    f.key_release(KEY_LEFTCTRL);

    assert_eq!(
        f.synoik().run_dialog.error(),
        Some("Command not found"),
        "Ctrl+Enter must have attempted the run, not been eaten by the entry"
    );
}

/// An unknown command shows "Command not found" in-dialog and keeps the
/// dialog open with the entry intact — and still enters the history
/// (gnome-shell's `_run` records the attempt before trying it).
#[test]
fn run_dialog_unknown_command_shows_error_and_stays_open() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    tap(&mut f, KEY_Z);
    tap(&mut f, KEY_Z);
    tap(&mut f, KEY_ENTER);

    assert!(
        f.synoik().run_dialog.is_open(),
        "a failed command must keep the dialog open"
    );
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "zz",
        "the entry must be intact"
    );
    assert_eq!(
        f.synoik().run_dialog.error(),
        Some("Command not found"),
        "the error must show in-dialog"
    );
    assert_eq!(
        f.synoik().gnome_settings.command_history,
        vec!["zz".to_owned()],
        "even a failed command enters the history"
    );

    // Enter on an empty entry is also an error (the tokenizer rejects it),
    // not a close; and empty input never enters the history.
    f.synoik_state().do_action(Action::ShowRunDialog, false);
    tap(&mut f, KEY_ENTER);
    assert!(f.synoik().run_dialog.is_open());
    assert_eq!(
        f.synoik().gnome_settings.command_history,
        vec!["zz".to_owned()]
    );
}

/// A valid command spawns and closes the dialog, entering the history; Up
/// then recalls it (gnome-shell's HistoryManager).
#[test]
fn run_dialog_runs_command_and_records_history() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    for key in [KEY_T, KEY_R, KEY_U, KEY_E] {
        tap(&mut f, key);
    }
    tap(&mut f, KEY_ENTER);

    assert!(
        !f.synoik().run_dialog.is_open(),
        "a successful run must close the dialog"
    );
    assert_eq!(
        f.synoik().gnome_settings.command_history,
        vec!["true".to_owned()]
    );

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "",
        "the entry must open cleared"
    );
    tap(&mut f, KEY_UP);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "true",
        "Up must recall the last history entry"
    );
    tap(&mut f, KEY_DOWN);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "",
        "Down past the end must clear the entry again"
    );
}

/// `org.gnome.desktop.lockdown disable-command-line` disables the run dialog
/// entirely (gnome-shell's `RunDialog.open` refuses).
#[test]
fn run_dialog_lockdown_disables() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().gnome_settings.disable_command_line = true;

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F2);
    f.key_release(KEY_LEFTALT);
    assert!(
        !f.synoik().run_dialog.is_open(),
        "the lockdown key must disable the run dialog"
    );

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    assert!(
        !f.synoik().run_dialog.is_open(),
        "the lockdown applies to the action itself, not just the keybinding"
    );
}

/// `org.gnome.Shell` accelerator grabs (what gsd-media-keys uses for
/// volume/media keys): a grabbed combo never reaches the focused client, a
/// conflicting grab is refused with 0 (mutter's first-grabber-wins), only the
/// owner can ungrab, and after ungrabbing the combo flows to the client
/// again.
#[test]
fn accelerator_grabs_intercept_conflict_and_ungrab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).get_keyboard();
    f.roundtrip(id);
    let _ = f.client(id).take_key_events();

    let action = f
        .synoik_state()
        .grab_accelerator("<Super>z", 1, 0, ":1.10".to_owned());
    assert_ne!(action, 0, "a free combo must be grabbable");

    assert_eq!(
        f.synoik_state()
            .grab_accelerator("<Super>z", 1, 0, ":1.11".to_owned()),
        0,
        "a combo held by another grab must be refused"
    );
    assert_eq!(
        f.synoik_state()
            .grab_accelerator("<Alt>F4", 1, 0, ":1.11".to_owned()),
        0,
        "a combo held by a GNOME keybinding must be refused"
    );
    // But a key only *we* have does not refuse a grab. The grabber is a session component
    // — gnome-settings-daemon owns lock, logout and the media keys — and our keybindings
    // always exist before anything can connect to D-Bus, so first-come-first-served would
    // hand every contest to us. Inherited-from-niri capability yields instead.
    assert_ne!(
        f.synoik_state()
            .grab_accelerator("<Super><Alt>l", 1, 0, ":1.11".to_owned()),
        0,
        "a combo held only by our own schema must not block a grab"
    );
    assert_eq!(
        f.synoik_state()
            .grab_accelerator("no such key", 1, 0, ":1.11".to_owned()),
        0,
        "an unparseable accelerator must be refused"
    );

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    assert!(
        !f.client(id)
            .take_key_events()
            .iter()
            .any(|(key, _)| *key == KEY_Z),
        "a grabbed accelerator must not reach the client"
    );
    assert!(
        f.synoik().accel_grab_release_pending.is_empty(),
        "the release must have cleared the pending deactivation"
    );

    assert!(
        !f.synoik_state().ungrab_accelerator(action, ":1.11"),
        "only the owner may ungrab"
    );
    assert!(f.synoik_state().ungrab_accelerator(action, ":1.10"));

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_Z);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    assert!(
        f.client(id)
            .take_key_events()
            .iter()
            .any(|(key, state)| *key == KEY_Z && *state == WlKeyState::Pressed),
        "after ungrabbing, the combo must reach the client again"
    );
}

/// The overlay key is rebindable: pointing the setting at `Super_R` makes the
/// right Super the trigger, and the (now non-overlay) left Super inert.
#[test]
fn overlay_key_setting_rebinds() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().gnome_settings.overlay_keys = vec![Keysym::Super_R];

    // Left Super is no longer the overlay key.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "left Super must be inert once the overlay key is Super_R"
    );

    // Right Super now toggles the overview.
    f.key_press(KEY_RIGHTMETA);
    f.key_release(KEY_RIGHTMETA);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "a right Super tap must open the overview when it is the overlay key"
    );
}

/// Map a window of the given size, optionally as a transient child of
/// `parent`, and return its surface.
fn map_window_sized(
    f: &mut Fixture,
    id: ClientId,
    size: (u16, u16),
    parent: Option<&WlSurface>,
) -> WlSurface {
    let parent_toplevel = parent.map(|p| f.client(id).window(p).xdg_toplevel.clone());

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_parent(parent_toplevel.as_ref());
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(size.0, size.1);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    surface
}

/// The focused window's position within the workspace view.
fn focused_window_pos(f: &mut Fixture) -> (f64, f64) {
    let synoik = f.synoik();
    let focused = synoik.layout.focus().unwrap().id();
    let ws = synoik.layout.active_workspace().unwrap();
    let (_, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| tile.window().id() == focused)
        .unwrap();
    (pos.x, pos.y)
}

/// Name of the output the focused window ended up on.
///
/// Goes through the window id rather than the surface: the fixture's client
/// `WlSurface` is `wayland_client`'s type, and the layout speaks
/// `wayland_server`'s.
#[track_caller]
fn focused_window_output(f: &mut Fixture) -> String {
    let synoik = f.synoik();
    // `Mapped::id()` is the `MappedId`; the layout keys windows by the smithay
    // `Window`, which is `<Mapped as LayoutElement>::Id`.
    let window = synoik
        .layout
        .focus()
        .expect("no focused window")
        .window
        .clone();
    synoik
        .layout
        .monitors()
        .find(|mon| mon.has_window(&window))
        .expect("the focused window must be on some monitor")
        .output()
        .name()
}

#[track_caller]
fn assert_pos_eq(actual: (f64, f64), expected: (f64, f64), what: &str) {
    // Render positions round to physical pixels; allow that.
    assert!(
        (actual.0 - expected.0).abs() <= 1. && (actual.1 - expected.1).abs() <= 1.,
        "{what}: expected about ({}, {}), got ({}, {})",
        expected.0,
        expected.1,
        actual.0,
        actual.1,
    );
}

/// GNOME windowing: new windows open floating by default; niri's scrollable
/// tiling remains available behind `windowing-mode "scrolling"`.
#[test]
fn windows_open_floating_by_default() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window_sized(&mut f, id, (100, 100), None);

    let synoik = f.synoik();
    let focused = synoik.layout.focus().unwrap().window.clone();
    let ws = synoik.layout.active_workspace().unwrap();
    assert!(
        ws.is_floating(&focused),
        "a new window must open floating by default"
    );

    let mut f = Fixture::with_config(scrolling(Config::default()));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window_sized(&mut f, id, (100, 100), None);

    let synoik = f.synoik();
    let focused = synoik.layout.focus().unwrap().window.clone();
    let ws = synoik.layout.active_workspace().unwrap();
    assert!(
        !ws.is_floating(&focused),
        "windowing-mode scrolling must keep synoik's tiled-by-default behavior"
    );
}

/// With `center-new-windows` off, new windows follow mutter's origin
/// algorithm (`place.c`): the first window goes to the "centered tile" slot,
/// and subsequent same-size windows first-fit *below* existing ones before
/// going anywhere else.
#[test]
fn placement_first_fit_prefers_below() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_center_new_windows(false);
    let id = f.add_client();

    // place.c center_tile_rect_in_area: the leftover space of a hypothetical
    // grid of same-size windows, halved horizontally, third-ed vertically —
    // within the work area, which the top panel insets to (0, 32, 1920, 1048).
    let slot = ((1920. % 101.) / 2., 32. + (1048. % 101.) / 3.);

    let _w1 = map_window_sized(&mut f, id, (100, 100), None);
    let w1_pos = focused_window_pos(&mut f);
    assert_pos_eq(
        w1_pos,
        slot,
        "first window must take the centered-tile slot",
    );

    let _w2 = map_window_sized(&mut f, id, (100, 100), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (w1_pos.0, w1_pos.1 + 100.),
        "second window must first-fit below the first",
    );

    let _w3 = map_window_sized(&mut f, id, (100, 100), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (w1_pos.0, w1_pos.1 + 200.),
        "third window must continue the downward first-fit chain",
    );
}

/// first-fit's third phase: when the grid slot is taken and nothing fits
/// *below* an existing window, a candidate *beside* one is tried
/// (place.c:724-751).
#[test]
fn placement_first_fit_falls_back_to_beside() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_center_new_windows(false);
    let id = f.add_client();

    // Work area (0, 32) 1920 × 1048. A 900-tall window leaves no room below
    // itself, but does leave room to its right.
    let slot = ((1920. % 901.) / 2., 32. + (1048. % 901.) / 3.);
    let _w1 = map_window_sized(&mut f, id, (900, 900), None);
    assert_pos_eq(focused_window_pos(&mut f), slot, "the first window");
    assert!(
        slot.1 + 900. + 900. > 1080.,
        "precondition: there must be no room below the first window"
    );

    let _w2 = map_window_sized(&mut f, id, (900, 900), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (slot.0 + 900., slot.1),
        "the second window must first-fit beside the first",
    );
}

/// The cascade's column overflow: when a diagonal run reaches the bottom of
/// the work area, the next window restarts at the seed shifted right by one
/// `CASCADE_INTERVAL` (place.c:281-312).
#[test]
fn placement_cascade_starts_a_new_column_when_it_overflows() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_center_new_windows(false);
    let id = f.add_client();

    // 1000 × 600 in a 1920 × 1048 work area: after the first takes the grid
    // slot, nothing ever fits again, so every later window cascades. Seeded at
    // the center (460, 256), the run is (460, 256), (510, 306), … stepping 50px
    // until the next step would put the bottom edge past the work area.
    for _ in 0..2 {
        map_window_sized(&mut f, id, (1000, 600), None);
    }
    assert_pos_eq(
        focused_window_pos(&mut f),
        (460., 256.),
        "the cascade must start at the work-area center",
    );

    for _ in 0..4 {
        map_window_sized(&mut f, id, (1000, 600), None);
    }
    assert_pos_eq(
        focused_window_pos(&mut f),
        (660., 456.),
        "precondition: the sixth window ends the first cascade column",
    );

    // (710, 506) would put the bottom edge at 1106, past the work area's 1080,
    // so the column restarts at the center plus one interval horizontally, back
    // at the center vertically.
    map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (510., 256.),
        "an overflowing cascade must start a new column 50px to the right",
    );
}

/// Transient windows (dialogs) center horizontally on their parent and sit
/// at the top-biased third vertically, leaving twice as much parent below as
/// above (place.c).
#[test]
fn placement_dialogs_center_on_parent() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window_sized(&mut f, id, (600, 400), None);
    let parent_pos = focused_window_pos(&mut f);

    let _dialog = map_window_sized(&mut f, id, (200, 100), Some(&parent));
    assert_pos_eq(
        focused_window_pos(&mut f),
        (
            parent_pos.0 + (600. - 200.) / 2.,
            parent_pos.1 + (400. - 100.) / 3.,
        ),
        "a transient must center on its parent, biased to the top third",
    );
}

/// Which monitor a new window opens on is seeded from the pointer, not from
/// the active monitor: mutter seeds `window->monitor` from the pointer for a
/// window that gave no position hint (`window.c:1245-1259`), and placement
/// later picks the same one up (`place.c:951-955`). niri's scrolling mode
/// keeps the active monitor instead, which is the behaviour this replaces.
///
/// This is the only seed in `layout::placement` fed by the pointer, and only
/// at the initial configure — see
/// [`the_monitor_choice_survives_the_pointer_moving_away`].
#[test]
fn a_new_window_opens_on_the_monitor_under_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let id = f.add_client();
    f.roundtrip(id);

    // Outputs tile left to right, so output 2 spans x = 1920..3840.
    pointer_motion_to(&mut f, 1920. + 960., 540.);

    let _surface = map_window_sized(&mut f, id, (800, 600), None);

    assert_eq!(
        focused_window_output(&mut f),
        "headless-2",
        "a new window must open under the pointer"
    );
}

/// The monitor is decided **once**, at the initial configure, and a request
/// that arrives before the window maps must not re-decide it. Re-consulting
/// the pointer from a later request would let a window hop monitors merely
/// because the mouse moved between the client's first commit and its first
/// buffer.
///
/// This pins the invariant that `layout::placement` documents: only
/// `send_initial_configure` seeds `pointer_output`. The stored output, pinned
/// at the initial configure, outranks both the pointer and the active monitor
/// for every later request.
#[test]
fn the_monitor_choice_survives_the_pointer_moving_away() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let id = f.add_client();
    f.roundtrip(id);

    // Make output 2 the *active* monitor, by opening a window there. Every
    // seed below output 1 in the chain — the pointer, and the active monitor
    // fallback — now points at output 2, so the assertion at the end can only
    // hold if the output pinned at the initial configure is what decided it.
    pointer_motion_to(&mut f, 1920. + 960., 540.);
    let _elsewhere = map_window_sized(&mut f, id, (400, 300), None);
    assert_eq!(
        focused_window_output(&mut f),
        "headless-2",
        "precondition: output 2 must be the active monitor"
    );

    // Decide our window's monitor on output 1, by having the pointer there at
    // the initial configure.
    pointer_motion_to(&mut f, 960., 540.);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // The window is now configured but not yet mapped. Move the pointer back
    // to the other output and have the client change its mind.
    pointer_motion_to(&mut f, 1920. + 960., 540.);
    f.client(id).window(&surface).set_maximized();
    f.roundtrip(id);

    // Now map it.
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    assert_eq!(
        focused_window_output(&mut f),
        "headless-1",
        "the monitor is decided at the initial configure; a later request must \
         not follow the pointer to another output"
    );
}

/// A dialog follows its parent's monitor rather than the pointer's. The
/// parent seed sits ahead of the pointer seed in `layout::placement`, and
/// unlike the other seeds it deliberately does *not* get pinned onto the
/// window, so that mapping re-fetches the parent's monitor in case the parent
/// moved in between (`PlacementTarget::output_to_store`).
#[test]
fn a_dialog_opens_on_its_parents_monitor_not_the_pointers() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let id = f.add_client();
    f.roundtrip(id);

    // Parent opens on output 1.
    pointer_motion_to(&mut f, 960., 540.);
    let parent = map_window_sized(&mut f, id, (600, 400), None);

    // Pointer moves to output 2, then the dialog appears.
    pointer_motion_to(&mut f, 1920. + 960., 540.);
    let _dialog = map_window_sized(&mut f, id, (200, 100), Some(&parent));

    assert_eq!(
        focused_window_output(&mut f),
        "headless-1",
        "a dialog must follow its parent, not the pointer"
    );
}

/// With `center-new-windows` off and nothing fitting, placement cascades in
/// 50px diagonal steps (place.c find_next_cascade) — seeded from the *center*
/// of the work area, which is divergence B (`window-placement.md` §5): mutter
/// only centers the cascade when it also skips first-fit.
#[test]
fn placement_cascades_when_nothing_fits() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_center_new_windows(false);
    let id = f.add_client();

    // 1000×600 windows: after the first takes the centered-tile slot,
    // below/right candidates all overflow the 1920×1048 work area (the top
    // panel insets it), so first-fit fails and every subsequent window cascades.
    // The centered corner is (1920/2 - 500, 32 + 1048/2 - 300) = (460, 256).
    let _w1 = map_window_sized(&mut f, id, (1000, 600), None);

    // The first window sits at the grid slot (459.5, 47.667): within the 15px
    // fuzz horizontally, but nowhere near it vertically, so it does not step
    // the cascade — both axes have to match.
    let _w2 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (460., 256.),
        "the first cascaded window must sit at the work-area center",
    );

    let _w3 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (510., 306.),
        "the next cascade slot is one 50px diagonal step down",
    );

    let _w4 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (560., 356.),
        "each occupied slot steps the cascade another 50px",
    );
}

/// New windows without launch information take focus (mutter's
/// `intervening_user_event_occurred`: no timestamps at all means no
/// intervening event, i.e. smart mode allows the focus change).
#[test]
fn new_window_without_launch_time_takes_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (100, 100), None);
    let a_id = f.synoik().layout.focus().unwrap().id();

    // Interacting with the focused window must not block a token-less window:
    // with no launch time there is nothing to compare against.
    tap(&mut f, KEY_A);

    let _b = map_window_sized(&mut f, id, (100, 100), None);
    let b_id = f.synoik().layout.focus().unwrap().id();
    assert_ne!(
        a_id, b_id,
        "a new window with no launch information must take focus"
    );
}

/// A focus-denied window is also moved out from under the window that kept
/// the focus (mutter's place.c step H, :1052-1086): first-fit is re-run
/// against the focus window alone, and when nothing fits `find_most_freespace`
/// puts it on whichever side shows the most of it.
#[test]
fn denied_focus_window_moves_off_the_focus_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Work area (0, 32) 1920 × 1048; centered, A lands at (510, 256).
    let _a = map_window_sized(&mut f, id, (900, 600), None);
    let a_id = f.synoik().layout.focus().unwrap().id();
    let a_pos = focused_window_pos(&mut f);
    assert_pos_eq(a_pos, (510., 256.), "the focus window");

    // Start mapping B with a launch token minted now, then keep typing into A
    // so B's launch predates the interaction and focus is denied.
    let window = f.client(id).create_window();
    let b_surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);
    {
        let synoik = f.synoik();
        let unmapped = synoik.unmapped_windows.values_mut().next().unwrap();
        unmapped.activation_token_data = Some(XdgActivationTokenData {
            client_id: None,
            serial: None,
            app_id: None,
            surface: None,
            timestamp: Instant::now(),
            user_data: Arc::new(UserDataMap::new()),
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(2));
    tap(&mut f, KEY_A);

    let window = f.client(id).window(&b_surface);
    window.attach_new_buffer();
    window.set_size(900, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    let synoik = f.synoik();
    assert_eq!(
        synoik.layout.focus().unwrap().id(),
        a_id,
        "precondition: B must have been denied the focus"
    );

    let ws = synoik.layout.active_workspace().unwrap();
    let (_, b_pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| tile.window().id() != a_id)
        .unwrap();

    // Nothing fits beside A within the work area, so find_most_freespace picks
    // the left of the two equal-area sides and flushes B to the work-area edge,
    // keeping A's y.
    assert_pos_eq(
        (b_pos.x, b_pos.y),
        (0., 256.),
        "the denied window must move off the focus window",
    );
}

/// A window whose launch (activation-token mint) predates the last user
/// interaction with the focused window is denied focus, marked urgent, and
/// stacked below the focus window (mutter `meta_window_show`).
#[test]
fn stale_launch_denied_focus_and_marked_urgent() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (100, 100), None);
    let a_id = f.synoik().layout.focus().unwrap().id();

    // Start mapping B; its launch token is minted now.
    let window = f.client(id).create_window();
    let b_surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    {
        let synoik = f.synoik();
        assert_eq!(synoik.unmapped_windows.len(), 1);
        let unmapped = synoik.unmapped_windows.values_mut().next().unwrap();
        unmapped.activation_token_data = Some(XdgActivationTokenData {
            client_id: None,
            serial: None,
            app_id: None,
            surface: None,
            timestamp: Instant::now(),
            user_data: Arc::new(UserDataMap::new()),
        });
    }

    // The user keeps typing into A after the launch.
    std::thread::sleep(std::time::Duration::from_millis(2));
    tap(&mut f, KEY_A);

    // B maps: taking focus now would be a steal.
    let window = f.client(id).window(&b_surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    let synoik = f.synoik();
    assert_eq!(
        synoik.layout.focus().unwrap().id(),
        a_id,
        "focus must stay on the interacted-with window"
    );

    let ws = synoik.layout.active_workspace().unwrap();
    let b = ws.windows().find(|w| w.id() != a_id).unwrap();
    assert!(
        b.is_urgent(),
        "the denied window must be marked urgent (demands attention)"
    );

    let top = ws.tiles_with_render_positions().next().unwrap().0;
    assert_eq!(
        top.window().id(),
        a_id,
        "the denied window must stack below the focus window"
    );
}

/// `focus-new-windows "strict"`: no new window takes focus — except
/// transients of the focused window (mutter `window_state_on_map`).
#[test]
fn strict_focus_new_windows_denies_all_but_transients() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().gnome_settings.focus_new_windows = FocusNewWindows::Strict;
    let id = f.add_client();

    // The first window still becomes active: synoik ties workspace state to
    // focus with nothing else focused. (mutter would literally leave it
    // unfocused; accepted divergence.)
    let a = map_window_sized(&mut f, id, (600, 400), None);
    let a_id = f.synoik().layout.focus().unwrap().id();

    let _b = map_window_sized(&mut f, id, (100, 100), None);
    {
        let synoik = f.synoik();
        assert_eq!(
            synoik.layout.focus().unwrap().id(),
            a_id,
            "strict mode must deny focus to a new non-transient window"
        );
        let ws = synoik.layout.active_workspace().unwrap();
        let b = ws.windows().find(|w| w.id() != a_id).unwrap();
        assert!(b.is_urgent(), "the denied window must be marked urgent");
    }

    let _c = map_window_sized(&mut f, id, (200, 100), Some(&a));
    assert_ne!(
        f.synoik().layout.focus().unwrap().id(),
        a_id,
        "a transient of the focused window must take focus even in strict mode"
    );
}

/// `toggle-tiled-left` (default `<Super>Left`, org.gnome.mutter.keybindings):
/// tiles the window to the left half of the work area — half width, full
/// height, xdg tiled states but NOT maximized — and pressing it again
/// untiles, restoring the saved pre-tile geometry (mutter
/// handle_toggle_tiled / meta_window_untile).
#[test]
fn super_left_tiles_and_toggles() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let original_pos = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    // Tile left.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 960 × 1048, bounds: 1920 × 1048, states: [Activated, TiledTop, TiledBottom, TiledLeft]"
    );

    // The client commits the tiled size; the tile sits at the left edge.
    let window = f.client(id).window(&surface);
    window.set_size(960, 1048);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 32.),
        "a left-tiled window must sit at the work-area origin",
    );

    // Toggle again: untile, restoring the saved geometry.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 800 × 600, bounds: 1920 × 1048, states: [Activated]"
    );

    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "untiling must restore the saved position",
    );
}

/// `maximize` (default `<Super>Up`) maximizes; `unmaximize` (`<Super>Down`)
/// restores the floating geometry. A window maximized from a tile restores
/// to the pre-tile rect (mutter carries saved_rect from tile to maximize).
#[test]
fn super_up_maximizes_super_down_restores() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let original_pos = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    // Tile left first, then maximize from the tile.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(960, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized"),
        "maximize must send the xdg Maximized state, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    // Unmaximize: back to floating at the pre-tile size and position.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_DOWN);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600") && !configures.contains("Maximized"),
        "unmaximize must restore the pre-tile size, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "unmaximizing a window maximized from a tile must restore the pre-tile position",
    );
}

/// Maximizes the focused window and acks a work-area-sized configure for it.
fn maximize_focused(f: &mut Fixture, id: ClientId, surface: &WlSurface, size: (u16, u16)) {
    f.key_press(KEY_LEFTMETA);
    tap(f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
}

/// A GNOME workspace is a stack, not a strip: two maximized windows sit on top of each other at
/// the work area origin, and focusing between them does not slide the view sideways.
///
/// niri would give each maximized window its own column and pan horizontally between them. On this
/// desktop the horizontal axis belongs to workspaces — what lies to the side of a workspace is
/// another workspace — so nothing on a workspace is ever placed beside anything else.
#[test]
fn maximized_windows_stack_instead_of_sitting_side_by_side() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let first = map_window_sized(&mut f, id, (800, 600), None);
    let first_id = f.synoik().layout.focus().unwrap().id();
    let second = map_window_sized(&mut f, id, (800, 600), None);
    let second_id = f.synoik().layout.focus().unwrap().id();

    // Maximize the second, then the first (Alt+Tab back to it first).
    maximize_focused(&mut f, id, &second, (1920, 1080));
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTALT);
    f.double_roundtrip(id);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), first_id);
    maximize_focused(&mut f, id, &first, (1920, 1080));

    let window_pos = |f: &mut Fixture, wanted| {
        let ws = f.synoik().layout.active_workspace().unwrap();
        let (_, pos, _) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == wanted)
            .unwrap();
        (pos.x, pos.y)
    };

    let first_pos = window_pos(&mut f, first_id);
    let second_pos = window_pos(&mut f, second_id);
    assert_pos_eq(
        second_pos,
        first_pos,
        "two maximized windows must occupy the same rect, not sit side by side",
    );

    // Focusing across them is a raise, not a horizontal pan.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), second_id);

    assert_pos_eq(
        window_pos(&mut f, first_id),
        first_pos,
        "switching focus between maximized windows must not move them",
    );
    assert_pos_eq(
        window_pos(&mut f, second_id),
        second_pos,
        "switching focus between maximized windows must not move them",
    );
}

/// Fullscreening a maximized window and unfullscreening it comes back to maximized, not to the
/// pre-maximize rect (mutter's `saved_maximize`, meta_window_unmake_fullscreen).
#[test]
fn unfullscreen_returns_a_maximized_window_to_maximized() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    maximize_focused(&mut f, id, &surface, (1920, 1080));
    let _ = f.client(id).window(&surface).recent_configures();

    let window_id = f.synoik().layout.focus().unwrap().window.clone();
    f.synoik().layout.toggle_fullscreen(&window_id);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Fullscreen"),
        "fullscreen must send the xdg Fullscreen state, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    f.synoik().layout.toggle_fullscreen(&window_id);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized") && !configures.contains("Fullscreen"),
        "unfullscreening a window that was maximized must return it to maximized, \
         got: {configures}"
    );
}

/// A fullscreen window covers the panel: mutter puts fullscreen windows above the top layer
/// (`meta_window_is_fullscreen` feeds the layer computation in `meta_window_update_layer`).
/// Ours live in the floating layout, so that is where the check has to look.
#[test]
fn fullscreen_window_covers_the_top_layer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let window_id = f.synoik().layout.focus().unwrap().window.clone();
    assert!(
        !f.synoik()
            .layout
            .active_workspace()
            .unwrap()
            .render_above_top_layer(),
        "an ordinary window must not cover the panel"
    );

    f.synoik().layout.toggle_fullscreen(&window_id);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    assert!(
        f.synoik()
            .layout
            .active_workspace()
            .unwrap()
            .render_above_top_layer(),
        "a fullscreen window must render above the top layer"
    );
}

/// `org.gnome.mutter center-new-windows`, GNOME's default since mutter 48:
/// a new window opens in the middle of the work area. `find_first_fit` never
/// runs, so nothing tries to avoid overlap — a window landing within
/// `CASCADE_FUZZ` of the slot pushes it one titlebar height down-right
/// instead (place.c `find_next_cascade`, `place_centered = TRUE`).
#[test]
fn new_windows_center_by_default() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Work area is 1920 × 1048 at (0, 32) — the top panel struts the rest.
    // Centered slot for 900 × 600: (960 - 450, 32 + 524 - 300).
    map_window_sized(&mut f, id, (900, 600), None);
    assert_pos_eq(focused_window_pos(&mut f), (510., 256.), "the first window");

    // The second lands on the first, so it cascades by 50 px diagonally.
    map_window_sized(&mut f, id, (900, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (560., 306.),
        "the second window, cascaded off the first",
    );

    // A differently-sized window centers on its own slot: at 50 px away, the
    // nearest peer is outside the 15 px fuzz, so no cascade fires and the two
    // simply overlap.
    map_window_sized(&mut f, id, (1000, 700), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (460., 206.),
        "a differently-sized window centers without cascading",
    );
}

/// A dialog is not an obstacle: `rectangle_overlaps_some_window` skips dialog
/// types (place.c:503-548), so a window may first-fit into a slot a dialog
/// covers instead of falling through to the cascade. Dialogs still *offer*
/// candidate positions — place.c:698 and :724 walk the unfiltered list.
#[test]
fn dialogs_do_not_block_first_fit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_center_new_windows(false);
    let id = f.add_client();

    // Work area (0, 32) 1920 × 1048.
    let parent = map_window_sized(&mut f, id, (100, 100), None);
    let parent_pos = focused_window_pos(&mut f);

    // Centered on the parent, this 400 × 400 dialog clamps to the work-area
    // corner and covers (0, 32)–(400, 432).
    let _dialog = map_window_sized(&mut f, id, (400, 400), Some(&parent));
    assert_pos_eq(focused_window_pos(&mut f), (0., 32.), "the dialog");

    // The grid slot for a 900 × 600 window is (59, 181) — clear of the parent,
    // but well inside the dialog. It is taken anyway.
    let _w = map_window_sized(&mut f, id, (900, 600), None);
    let slot = ((1920. % 901.) / 2., 32. + (1048. % 601.) / 3.);
    assert!(
        slot.1 > parent_pos.1 + 100.,
        "precondition: the grid slot must be clear of the parent, got {slot:?} vs {parent_pos:?}"
    );
    assert_pos_eq(
        focused_window_pos(&mut f),
        slot,
        "a dialog must not block the grid slot",
    );
}

/// mutter places a first-shown window on the monitor holding the *pointer*
/// (`meta_backend_get_current_logical_monitor`, place.c:951-955), not on the
/// monitor the keyboard focus last landed on.
#[test]
fn new_windows_open_on_the_pointer_monitor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    // Whichever output sits on the right, asked rather than assumed.
    let mut outs = [f.synoik_output(1), f.synoik_output(2)];
    outs.sort_by_key(|o| f.synoik().global_space.output_geometry(o).unwrap().loc.x);
    let (left, right) = (outs[0].clone(), outs[1].clone());
    let right_geo = f.synoik().global_space.output_geometry(&right).unwrap();

    // Park the pointer on the right-hand output while the left one is active.
    // Without this precondition the assertion below could not tell the pointer
    // monitor from the active one.
    pointer_motion_to(
        &mut f,
        right_geo.loc.x as f64 + 100.,
        right_geo.loc.y as f64 + 100.,
    );
    assert_eq!(
        f.synoik().layout.active_output(),
        Some(&left),
        "precondition: moving the pointer must not have changed the active monitor"
    );

    let id = f.add_client();
    let _surface = map_window_sized(&mut f, id, (400, 300), None);

    let synoik = f.synoik();
    let win = synoik.layout.focus().unwrap().window.clone();
    let placed = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win))
        .and_then(|(mon, _, _)| mon)
        .map(|mon| mon.output().clone());
    assert_eq!(
        placed,
        Some(right),
        "a new window must open on the monitor under the pointer"
    );
}

/// `org.gnome.mutter auto-maximize` off leaves an oversized window alone
/// (place.c:1088 gates the whole branch on the pref).
#[test]
fn auto_maximize_can_be_disabled() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().layout.set_gnome_auto_maximize(false);
    let id = f.add_client();

    // The same 1800×1000 that auto-maximizes below.
    let surface = map_window_sized(&mut f, id, (1800, 1000), None);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("Maximized"),
        "with auto-maximize off an oversized window must stay floating, got: {configures}"
    );
}

/// mutter's auto-maximize: a window covering more than 80% of the work area
/// opens maximized; unmaximizing restores a size clamped to sqrt(0.8) of the
/// work area, aspect preserved (place.c / window.c).
#[test]
fn oversized_window_auto_maximizes_with_clamped_restore() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // 1800×1000 = 1.8M > 0.8 * 1920×1080 ≈ 1.66M.
    let surface = map_window_sized(&mut f, id, (1800, 1000), None);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized"),
        "an oversized window must auto-maximize on map, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_DOWN);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    // Clamped to the work area (the top panel insets it to 1920×1048):
    // scale = min(1920·√0.8/1800, 1048·√0.8/1000) ≈ 0.939 → 1691×939.
    let factor = 0.8f64.sqrt();
    let scale = f64::min(1920. * factor / 1800., 1048. * factor / 1000.);
    let expected = format!(
        "size: {} × {}",
        (1800. * scale).round() as i32,
        (1000. * scale).round() as i32
    );
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains(&expected),
        "the auto-maximize restore size must be clamped ({expected}), got: {configures}"
    );
}

/// GNOME stacking: mutter raises on click/activation, so the focused window
/// stays visually topmost after being maximized, map order notwithstanding.
/// Switching back to the other window puts that one on top again.
#[test]
fn active_maximized_window_covers_floating() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let first = map_window_sized(&mut f, id, (800, 600), None);
    let first_id = f.synoik().layout.focus().unwrap().id();
    let _second = map_window_sized(&mut f, id, (800, 600), None);
    let second_id = f.synoik().layout.focus().unwrap().id();
    f.double_roundtrip(id);

    let window_pos = |f: &mut Fixture, wanted| {
        let ws = f.synoik().layout.active_workspace().unwrap();
        let (_, pos, _) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == wanted)
            .unwrap();
        (pos.x, pos.y)
    };
    let window_under = |f: &mut Fixture, pos: (f64, f64)| {
        let ws = f.synoik().layout.active_workspace().unwrap();
        ws.window_under(pos.into()).map(|(w, _)| w.id())
    };

    // Click the first window: GNOME activates and raises it.
    let (x, y) = window_pos(&mut f, first_id);
    f.pointer_motion(x + 20., y + 20.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(id);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), first_id);

    // Maximize it and ack the full-size configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&first);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    // The maximized window is active: it must cover the floating window,
    // map order notwithstanding.
    let (sx, sy) = window_pos(&mut f, second_id);
    let over_second = (sx + 400., sy + 300.);
    assert_eq!(
        window_under(&mut f, over_second),
        Some(first_id),
        "the active maximized window must cover unfocused floating windows"
    );

    // Alt+Tab back to the floating window: floating is on top again.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), second_id);
    assert_eq!(
        window_under(&mut f, over_second),
        Some(second_id),
        "activating a floating window must put the floating layer back on top"
    );
}

/// Super+drag the focused window: grab it at `grab_offset` from its current
/// position, drag so the pointer lands on `drop_pos`, and drop it there.
fn super_drag_to(f: &mut Fixture, id: ClientId, grab_offset: (f64, f64), drop_pos: (f64, f64)) {
    let (x, y) = focused_window_pos(f);
    let grab = (x + grab_offset.0, y + grab_offset.1);
    f.pointer_motion(grab.0, grab.1);

    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // First a small motion so the grab recognizes a move (8px), then to the
    // target: the drop position is where the last motion left the pointer.
    f.pointer_motion(0., 10.);
    f.pointer_motion(drop_pos.0 - grab.0, drop_pos.1 - grab.1 - 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
}

/// Dragging a window into the 48px band at a side of the work area tiles it
/// to that half (mutter `meta-window-drag.c`, `update_move_maybe_tile`), and
/// untiling restores the pre-drag rect (`end_grab_op` passes the pre-drag
/// geometry as the saved rect).
#[test]
fn drag_to_left_edge_tiles() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let original_pos = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    super_drag_to(&mut f, id, (100., 100.), (20., 500.));

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("TiledLeft") && configures.contains("size: 960 × 1048"),
        "dropping in the left edge band must tile left, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(960, 1048);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 32.),
        "the tiled window must sit at the left work-area edge",
    );
    let _ = f.client(id).window(&surface).recent_configures();

    // Untile: back to the pre-drag rect, not the drop position.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_DOWN);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600"),
        "untiling must restore the pre-drag size, got: {configures}"
    );
    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "untiling must restore the pre-drag position",
    );
}

/// Dragging a window to the top edge of the screen maximizes it on drop
/// (mutter tiles it `META_TILE_MAXIMIZED`).
#[test]
fn drag_to_top_edge_maximizes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let original_pos = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    super_drag_to(&mut f, id, (100., 100.), (960., 0.));

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized"),
        "dropping on the top edge must maximize, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).window(&surface).recent_configures();

    // Unmaximize: back to the pre-drag rect.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_DOWN);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600") && !configures.contains("Maximized"),
        "unmaximizing must restore the pre-drag size, got: {configures}"
    );
    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "unmaximizing must restore the pre-drag position",
    );
}

/// `org.gnome.mutter edge-tiling` off disables drag-to-edge tiling: the drop
/// is an ordinary floating move.
#[test]
fn edge_tiling_can_be_disabled() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let _ = f.client(id).window(&surface).recent_configures();

    f.synoik().layout.set_gnome_edge_tiling(false);
    super_drag_to(&mut f, id, (100., 100.), (20., 500.));

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("TiledLeft"),
        "with edge-tiling off an edge drop must not tile, got: {configures}"
    );
    let synoik = f.synoik();
    let focused = synoik.layout.focus().unwrap().window.clone();
    let ws = synoik.layout.active_workspace().unwrap();
    assert!(
        ws.is_floating(&focused),
        "with edge-tiling off the window must stay floating"
    );
}

/// mutter's shake-loose: a maximized window stays maximized while the drag
/// moves less than shake_threshold (48px) vertically, then pops out with the
/// restore size and follows the pointer (meta-window-drag.c, update_move).
#[test]
fn dragging_maximized_window_shakes_loose_after_threshold() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let _ = f.client(id).window(&surface).recent_configures();

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    let _ = f.client(id).window(&surface).recent_configures();

    // Grab it and drag: sideways and a little down — still maximized.
    f.pointer_motion(960., 100.);
    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(100., 0.);
    f.pointer_motion(0., 30.);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("size: 800 × 600"),
        "a small drag must not shake the window loose, got: {configures}"
    );

    // Past 48px of vertical movement it pops out at the restore size.
    f.pointer_motion(0., 30.);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600") && !configures.contains("Maximized"),
        "crossing the shake threshold must unmaximize to the restore size, got: {configures}"
    );

    // Drop it in the middle of the screen: an ordinary floating move.
    f.pointer_motion(0., 400.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    let synoik = f.synoik();
    let focused = synoik.layout.focus().unwrap().window.clone();
    let ws = synoik.layout.active_workspace().unwrap();
    assert!(
        ws.is_floating(&focused),
        "the shaken-loose window must land floating"
    );
}

/// Without an input method, a text-input client is told nothing at all: no `enter`, so per
/// protocol it never sends `enable`, and every request it makes is discarded.
///
/// This is the state that breaks dead keys in GTK apps. GTK picks its IM backend off the mere
/// existence of `zwp_text_input_manager_v3` (`gtk/gtkimmodule.c`, `match_backend`), and
/// `GtkIMContextWayland` has no compose table of its own — so advertising the global with
/// nothing behind it replaces GTK's composition with nothing.
#[test]
fn text_input_hears_nothing_without_an_input_method() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_focused_window(&mut f, id);
    f.client(id).create_text_input();
    f.double_roundtrip(id);
    f.client(id).enable_text_input();
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).text_input_events(),
        Vec::new(),
        "a text input must stay silent while nothing is acting as the input method"
    );
}

/// With a compositor-internal input method registered, the same client is heard: it gets
/// `enter` on focus, and its committed state arrives as `TextInputEvent`s.
///
/// This is the smithay patch under test — stock smithay gates all of this on a Wayland
/// `zwp_input_method_v2` client existing, which is not how GNOME works (gnome-shell *is* the
/// input method, talking to IBus over D-Bus).
#[test]
fn an_internal_input_method_receives_client_text_input_state() {
    use std::sync::{Arc, Mutex};

    use smithay::wayland::text_input::{TextInputEvent as Ev, TextInputSeat};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let seen: Arc<Mutex<Vec<Ev>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    f.synoik()
        .seat
        .text_input()
        .set_internal_input_method(Some(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        })));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).create_text_input();
    f.double_roundtrip(id);

    // Focus reaches the text input now, which is what licenses the client to enable it.
    assert!(
        f.client(id).text_input_events().contains(&ClientEv::Enter),
        "an internal input method must get the client an `enter`"
    );

    f.client(id).enable_text_input();
    f.double_roundtrip(id);
    f.client(id).set_surrounding_text("héllo", 6, 6);
    f.double_roundtrip(id);

    let events = seen.lock().unwrap().clone();
    assert!(
        events.contains(&Ev::Enabled),
        "enable must reach the internal input method, got: {events:?}"
    );
    assert!(
        events.contains(&Ev::SurroundingText {
            // Byte offsets, not characters: "héllo" is 6 bytes, and a caret at the end is 6.
            text: "héllo".to_owned(),
            cursor: 6,
            anchor: 6,
        }),
        "surrounding text must reach the internal input method, got: {events:?}"
    );
    assert!(
        events.iter().filter(|e| **e == Ev::Done).count() >= 2,
        "each commit ends in a Done, got: {events:?}"
    );
}

/// The outbound half of the input method: what an engine produces reaches the client as
/// preedit and commit strings.
///
/// Driven by injecting [`ImEvent`]s rather than by a live ibus-daemon — the daemon round trip is
/// covered by `examples/ibus_probe.rs`, and what needs pinning here is the translation, above all
/// the character→byte cursor conversion that only misbehaves on the accented text this feature
/// exists for.
#[test]
fn engine_output_reaches_the_client_as_preedit_then_commit() {
    use std::sync::{Arc, Mutex};

    use smithay::wayland::text_input::{TextInputEvent as Ev, TextInputSeat};

    use crate::dbus::ibus::{ImEvent, PreeditMode};
    use crate::input_method::{ImUpdate, InputMethod};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (to_worker, requests) = async_channel::unbounded();
    f.synoik().input_method = Some(InputMethod::new(to_worker));

    let seen: Arc<Mutex<Vec<Ev>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    f.synoik()
        .seat
        .text_input()
        .set_internal_input_method(Some(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        })));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).create_text_input();
    f.double_roundtrip(id);
    f.client(id).enable_text_input();
    f.double_roundtrip(id);

    // Feed the client's state through, as the real sink would.
    for event in seen.lock().unwrap().drain(..).collect::<Vec<_>>() {
        f.synoik_state().on_text_input_event(event);
    }
    assert_eq!(
        requests.try_recv(),
        Ok(crate::input_method::ImRequest::FocusIn),
        "enabling text input must focus the engine in"
    );
    let _ = f.client(id).text_input_events();

    // The engine shows a composition. Cursor 2 is *characters* into "héllo" — byte 3.
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::Preedit {
            text: Some("héllo".to_owned()),
            cursor: 2,
            visible: true,
            mode: PreeditMode::Clear,
        }));
    f.double_roundtrip(id);

    let events = f.client(id).text_input_events();
    assert!(
        events.contains(&ClientEv::PreeditString {
            text: Some("héllo".to_owned()),
            cursor_begin: 3,
            cursor_end: 3,
        }),
        "preedit cursor must be a byte offset, got: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(ClientEv::Done(_))),
        "a preedit must be followed by done, got: {events:?}"
    );

    // Then it commits. The preedit must be cleared first or the composition stays on screen
    // beside the text it became (mutter sends preedit_string(NULL) before commit_string).
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::Commit("héllo".to_owned())));
    f.double_roundtrip(id);

    let events = f.client(id).text_input_events();
    let preedit_cleared = events.iter().position(|e| {
        *e == ClientEv::PreeditString {
            text: None,
            cursor_begin: 0,
            cursor_end: 0,
        }
    });
    let committed = events
        .iter()
        .position(|e| *e == ClientEv::CommitString(Some("héllo".to_owned())));
    assert!(
        preedit_cleared.is_some() && committed.is_some() && preedit_cleared < committed,
        "the preedit must be cleared before the commit, got: {events:?}"
    );
    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().preedit(),
        None,
        "committing clears the tracked preedit"
    );
}

/// What the client's committed text-input state lands in before it is pumped into the model.
type ImSink = std::sync::Arc<std::sync::Mutex<Vec<smithay::wayland::text_input::TextInputEvent>>>;

/// Answer every key the engine is still holding, as a real daemon would, and return everything
/// the model asked for along the way.
///
/// With an input method connected, a key bound for a text entry is *held* until a verdict comes
/// back — so a test that presses one and never answers sees nothing happen at all. The drained
/// requests are returned rather than discarded, because the focus and content-type traffic a
/// test wants to assert on arrives interleaved with the keys.
fn answer_im_keys(
    f: &mut Fixture,
    requests: &async_channel::Receiver<crate::input_method::ImRequest>,
    filtered: bool,
) -> Vec<crate::input_method::ImRequest> {
    use crate::input_method::{ImRequest, ImUpdate};

    let drained: Vec<ImRequest> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    for request in &drained {
        if let ImRequest::ProcessKey { id, .. } = request {
            f.synoik_state()
                .on_im_update(ImUpdate::KeyResult { id: *id, filtered });
        }
    }
    drained
}

/// Switch the fixture to `us+intl`, where the apostrophe key really is `dead_acute`.
///
/// The default `us` keymap makes `KEY_APOSTROPHE` an ordinary character, which quietly turns any
/// dead-key test into a plain-typing test — that is exactly how the bug this guards against got
/// through.
fn use_us_intl(f: &mut Fixture) {
    let sources = &mut f.synoik().gnome_settings.input_sources;
    sources.present = true;
    sources.sources = vec![("xkb".to_owned(), "us+intl".to_owned())];
    sources.mru_sources = sources.sources.clone();
    f.synoik_state().apply_effective_xkb();
}

/// Feed everything the client has committed into the model, as the production calloop source
/// does. Manual here because the fixture's sink is a `Vec`, not the real channel.
fn pump_im(f: &mut Fixture, seen: &ImSink) {
    for event in seen.lock().unwrap().drain(..).collect::<Vec<_>>() {
        f.synoik_state().on_text_input_event(event);
    }
}

/// Stand up a client with text input enabled and a connected input method behind it, and
/// return the channel the model's requests arrive on plus the sink [`pump_im`] drains.
fn im_fixture() -> (
    Fixture,
    ClientId,
    async_channel::Receiver<crate::input_method::ImRequest>,
    ImSink,
) {
    use std::sync::{Arc, Mutex};

    use smithay::wayland::text_input::{TextInputEvent as Ev, TextInputSeat};

    use crate::input_method::{ImUpdate, InputMethod};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (to_worker, requests) = async_channel::unbounded();
    f.synoik().input_method = Some(InputMethod::new(to_worker));

    let seen: Arc<Mutex<Vec<Ev>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    f.synoik()
        .seat
        .text_input()
        .set_internal_input_method(Some(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        })));

    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.client(id).get_keyboard();
    f.client(id).create_text_input();
    f.double_roundtrip(id);
    f.client(id).enable_text_input();
    f.double_roundtrip(id);

    pump_im(&mut f, &seen);
    // A daemon answered, so the key path is live. Until this the model must behave as if there
    // were no input method at all.
    f.synoik_state().on_im_update(ImUpdate::Connected(true));

    let _ = f.client(id).take_key_events();
    let _ = f.client(id).text_input_events();
    // Drain the focus-in the setup itself produced, so a test sees only its own traffic.
    while requests.try_recv().is_ok() {}
    (f, id, requests, seen)
}

/// A keystroke offered to the engine must not reach the client until the engine declines it.
///
/// This is the whole point of the round trip: if the key were delivered eagerly, a composed
/// character would arrive *after* the raw key that produced it — the user would get `'a` where
/// they typed `á`.
#[test]
fn a_key_waits_for_the_engine_before_reaching_the_client() {
    use crate::input_method::{ImRequest, ImUpdate};

    let (mut f, id, requests, _seen) = im_fixture();

    f.key_press(KEY_A);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![],
        "a key under consideration by the engine must not reach the client yet"
    );
    let request = requests.try_recv().expect("the key must reach the worker");
    let ImRequest::ProcessKey { id: key_id, .. } = request else {
        panic!("expected a ProcessKey request, got {request:?}");
    };

    // The engine declines it, so it is an ordinary keystroke after all.
    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: key_id,
        filtered: false,
    });
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).take_key_events(),
        vec![(KEY_A, WlKeyState::Pressed)],
        "a declined key must be delivered once the verdict arrives"
    );
}

/// A key the engine consumed must never reach the client — it will arrive as committed text
/// instead, and delivering both would double the input.
#[test]
fn a_key_the_engine_took_never_reaches_the_client() {
    use crate::input_method::{ImRequest, ImUpdate};

    let (mut f, id, requests, _seen) = im_fixture();

    f.key_press(KEY_A);
    f.double_roundtrip(id);
    let Ok(ImRequest::ProcessKey { id: key_id, .. }) = requests.try_recv() else {
        panic!("expected a ProcessKey request");
    };

    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: key_id,
        filtered: true,
    });
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![],
        "a key the engine consumed must not also be delivered as a key event"
    );
    assert!(
        !f.synoik().input_method.as_ref().unwrap().has_pending_keys(),
        "the answered key must leave the queue"
    );
}

/// Deferred keys are delivered in the order they were typed, never the order they were answered
/// in. A release that overtook its own press would leave the key stuck down in the client.
#[test]
fn deferred_keys_are_delivered_in_typing_order() {
    use crate::input_method::{ImRequest, ImUpdate};

    let (mut f, id, requests, _seen) = im_fixture();

    f.key_press(KEY_A);
    f.key_press(KEY_S);
    f.key_release(KEY_A);
    f.double_roundtrip(id);

    let mut ids = Vec::new();
    while let Ok(request) = requests.try_recv() {
        if let ImRequest::ProcessKey { id, .. } = request {
            ids.push(id);
        }
    }
    assert_eq!(ids.len(), 3, "every key must be offered to the engine");
    assert_eq!(
        f.client(id).take_key_events(),
        vec![],
        "nothing is delivered while the engine is still deciding"
    );

    // Answer the *last* one first. The two before it never got a verdict, so they are delivered
    // rather than dropped — and they must still come out in typing order.
    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: ids[2],
        filtered: false,
    });
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![
            (KEY_A, WlKeyState::Pressed),
            (KEY_S, WlKeyState::Pressed),
            (KEY_A, WlKeyState::Released),
        ],
        "keys must reach the client in the order they were typed"
    );
}

/// With no daemon answering, the key path must behave exactly as it does with no input method:
/// keys go straight to the client. Anything else means an unreachable `ibus-daemon` costs the
/// user their keyboard.
#[test]
fn keys_go_straight_through_while_the_engine_is_disconnected() {
    use crate::input_method::ImUpdate;

    let (mut f, id, requests, _seen) = im_fixture();
    f.synoik_state().on_im_update(ImUpdate::Connected(false));

    f.key_press(KEY_A);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![(KEY_A, WlKeyState::Pressed)],
        "a disconnected input method must not hold keys back"
    );
    assert!(
        requests.try_recv().is_err(),
        "a disconnected input method must not be sent keys"
    );
}

/// Disabling text input while keys are in flight delivers them rather than dropping them: the
/// client just said it wants ordinary key events, and those keystrokes are its.
#[test]
fn disabling_text_input_delivers_the_keys_still_in_flight() {
    use smithay::wayland::text_input::TextInputEvent as Ev;

    let (mut f, id, _requests, _seen) = im_fixture();

    f.key_press(KEY_A);
    f.double_roundtrip(id);
    assert!(
        f.synoik().input_method.as_ref().unwrap().has_pending_keys(),
        "the key should be held back to begin with"
    );

    f.synoik_state().on_text_input_event(Ev::Disabled);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![(KEY_A, WlKeyState::Pressed)],
        "keys in flight must survive the input being disabled"
    );
}

/// A client's password field must reach the engine *as* a password, so it can stop composing and
/// stop offering candidates over a secret. This is the one content-type mapping whose failure is
/// a privacy bug rather than a papercut.
#[test]
fn a_password_field_is_announced_to_the_engine() {
    use smithay::reexports::wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3 as c;

    use crate::dbus::ibus::{hints, purpose};
    use crate::input_method::ImRequest;

    let (mut f, id, requests, seen) = im_fixture();

    f.client(id)
        .set_content_type(c::ContentHint::SensitiveData, c::ContentPurpose::Password);
    f.double_roundtrip(id);
    pump_im(&mut f, &seen);

    let content: Vec<ImRequest> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter(|r| matches!(r, ImRequest::ContentType { .. }))
        .collect();
    assert_eq!(
        content,
        vec![ImRequest::ContentType {
            purpose: purpose::PASSWORD,
            hints: hints::PRIVATE,
        }],
        "a password field must be announced as one"
    );

    // ...and when the field goes away, the engine must not still believe it is in one.
    f.synoik_state()
        .on_text_input_event(smithay::wayland::text_input::TextInputEvent::Disabled);
    let after: Vec<ImRequest> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter(|r| matches!(r, ImRequest::ContentType { .. }))
        .collect();
    assert_eq!(
        after,
        vec![ImRequest::ContentType {
            purpose: purpose::FREE_FORM,
            hints: 0,
        }],
        "disabling the input must reset the content type"
    );
}

/// Typing into the compositor's *own* entries composes too. The overview search is the
/// everyday one: `'` then `a` must put `á` in the query, not `'a`.
#[test]
fn the_overview_search_composes_through_the_engine() {
    use crate::dbus::ibus::ImEvent;
    use crate::input_method::{ImFocus, ImRequest, ImUpdate, ShellEntry};

    let (mut f, id, requests, _seen) = im_fixture();
    use_us_intl(&mut f);
    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    while requests.try_recv().is_ok() {}

    // A dead key on an *idle* search entry: nothing is active or expanded yet, and it carries no
    // text, so the entry has to claim it on the strength of being a composition key alone.
    //
    // This used to work only by accident — the key fell past the search block entirely and was
    // offered down the generic client-forward path, which offers everything. The lock screen,
    // which intercepts unconditionally, had no such accident and stayed broken.
    f.key_press(KEY_APOSTROPHE);
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Shell(ShellEntry::OverviewSearch),
        "the overview search must hold the engine while it has focus"
    );
    let offered: Vec<ImRequest> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter(|r| matches!(r, ImRequest::ProcessKey { .. }))
        .collect();
    assert_eq!(offered.len(), 1, "the key must be offered to the engine");
    assert_eq!(
        f.synoik().overview_search.query(),
        "",
        "nothing may reach the entry until the engine has ruled"
    );

    // The engine takes it and, once the `a` follows, commits the composed character.
    let ImRequest::ProcessKey { id: key_id, .. } = offered[0].clone() else {
        unreachable!()
    };
    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: key_id,
        filtered: true,
    });
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::Commit("á".to_owned())));
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik().overview_search.query(),
        "á",
        "composed text must land in the search entry"
    );
}

/// The lock screen's password field is announced to the engine as a password — the same thing
/// GNOME's `StPasswordEntry` does (`st-password-entry.c:241`) — and takes composed text.
#[test]
fn the_lock_screen_password_entry_is_announced_as_a_password() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::dbus::ibus::{hints, purpose, ImEvent};
    use crate::input_method::{ImFocus, ImRequest, ImUpdate, ShellEntry};
    use crate::unlock_dialog::Page;

    let (mut f, id, requests, _seen) = im_fixture();

    // A real lock with a live verifier, not a screensaver — a dismissible shield treats any key
    // as "wake up" rather than as text.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    // The first key raises the prompt; from here the entry is live. It goes to the engine first
    // like any other, so the verdict has to come back before anything happens — which is itself
    // the deferral working on the lock screen.
    f.key_press(KEY_A);
    f.key_release(KEY_A);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        Page::Clock,
        "the key must be held until the engine rules on it"
    );
    let sent = answer_im_keys(&mut f, &requests, false);
    assert_eq!(
        sent.iter()
            .filter(|r| matches!(r, ImRequest::ProcessKey { .. }))
            .count(),
        1,
        "exactly the press is offered; releases keep their original path"
    );
    f.synoik_state().update_keyboard_focus();
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Prompt);

    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Shell(ShellEntry::Shield),
        "the shield must take the engine from the client underneath it"
    );
    assert!(
        sent.contains(&ImRequest::ContentType {
            purpose: purpose::PASSWORD,
            hints: hints::PRIVATE | hints::HIDDEN_TEXT,
        }),
        "the shield entry must be announced as a password, got: {sent:?}"
    );

    // ...and composed text still reaches it, so an accented password can be typed. Asserted
    // through the masked display, because the entry's own text is a secret the test has no
    // business reading — one bullet per character is the observable that matters.
    let before = f.synoik().unlock_dialog.entry_display().chars().count();
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::Commit("á".to_owned())));
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().unlock_dialog.entry_display().chars().count(),
        before + 1,
        "composed text must reach the password entry"
    );
}

/// The whole dead-key sequence on the lock screen, engine round trip and all: the dead key is
/// offered and consumed, then the letter is offered and comes back as composed text.
#[test]
fn a_dead_key_composes_on_the_lock_screen() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::dbus::ibus::ImEvent;
    use crate::input_method::{ImFocus, ImRequest, ImUpdate, ShellEntry};

    let (mut f, id, requests, _seen) = im_fixture();
    use_us_intl(&mut f);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state().update_keyboard_focus();
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Shell(ShellEntry::Shield),
    );
    while requests.try_recv().is_ok() {}

    // The dead key. `dead_acute.key_char()` is `None`, so it carries no text at all — gating on
    // "carries text" drops exactly this key and composition never starts.
    f.key_press(KEY_APOSTROPHE);
    f.key_release(KEY_APOSTROPHE);
    let offered: Vec<u64> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter_map(|r| match r {
            ImRequest::ProcessKey { id, keysym, .. } => {
                assert_eq!(keysym, Keysym::dead_acute.raw(), "really a dead key");
                Some(id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        offered.len(),
        1,
        "the dead key must reach the engine, or there is nothing to compose with"
    );

    // The engine keeps it.
    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: offered[0],
        filtered: true,
    });
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().unlock_dialog.entry_display(),
        "",
        "a consumed dead key must put nothing in the entry"
    );

    // Then the letter, which the engine also keeps, committing the composed character.
    f.key_press(KEY_A);
    f.key_release(KEY_A);
    let offered: Vec<u64> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter_map(|r| match r {
            ImRequest::ProcessKey { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(offered.len(), 1, "the letter must reach the engine too");
    f.synoik_state().on_im_update(ImUpdate::KeyResult {
        id: offered[0],
        filtered: true,
    });
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::Commit("á".to_owned())));
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik().unlock_dialog.entry_display(),
        "\u{25cf}",
        "the composed character must land in the password entry, masked"
    );
}

/// A modal entry of ours takes the engine from a client that still has text input enabled.
/// Otherwise the engine would be composing against the client's surrounding text while the
/// keystrokes went into a dialog — at worst, a password one.
#[test]
fn a_modal_entry_takes_the_engine_from_the_client_underneath() {
    use crate::input_method::{ImFocus, ImRequest, ShellEntry};

    let (mut f, id, requests, _seen) = im_fixture();
    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Client,
        "the client starts with the engine"
    );
    while requests.try_recv().is_ok() {}

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Shell(ShellEntry::RunDialog),
        "the dialog must take the engine"
    );
    let sent: Vec<ImRequest> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    let out = sent.iter().position(|r| *r == ImRequest::FocusOut);
    let back_in = sent.iter().position(|r| *r == ImRequest::FocusIn);
    assert!(
        out.is_some() && back_in.is_some() && out < back_in,
        "the client must be focused out before the dialog is focused in, got: {sent:?}"
    );

    // Closing it gives the engine back to the client.
    f.synoik().run_dialog.close();
    f.synoik_state().update_keyboard_focus();
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().input_method.as_ref().unwrap().focus(),
        ImFocus::Client,
        "the client must get the engine back"
    );
}

/// A key the engine hands back reaches the client as an ordinary key event, and does not loop
/// back into the engine that produced it.
#[test]
fn a_key_forwarded_by_the_engine_reaches_the_client_once() {
    use crate::dbus::ibus::ImEvent;
    use crate::input_method::ImUpdate;

    let (mut f, id, requests, _seen) = im_fixture();

    // IBus counts in evdev codes, which is what the client sees too.
    f.synoik_state()
        .on_im_update(ImUpdate::Event(ImEvent::ForwardKey {
            keyval: 0x61,
            keycode: KEY_A,
            state: 0,
            press: true,
        }));
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).take_key_events(),
        vec![(KEY_A, WlKeyState::Pressed)],
        "a forwarded key must reach the client"
    );
    assert!(
        requests.try_recv().is_err(),
        "a forwarded key must not be offered back to the engine that sent it"
    );
}

/// A compositor keybinding still resolves while a text input is focused, and the key it claimed
/// is never offered to the engine — the deliberate ordering divergence from mutter, which
/// consults the input method first (`events.c:168` vs `:262`).
///
/// The overlay key shows both halves at once: its arming *press* is propagated (so it is offered,
/// exactly as mutter would), while the *release* that fires the binding is swallowed by the
/// compositor and must reach neither the client nor the engine.
#[test]
fn a_binding_claims_its_key_before_the_engine_sees_it() {
    use crate::input_method::ImRequest;

    let (mut f, id, requests, _seen) = im_fixture();

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.synoik().layout.is_overview_open(),
        "the overlay key must still fire while a text input has focus"
    );

    let offered: Vec<u32> = std::iter::from_fn(|| requests.try_recv().ok())
        .filter_map(|request| match request {
            ImRequest::ProcessKey { keycode, .. } => Some(keycode),
            _ => None,
        })
        .collect();
    assert_eq!(
        offered,
        vec![KEY_LEFTMETA],
        "only the propagated press may reach the engine; the firing release was claimed"
    );
}

/// mutter's three passive window button grabs are Mod+LMB to move, Mod+MMB to
/// resize and Mod+RMB for the window menu (`window.c:7743-7844`;
/// `meta_prefs_get_mouse_button_resize` returns 2 unless `resize-with-right-button`).
/// The resize edges come from which third of the frame the press lands in
/// (`window.c:7795-7807`).
#[test]
fn super_middle_drag_resizes_from_the_pressed_corner() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let (x, y) = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    // Press in the bottom-right third of the frame: south|east edges.
    pointer_motion_to(&mut f, x + 700., y + 550.);
    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_MIDDLE, ButtonState::Pressed);
    f.pointer_motion(100., 100.);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 900 × 700"),
        "Mod+MMB in the bottom-right third must resize from that corner, got: {configures}"
    );

    f.pointer_button(BTN_MIDDLE, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
}

/// The centre third of the frame has no edges, so mutter starts no grab there
/// (`op != META_GRAB_OP_WINDOW_BASE`, `window.c:7809`).
#[test]
fn super_middle_drag_in_the_frame_centre_does_not_resize() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let (x, y) = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    pointer_motion_to(&mut f, x + 400., y + 300.);
    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_MIDDLE, ButtonState::Pressed);
    f.pointer_motion(100., 100.);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("size: 900 × 700"),
        "the centre third has no resize edges, got: {configures}"
    );

    f.pointer_button(BTN_MIDDLE, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
}

/// Mod+RMB is mutter's window-menu button, not a resize (niri's mapping). The
/// menu itself is not ported yet, so the press must simply not resize.
#[test]
fn super_right_drag_does_not_resize() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let (x, y) = focused_window_pos(&mut f);
    let _ = f.client(id).window(&surface).recent_configures();

    pointer_motion_to(&mut f, x + 700., y + 550.);
    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_motion(100., 100.);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("size: 900 × 700"),
        "Mod+RMB must not resize, got: {configures}"
    );

    f.pointer_button(BTN_RIGHT, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
}

/// The overview is GNOME's window picker (Experiment 1): windows spread into
/// slots computed by gnome-shell's layout strategy (`src/layout/expose.rs`),
/// hit-testing follows the slots, and clicking a preview activates that
/// window and leaves the overview.
#[test]
fn overview_spreads_windows_into_picker_slots() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _first = map_window_sized(&mut f, id, (800, 600), None);
    let first_id = f.synoik().layout.focus().unwrap().id();
    let first_win = f.synoik().layout.focus().unwrap().window.clone();
    let _second = map_window_sized(&mut f, id, (800, 600), None);
    let second_win = f.synoik().layout.focus().unwrap().window.clone();

    // A lone Super tap opens the picker.
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(f.synoik().layout.is_overview_open());

    // Every window has a picker slot, and slots don't overlap.
    let first_rect = f
        .synoik()
        .layout
        .expose_target_rect(&first_win)
        .expect("windows must have picker slots in the overview");
    let second_rect = f.synoik().layout.expose_target_rect(&second_win).unwrap();
    let disjoint = first_rect.loc.x + first_rect.size.w <= second_rect.loc.x
        || second_rect.loc.x + second_rect.size.w <= first_rect.loc.x
        || first_rect.loc.y + first_rect.size.h <= second_rect.loc.y
        || second_rect.loc.y + second_rect.size.h <= first_rect.loc.y;
    assert!(
        disjoint,
        "picker slots must not overlap: {first_rect:?} {second_rect:?}"
    );

    // Click the unfocused window's preview: it activates and the overview
    // closes (gnome-shell's Main.activateWindow on 'selected').
    f.pointer_motion(
        first_rect.loc.x + first_rect.size.w / 2.,
        first_rect.loc.y + first_rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        !f.synoik().layout.is_overview_open(),
        "clicking a preview must leave the overview"
    );
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        first_id,
        "clicking a preview must activate that window"
    );
}

/// Every workspace's render rect on output 1 — the geometry the overview's row
/// tests measure, settled.
fn workspace_geo(f: &mut Fixture) -> Vec<smithay::utils::Rectangle<f64, smithay::utils::Logical>> {
    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    mon.expect("workspaces must be on a monitor")
        .workspaces_render_geo()
        .collect()
}

/// Two workspace-row snapshots agree to within a physical pixel (the row's
/// coordinates are rounded to the pixel grid, so exactness is not the contract).
#[track_caller]
fn assert_geo_eq(
    a: &[smithay::utils::Rectangle<f64, smithay::utils::Logical>],
    b: &[smithay::utils::Rectangle<f64, smithay::utils::Logical>],
    what: &str,
) {
    assert_eq!(a.len(), b.len(), "{what}: workspace count changed");
    for (i, (a, b)) in a.iter().zip(b).enumerate() {
        assert!(
            (a.loc.x - b.loc.x).abs() <= 1.
                && (a.loc.y - b.loc.y).abs() <= 1.
                && (a.size.w - b.size.w).abs() <= 1.
                && (a.size.h - b.size.h).abs() <= 1.,
            "{what}: workspace {i} differs: {a:?} vs {b:?}"
        );
    }
}

/// Every workspace moves toward its final position and never away from it, and
/// no single frame carries an implausible share of the travel.
///
/// Monotonicity is the load-bearing half, and it is structural rather than tuned:
/// the row is a lerp between two *fixed* layouts on a monotone ease, so position
/// is monotone by construction — give or take the pixel rounding the row is
/// snapped to, hence the 1px slack. The per-step ceiling is only a smoke check for
/// gross discontinuity (a third of the whole trip in one frame); the *small* snap
/// that ends a mis-terminated animation is caught by comparing the last sample
/// with the settled state, not by this.
#[track_caller]
fn assert_row_travels_monotonically(
    samples: &[Vec<smithay::utils::Rectangle<f64, smithay::utils::Logical>>],
    what: &str,
) {
    let n = samples.len();
    assert!(n >= 4, "{what}: too few samples");
    let count = samples[0].len();

    for i in 0..count {
        let xs: Vec<f64> = samples.iter().map(|s| s[i].loc.x).collect();
        let travel = xs[n - 1] - xs[0];
        let sign = if travel >= 0. { 1. } else { -1. };

        for w in xs.windows(2) {
            let step = (w[1] - w[0]) * sign;
            assert!(
                step >= -1.,
                "{what}: workspace {i} moved backwards ({:.1} -> {:.1}) in {xs:?}",
                w[0],
                w[1]
            );
            assert!(
                step <= f64::max(travel.abs() / 3., 8.),
                "{what}: workspace {i} jumped {step:.1}px in one frame, out of \
                 {:.1}px of travel over {n} samples: {xs:?}",
                travel.abs()
            );
        }
    }
}

/// The workspace row must travel *monotonically* between the window picker and
/// the app grid. gnome-shell interpolates between two frozen endpoint layouts —
/// `_getInitialBoxes` (`workspacesView.js:281-324`) takes the workspaces box of
/// the initial state and of the final state, not the live one — so every
/// coordinate is affine in the eased parameter and the row cannot overshoot.
/// Evaluating both ends at the current (moving) zoom instead made the row swing
/// ~85px past its landing spot and come back, which is only visible mid-flight:
/// both settled ends were correct the whole time.
#[test]
fn overview_grid_transition_moves_the_row_monotonically() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    let picker = workspace_geo(&mut f);

    // Into the app grid.
    f.synoik().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(400), 32);
    assert_geo_eq(
        &samples[0],
        &picker,
        "the transition starts where the picker was",
    );
    assert_row_travels_monotonically(&samples, "picker -> app grid");
    f.settle_animations();
    let grid = workspace_geo(&mut f);
    assert_geo_eq(
        samples.last().unwrap(),
        &grid,
        "the transition ends where the settled app grid is",
    );

    // And back out of it.
    f.synoik().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(400), 32);
    assert_geo_eq(&samples[0], &grid, "the way back starts where the grid was");
    assert_row_travels_monotonically(&samples, "app grid -> picker");
    f.settle_animations();
    assert_geo_eq(
        samples.last().unwrap(),
        &workspace_geo(&mut f),
        "the way back ends where the settled picker is",
    );
}

/// Closing the overview *from the app grid* has to land on the desktop, not
/// beside it. gnome-shell's one adjustment travels 2 -> 0 through WINDOW_PICKER,
/// so the fit mode is back to SINGLE by the time it is hidden
/// (`overviewControls.js:278-308,593-606`); we freeze the show-apps scalar across
/// a close, so it is gated on the overview progress to reach zero with it.
/// Without that the row ended the animation offset by the fit-all spacing and
/// snapped into place afterwards — a jump with no animation at all behind it.
#[test]
fn overview_close_from_the_app_grid_lands_on_the_desktop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    let desktop = workspace_geo(&mut f);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();

    f.synoik_state().do_action(Action::CloseOverview, false);
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 48);
    assert_row_travels_monotonically(&samples, "app grid -> desktop");
    assert_geo_eq(
        samples.last().unwrap(),
        &desktop,
        "the close must end on the desktop layout, with nothing left to snap",
    );

    // ...and the snap that used to follow is gone: settling changes nothing.
    let last = samples.last().unwrap().clone();
    f.settle_animations();
    assert_geo_eq(
        &workspace_geo(&mut f),
        &last,
        "settling after the close must not move the row",
    );
}

/// Hovering a window preview grows it: gnome-shell's `showOverlay` eases the
/// preview's container up by `WINDOW_ACTIVE_SIZE_INC` (5px) in each direction
/// about its center (`windowPreview.js:340-352`), so the slot's center stays put
/// while the preview overlaps its neighbours a little. Leaving eases it back.
#[test]
fn overview_hovering_a_preview_grows_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _first = map_window_sized(&mut f, id, (800, 600), None);
    let first_win = f.synoik().layout.focus().unwrap().window.clone();
    let _second = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let rest = f
        .synoik()
        .layout
        .expose_drawn_rect(&first_win)
        .expect("a preview draws in the overview");
    assert_eq!(
        rest,
        f.synoik().layout.expose_target_rect(&first_win).unwrap(),
        "un-hovered, a preview draws exactly in its slot"
    );

    let center = rest.loc + rest.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    f.settle_animations();

    let grown = f.synoik().layout.expose_drawn_rect(&first_win).unwrap();
    assert!(
        grown.size.w > rest.size.w && grown.size.h > rest.size.h,
        "hovering must grow the preview, got {grown:?} from {rest:?}"
    );
    // The growth is in screen pixels — 5 in each direction on the longest side,
    // whatever the row's zoom — and the short side follows the same ratio.
    let longest = f64::max(rest.size.w, rest.size.h);
    let expected = (longest + 10.) / longest;
    assert!(
        (grown.size.w / rest.size.w - expected).abs() <= 0.001
            && (grown.size.h / rest.size.h - expected).abs() <= 0.001,
        "the longest side must grow by 2x5 screen px: {grown:?} vs {rest:?}"
    );
    let grown_center = grown.loc + grown.size.downscale(2.).to_point();
    assert!(
        (grown_center.x - center.x).abs() <= 1. && (grown_center.y - center.y).abs() <= 1.,
        "it grows about its center: {grown_center:?} vs {center:?}"
    );

    // Off the preview again and it eases back to the slot.
    pointer_motion_to(&mut f, 5., 5.);
    f.settle_animations();
    assert_eq!(
        f.synoik().layout.expose_drawn_rect(&first_win).unwrap(),
        rest,
        "leaving a preview eases it back into its slot"
    );
}

/// **Divergence (approved 2026-07-28).** A picker slot keeps the same clearance at both edges
/// of the workspace background it sits on. gnome-shell lays out over the raw work area while
/// the background is the whole monitor, so the top panel's strut is clearance the top edge
/// gets and the bottom does not — a maximized window's preview came out with 40px at the
/// sides and 51 above but only 22 below, which reads as the window touching the bottom.
///
/// Symmetrizing the area costs the preview no size: the `MAXIMUM_SCALE` cap still binds, so
/// the slot only moves. (Which is also why a padding constant would have done nothing here —
/// it would have had to exceed the slack under the cap before it moved anything at all.)
#[test]
fn overview_picker_slots_clear_both_workspace_edges_evenly() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A maximized window: the case where the slot comes closest to the edges.
    let _w = map_window_sized(&mut f, id, (1920, 1048), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let output = f.synoik_output(1);
    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    let bg = f
        .synoik()
        .layout
        .monitor_for_output(&output)
        .unwrap()
        .workspace_under(smithay::utils::Point::from((960., 500.)))
        .expect("the active workspace is under the middle of the view")
        .1;

    let top = slot.loc.y - bg.loc.y;
    let bottom = (bg.loc.y + bg.size.h) - (slot.loc.y + slot.size.h);
    let left = slot.loc.x - bg.loc.x;
    // Even to within a rounding of the zoomed row, not to the pixel.
    assert!(
        (top - bottom).abs() <= bg.size.h * 0.005,
        "the preview must clear both workspace edges evenly, got top={top} bottom={bottom}"
    );
    // …and the clearance is real, not a hairline: comparable to what the sides get.
    assert!(
        bottom > left / 2.,
        "the bottom clearance must read like a margin, got {bottom} against {left} at the side"
    );

    // The cap still decides the size, so nothing was scaled down to buy that room.
    let want = (1048. * 0.95) / 1080.;
    assert!(
        (slot.size.h / bg.size.h - want).abs() < 1e-9,
        "the preview must keep its MAXIMUM_SCALE size, got {}",
        slot.size.h / bg.size.h
    );
}

/// …and the symmetry is bought by insetting to the *larger* strut, never by centering on the
/// view outright — so a bottom dock still keeps the picker out of its space.
#[test]
fn overview_picker_slots_stay_out_of_a_bottom_strut() {
    use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
    use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;

    use crate::tests::client::LayerConfigureProps;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A 200px bottom dock, taller than the top panel's strut.
    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        size: Some((0, 200)),
        anchor: Some(Anchor::Left | Anchor::Right | Anchor::Bottom),
        exclusive_zone: Some(200),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);
    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.set_size(1920, 200);
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let _w = map_window_sized(&mut f, id, (1920, 800), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let output = f.synoik_output(1);
    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    let bg = f
        .synoik()
        .layout
        .monitor_for_output(&output)
        .unwrap()
        .workspace_under(smithay::utils::Point::from((960., 300.)))
        .expect("the active workspace is under the middle of the view")
        .1;

    // The dock takes 200 of 1080, so the picker must stay at least that far off both edges
    // (the inset is the larger strut, applied to both).
    let strut_frac = 200. / 1080.;
    let top = (slot.loc.y - bg.loc.y) / bg.size.h;
    let bottom = ((bg.loc.y + bg.size.h) - (slot.loc.y + slot.size.h)) / bg.size.h;
    assert!(
        bottom >= strut_frac - 0.001,
        "a preview must not reach into the bottom dock's space, got {bottom} of the workspace"
    );
    assert!(
        top >= strut_frac - 0.001,
        "…and the inset is symmetric, got {top} at the top"
    );
}

/// **Lifecycle L3.** The preview's app icon is not hover chrome: it shows for every
/// preview in the window picker, and its *scale* — not its opacity — ramps with the
/// overview axis (`_updateIconScale`, `windowPreview.js:238-252`). It must therefore
/// survive the app-grid transition, shrinking to nothing, rather than being dropped
/// the way the hover overlay is (`_syncOverlay`, `workspace.js:775-777`).
#[test]
fn overview_preview_icon_scales_out_into_the_app_grid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window_sized(&mut f, id, (800, 600), None);

    let icon_scale = |f: &mut Fixture| -> f64 {
        let output = f.synoik_output(1);
        f.synoik()
            .layout
            .monitor_for_output(&output)
            .unwrap()
            .preview_icon_scale()
    };
    let previews = |f: &mut Fixture| -> usize {
        let output = f.synoik_output(1);
        f.synoik()
            .layout
            .monitor_for_output(&output)
            .unwrap()
            .preview_rects()
            .len()
    };

    assert_eq!(icon_scale(&mut f), 0., "no icon with the overview closed");

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    assert_eq!(icon_scale(&mut f), 1., "full size in the window picker");
    assert_eq!(previews(&mut f), 1);

    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert_eq!(
        icon_scale(&mut f),
        0.,
        "the icon has scaled away in the app grid"
    );
    assert_eq!(
        previews(&mut f),
        1,
        "but the preview is still drawn — the icon shrinks, it is not dropped"
    );
    let output = f.synoik_output(1);
    assert!(
        f.synoik()
            .layout
            .monitor_for_output(&output)
            .unwrap()
            .preview_overlays()
            .is_empty(),
        "the hover overlay, unlike the icon, is dropped outright"
    );
}

/// The close button half-overhangs its preview (`windowPreview.js:203-218`), so reaching for
/// that half takes the pointer *off* the picker slot. gnome-shell doesn't care — the button
/// is a child actor, so it is inside the preview's own reactive box — but ours are separate
/// rects hit-tested against the slot, and the preview used to de-emphasize and fade the
/// button out from under the pointer aiming at it (reported from live use, 2026-07-28).
///
/// The whole button is on the preview as far as hover is concerned, however long you take.
#[test]
fn overview_preview_stays_hovered_over_the_close_button_overhang() {
    use crate::ui::window_preview::{close_rect, CLOSE_SIZE};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let overlay_alpha = |f: &mut Fixture| -> f64 {
        let output = f.synoik_output(1);
        let mon = f.synoik().layout.monitor_for_output(&output).unwrap();
        mon.preview_overlays()
            .into_iter()
            .find(|(w, _, _)| *w == win)
            .map_or(0., |(_, _, alpha)| alpha)
    };

    // Hover the preview and let the overlay fade all the way in.
    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    let inside = slot.loc + slot.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, inside.x, inside.y);
    f.settle_animations();
    assert_eq!(overlay_alpha(&mut f), 1., "hovering must show the overlay");

    // Now move onto the overhanging half of the button — outside the slot on both axes —
    // and let everything settle, which is what a human aiming at it does.
    let drawn = f.synoik().layout.expose_drawn_rect(&win).unwrap();
    let button = close_rect(drawn);
    let overhang = button.loc + smithay::utils::Point::from((CLOSE_SIZE * 0.75, CLOSE_SIZE * 0.25));
    assert!(
        !drawn.contains(overhang),
        "the sample point must be off the preview, or this pins nothing"
    );
    pointer_motion_to(&mut f, overhang.x, overhang.y);
    f.settle_animations();

    assert_eq!(
        overlay_alpha(&mut f),
        1.,
        "the preview must stay hovered while the pointer is on its close button"
    );

    // …and the button is still there to be clicked.
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(id);
    assert!(
        f.client(id).window(&surface).close_requested,
        "a click on the overhanging half of the button must still close the window"
    );
}

/// Hovering a preview also reveals its close button — the other half of
/// `showOverlay` (`windowPreview.js:326-337`). It is centered on the preview's
/// top-right corner (`:203-218`) and asks the window to close (`_deleteAll`),
/// leaving the overview open. No hover, no button.
#[test]
fn overview_preview_close_button_closes_the_window() {
    use crate::ui::window_preview::{close_rect, CLOSE_SIZE};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // Un-hovered there is no button: clicking where it would be — the half of its
    // box that overhangs the preview's corner, clear of the preview itself —
    // closes nothing.
    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    let button = close_rect(slot);
    let outside = button.loc + smithay::utils::Point::from((CLOSE_SIZE * 0.75, CLOSE_SIZE * 0.25));
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(id);
    assert!(
        !f.client(id).window(&surface).close_requested,
        "there is no close button until the preview is hovered"
    );

    // That click landed on the overview backdrop, which leaves the overview.
    if !f.synoik().layout.is_overview_open() {
        tap(&mut f, KEY_LEFTMETA);
    }
    f.settle_animations();

    // Hover the preview, then click the button on its top-right corner. The
    // preview has grown by then, so take the button from the drawn rect.
    let inside = slot.loc + slot.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, inside.x, inside.y);
    f.settle_animations();

    let drawn = f.synoik().layout.expose_drawn_rect(&win).unwrap();
    let button = close_rect(drawn);
    let center = button.loc + button.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(id);

    assert!(
        f.client(id).window(&surface).close_requested,
        "clicking the close button must ask the window to close"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "and must leave the overview open"
    );
}

/// GNOME overview geometry (gnome-shell `ControlsManagerLayout`): the workspace
/// row is fit by height into the window-picker box the overview chrome leaves
/// over — it is *not* centered in the output, and the scale is not a constant.
/// The picker slot of a lone window pins both the box and the scale it implies.
#[test]
fn overview_workspace_fills_its_allocated_picker_box() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // The small workspace row is always reserved (divergence, see
    // `docs/fork/dynamic-workspaces-divergence.md`), and it is one app-grid workspace
    // tall. Work area 1048 tall ⇒ spacing round(20.96) = 21, row height
    // round(1048·0.15) = 157 at y = 32 + 40 (the search puck's midline), round(21·1.2) =
    // 25 below it. The search entry floats (approved divergence), so it costs the picker
    // nothing:
    //   y = 32 + 40 + 157 + 25               = 254
    //   h = 1048 − 112(dash) − 21 − 40 − 157 − 25 = 693
    let controls = overview_controls(&mut f);
    assert_eq!(controls.workspaces.loc.y, 254.);
    assert_eq!(controls.workspaces.size.h, 693.);

    // The row is fit by height into that box, and centered on what width is left.
    let zoom: f64 = 693. / 1080.;
    let ws_w = (1920. * zoom).ceil();
    let offset_x = ((1920. - ws_w) / 2.).round();

    // Workspace-local slot (see expose::tests): 760 × 570 centered in the picker's area,
    // which is the work area symmetrized about the view — the 32px panel strut is applied
    // at both edges, giving 1920×1016 at y = 32, so the slot sits at
    // (580, 32 + (1016−570)/2) = (580, 255), scaled into the picker box at y = 227.
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    assert_pos_eq(
        (rect.loc.x, rect.loc.y),
        (offset_x + 580. * zoom, 254. + 255. * zoom),
        "picker slot must sit in the allocated window-picker box",
    );
    assert!(
        (rect.size.w - 760. * zoom).abs() <= 1. && (rect.size.h - 570. * zoom).abs() <= 1.,
        "picker slot size must scale by the workspace zoom, got {rect:?}"
    );
}

/// In the overview a workspace's background rounds to gnome-shell's
/// `.workspace-background` `border-radius: 30px`, and its `box-shadow` sits on
/// that same rounded box (`_window-picker.scss:56-60`,
/// `WorkspaceBackground._updateBorderRadius`). The wallpaper and the shadow
/// derived the radius separately and the shadow's stayed square, so bare backdrop
/// poked out of every rounded corner as a pointy dark tab.
///
/// Pins the sharing — one accessor, growing with the overview and zero on the
/// desktop — since that is exactly what drifted. (Pixel-sampling the corner can't
/// pin it headless: with no wallpaper the workspace falls back to its opaque solid
/// background, which covers the corner.)
#[test]
fn overview_workspace_shadow_shares_the_background_radius() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = map_window_sized(&mut f, id, (800, 600), None);

    let radius = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .workspace_background_radius()
    };

    assert_eq!(
        radius(&mut f),
        0.,
        "on the desktop the workspace fills the screen with square corners"
    );

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // 30 pre-zoom units divided by the zoom, so it lands at 30 on screen.
    let zoom = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().overview_zoom()
    };
    let open = radius(&mut f);
    assert!(
        (open - 30. / zoom).abs() < 1e-6,
        "the overview radius must be gnome-shell's 30px on screen, got {open} at zoom {zoom}"
    );
}

/// GNOME draws no compositor-side chrome around a window — mutter has no
/// border or focus-ring concept, and focus is communicated by raising the window
/// and by its own CSD. niri's border and focus ring are its own idiom, so GNOME
/// windowing mode forces both off, **window rules included**: a rule that turns
/// one back on would paint an outline GNOME never draws.
///
/// The geometry follows: with no border the window keeps the whole tile.
#[test]
fn gnome_mode_draws_no_border_or_focus_ring() {
    let mut config = Config::default();
    // Everything a user could turn them on with, at once.
    config.layout.border = synoik_config::Border {
        off: false,
        width: 8.,
        ..Default::default()
    };
    config.layout.focus_ring = synoik_config::FocusRing {
        off: false,
        width: 8.,
        ..Default::default()
    };
    // A rule that explicitly turns the border back on: the case that must still
    // lose to GNOME mode.
    config.window_rules.push(synoik_config::WindowRule {
        border: synoik_config::BorderRule {
            on: true,
            width: Some(synoik_config::FloatOrInt(6.)),
            ..Default::default()
        },
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = map_window_sized(&mut f, id, (800, 600), None);

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let tile = mon
        .expect("workspaces must be on a monitor")
        .active_workspace_ref()
        .tiles()
        .next()
        .expect("a mapped window must have a tile");

    assert!(
        tile.border().is_off() && tile.focus_ring().is_off(),
        "GNOME mode must force both off whatever the config and rules say"
    );
    assert_eq!(
        tile.tile_size(),
        tile.window_size(),
        "with no border the window keeps the whole tile"
    );
}

/// The workspace row lands on its picker box only when the overview is fully
/// open; closed it covers the output exactly (a pointer against the screen edge
/// at y = 0 must still hit it), and it travels between the two continuously.
/// gnome-shell gets this by interpolating its `HIDDEN` and `WINDOW_PICKER`
/// boxes (`overviewControls.js:207-216`); anchoring the row at the picker box
/// outright would make every overview open/close jump.
#[test]
fn overview_workspace_offset_interpolates_out_of_the_desktop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    let row_y = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .workspaces_render_geo()
            .next()
            .unwrap()
            .loc
            .y
    };

    // Closed: exactly on the desktop.
    assert_eq!(row_y(&mut f), 0.);

    tap(&mut f, KEY_LEFTMETA);
    // Mid-animation: strictly between the desktop and the picker box.
    {
        let synoik = f.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik.clock.set_unadjusted(now + Duration::from_millis(60));
        synoik.advance_animations();
    }
    let box_y = overview_controls(&mut f).workspaces.loc.y;
    let mid = row_y(&mut f);
    assert!(
        mid > 0. && mid < box_y,
        "mid-open the row must be travelling toward the picker box, got y={mid} (box {box_y})"
    );

    f.settle_animations();
    assert_eq!(row_y(&mut f), box_y);

    // And all the way back down on close.
    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    assert_eq!(row_y(&mut f), 0.);
}

/// **Divergence (approved 2026-07-28/29, revised 2026-08-03).** The search entry floats at
/// the top right instead of taking a full-width row, and the workspace row is one of
/// app-grid workspaces rather than gnome-shell's 5% specks — full width, with its top on
/// the entry puck's midline, so the pill *overlaps* its top-right corner instead of the row
/// dodging the pill's column. Judged on both the reference canvas and the 1024×665 one the
/// adaptive chrome ramp was written for (`docs/fork/adaptive-overview-chrome.md`), because
/// that is the canvas the sizes actually have to work on.
#[test]
fn overview_entry_floats_over_an_app_grid_sized_workspace_row() {
    for size in [(1920u16, 1080u16), (1024, 665)] {
        let mut f = Fixture::new();
        f.add_output(1, size);
        let id = f.add_client();
        let (_a, _b) = setup_two_desktops_in_overview_on(&mut f, id, size);
        f.settle_animations();

        let controls = overview_controls(&mut f);
        let pill = f.synoik().overview_search.entry_pill(controls.into());
        let band = controls.workspace_row;
        let view_w = f64::from(size.0);
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let strip = mon
            .expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("three workspaces must show the strip");

        // The pill hugs the right edge rather than centering — and by less than it is
        // wide, so this is genuinely "floating right" and not a re-centered box.
        let right_gap = view_w - (pill.loc.x + pill.size.w);
        assert!(
            right_gap > 0. && right_gap < pill.size.w,
            "{size:?}: the entry must float at the right edge, gap {right_gap}"
        );

        // It costs the row no vertical space: the band starts *above* where the entry's
        // reserved height ends, which is where GNOME would have had its whole row.
        assert!(
            band.loc.y - crate::ui::panel::panel_height()
                < crate::ui::overview_search::PREFERRED_ENTRY_HEIGHT,
            "{size:?}: the entry still displaces the row (band at {})",
            band.loc.y
        );

        // The strip is at the doubled cap: a thumbnail is a tenth of the view tall.
        assert_eq!(
            strip.thumbs[0].size.h,
            crate::ui::overview_layout::small_workspace_height(
                smithay::utils::Size::from((f64::from(size.0), f64::from(size.1))),
                crate::ui::panel::panel_height(),
            ),
            "{size:?}: a thumbnail must be the app-grid row's workspace height"
        );

        // And the two deliberately *do* overlap: the row is full width and starts at the
        // puck's midline, so the pill floats over its top-right corner. This is the
        // divergence, pinned so it cannot be undone by accident.
        assert_eq!(
            (band.loc.x, band.size.w),
            (0., view_w),
            "{size:?}: the row must span the full width"
        );
        assert!(
            band.loc.y < pill.loc.y + pill.size.h,
            "{size:?}: the row's top must sit inside the pill's band, not below it"
        );
        assert!(
            strip.thumbs[0].loc.x >= band.loc.x,
            "{size:?}: the row must start inside its band"
        );
    }
}

/// **Divergence (approved 2026-08-03).** The strip and the app-grid row are the *same*
/// row: one box, one layout, drawn identically in both overview states, so the show-apps
/// transition cannot move it and the user cannot tell the two apart. gnome-shell has two
/// unrelated rows — thumbnails at `MAX_THUMBNAIL_SCALE` (5%) in the window picker, and the
/// picker itself shrunk to `SMALL_WORKSPACE_RATIO` (15%) in the app grid.
///
/// Asserted across the toggle rather than against a constant, so neither state can drift.
#[test]
fn the_workspace_row_is_the_same_in_both_overview_states() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let row = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("the row is always shown in the overview")
            .thumbs
    };

    let picker = row(&mut f);
    assert!(picker.len() >= 3, "three workspaces must be laid out");

    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");

    assert_eq!(
        row(&mut f),
        picker,
        "the workspace row must not move when the app grid opens"
    );

    // And the picker behind it does not travel into the row either — it fades away in
    // place, so nothing is left to reconcile with the row that is already there.
    let controls = overview_controls(&mut f);
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert_eq!(
        overview_controls(&mut f).workspaces,
        controls.workspaces,
        "the window picker's box must not depend on the app-grid state"
    );
}

/// Past the point where the row fills its band, the strip scrolls to follow the
/// active workspace instead of shrinking to fit (**divergence**, approved
/// 2026-07-29 — gnome-shell's `vfunc_get_preferred_height` shrinks below
/// `MAX_THUMBNAIL_SCALE` until everything fits, which turns a long strip into a
/// row of specks). Whatever the count, a thumbnail is the same size, the active
/// one is on screen, and nothing draws over the floating search entry.
#[test]
fn overview_thumbnail_strip_scrolls_instead_of_shrinking() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    setup_n_desktops(&mut f, id, 12);

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    let controls = overview_controls(&mut f);
    let pill = f.synoik().overview_search.entry_pill(controls.into());
    let band = controls.workspace_row;

    let strip_now = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("many workspaces must show the strip")
    };

    let strip = strip_now(&mut f);
    let n = strip.thumbs.len();
    assert!(
        n >= 12,
        "expected the populated workspaces plus a spare, got {n}"
    );
    // The row genuinely overflows — otherwise there is nothing to scroll.
    assert!(
        strip.bounds().size.w > band.size.w,
        "this test is vacuous unless the row overflows its band"
    );
    // …and the thumbnails are still the full doubled cap, not shrunk to fit.
    assert_eq!(
        strip.thumbs[0].size.h,
        crate::ui::overview_layout::small_workspace_height(
            smithay::utils::Size::from((1920., 1080.)),
            crate::ui::panel::panel_height(),
        ),
        "the size must not give way to the workspace count"
    );

    // Walk down the row: the active workspace's thumbnail is inside the band every step
    // of the way. The band no longer dodges the entry — it is full width and the pill
    // floats over its top-right corner (see
    // `overview_entry_floats_over_an_app_grid_sized_workspace_row`).
    assert!(
        pill.loc.x + pill.size.w <= band.loc.x + band.size.w,
        "the pill must float over the row, not beside it"
    );
    for _ in 0..n {
        let active = f
            .synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx();
        let strip = strip_now(&mut f);
        let rect = strip.thumbs[active];
        assert!(
            rect.loc.x >= band.loc.x && rect.loc.x + rect.size.w <= band.loc.x + band.size.w,
            "thumbnail {active} is outside the band at {rect:?}"
        );
        // The visible row is exactly what can be clicked: a thumbnail scrolled out
        // is not hit-testable where it would otherwise be.
        let center = smithay::utils::Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + rect.size.h / 2.,
        ));
        assert_eq!(strip.thumb_under(center), Some(active));

        f.synoik_state()
            .do_action(Action::FocusWorkspaceDown, false);
        f.settle_animations();
    }
}

/// The thumbnails strip fills the band the overview layout allocates it, just
/// below the search entry — not a band derived from the workspace zoom.
#[test]
fn overview_thumbnail_strip_fills_its_allocated_band() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let band = overview_controls(&mut f).workspace_row;
    // 32 + 40, the search puck's midline (the entry floats and takes no row), and one
    // small workspace tall, round((1080 - 32) × SMALL_WORKSPACE_RATIO) = 157.
    assert_eq!((band.loc.y, band.size.h), (72., 157.));
    // Full width, in both overview states.
    assert_eq!((band.loc.x, band.size.w), (0., 1920.));

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let strip = mon
        .expect("workspaces must be on a monitor")
        .thumbnail_strip()
        .expect("three workspaces must show the strip");
    assert_eq!(strip.thumbs[0].loc.y, band.loc.y);
    assert_eq!(strip.thumbs[0].size.h, band.size.h);
    assert!(
        strip.thumbs[0].loc.y >= crate::ui::panel::panel_height(),
        "the strip must clear the top panel, got y={}",
        strip.thumbs[0].loc.y
    );
}

/// The thumbnails band is reserved whatever the workspace count (divergence, see
/// `docs/fork/dynamic-workspaces-divergence.md`), so the picker box — and the workspace
/// zoom derived from it — no longer moves when a second desktop is populated or emptied.
/// gnome-shell instead crosses `NUM_WORKSPACES_THRESHOLD` here and eases
/// `ThumbnailsBox.expandFraction` (`overviewControls.js:358-366`); there is nothing left
/// to ease, and this pins that the box really is unmoved rather than merely un-animated.
#[test]
fn overview_picker_box_does_not_move_with_the_workspace_count() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let _w2 = map_window_sized(&mut f, id, (640, 480), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // One populated desktop plus the trailing empty: the band is already there.
    let one = overview_controls(&mut f).workspaces;
    assert_eq!((one.loc.y, one.size.h), (254., 693.));

    // Populate a second desktop. Sample mid-transition too: an eased band would be
    // caught here, and a popped one would show a different box on the next frame.
    f.synoik().layout.move_to_workspace_down(true);
    f.synoik().advance_animations();
    {
        let synoik = f.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik.clock.set_unadjusted(now + Duration::from_millis(60));
        synoik.advance_animations();
    }
    let mid = overview_controls(&mut f).workspaces;
    assert_eq!((mid.loc.y, mid.size.h), (254., 693.));

    f.settle_animations();
    let two = overview_controls(&mut f).workspaces;
    assert_eq!((two.loc.y, two.size.h), (254., 693.));

    // …and back. The emptied desktop is not reaped, so this is now three workspaces.
    f.synoik().layout.move_to_workspace_up(true);
    f.settle_animations();
    let back = overview_controls(&mut f).workspaces;
    assert_eq!((back.loc.y, back.size.h), (254., 693.));

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let mon = mon.expect("workspaces must be on a monitor");
    assert!(
        mon.thumbnails_visible(),
        "the strip is shown at every count while the overview is open"
    );
}

/// GNOME overview click semantics (gnome-shell Workspace click): clicking a
/// neighbor workspace (peeking at the screen edge of the horizontal row)
/// switches to it and stays in the overview; clicking the empty area of the
/// active workspace leaves the overview.
#[test]
fn overview_click_neighbor_switches_and_stays() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // The trailing empty workspace peeks at the right edge of the row:
    // the active workspace spans 161..1760 and the neighbor, drawn a touch
    // smaller, is visible from 1832 on (gnome-shell keeps the spacing at
    // its minimum exactly so neighbors peek in).
    f.pointer_motion(1850., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "clicking a neighbor workspace must not leave the overview"
    );
    let active = f.synoik().layout.active_workspace().unwrap().id();
    assert_ne!(
        active, ws1_id,
        "clicking a neighbor workspace must switch to it"
    );

    // Clicking the empty area of the (now centered) active workspace leaves
    // the overview.
    f.pointer_motion(-940., 0.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().layout.is_overview_open(),
        "clicking the active workspace's empty area must leave the overview"
    );
    assert_eq!(
        f.synoik().layout.active_workspace().unwrap().id(),
        active,
        "leaving the overview must keep the clicked workspace active"
    );
}

/// An overview drag is gnome-shell's WindowPreview drag: the preview's
/// location isn't the window's, so dropping it back on its own workspace
/// must not reposition the window on the desktop.
#[test]
fn overview_drag_within_workspace_keeps_desktop_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();
    let original_pos = focused_window_pos(&mut f);

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // Drag the preview towards the workspace's top-left corner and drop it.
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    f.pointer_motion(rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(-400., -300.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.synoik().layout.is_overview_open(),
        "dropping a preview must not leave the overview"
    );

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "a drop on the window's own workspace must not move it on the desktop",
    );
}

/// Picking a preview up in the overview shrinks it: gnome-shell hands its
/// draggable a `dragActorMaxSize` of `WINDOW_DND_SIZE` (256px,
/// `windowPreview.js:14,108`) and `dnd.js:261-288` eases the drag actor down to
/// fit it over `SCALE_ANIMATION_TIME`, so what you carry across the row is small
/// enough to see the target under it.
#[test]
fn overview_dragged_preview_shrinks_to_the_dnd_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    assert!(
        f64::max(slot.size.w, slot.size.h) > 256.,
        "the preview must start bigger than WINDOW_DND_SIZE for this to test anything"
    );

    // Pick it up: the drag starts at the footprint it was picked up at.
    let center = slot.loc + slot.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(-100., -100.);

    let picked = f
        .synoik()
        .layout
        .interactive_move_drawn_size()
        .expect("the drag must be in flight");
    assert!(
        (picked.w - slot.size.w).abs() <= 2.,
        "the drag starts at the preview's own footprint, got {picked:?} vs {:?}",
        slot.size
    );

    f.settle_animations();
    let shrunk = f.synoik().layout.interactive_move_drawn_size().unwrap();
    assert!(
        (f64::max(shrunk.w, shrunk.h) - 256.).abs() <= 1.,
        "the dragged preview must shrink to 256px on its longest side, got {shrunk:?}"
    );
    assert!(
        (shrunk.w / shrunk.h - picked.w / picked.h).abs() <= 0.01,
        "and keep its aspect, got {shrunk:?} from {picked:?}"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
}

/// Dropping a preview on a neighbor workspace peeking at the screen edge
/// moves the window there and nothing else: it keeps its desktop position
/// (not flush against the neighbor's left edge) and the overview stays open.
#[test]
fn overview_drag_to_neighbor_keeps_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();
    let original_pos = focused_window_pos(&mut f);
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // Drag the preview onto the trailing workspace peeking at the right
    // screen edge (visible from 1752 on; see the neighbor click test).
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(1800. - grab.0, 540. - grab.1 - 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.synoik().layout.is_overview_open(),
        "dropping a preview on a neighbor must not leave the overview"
    );

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win))
        .expect("the window must still be mapped somewhere");
    assert_ne!(
        ws.id(),
        ws1_id,
        "dropping on the neighbor's peeking edge must move the window there"
    );
    let (_, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| tile.window().window == win)
        .unwrap();
    assert_pos_eq(
        (pos.x, pos.y),
        original_pos,
        "the window must keep its desktop position on the new workspace",
    );
}

/// The workspace row is laid out with gnome-shell's `FitMode.ALL`: every workspace inside
/// the allocation with the run centered as a whole (`_getFirstFitAllWorkspaceBox`,
/// `workspacesView.js:128-170`), so which workspace is active does not shift anything, and
/// packed at `WORKSPACE_MIN_SPACING` rather than the picker's roomy peek-at-the-edges gap
/// (`_getSpacing`'s `(1 - fitMode)` factor, `:207-226`).
///
/// **Divergence (approved 2026-08-03).** gnome-shell only reaches fit-all in the app-grid
/// state, by sliding the picker into it. Ours is the row's layout in *both* states — the
/// picker itself stays fit-single behind it and simply fades away.
///
/// Driven from the *first* of three workspaces, where fit-all and the picker's fit-single
/// are furthest apart: fit-single puts a third of the row off the left edge.
#[test]
fn the_workspace_row_fits_all_of_the_workspaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = setup_two_desktops_in_overview(&mut f, id);

    // Back to the first workspace, so the active one is off-center in the row.
    while f
        .synoik()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    use smithay::utils::{Logical, Rectangle};

    let picker_row = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .workspaces_render_geo()
            .take(3)
            .collect()
    };
    let row = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("the row is always shown in the overview")
            .thumbs
    };
    // Each workspace is centered in its slot, so slot geometry is read off the
    // rect *centers* — the inactive-workspace shrink leaves those untouched.
    let center_of = |r: &Rectangle<f64, Logical>| r.loc.x + r.size.w / 2.;
    let run_center = |row: &[Rectangle<f64, Logical>]| {
        (center_of(&row[0]) + center_of(&row[row.len() - 1])) / 2.
    };
    let view_center = 1920. / 2.;

    // The picker behind it is fit-single, so the *active* workspace is the centered one
    // and the run as a whole hangs off to the right. That is the arrangement the row is
    // deliberately *not* in.
    let picker = picker_row(&mut f);
    assert_eq!(picker.len(), 3, "three workspaces must be laid out");
    assert!(
        (center_of(&picker[0]) - view_center).abs() <= 1.,
        "in the picker the active workspace must be centered, got {picker:?}"
    );
    assert!(
        run_center(&picker) > view_center + 100.,
        "in the picker the run must hang off to one side, got {picker:?}"
    );

    // The row: fit-all, so the run is centered and the active workspace is wherever its
    // index puts it — and the same before and after the app grid opens.
    let before = row(&mut f);
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");
    let grid = row(&mut f);
    assert_eq!(
        grid, before,
        "the row must not move with the app-grid state"
    );

    assert!(
        (run_center(&grid) - view_center).abs() <= 1.,
        "the whole run must be centered, got {grid:?}"
    );
    assert!(
        center_of(&grid[1]) > center_of(&grid[0]) && center_of(&grid[2]) > center_of(&grid[1]),
        "the row must stay in workspace order, got {grid:?}"
    );

    // Packed at WORKSPACE_MIN_SPACING. `_getSpacing`'s `(1 - fitMode)` factor is what does
    // this: at the row's small zoom the workspace takes little of the width, so the
    // fit-*single* formula would run all the way up to WORKSPACE_MAX_SPACING (80) instead.
    let gap = center_of(&grid[1]) - center_of(&grid[0]) - grid[0].size.w;
    assert!(
        (gap - 24.).abs() <= 1.,
        "the fitted row must pack at WORKSPACE_MIN_SPACING 24, got {gap}"
    );
}

/// Populates `n` workspaces, one window each, leaving the trailing empty one that
/// dynamic workspaces always keep. A window maps onto the focused workspace, so
/// stepping down after each map lands the next one on a fresh desktop.
fn setup_n_desktops(f: &mut Fixture, id: ClientId, n: usize) {
    for _ in 0..n {
        let _ = map_window_sized(f, id, (800, 600), None);
        f.synoik_state()
            .do_action(Action::FocusWorkspaceDown, false);
    }
    f.synoik_complete_animations();
}

/// With enough workspaces the fitted row no longer fits, and every workspace past the edge
/// used to be unreachable: gnome-shell keeps them on screen by narrowing each box to
/// `availableWidth / n` (`_getFirstFitAllWorkspaceBox`, `workspacesView.js:127-169`), which
/// we can't do with one aspect-locked zoom per monitor. The overflowing row scrolls to
/// follow the active workspace instead (**divergence**, approved 2026-07-29).
///
/// Driven with the app grid *open*, where the row is all there is to aim at;
/// `overview_thumbnail_strip_scrolls_instead_of_shrinking` drives the same row in the
/// window-picker state.
#[test]
fn app_grid_scrolls_an_overflowing_workspace_row_into_view() {
    use smithay::utils::{Logical, Rectangle};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    // Eight populated workspaces at the app grid's zoom overflow 1920 (~279px wide,
    // packed at the 24px minimum spacing) with room to spare.
    setup_n_desktops(&mut f, id, 8);

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");

    let row = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("the row is always shown in the overview")
            .thumbs
    };
    let n = row(&mut f).len();
    assert!(
        n >= 8,
        "expected the populated workspaces plus a spare, got {n}"
    );
    let run = {
        let r = row(&mut f);
        r[n - 1].loc.x + r[n - 1].size.w - r[0].loc.x
    };
    assert!(
        run > 1920.,
        "this test is vacuous unless the row actually overflows, got {run}"
    );

    // Walk the whole row from the top. Whichever workspace is active is fully on
    // screen — that is the property the report was about.
    while f
        .synoik()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    let mut visited = 0;
    loop {
        let active = f
            .synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx();
        visited += 1;
        let geo = row(&mut f);
        assert!(
            geo[active].loc.x >= -1. && geo[active].loc.x + geo[active].size.w <= 1921.,
            "workspace {active} is off screen at {:?}",
            geo[active]
        );
        // Rigid: scrolling moves the run, it never re-spaces it. Read off the slot
        // *centers*, which the active workspace's unshrunk rect leaves untouched.
        let center = |r: &Rectangle<f64, Logical>| r.loc.x + r.size.w / 2.;
        let pitch = center(&geo[1]) - center(&geo[0]);
        for w in geo.windows(2) {
            assert!(
                (center(&w[1]) - center(&w[0]) - pitch).abs() <= 2.,
                "the row must stay uniform at active={active}, got {geo:?}"
            );
        }
        f.synoik_state()
            .do_action(Action::FocusWorkspaceDown, false);
        f.settle_animations();
        let moved = f
            .synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx();
        if moved == active {
            break;
        }
    }
    assert!(
        visited >= 8,
        "the walk must reach every populated workspace, only saw {visited}"
    );
}

/// The workspace the row sits on draws at full size while every other one is
/// shrunk to `WORKSPACE_INACTIVE_SCALE` about its own center
/// (`WorkspacesView._updateWorkspacesState`, `workspacesView.js:243-266`, with
/// the centered pivot at `workspace.js:1039`) — what makes the workspace you are
/// on read as slightly larger than its neighbors.
///
/// The shrink belongs to the overview: on the desktop, where the same row is the
/// live session, everything stays at 1.
#[test]
fn overview_shrinks_the_inactive_workspaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = setup_two_desktops_in_overview(&mut f, id);

    while f
        .synoik()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    use crate::layout::monitor::WORKSPACE_INACTIVE_SCALE;

    let scales = |f: &mut Fixture| -> Vec<f64> {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let mon = mon.expect("workspaces must be on a monitor");
        (0..3).map(|i| mon.workspace_render_scale(i)).collect()
    };

    let open = scales(&mut f);
    assert_eq!(
        open,
        vec![1., WORKSPACE_INACTIVE_SCALE, WORKSPACE_INACTIVE_SCALE],
        "in the overview only the workspace the row sits on stays full size"
    );

    // It reaches the drawn geometry, centered in an unmoved slot: the shrunk
    // workspace is narrower yet its center is exactly where the full-size pitch
    // puts it.
    {
        use smithay::utils::{Logical, Rectangle};
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let row: Vec<Rectangle<f64, Logical>> =
            mon.unwrap().workspaces_render_geo().take(2).collect();
        assert!(
            row[1].size.w < row[0].size.w && row[1].size.h < row[0].size.h,
            "the inactive workspace must draw smaller, got {row:?}"
        );
        // A centered pivot inset the shrunk rect by exactly half the size it lost,
        // so its origin sits that much further along than the slot pitch would put
        // it. Shrinking about the origin instead would leave the two equal.
        let pitch = (row[1].loc.x + row[1].size.w / 2.) - (row[0].loc.x + row[0].size.w / 2.);
        let origin_step = row[1].loc.x - row[0].loc.x;
        let inset = (row[0].size.w - row[1].size.w) / 2.;
        assert!(
            (origin_step - pitch - inset).abs() <= 1.,
            "the shrink must be about the workspace's own center (pitch {pitch}, \
             origin step {origin_step}, half the width lost {inset})"
        );
    }

    // Closed: the row is the live desktop, so nothing is scaled.
    f.synoik_state().do_action(Action::CloseOverview, false);
    f.settle_animations();
    assert_eq!(
        scales(&mut f),
        vec![1., 1., 1.],
        "on the desktop no workspace may be shrunk"
    );
}

/// Two populated desktops with the overview open: window A stays on the
/// first workspace, window B is dragged to the trailing one (leaving a new
/// trailing empty third). Returns (A's window, B's window).
fn setup_two_desktops_in_overview(
    f: &mut Fixture,
    id: ClientId,
) -> (smithay::desktop::Window, smithay::desktop::Window) {
    setup_two_desktops_in_overview_on(f, id, (1920, 1080))
}

/// [`setup_two_desktops_in_overview`] on an output of an arbitrary size: the drop point
/// is the peeking neighbour at the right edge, which is where it is on any canvas.
fn setup_two_desktops_in_overview_on(
    f: &mut Fixture,
    id: ClientId,
    size: (u16, u16),
) -> (smithay::desktop::Window, smithay::desktop::Window) {
    let (drop_x, drop_y) = (f64::from(size.0) - 20., f64::from(size.1) / 2.);
    let _a = map_window_sized(f, id, (800, 600), None);
    let win_a = f.synoik().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(f, id, (640, 480), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();

    tap(f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // Drag B's preview onto the trailing workspace peeking at the right.
    let rect = f.synoik().layout.expose_target_rect(&win_b).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(drop_x - grab.0, drop_y - grab.1 - 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    (win_a, win_b)
}

/// Absolute pointer motion: `Fixture::pointer_motion` takes deltas.
pub(super) fn pointer_motion_to(f: &mut Fixture, x: f64, y: f64) {
    let cur = f.synoik().seat.get_pointer().unwrap().current_location();
    f.pointer_motion(x - cur.x, y - cur.y);
}

/// Horizontal centre of the dateMenu's clock button on an `output_w`-wide output — asked
/// of the panel, never hardcoded. Our clock is anchored to the output's right corner past
/// the status indicators (a divergence from GNOME's centre box), and its x further depends
/// on the label's width and on whether the messages dot is showing, so a literal
/// screen-centre would just click the wallpaper.
fn clock_center_x(f: &mut Fixture, output_w: f64) -> f64 {
    let rect = f.synoik().panel.date_menu_rect(output_w);
    rect.loc.x + rect.size.w / 2.
}

/// Horizontal centre of the quick-settings indicator — likewise asked of the panel. It no
/// longer owns the top-right corner: the dateMenu was moved past it (see
/// [`crate::ui::panel`]), so the cluster starts wherever the clock's box ends.
pub(super) fn qs_center_x(f: &mut Fixture, output_w: f64) -> f64 {
    let rect = f.synoik().panel.quick_settings_rect(output_w);
    rect.loc.x + rect.size.w / 2.
}

/// The open panel popover's actual top-left — asked of the popover rather than
/// recomputed from its anchor. The anchor is frozen when the menu opens, while the
/// indicator it hangs off keeps moving (showing the messages dot widens the dateMenu box,
/// sliding every right-box role left of it), so a hand-rolled origin goes stale mid-test.
pub(super) fn popover_origin(
    f: &mut Fixture,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let output = f.synoik_output(1);
    f.synoik().panel_popover.location(&output)
}

/// The center of the given strip thumbnail, for pointer input.
fn thumbnail_center(f: &mut Fixture, idx: usize) -> (f64, f64) {
    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let strip = mon
        .expect("workspaces must be on a monitor")
        .thumbnail_strip()
        .expect("the thumbnails strip must be visible");
    let rect = strip.thumbs[idx];
    (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.)
}

/// The close button of the strip thumbnail at `idx`, if it has one.
fn thumbnail_close_rect(
    f: &mut Fixture,
    idx: usize,
) -> Option<smithay::utils::Rectangle<f64, smithay::utils::Logical>> {
    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let mon = mon.expect("workspaces must be on a monitor");
    let id = mon.workspace_at(idx).expect("a workspace at idx").id();
    mon.thumbnail_close_rects()
        .into_iter()
        .find(|(ws, _)| *ws == id)
        .map(|(_, rect)| rect)
}

/// A fixture with two populated desktops in the overview, the first of them then emptied:
/// [empty, window, trailing empty]. Three thumbs of 279 plus 24 spacing fit the 1168-wide
/// band with room to spare, so nothing under test is scrolled out of reach — which is the
/// only way to hit a thumbnail, since the strip clips to its band.
///
/// Returns the emptied workspace's id and the survivor's.
fn dismissable_desktop_fixture(
    f: &mut Fixture,
    id: ClientId,
) -> (
    crate::layout::workspace::WorkspaceId,
    crate::layout::workspace::WorkspaceId,
) {
    setup_n_desktops(f, id, 2);
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    let win = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap()
            .workspace_at(0)
            .unwrap()
            .windows()
            .next()
            .unwrap()
            .window
            .clone()
    };
    f.synoik()
        .layout
        .remove_window(&win, crate::utils::transaction::Transaction::new());
    f.settle_animations();

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let mon = mon.unwrap();
    assert_eq!(mon.workspace_count(), 3);
    (
        mon.workspace_at(0).unwrap().id(),
        mon.workspace_at(1).unwrap().id(),
    )
}

/// **Divergence (approved 2026-08-03, `docs/fork/dynamic-workspaces-divergence.md`).**
/// An emptied workspace is not reaped; it grows a close button instead. Which thumbnails
/// get one is the whole policy: windowless, unnamed, not the trailing empty, and never so
/// many closed that the monitor drops under `MIN_NUM_WORKSPACES`.
#[test]
fn thumbnail_close_button_only_on_dismissable_desktops() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (empty_id, _) = dismissable_desktop_fixture(&mut f, id);

    // A populated desktop has no close button, and neither has the trailing empty one —
    // closing that would re-append it on the spot.
    assert!(
        thumbnail_close_rect(&mut f, 1).is_none(),
        "a populated desktop is not dismissable"
    );
    assert!(
        thumbnail_close_rect(&mut f, 2).is_none(),
        "the trailing empty desktop is not dismissable"
    );

    let rect = thumbnail_close_rect(&mut f, 0).expect("the emptied desktop must be dismissable");
    // Inside its thumbnail: the strip clips to its band, so an overhanging button would be
    // sliced in half along the band's top edge.
    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let strip = mon.unwrap().thumbnail_strip().unwrap();
    let thumb = strip.thumbs[0];
    assert!(
        rect.loc.x >= thumb.loc.x
            && rect.loc.y >= thumb.loc.y
            && rect.loc.x + rect.size.w <= thumb.loc.x + thumb.size.w
            && rect.loc.y + rect.size.h <= thumb.loc.y + thumb.size.h,
        "the close button must sit inside its thumbnail, got {rect:?} in {thumb:?}"
    );

    // Naming a workspace is how you say you want it kept — that is already what makes one
    // un-reapable, and it takes the close button away too.
    f.synoik().layout.set_workspace_name(
        String::from("kept"),
        Some(synoik_config::WorkspaceReference::Id(empty_id.get())),
    );
    f.settle_animations();
    assert!(
        thumbnail_close_rect(&mut f, 0).is_none(),
        "a named empty desktop is not dismissable"
    );
}

/// Clicking that button dismisses the desktop and the strip closes the gap — the point of
/// the whole divergence is that the *other* desktops survive, keeping the indices you have
/// been aiming `Super+N` at all day.
#[test]
fn thumbnail_close_button_dismisses_the_desktop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_, survivor) = dismissable_desktop_fixture(&mut f, id);

    let rect = thumbnail_close_rect(&mut f, 0).expect("the emptied desktop must be dismissable");
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let mon = mon.unwrap();
    assert_eq!(mon.workspace_count(), 2, "the desktop must be gone");
    assert_eq!(
        mon.workspace_at(0).unwrap().id(),
        survivor,
        "the desktop below it must have moved up into the gap"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "dismissing a desktop must leave the overview open"
    );
}

/// The button is hover-driven: it is drawn only while the pointer is on its thumbnail, and
/// lightens only while the pointer is on the button. Both flags gate the draw, so a wrong
/// one is an invisible button or a permanently lit one.
#[test]
fn thumbnail_close_button_follows_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (empty_id, _) = dismissable_desktop_fixture(&mut f, id);

    let rect = thumbnail_close_rect(&mut f, 0).expect("the emptied desktop must be dismissable");

    // On the thumbnail but away from the button. Not its centre: the row pins the *active*
    // workspace to the band's centre, so the left part of the first thumbnail is scrolled
    // out of the band and, being undrawn, is deliberately un-hittable.
    pointer_motion_to(&mut f, rect.loc.x - 10., rect.loc.y + rect.size.h / 2.);
    assert_eq!(f.synoik().thumbnail_hovered, Some(empty_id));
    assert_eq!(f.synoik().thumbnail_close_hovered, None);

    // On the button: lit.
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    assert_eq!(f.synoik().thumbnail_hovered, Some(empty_id));
    assert_eq!(f.synoik().thumbnail_close_hovered, Some(empty_id));

    // Off the strip entirely: neither.
    pointer_motion_to(&mut f, 960., 900.);
    assert_eq!(f.synoik().thumbnail_hovered, None);
    assert_eq!(f.synoik().thumbnail_close_hovered, None);
}

/// Dismissing a desktop slides the survivors into the gap rather than teleporting them:
/// mid-animation the row must be strictly between where it stood with the workspace still
/// in it and where it lands without. (Sampling only the endpoints cannot tell a real ease
/// from a settled one.)
///
/// The row pins the *active* workspace to the band's centre, so only the desktops *below*
/// the closed one move at all — closing one above the active desktop leaves every offset
/// unchanged, which is a feature (nothing jumps) but shows no slide. Hence: a survivor
/// below the doomed desktop, and the focus above it.
#[test]
fn dismissing_a_desktop_slides_the_strip_closed() {
    let mut f = Fixture::new();
    // Wide enough for four thumbnails (4x375 + 3x24 = 1572) inside the 1808-wide band:
    // a scrolled-out thumbnail is not drawn, and so cannot be aimed at.
    f.add_output(1, (2560, 1440));
    let id = f.add_client();

    setup_n_desktops(&mut f, id, 3);
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    // Empty desktop 1 and focus desktop 0: [window, empty, window, trailing empty].
    let win = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap()
            .workspace_at(1)
            .unwrap()
            .windows()
            .next()
            .unwrap()
            .window
            .clone()
    };
    f.synoik()
        .layout
        .remove_window(&win, crate::utils::transaction::Transaction::new());
    for _ in 0..3 {
        f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();
    let survivor = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let mon = mon.unwrap();
        assert_eq!(mon.workspace_count(), 4);
        mon.workspace_at(2).unwrap().id()
    };

    // The survivor's own thumbnail, followed by identity rather than by index: closing a
    // workspace renumbers everything below it, so an index would track two different
    // thumbnails on either side of the close.
    let survivor_x = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let mon = mon.unwrap();
        let idx = (0..mon.workspace_count())
            .find(|i| mon.workspace_at(*i).unwrap().id() == survivor)
            .expect("the survivor must still be there");
        mon.thumbnail_strip().unwrap().thumbs[idx].loc.x
    };

    let rect = thumbnail_close_rect(&mut f, 1).expect("the emptied desktop must be dismissable");
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // At t = 0 the row is still drawn exactly as it stood before the close.
    let start = survivor_x(&mut f);
    {
        let synoik = f.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik.clock.set_unadjusted(now + Duration::from_millis(60));
        synoik.advance_animations();
    }
    let mid = survivor_x(&mut f);
    f.settle_animations();
    let end = survivor_x(&mut f);

    assert!(start != end, "the strip must actually have a gap to close");
    let (lo, hi) = if start < end {
        (start, end)
    } else {
        (end, start)
    };
    assert!(
        mid > lo && mid < hi,
        "mid-slide the survivor must be between {start} and {end}, got {mid}"
    );
}

/// The row's two affordances — dismiss an emptied desktop, drag one to reorder — are there
/// in the **app-grid state** too, because it is the same row.
///
/// **Divergence (approved 2026-08-03).** gnome-shell's app-grid workspaces are inert
/// scenery: the thumbnail strip that carries its (window-drop) interactions is a different
/// actor, and it is faded out by then (`overviewControls.js:512-548`). Here there is one
/// row, so what it can do cannot depend on which state is showing. Window *previews* inside
/// it stay inert in both — that part is gnome-shell's, and
/// `app_grid_makes_the_shrunken_workspaces_inert` pins it.
#[test]
fn the_workspace_row_closes_and_reorders_in_the_app_grid_too() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (win_a, win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");

    let ws_idx_of = |f: &mut Fixture, win: &smithay::desktop::Window| {
        f.synoik()
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(win))
            .map(|(_, idx, _)| idx)
            .expect("the window must be on a workspace")
    };
    assert_eq!(
        (ws_idx_of(&mut f, &win_a), ws_idx_of(&mut f, &win_b)),
        (0, 1)
    );

    // Reorder by dragging, with the grid up.
    let (t0x, t0y) = thumbnail_center(&mut f, 0);
    let (t1x, _) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, t0x, t0y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, t1x + 1., t0y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.synoik().layout.is_app_grid_open(),
        "reordering must not drop out of the app grid"
    );
    assert_eq!(
        (ws_idx_of(&mut f, &win_a), ws_idx_of(&mut f, &win_b)),
        (1, 0),
        "dragging a workspace past its neighbour must reorder it in the app grid too"
    );

    // And an emptied desktop can still be dismissed from here. Empty the one that is now
    // first, then aim at its close button.
    let win = f.synoik().layout.focus().unwrap().window.clone();
    let empty_idx = ws_idx_of(&mut f, &win_b);
    f.synoik()
        .layout
        .remove_window(&win_b, crate::utils::transaction::Transaction::new());
    let _ = win;
    f.settle_animations();

    let before = f.synoik().layout.workspaces().count();
    let rect = thumbnail_close_rect(&mut f, empty_idx)
        .expect("an emptied desktop must be dismissable from the app grid");
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();

    assert_eq!(
        f.synoik().layout.workspaces().count(),
        before - 1,
        "the close button must dismiss the desktop in the app grid too"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "dismissing must not drop out of the app grid"
    );
}

/// A press aimed at the close button must not be swallowed by the reorder drag: the button
/// sits *inside* its thumbnail's body, so the hit test has to run before `ThumbGrab` takes
/// the press. (It went the other way round first, and every click reordered instead.)
#[test]
fn thumbnail_close_press_beats_the_reorder_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_, _) = dismissable_desktop_fixture(&mut f, id);

    let rect = thumbnail_close_rect(&mut f, 0).expect("the emptied desktop must be dismissable");
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        assert!(
            !mon.unwrap().thumb_drag_active(),
            "a press on the close button must not arm a reorder drag"
        );
    }
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    assert_eq!(mon.unwrap().workspace_count(), 2);
}

/// **Divergence (approved 2026-07-28).** Dragging a *thumbnail* along the strip reorders
/// the workspaces, macOS Mission Control style. gnome-shell's thumbnails never reorder — a
/// drag there is only ever a window being moved — and that gesture is kept, because the two
/// are told apart by what the press landed on.
#[test]
fn overview_dragging_a_thumbnail_reorders_the_workspaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (win_a, win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let ws_idx_of = |f: &mut Fixture, win: &smithay::desktop::Window| {
        f.synoik()
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(win))
            .map(|(_, idx, _)| idx)
            .expect("the window must be on a workspace")
    };
    assert_eq!(
        (ws_idx_of(&mut f, &win_a), ws_idx_of(&mut f, &win_b)),
        (0, 1)
    );

    // The at-rest row, captured before the press: the drag re-lays it underneath.
    let (t0x, t0y) = thumbnail_center(&mut f, 0);
    let (t1x, _) = thumbnail_center(&mut f, 1);

    // Grab the first thumbnail and carry it just past the second one's center.
    pointer_motion_to(&mut f, t0x, t0y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, t1x + 1., t0y);

    // Mid-drag the row parts: the dragged thumbnail follows the pointer, and the one it
    // passed heads for the slot the drag left — easing, having been overtaken rather than
    // having jumped out of the way.
    let (dest, start) = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let strip = mon.unwrap().thumbnail_strip().unwrap();
        assert_eq!(
            strip.thumbs[0].loc.x + strip.thumbs[0].size.w / 2.,
            t1x + 1.,
            "the dragged thumbnail must hang off the pointer"
        );
        let half = strip.thumbs[1].size.w / 2.;
        (t0x - half, t1x - half)
    };
    let passed = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[1].loc.x
    };
    let crossing = passed(&mut f);
    assert!(
        (crossing - dest).abs() > 1.,
        "the passed thumbnail must ease out of the way, not jump: {crossing} == {dest}",
    );
    assert!(
        crossing <= start + 1. && crossing >= dest - 1.,
        "and it must be heading the right way: {crossing} outside {dest}..={start}",
    );
    f.settle_animations();
    assert!(
        (passed(&mut f) - dest).abs() <= 1.,
        "the passed thumbnail must arrive in the slot the drag left",
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert_eq!(
        (ws_idx_of(&mut f, &win_a), ws_idx_of(&mut f, &win_b)),
        (1, 0),
        "dropping a thumbnail past its neighbour must swap the workspaces"
    );
    assert_eq!(
        f.synoik().layout.workspaces().count(),
        3,
        "reordering must not add or drop a workspace"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "reordering must not leave the overview"
    );
}

/// A dropped thumbnail flies from the box it was let go at into its slot, rather than
/// appearing there — the same rule the dropped *window* preview follows.
#[test]
fn a_dropped_thumbnail_flies_into_its_slot() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (win_a, _win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let (t0x, t0y) = thumbnail_center(&mut f, 0);
    let (t1x, _) = thumbnail_center(&mut f, 1);

    // Carry the first thumbnail past the second, then let go a little short of the
    // slot's own centre so the flight has somewhere to come from.
    pointer_motion_to(&mut f, t0x, t0y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, t1x + 20., t0y);
    f.settle_animations();

    // Where the thumbnail it is carrying sits at the moment of release, and which index
    // its workspace lands at.
    let released = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[0].loc.x
    };
    // No roundtrip before sampling: it advances the clock, and this ease is 200ms long.
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // The carried workspace swapped with its neighbour, so it is the row's second slot now.
    let carried = |f: &mut Fixture| {
        let idx = f
            .synoik()
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(&win_a))
            .map(|(_, idx, _)| idx)
            .expect("the carried workspace must still exist");
        assert_eq!(idx, 1, "the drop must have swapped it past its neighbour");
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[idx].loc.x
    };
    let samples = f.sample_animation(Duration::from_millis(200), 4, carried);
    assert!(
        (samples[0] - released).abs() <= 1.,
        "the drop must start from the box the thumbnail was let go at: {samples:?}",
    );

    f.settle_animations();
    let home = carried(&mut f);
    assert!(
        (home - released).abs() > 1.,
        "the slot must be somewhere other than the release point, or nothing is proven",
    );
    for (i, sample) in samples[1..4].iter().enumerate() {
        assert!(
            (sample - released).abs() > 1. && (sample - home).abs() > 1.,
            "sample {} sits on an endpoint — the thumbnail snapped rather than flew: \
             {samples:?} -> {home}",
            i + 1,
        );
    }
}

/// The row gets out of the way at half overlap, in both directions.
///
/// Comparing the two centres — the obvious rule, and what this did first — costs a whole
/// slot of travel, because the carried centre starts a whole slot from its neighbour's. By
/// the time it fires the carried thumbnail is squarely on top of the neighbour and has been
/// for a while, which reads as the row refusing to move. See `Monitor::thumb_drag_target`.
#[test]
fn the_row_parts_when_the_carried_thumbnail_is_half_over_a_neighbour() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let thumbs = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs.clone()
    };
    let rest = thumbs(&mut f);
    let (w, gap) = (
        rest[0].size.w,
        rest[1].loc.x - rest[0].loc.x - rest[0].size.w,
    );

    // Where the *other* thumbnail is drawn tells us whether the row has parted: it only
    // leaves its own slot once the drag has passed it.
    // Settled, because the row now *eases* out of the way: read on the frame of the
    // crossing it is still sitting at home, and every crossing would look like a refusal.
    let parted = |f: &mut Fixture, other: usize, home: f64| {
        f.settle_animations();
        (thumbs(f)[other].loc.x - home).abs() > 1.
    };

    for (from, other, dir) in [(1usize, 0usize, -1.), (0, 1, 1.)] {
        let (gx, gy) = thumbnail_center(&mut f, from);
        let home = rest[other].loc.x;
        pointer_motion_to(&mut f, gx, gy);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);

        // Just short of half overlap: the carried box covers less than half of its
        // neighbour, and the row has not moved. Half overlap is a half-width of travel
        // plus the gap that was between them; a full slot, `w + gap`, would be the two
        // centres meeting.
        let half = w / 2. + gap;
        pointer_motion_to(&mut f, gx + dir * (half - 4.), gy);
        assert!(
            !parted(&mut f, other, home),
            "carrying {from} by {} must not part the row yet",
            half - 4.,
        );

        // A few pixels further and it has.
        pointer_motion_to(&mut f, gx + dir * (half + 4.), gy);
        assert!(
            parted(&mut f, other, home),
            "carrying {from} by {} must part the row",
            half + 4.,
        );

        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.settle_animations();
        // Put it back for the next direction.
        let (bx, by) = thumbnail_center(&mut f, other);
        pointer_motion_to(&mut f, bx, by);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        pointer_motion_to(&mut f, bx - dir * (w + gap), by);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.settle_animations();
        assert_eq!(thumbs(&mut f)[0].loc.x, rest[0].loc.x, "reset failed");
    }
}

/// The same, carried the other way — right to left, which is the direction that renumbers
/// the row *behind* the carried workspace and so is the one a positional snapshot would
/// skew (`Monitor::thumb_xs` maps by identity for exactly this).
#[test]
fn a_thumbnail_carried_leftwards_flies_into_its_slot() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_win_a, win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let (t0x, t0y) = thumbnail_center(&mut f, 0);
    let (t1x, _) = thumbnail_center(&mut f, 1);

    // B's thumbnail is the second one; carry it left past the first.
    pointer_motion_to(&mut f, t1x, t0y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, t0x - 20., t0y);
    f.settle_animations();

    let released = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[1].loc.x
    };
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let carried = |f: &mut Fixture| {
        let idx = f
            .synoik()
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(&win_b))
            .map(|(_, idx, _)| idx)
            .expect("the carried workspace must still exist");
        assert_eq!(idx, 0, "the drop must have swapped it past its neighbour");
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[idx].loc.x
    };
    let samples = f.sample_animation(Duration::from_millis(200), 4, carried);
    assert!(
        (samples[0] - released).abs() <= 1.,
        "the drop must start from the box the thumbnail was let go at: {samples:?}",
    );

    f.settle_animations();
    let home = carried(&mut f);
    for (i, sample) in samples[1..4].iter().enumerate() {
        assert!(
            (sample - released).abs() > 1. && (sample - home).abs() > 1.,
            "sample {} sits on an endpoint — the thumbnail snapped rather than flew: \
             {samples:?} -> {home}",
            i + 1,
        );
    }
}

/// …and under the movement threshold the same press is still a plain click, which is what
/// makes the reorder gesture free: a thumbnail activates its workspace as it always did.
#[test]
fn overview_a_short_thumbnail_press_still_activates_the_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (win_a, win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let active = |f: &mut Fixture| f.synoik().layout.active_workspace().unwrap().id();
    let ws_of = |f: &mut Fixture, win: &smithay::desktop::Window| {
        f.synoik()
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(win))
            .map(|(_, idx, ws)| (idx, ws.id()))
            .expect("the window must be on a workspace")
    };
    let (idx_a, _) = ws_of(&mut f, &win_a);
    let (idx_b, id_b) = ws_of(&mut f, &win_b);
    assert_ne!(active(&mut f), id_b, "B's desktop must not start active");

    // Press, twitch by less than the threshold, release.
    let (t1x, t1y) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, t1x, t1y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(3., 0.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        active(&mut f),
        id_b,
        "a click on a thumbnail must switch to its workspace"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "…and stay in the overview, as clicking a non-active workspace does"
    );
    assert_eq!(
        (ws_of(&mut f, &win_a).0, ws_of(&mut f, &win_b).0),
        (idx_a, idx_b),
        "a click must not reorder anything"
    );
}

/// **Divergence** (`docs/fork/dynamic-workspaces-divergence.md`): the strip is shown at
/// every workspace count. gnome-shell's `ThumbnailsBox._updateShouldShow`
/// (`workspaceThumbnail.js:697-706`) hides it at or below `NUM_WORKSPACES_THRESHOLD`, so
/// with dynamic workspaces it only appears once a second desktop is populated. The strip
/// is the desktop switcher; one that comes and goes is not one you can aim at.
#[test]
fn thumbnail_strip_is_shown_at_every_workspace_count() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let visible = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnails_visible()
    };
    let count = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().workspace_count()
    };

    // A bare session: MIN_NUM_WORKSPACES empties, and the strip already showing.
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert_eq!(count(&mut f), 2, "a fresh monitor shows two desktops");
    assert!(visible(&mut f), "an empty session must show the strip");
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        visible(&mut f),
        "one populated desktop must show the strip too"
    );
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    assert!(visible(&mut f), "…and so must two");
}

/// Clicking a strip thumbnail follows gnome-shell's
/// WorkspaceThumbnail.activate: a non-active workspace switches and stays in
/// the overview; the active one leaves the overview.
#[test]
fn thumbnail_click_switches_workspace_and_stays() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();

    // Click the second desktop's thumbnail: switch, stay in the overview.
    let (x, y) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, x, y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "clicking a non-active thumbnail must stay in the overview"
    );
    let active = f.synoik().layout.active_workspace().unwrap().id();
    assert_ne!(active, ws1_id, "clicking a thumbnail must switch to it");

    // Click it again (now active): leave the overview. The pointer has to be re-aimed —
    // the row keeps the active workspace on the band's center, so switching to a
    // thumbnail slides it out from under wherever it was clicked.
    let (x, y) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, x, y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "clicking the active thumbnail must leave the overview"
    );
    assert_eq!(f.synoik().layout.active_workspace().unwrap().id(), active);
}

/// Dropping a window preview on a strip thumbnail moves the window to that
/// workspace, keeping its desktop position (gnome-shell's
/// WorkspaceThumbnail.acceptDrop → moveWindowToMonitorAndWorkspace).
#[test]
fn thumbnail_drop_moves_window_keeping_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (win_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();
    let original_pos = focused_window_pos(&mut f);

    // Drag A's preview onto the second desktop's thumbnail.
    let rect = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    let (tx, ty) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    pointer_motion_to(&mut f, tx, ty);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.synoik().layout.is_overview_open(),
        "dropping on a thumbnail must not leave the overview"
    );

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win_a))
        .unwrap();
    assert_ne!(
        ws.id(),
        ws1_id,
        "dropping on a thumbnail must move the window to that workspace"
    );
    let (_, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| tile.window().window == win_a)
        .unwrap();
    assert_pos_eq(
        (pos.x, pos.y),
        original_pos,
        "the window must keep its desktop position on the thumbnail's workspace",
    );
}

/// Dropping a window preview into the gap between two thumbnails inserts a
/// new workspace there and moves the window onto it (gnome-shell's
/// drop-placeholder path: Main.wm.insertWorkspace).
#[test]
fn thumbnail_gap_drop_inserts_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (win_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    let workspace_count = |f: &mut Fixture| f.synoik().layout.workspaces().count();
    assert_eq!(workspace_count(&mut f), 3);

    // Drag A's preview into the gap between the first two thumbnails.
    let rect = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    let (t1x, t1y) = thumbnail_center(&mut f, 1);
    let (t0x, _) = thumbnail_center(&mut f, 0);
    let gap = ((t0x + t1x) / 2., t1y);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    pointer_motion_to(&mut f, gap.0, gap.1);

    // While hovering the gap, the strip makes room for the drop placeholder
    // (gnome-shell's placeholder affordance). The insert hint updates on
    // render; drive that like a frame would.
    f.synoik().layout.update_render_elements(None);
    {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let strip = mon.unwrap().thumbnail_strip().unwrap();
        assert!(
            strip.placeholder.is_some(),
            "hovering a thumbnail gap must show the drop placeholder"
        );
    }

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    assert_eq!(
        workspace_count(&mut f),
        4,
        "dropping into a thumbnail gap must insert a workspace"
    );
    let synoik = f.synoik();
    let (_, ws_idx, _) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win_a))
        .unwrap();
    assert_eq!(
        ws_idx, 1,
        "the window must land on the workspace inserted at the gap"
    );
}

/// An edge-tiled window's preview drag never touches the window: no
/// configure in flight, and it is still edge-tiled on the drop workspace
/// (like the maximized case below).
#[test]
fn overview_drag_of_edge_tiled_window_stays_tiled() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();

    // Tile left and ack the half-width configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(960, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    let _ = f.client(id).window(&surface).recent_configures();

    // Drag the preview onto the trailing workspace's peeking edge. The real
    // window is never touched (gnome-shell drags the preview), so the client
    // must not see any untile/resize along the way.
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.double_roundtrip(id);
    pointer_motion_to(&mut f, 1800., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    for configure in f.client(id).window(&surface).recent_configures() {
        assert_eq!(
            configure.size,
            (960, 1080),
            "an overview drag must never resize the tiled window, got: {configure}"
        );
    }

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win))
        .unwrap();
    assert_ne!(
        ws.id(),
        ws1_id,
        "the drop must move the window to the neighbor workspace"
    );
    let mapped = ws.windows().next().unwrap();
    assert_eq!(
        crate::layout::LayoutElement::edge_tiled_side(mapped),
        Some(crate::gnome::TileSide::Left),
        "the window must still be edge-tiled after the overview drag"
    );
}

/// A maximized window's preview picks up immediately — mutter's 48px
/// shake-loose is for dragging the real window, not the picker — and the
/// window stays maximized the whole way: gnome-shell drags the preview, so
/// the client never sees an unmaximize/resize.
#[test]
fn overview_drag_of_maximized_window_picks_up_and_stays_maximized() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();
    let ws1_id = f.synoik().layout.active_workspace().unwrap().id();

    // Maximize and ack the full-size configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(1920, 1048);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();
    let _ = f.client(id).window(&surface).recent_configures();

    // A 20px drag is well under the 48px shake threshold, yet the preview
    // must already be moving (picked out of its workspace).
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 20.);
    assert!(
        f.synoik()
            .layout
            .workspaces()
            .all(|(_, _, ws)| !ws.has_window(&win)),
        "a preview pick-up must not need mutter's shake-loose threshold"
    );

    // Drop it on the neighbor workspace peeking at the right edge.
    f.pointer_motion(1800. - grab.0, 540. - grab.1 - 20.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    for configure in f.client(id).window(&surface).recent_configures() {
        assert_eq!(
            configure.size,
            (1920, 1048),
            "an overview drag must never resize the maximized window, got: {configure}"
        );
        assert!(
            configure.states.contains(&xdg_toplevel::State::Maximized),
            "the window must stay maximized through the drag, got: {configure}"
        );
    }

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win))
        .unwrap();
    assert_ne!(
        ws.id(),
        ws1_id,
        "the drop must move the window to the neighbor workspace"
    );
    let mapped = ws.windows().next().unwrap();
    assert!(
        crate::layout::LayoutElement::pending_sizing_mode(mapped).is_maximized(),
        "the window must still be maximized after the overview drag"
    );
}

/// The row and the phantom slot it is holding open, as drawn.
fn strip_now(f: &mut Fixture) -> (Vec<f64>, Option<(f64, f64, f64)>) {
    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let strip = mon
        .expect("workspaces must be on a monitor")
        .thumbnail_strip()
        .expect("the thumbnails strip must be visible");
    let xs = strip.thumbs.iter().map(|r| r.loc.x).collect();
    let phantom = strip
        .phantom
        .map(|(rect, ph)| (rect.loc.x, rect.size.w, ph.reveal));
    (xs, phantom)
}

/// Dragging a window towards the trailing workspace opens the row for the workspace the
/// drop would append, in proportion to how close the drag has come.
///
/// **Divergence (approved 2026-08-11).** gnome-shell moves nothing during a drag — it
/// shows a fixed-width `.placeholder` and only expands after the drop
/// (`workspaceThumbnail.js:1352-1390`, `_updateStates` at `:1144-1181`). See
/// `docs/fork/dynamic-workspaces-divergence.md`.
#[test]
fn dragging_toward_the_last_workspace_opens_the_row_for_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    let win_a = f.synoik().layout.focus().unwrap().window.clone();
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // One occupied workspace and the trailing empty one.
    let (at_rest, phantom) = strip_now(&mut f);
    assert_eq!(at_rest.len(), 2);
    assert_eq!(phantom, None, "nothing is dragging yet");
    let last = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs[1]
    };
    let gap = at_rest[1] - at_rest[0] - last.size.w;

    // Pick the preview up, well away from the row.
    let rect = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Two motions: the first begins the move, the second promotes it to `Moving`.
    f.pointer_motion(0., 10.);
    f.pointer_motion(0., 90.);

    // Approach the trailing thumbnail. The slot opens monotonically, and the run grows
    // with it.
    let target = (last.loc.x + last.size.w / 2., last.loc.y + last.size.h / 2.);
    // A sequence of positions closing on the thumbnail from below, each a step further in.
    let mut widths = Vec::new();
    for dy in [300., 220., 150., 90., 0.] {
        pointer_motion_to(&mut f, target.0, target.1 + dy);
        let (_, phantom) = strip_now(&mut f);
        let (_, w, reveal) = phantom.expect("approaching must arm the slot");
        widths.push((dy, w, reveal));
    }
    for pair in widths.windows(2) {
        assert!(
            pair[1].1 >= pair[0].1,
            "the slot must not shrink as the drag closes in: {widths:?}",
        );
    }
    assert!(
        widths[0].1 > 0. && widths[0].1 < last.size.w,
        "the slot must start partly open, not jump: got {widths:?}",
    );
    assert_eq!(
        widths.last().unwrap().2,
        1.,
        "on the trailing thumbnail the slot must be all the way open, got {widths:?}",
    );

    // The slot grew off the right end and the row itself never budged — a still target
    // to aim at for the whole drag.
    let (before, phantom) = strip_now(&mut f);
    let (phantom_x, phantom_w, _) = phantom.unwrap();
    assert_eq!(
        before, at_rest,
        "the row must hold still while the drag runs"
    );
    assert_eq!(
        (phantom_x, phantom_w),
        (last.loc.x + last.size.w + gap, last.size.w),
        "the slot must stand one gap past the last thumbnail, a full workspace wide",
    );

    // No roundtrip before sampling: it advances the clock, and this ease is 200ms long.
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // The new thumbnail takes over the slot exactly as it stood, and *then* the row
    // eases into its recentred shape.
    let samples = f.sample_animation(Duration::from_millis(200), 4, |f| strip_now(f).0);
    assert_eq!(
        samples[0],
        vec![before[0], before[1], phantom_x],
        "the row must start the ease from where the drag left it, new thumbnail included",
    );

    f.settle_animations();
    let (settled, phantom) = strip_now(&mut f);
    assert_eq!(phantom, None, "the drop retires the slot");
    assert_eq!(settled.len(), 3, "the drop must append a workspace");

    // It lands centred, half a slot to the left of where it stood...
    let shift = samples[0][0] - settled[0];
    assert!(
        (shift - (last.size.w + gap) / 2.).abs() <= 1.,
        "the row must recentre by half a slot: {:?} -> {settled:?}",
        samples[0],
    );
    for pair in settled.windows(2) {
        assert!(
            (pair[1] - pair[0] - (last.size.w + gap)).abs() <= 1.,
            "the settled row must be evenly spaced: {settled:?}",
        );
    }
    // ...having travelled, rather than snapped, to get there.
    for (i, sample) in samples[1..4].iter().enumerate() {
        assert!(
            *sample != samples[0] && *sample != settled,
            "sample {} sits on an endpoint — the row snapped rather than eased: {samples:?}",
            i + 1,
        );
    }
}

/// A drag aimed into an *interior* gap gets the pill, not the trailing slot.
///
/// Proximity alone would open both: a gap near the end of the row is well within a
/// thumbnail's width of the trailing one, and the phantom is armed on distance. But the
/// drop is going to insert *there*, so the row would be pointing at a workspace that
/// never arrives — and the pill, which is the thing that marks the real insert point,
/// would be gone, because the two are alternatives in one slot.
///
/// Only the phantom's half is asserted here: the pill is armed off `insert_hint`, which
/// the render path fills in, and `Fixture::refresh` deliberately does not draw.
#[test]
fn a_drag_into_an_interior_gap_keeps_the_pill() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik_state()
        .do_action(Action::MoveWindowToWorkspaceDown(true), false);
    f.synoik_complete_animations();
    let _b = map_window_sized(&mut f, id, (640, 480), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();
    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    // Two occupied workspaces and the trailing empty one.
    let thumbs = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnail_strip().unwrap().thumbs.clone()
    };
    assert_eq!(thumbs.len(), 3);

    // Pick the preview up, well away from the row.
    let rect = f.synoik().layout.expose_target_rect(&win_b).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(0., 90.);

    // Into the gap between the last two thumbnails — an interior insert, and close
    // enough to the trailing thumbnail that distance alone would open the end slot.
    let gap_x = (thumbs[1].loc.x + thumbs[1].size.w + thumbs[2].loc.x) / 2.;
    pointer_motion_to(&mut f, gap_x, thumbs[2].loc.y + thumbs[2].size.h / 2.);

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let strip = mon.unwrap().thumbnail_strip().unwrap();
    assert_eq!(
        strip.drop_target(smithay::utils::Point::from((
            gap_x,
            thumbs[2].loc.y + thumbs[2].size.h / 2.,
        ))),
        Some(crate::layout::thumbnails::DropTarget::NewAt(2)),
        "the fixture must actually be aiming at an interior gap",
    );
    assert_eq!(
        strip.phantom, None,
        "the end slot must not open for a drop that is not going there — and it shares \
         its slot with the pill, so an open one would erase the mark as well",
    );
}

/// While a preview drag is in flight, the source desktop's picker layout is
/// frozen (gnome-shell's layout_frozen): the remaining previews hold their
/// slots — the dragged window leaves a gap — until the drop.
#[test]
fn overview_drag_freezes_the_other_previews() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    let win_a = f.synoik().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(&mut f, id, (640, 480), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    let slot_a = f.synoik().layout.expose_target_rect(&win_a).unwrap();

    // Pick up B's preview and move it away from its slot.
    let rect = f.synoik().layout.expose_target_rect(&win_b).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Two motions, not one: the first begins the move (`Starting`, zero delta)
    // and the second is the first update, which in the overview promotes to
    // `Moving` and takes the freeze.
    f.pointer_motion(0., 10.);
    f.pointer_motion(0., 90.);

    assert_eq!(
        f.synoik().layout.expose_target_rect(&win_a),
        Some(slot_a),
        "the other previews must hold their slots while the drag is in flight"
    );

    // Drop B on the trailing workspace: A, now alone, re-layouts.
    pointer_motion_to(&mut f, 1800., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_ne!(
        f.synoik().layout.expose_target_rect(&win_a),
        Some(slot_a),
        "the drop must let the source desktop's picker layout recompute"
    );
}

/// Picking a preview up must not re-flow the picker. The tile is still in the
/// workspace while the move is `Starting`, so any offset on it feeds
/// `compute_slots`, whose row assignment sorts by `center().y` — a large enough
/// first motion re-orders the slots, and the freeze then holds the shuffle for
/// the whole drag. gnome-shell's `WindowPreview` drag never moves the window in
/// the workspace layout at all, so nothing there can perturb the layout.
#[test]
fn overview_drag_does_not_reflow_the_picker_on_pickup() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Same-size windows cascade 50px apart (window-placement.md §4), so their
    // centers sit 50px apart in y — near enough that a rubberbanded pickup can
    // sort one past another.
    let _a = map_window_sized(&mut f, id, (700, 500), None);
    let win_a = f.synoik().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(&mut f, id, (700, 500), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();
    let _c = map_window_sized(&mut f, id, (700, 500), None);
    let win_c = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.synoik_complete_animations();

    let slot_a = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let slot_b = f.synoik().layout.expose_target_rect(&win_b).unwrap();

    // Grab the bottom-most preview and yank it up past the row above it. One
    // motion, and a big one: in the overview the drag promotes on the *first*
    // update, so only that first delta ever perturbs anything — and the
    // rubberband damps small deltas to sub-pixel, which is why this went
    // unnoticed. Reverting the fix moves B from beside A down into C's
    // vacated row.
    let rect = f.synoik().layout.expose_target_rect(&win_c).unwrap();
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., -400.);

    assert_eq!(
        (
            f.synoik().layout.expose_target_rect(&win_a),
            f.synoik().layout.expose_target_rect(&win_b),
        ),
        (Some(slot_a), Some(slot_b)),
        "picking up a preview must not re-flow the remaining ones"
    );
}

/// Dragging a preview against a screen edge snaps one desktop at a time:
/// the switch happens right away (after the anti-flicker delay), then a
/// grace period has to pass before the next snap while the pointer stays on
/// the edge. No GNOME counterpart — the behavior is by design (continuous
/// panning would make aiming at a desktop impossible).
#[test]
fn overview_drag_edge_scroll_snaps_one_desktop_at_a_time() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Three populated desktops (plus the trailing empty) so there are two
    // snaps to make from the first.
    let (win_a, _win_b) = setup_two_desktops_in_overview(&mut f, id);
    {
        tap(&mut f, KEY_LEFTMETA);
        f.synoik_complete_animations();
        let _c = map_window_sized(&mut f, id, (500, 400), None);
        let win_c = f.synoik().layout.focus().unwrap().window.clone();
        tap(&mut f, KEY_LEFTMETA);
        f.synoik_complete_animations();
        let rect = f.synoik().layout.expose_target_rect(&win_c).unwrap();
        let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
        pointer_motion_to(&mut f, grab.0, grab.1);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_motion(0., 10.);
        let (tx, ty) = thumbnail_center(&mut f, 2);
        pointer_motion_to(&mut f, tx, ty);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.synoik_complete_animations();
        f.double_roundtrip(id);
    }
    assert!(f.synoik().layout.is_overview_open());

    let active_idx = |f: &mut Fixture| {
        let active = f.synoik().layout.active_workspace().unwrap().id();
        f.synoik()
            .layout
            .workspaces()
            .position(|(_, _, ws)| ws.id() == active)
            .unwrap()
    };
    assert_eq!(active_idx(&mut f), 0, "the drags must not have switched");

    // Pick up A's preview and hold it against the right screen edge,
    // driving the DnD scroll by hand with a pinned clock.
    let rect = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    assert!(
        f.synoik()
            .layout
            .workspaces()
            .all(|(_, _, ws)| !ws.has_window(&win_a)),
        "the preview must be picked up before it reaches the edge"
    );
    pointer_motion_to(&mut f, 1919., 540.);

    let base = f.synoik().clock.now_unadjusted() + Duration::from_millis(200);
    let at = |f: &mut Fixture, offset_ms: u64| {
        let mut clock = f.synoik().clock.clone();
        clock.set_unadjusted(base + Duration::from_millis(offset_ms));
        f.synoik().layout.advance_animations();
    };

    // The first frame on the edge arms the anti-flicker delay (100ms); the
    // next one past it snaps — once, no matter how many frames pass within
    // the grace period.
    at(&mut f, 0);
    assert_eq!(active_idx(&mut f), 0, "the anti-flicker delay must hold");
    at(&mut f, 150);
    assert_eq!(active_idx(&mut f), 1, "entering the edge must snap once");
    at(&mut f, 300);
    at(&mut f, 700);
    assert_eq!(
        active_idx(&mut f),
        1,
        "the next snap must wait out the grace period"
    );

    // Past the 750ms grace: the second snap.
    at(&mut f, 950);
    assert_eq!(
        active_idx(&mut f),
        2,
        "staying on the edge must snap again after the grace period"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "the edge snaps must not leave the overview"
    );
}

/// The GNOME top panel reserves a strut at the top of the work area (like
/// gnome-shell's `set_builtin_struts`): a maximized window fills the output
/// below the 32px panel band, never underneath it.
#[test]
fn panel_reserves_top_strut() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);

    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1920 × 1048"),
        "a maximized window must fill the work area below the panel, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1048);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 32.),
        "the maximized window must start below the panel strut",
    );
}

/// The panel clock formats local wall time as `HH:MM` and advances with it.
#[test]
fn panel_clock_is_hh_mm() {
    let mut panel = crate::ui::panel::Panel::new(
        crate::animation::Clock::default(),
        std::rc::Rc::new(std::cell::RefCell::new(synoik_config::Config::default())),
    );

    // Epoch 0 and one hour later differ by exactly one hour in any timezone.
    panel.update_clock_at(0);
    let at_epoch = panel.clock_text().to_string();
    assert_eq!(at_epoch.len(), 5, "clock must be HH:MM, got {at_epoch:?}");
    assert_eq!(
        at_epoch.as_bytes()[2],
        b':',
        "clock must be HH:MM, got {at_epoch:?}"
    );

    assert!(
        panel.update_clock_at(3600),
        "an hour later the clock text must change"
    );
    assert_ne!(
        at_epoch,
        panel.clock_text(),
        "an hour later must show a different time"
    );
}

/// The clock label sits 24px inside its panel button, not the 16px a status icon gets.
///
/// GNOME gives every `.panel-button` `-natural-hpadding: $base_padding * 2` = 12
/// (`_panel.scss:28`); what differs is what the child adds. A `.system-status-icon` adds
/// `margin: 0 $base_margin` = 4 (`:34`), but `.clock` adds `padding-left/right:
/// $scaled_padding * 2` = 12 (`:161-164`). Measured on a live 50.3 shell at the default
/// font: the button spans 1183..1377 and the clock's text starts at 1207 — 24 in.
///
/// We had 16 for both, from reading the inset as "pill margin + breathing room". That
/// coincidentally matches the icons (4 + 12) and is 8px short for the clock.
///
/// Asserted as a *difference* so it does not depend on which face the test machine
/// resolves for the UI sans — the shaped text width cancels out.
#[test]
fn panel_clock_label_sits_24px_inside_its_button() {
    let mut panel = crate::ui::panel::Panel::new(
        crate::animation::Clock::default(),
        std::rc::Rc::new(std::cell::RefCell::new(synoik_config::Config::default())),
    );
    panel.update_clock_at(0);

    let rect = panel.date_menu_rect(2560.);
    let text_w = synoik_vk::text::measure_line_width_weighted(
        panel.clock_text(),
        crate::ui::pt_to_px(11.) as f32,
        true,
    );
    let pad = (rect.size.w - text_w) / 2.;
    assert!(
        (pad - 24.).abs() < 0.001,
        "clock label must sit 24px inside the button (12 -natural-hpadding + 12 .clock \
         padding), got {pad}"
    );
}

/// Clicking the panel's Activities button toggles the overview (the mouse
/// counterpart of the Super-tap), and the button's checked highlight tracks
/// the overview state.
#[test]
fn panel_activities_click_toggles_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(!f.synoik().layout.is_overview_open());
    f.refresh();
    assert!(
        !f.synoik().panel.activities_checked(),
        "Activities starts unchecked"
    );

    // Click within the Activities button at the top-left of the panel.
    f.pointer_motion(10., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "clicking Activities must open the overview"
    );
    f.refresh();
    assert!(
        f.synoik().panel.activities_checked(),
        "Activities must be checked while the overview is open"
    );

    // A second click toggles it back.
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "clicking Activities again must close the overview"
    );
    f.refresh();
    assert!(
        !f.synoik().panel.activities_checked(),
        "Activities must be unchecked once the overview closes"
    );
}

/// Scrolling over the workspace indicator switches workspaces (gnome-shell's
/// `handleWorkspaceScroll`): a wheel notch down goes to the next workspace.
#[test]
fn panel_scroll_over_indicator_switches_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Mapping a window gives the monitor an occupied workspace plus the trailing
    // empty one, so there is a workspace 2 to scroll to.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0
    );

    // Park the pointer over the indicator (top-left of the panel) and scroll down.
    pointer_motion_to(&mut f, 10., 10.);
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "a wheel notch over the indicator must switch to the next workspace"
    );

    // A scroll far from the indicator (mid-screen) must NOT switch workspaces.
    pointer_motion_to(&mut f, 960., 540.);
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "a scroll away from the indicator must not switch workspaces"
    );
}

/// Clicking the dateMenu (clock) opens the calendar popover; Escape and an
/// outside click both dismiss it (gnome-shell's popup-menu grab).
#[test]
fn panel_date_menu_click_opens_and_dismisses_calendar() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert!(!f.synoik().panel_popover.is_open());

    // Click the clock, wherever the panel put it.
    let open = |f: &mut Fixture| {
        let x = clock_center_x(f, 1920.);
        pointer_motion_to(f, x, 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    open(&mut f);
    assert!(
        f.synoik().panel_popover.is_open(),
        "clicking the clock must open the calendar popover"
    );

    // Escape closes it (the modal keyboard grab). The close is animated (fade-out),
    // so settle the animation before asserting it's gone.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "Escape must close the popover"
    );

    // Reopen, then a click well outside the popover dismisses it.
    open(&mut f);
    assert!(f.synoik().panel_popover.is_open());
    pointer_motion_to(&mut f, 10., 700.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "a click outside the popover must dismiss it"
    );
}

/// **Divergence — the clock lives in the top-RIGHT corner**, not GNOME's centre box
/// (`js/ui/panel.js` `_centerBox`, `sessionMode.js:98-99`). Driven through real pointer
/// input, because the whole point is where a click lands:
///
/// - the panel's right corner opens the calendar (GNOME would find nothing there);
/// - the centre of the panel opens nothing (GNOME's clock is exactly there);
/// - the status indicators are still *left* of the clock, and clicking them still opens quick
///   settings — the clock moved past the cluster, it did not displace it.
///
/// The popover follows its button: it is centred on the clock and clamped a
/// `POPOVER_MARGIN` in from the screen edge, so it hugs the right side.
#[test]
fn panel_clock_sits_right_of_the_status_indicators() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().update_render_elements(None);

    let clock = f.synoik().panel.date_menu_rect(1920.);
    let qs = f.synoik().panel.quick_settings_rect(1920.);
    assert_eq!(
        clock.loc.x + clock.size.w,
        1920.,
        "the clock button must own the output's right corner"
    );
    assert!(
        qs.loc.x + qs.size.w <= clock.loc.x,
        "the quick-settings cluster must sit left of the clock ({qs:?} vs {clock:?})"
    );

    let click = |f: &mut Fixture, x: f64| {
        pointer_motion_to(f, x, 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    // The right corner is the clock.
    click(&mut f, 1919.);
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_DATE_MENU),
        "the panel's right corner must open the calendar"
    );

    // The calendar hugs the right edge — where GNOME's would sit mid-screen. Centring it
    // on a clock this far right would overflow the output, so the clamp is what decides
    // the position, and the assertion is on the *edge* rather than on the origin: an
    // unclamped popover would land right of this and hang off the screen.
    //
    // Asserted against the clock's lit pill rather than a screen-edge constant, because
    // lining those two up is the actual intent (`PANEL_EDGE_INSET`) — a menu that stopped
    // 2px short of its own button read as a misalignment, and would again if the clamp
    // ever drifted back onto `POPOVER_MARGIN`.
    let output = f.synoik_output(1);
    let origin = f.synoik().panel_popover.location(&output);
    let size = f.synoik().panel_popover.content_size().expect("open");
    assert!(
        origin.x > 960.,
        "the calendar must follow the clock to the right half, got x={}",
        origin.x
    );
    let pill_right = clock.loc.x + clock.size.w - crate::ui::panel::BTN_MARGIN_X;
    assert_eq!(
        origin.x + size.w,
        pill_right,
        "the calendar's right edge must line up with the clock pill's"
    );

    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();

    // The centre of the panel — GNOME's clock — is now bare bar.
    click(&mut f, 960.);
    assert!(
        !f.synoik().panel_popover.is_open(),
        "nothing lives in the centre box any more"
    );

    // The cluster the clock moved past still works.
    let x = qs_center_x(&mut f, 1920.);
    click(&mut f, x);
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        "the status cluster must still open quick settings from its new place"
    );
}

/// Panel menus work inside the overview: a popover opened while the overview is
/// up pushes its own grab on top of the overview's modal and stays open
/// (`js/ui/popupMenu.js:1520`) — it must not be dismissed on the next frame.
#[test]
fn panel_popover_stays_open_in_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.refresh();
    assert!(f.synoik().layout.is_overview_open());

    // Click the clock: the calendar popover opens.
    let x = clock_center_x(&mut f, 1920.);
    pointer_motion_to(&mut f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.synoik().panel_popover.is_open(),
        "clicking the clock in the overview must open the calendar popover"
    );

    // Subsequent cycles must not dismiss it (a level-triggered overview check
    // once closed the popover on the very next reconcile after it opened).
    f.refresh();
    f.refresh();
    f.settle_animations();
    assert!(
        f.synoik().panel_popover.is_open(),
        "a popover opened in the overview must stay open across cycles"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "the overview stays open under the popover"
    );
}

/// The Activities highlight tracks the overview **without a render**.
///
/// It used to be armed inside `update_render_elements`, so the frame that opened the overview
/// drew the button unlit and the highlight only latched on the next advance+render — one frame
/// late on the seat, and invisible to any test that did not render first. The sync belongs where
/// the state changes (`State::refresh`), which is what this pins: not one render happens here.
#[test]
fn panel_activities_highlight_needs_no_render() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.refresh();
    assert!(
        !f.synoik().panel.activities_checked(),
        "Activities starts unchecked"
    );

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.refresh();
    assert!(
        f.synoik().panel.activities_checked(),
        "one refresh after the overview opens, Activities must already be lit — no render",
    );

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.refresh();
    assert!(
        !f.synoik().panel.activities_checked(),
        "and unlit again one refresh after it closes",
    );
}

/// A popover that is open when the overview *opens* is dismissed: GNOME's
/// overview modal does not coexist with a held menu grab
/// (`js/ui/overview.js:461` hides rather than fight an existing grab).
#[test]
fn overview_open_dismisses_open_panel_popover() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.refresh();

    let x = clock_center_x(&mut f, 1920.);
    pointer_motion_to(&mut f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.synoik().panel_popover.is_open());

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.refresh();
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "opening the overview must dismiss an already-open popover"
    );
    assert!(f.synoik().layout.is_overview_open());
}

/// Clicking the right-box quick-settings indicator opens its popover; Escape and
/// an outside click both dismiss it (the same popup-menu grab as the calendar).
/// Clicking a tile inside flips its gsettings-backed state.
#[test]
fn panel_quick_settings_click_opens_toggles_and_dismisses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert!(!f.synoik().panel_popover.is_open());

    // The indicator sits at the right end of the status cluster, just left of the clock;
    // with the default toggles it's a single anchor icon. Click it.
    let open = |f: &mut Fixture| {
        let x = qs_center_x(f, 1920.);
        pointer_motion_to(f, x, 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    open(&mut f);
    assert!(
        f.synoik().panel_popover.is_open(),
        "clicking the quick-settings indicator must open its popover"
    );

    // A click on the Do Not Disturb tile flips the local state and keeps the menu
    // open. The grid is [Network, Dark Style, Do Not Disturb, Night Light] row-major over
    // two columns, so DND is the bottom-left tile (row 1, col 0).
    let origin = popover_origin(&mut f);
    // DND tile center (row 1, col 0), menu-local: x = PAD + TILE_W/2; y = PAD + SYS_H
    // + TILE_GAP + (TILE_H + TILE_GAP) [second row] + TILE_H/2.
    let tile_x = origin.x + 12. + 75.;
    let tile_y = origin.y + (12. + 44. + 8.) + (56. + 8.) + 28.;
    pointer_motion_to(&mut f, tile_x, tile_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.synoik().panel_popover.is_open(),
        "a tile click must not close the quick-settings menu"
    );
    assert!(
        f.synoik().gnome_settings.quick_toggles.do_not_disturb,
        "clicking the Do Not Disturb tile must flip its state on"
    );

    // Escape closes it (animated fade-out; settle before asserting it's gone).
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "Escape must close the quick-settings popover"
    );

    // Reopen, then a click well outside dismisses it.
    open(&mut f);
    assert!(f.synoik().panel_popover.is_open());
    pointer_motion_to(&mut f, 960., 700.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "a click outside the quick-settings popover must dismiss it"
    );
}

/// Flipping DND from the quick-settings tile toggles the panel messages dot
/// WITHOUT a new notification (`js/ui/dateMenu.js:757-761,796-797`): the dot is
/// gated on `show-banners`, and the QS-toggle path must recompute it (there is
/// no gsettings writer headless, so the settings round-trip can't cover it).
#[test]
fn messages_indicator_toggles_with_dnd_tile() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    // An unseen LOW notification lights the dot (it never banners).
    let mut low = banner_req("app", ":1.1");
    low.urgency = crate::notifications::Urgency::Low;
    banner_notify(&mut f, low);
    assert!(f.synoik().panel.messages_indicator_visible());

    // The DND tile center, computed the way `panel_quick_settings_*` does — and
    // re-derived per click, because toggling DND toggles the messages dot, which moves
    // the quick-settings indicator (and so the menu the next open hangs off it).
    let open_qs = |f: &mut Fixture| {
        let x = qs_center_x(f, 1920.);
        pointer_motion_to(f, x, 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let click_dnd = |f: &mut Fixture| {
        let origin = popover_origin(f);
        pointer_motion_to(
            f,
            origin.x + 12. + 75.,
            origin.y + (12. + 44. + 8.) + (56. + 8.) + 28.,
        );
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    // Enable DND → the dot clears even though the notification is still unseen.
    open_qs(&mut f);
    click_dnd(&mut f);
    assert!(f.synoik().gnome_settings.quick_toggles.do_not_disturb);
    assert!(
        !f.synoik().panel.messages_indicator_visible(),
        "DND hides the dot with no new notification"
    );

    // Disable DND again → the dot re-lights (the notification is still unseen).
    click_dnd(&mut f);
    assert!(!f.synoik().gnome_settings.quick_toggles.do_not_disturb);
    assert!(
        f.synoik().panel.messages_indicator_visible(),
        "clearing DND re-lights the dot"
    );
}

/// The brightness card's rows drive the shell's scale algebra, not the hardware directly: moving
/// ONE monitor re-derives every scale factor from the new maximum and pulls the global scale (the
/// quick-settings slider) to it, while moving the GLOBAL scale fans back out through those factors
/// (`js/misc/brightnessManager.js:203-240`).
#[test]
fn brightness_card_rows_drive_the_scale_algebra() {
    use crate::backlight::{BacklightRange, BacklightSnapshot, OutputBacklight};
    use crate::ui::popover::PopoverAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Two backlit monitors, the external one at half the panel's brightness. A 0..100 range makes
    // a raw value read as a percentage of the usable span.
    let backlight = |connector: &str, name: &str, brightness| OutputBacklight {
        connector: connector.to_owned(),
        display_name: name.to_owned(),
        range: BacklightRange { min: 0, max: 100 },
        brightness,
    };
    let snapshot = BacklightSnapshot {
        outputs: vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24\u{2033}", 50),
        ],
    };
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik().backlight = snapshot;

    // The first sync adopts the hardware, so the global slider sits at the maximum.
    assert_eq!(f.synoik().brightness.global_scale().unwrap().value(), 1.0);
    assert_eq!(f.synoik().brightness.scales()[1].value(), 0.5);

    // A card row: pushing the external monitor to full makes IT the maximum, so the global scale
    // follows it and the two are now in step.
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetMonitorBrightness("DP-2".into(), 1.0));
    assert_eq!(f.synoik().brightness.global_scale().unwrap().value(), 1.0);
    assert_eq!(f.synoik().brightness.scales()[0].value(), 1.0);
    assert_eq!(f.synoik().brightness.scales()[1].value(), 1.0);

    // The top-level slider now moves both together, through the re-derived factors.
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetBrightness(0.4));
    assert_eq!(f.synoik().brightness.scales()[0].value(), 0.4);
    assert_eq!(f.synoik().brightness.scales()[1].value(), 0.4);

    // An unknown connector is a no-op, not a panic.
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetMonitorBrightness("HDMI-A-1".into(), 0.9));
    assert_eq!(f.synoik().brightness.scales()[0].value(), 0.4);
}

/// The brightness keys (`org.gnome.shell.keybindings screen-brightness-*`) step the shell's
/// scales: the plain ones move the global scale (and so every monitor, through its factor), the
/// `-monitor` ones only the monitor under the pointer -- gnome-shell's
/// `get_current_logical_monitor()` (`js/misc/brightnessManager.js:107-132`).
#[test]
fn brightness_keys_step_the_scales() {
    use crate::brightness::Step;

    // The scale arithmetic is in twentieths, so compare with a tolerance rather than exactly.
    fn close(value: f64, expected: f64) -> bool {
        (value - expected).abs() < 1e-9
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    let backlight = |connector: &str, name: &str, brightness| crate::backlight::OutputBacklight {
        connector: connector.to_owned(),
        display_name: name.to_owned(),
        range: crate::backlight::BacklightRange { min: 0, max: 100 },
        brightness,
    };
    // Both outputs backlit and equally bright, so the factors are 1:1 and a global step moves
    // both by the same amount.
    let snapshot = crate::backlight::BacklightSnapshot {
        outputs: vec![
            backlight("headless-1", "Built-in display", 100),
            backlight("headless-2", "Dell 24\u{2033}", 100),
        ],
    };
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik().backlight = snapshot;

    // A plain brightness-down key: one step of 1/20 off the global scale, fanned out to both.
    f.synoik_state().step_brightness(Step::Down, false);
    assert!(close(
        f.synoik().brightness.global_scale().unwrap().value(),
        0.95
    ));
    assert!(close(f.synoik().brightness.scales()[0].value(), 0.95));
    assert!(close(f.synoik().brightness.scales()[1].value(), 0.95));

    // The `-monitor` variant follows the pointer. Park it on the second output.
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    f.synoik_state().step_brightness(Step::Down, true);
    assert!(
        close(f.synoik().brightness.scales()[0].value(), 0.95),
        "the other monitor must not move"
    );
    assert!(close(f.synoik().brightness.scales()[1].value(), 0.9));

    // Cycle wraps at the top rather than stopping there -- the single-key control.
    f.synoik_state().step_brightness(Step::Up, false);
    assert!(close(
        f.synoik().brightness.global_scale().unwrap().value(),
        1.0
    ));
    f.synoik_state().step_brightness(Step::Cycle, false);
    assert!(close(
        f.synoik().brightness.global_scale().unwrap().value(),
        0.0
    ));
}

/// Every brightness change the shell makes puts an OSD on screen (`_showOSD`,
/// `js/misc/brightnessManager.js:227-239,264-275`): `display-brightness-symbolic`, no label, a bar
/// that maxes out at 1.0. WHICH monitors show one is the branch `_sync` took — the global branch
/// shows all of them, the per-monitor branch only the scales that moved, and the rest are
/// cancelled (`osdWindow.js:172-182`). A hotplug is `_sync({showOSD: false})` (`:181`), and so is a
/// change that moves no scale at all, like gsd-power's idle dimming.
#[test]
fn brightness_changes_show_the_osd() {
    use crate::brightness::Step;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let one = f.synoik_output(1);
    let two = f.synoik_output(2);

    let backlight = |connector: &str, name: &str| crate::backlight::OutputBacklight {
        connector: connector.to_owned(),
        display_name: name.to_owned(),
        range: crate::backlight::BacklightRange { min: 0, max: 100 },
        brightness: 100,
    };
    let snapshot = crate::backlight::BacklightSnapshot {
        outputs: vec![
            backlight("headless-1", "Built-in display"),
            backlight("headless-2", "Dell 24\u{2033}"),
        ],
    };
    // The hotplug pass re-derives every scale, and asks for no OSD for any of them.
    let update = f.synoik().brightness.monitors_changed(&snapshot);
    assert!(
        update.osd.is_empty(),
        "a monitors-changed pass must not put an OSD on screen"
    );
    f.synoik().backlight = snapshot;
    assert!(!f.synoik().osd.is_visible());

    // A plain brightness key moves the global scale, which fans out to every monitor -- so every
    // monitor shows the bar, at its own (here identical) level.
    f.synoik_state().step_brightness(Step::Down, false);
    let content = f
        .synoik()
        .osd
        .content(&one)
        .expect("output 1 shows the OSD");
    assert_eq!(content.icon, vec!["display-brightness-symbolic"]);
    assert_eq!(content.label, None, "the brightness OSD carries no label");
    assert_eq!(content.max_level, 1.0, "brightness tops out at 1.0");
    assert!((content.level.unwrap() - 0.95).abs() < 1e-9);
    assert!(f.synoik().osd.content(&two).is_some());

    // The `-monitor` variant moves one scale, so only that monitor shows one and the other's is
    // cancelled -- the behavior `osdWindowManager.show`'s level map exists for.
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    f.synoik_state().step_brightness(Step::Down, true);
    // A cancel is a fade-out, not an instant hide, so let it finish before looking.
    tick(&mut f, 200);
    assert!(
        f.synoik().osd.content(&one).is_none(),
        "the monitor that did not move must have its OSD cancelled"
    );
    let content = f.synoik().osd.content(&two).unwrap();
    assert!((content.level.unwrap() - 0.9).abs() < 1e-9);

    // The quick-settings slider is the global scale too, so it is back to both.
    f.synoik_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetBrightness(0.5));
    assert!(f.synoik().osd.content(&one).is_some());
    assert!(f.synoik().osd.content(&two).is_some());

    // Idle dimming clamps the hardware without moving a scale, so neither branch runs: whatever is
    // on screen is left to expire on its own deadline rather than being replaced or cancelled.
    let before = f.synoik().osd.content(&two);
    let snapshot = f.synoik().backlight.clone();
    let update = f.synoik().brightness.set_dimming(true, &snapshot);
    assert!(update.osd.is_empty(), "dimming moves no scale");
    assert_eq!(f.synoik().osd.content(&two), before);
}

/// `org.gnome.Shell.Brightness` is gsd-power's way in (`js/ui/shellDBus.js:595-637`): idle dimming
/// clamps the backlight without moving the scales, and the auto-brightness target biases them.
/// `BrightnessChanged` marks changes *the user* made, so the ambient-light loop can tell its own
/// adjustments apart from ours (`brightnessManager.js:151-158,172-179` emit `user-update` only
/// from the slider handlers).
#[test]
fn brightness_dbus_dims_without_moving_the_scales() {
    use crate::dbus::gnome_shell_brightness::{BrightnessToSynoik, SynoikToBrightness};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let snapshot = crate::backlight::BacklightSnapshot {
        outputs: vec![crate::backlight::OutputBacklight {
            connector: "headless-1".to_owned(),
            display_name: "Built-in display".to_owned(),
            range: crate::backlight::BacklightRange { min: 0, max: 100 },
            brightness: 100,
        }],
    };
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik().backlight = snapshot;

    // Stand in for the D-Bus service's outbound half so the emissions are observable.
    let (tx, rx) = async_channel::unbounded();
    f.synoik().brightness_emit = Some(tx);

    // gsd-power dims: the scale stays where the user put it; only the written brightness drops.
    f.synoik_state()
        .on_brightness_msg(BrightnessToSynoik::SetDimming(true));
    assert!(f.synoik().brightness.dimming());
    assert_eq!(f.synoik().brightness.global_scale().unwrap().value(), 1.0);

    // ... and none of that is a user change.
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, SynoikToBrightness::UserChanged)),
        "gsd-power's own request must not come back as BrightnessChanged"
    );
    // The property is pushed (the service dedups it), and it is true: we have a backlight.
    assert!(emitted
        .iter()
        .any(|m| matches!(m, SynoikToBrightness::HasControl(true))));

    // An auto-brightness target biases around the scale's midpoint, still not a user change.
    f.synoik_state()
        .on_brightness_msg(BrightnessToSynoik::SetAutoBrightnessTarget(0.6));
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(!emitted
        .iter()
        .any(|m| matches!(m, SynoikToBrightness::UserChanged)));

    // A brightness KEY is a user change, so it does emit.
    f.synoik_state()
        .step_brightness(crate::brightness::Step::Down, false);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        emitted
            .iter()
            .any(|m| matches!(m, SynoikToBrightness::UserChanged)),
        "a brightness key is a user change"
    );

    // Losing the backlight clears HasBrightnessControl.
    f.synoik().backlight = crate::backlight::BacklightSnapshot::default();
    let snapshot = f.synoik().backlight.clone();
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik_state()
        .on_brightness_msg(BrightnessToSynoik::SetDimming(false));
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(emitted
        .iter()
        .any(|m| matches!(m, SynoikToBrightness::HasControl(false))));
}

/// gnome-shell registers the brightness keys with `Shell.ActionMode.ALL`
/// (`js/misc/brightnessManager.js:35-76`), so they keep working on the lock screen -- which is
/// when you need them most, gsd-power having dimmed the panel -- and while the screenshot UI is up.
#[test]
fn brightness_keys_work_when_locked() {
    use synoik_config::Action;

    for action in [
        Action::ScreenBrightnessUp(false),
        Action::ScreenBrightnessDown(false),
        Action::ScreenBrightnessCycle(false),
        Action::ScreenBrightnessUp(true),
    ] {
        assert!(
            crate::input::allowed_when_locked(&action),
            "{action:?} must survive the lock gate"
        );
        assert!(
            crate::input::allowed_during_screenshot(&action),
            "{action:?} must survive the screenshot gate"
        );
    }
}

/// `BrightnessChanged` marks a change the user made to a scale that exists. gnome-shell's key
/// handlers are `this._globalScale?.stepUp()` (`brightnessManager.js:107-132`): with no scale
/// there is no `notify::value` and so no `user-update`. Emitting anyway would tell gsd-power's
/// ambient-light loop to back off over a key press that did nothing.
#[test]
fn a_brightness_key_with_no_backlight_is_silent() {
    use crate::dbus::gnome_shell_brightness::SynoikToBrightness;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().brightness_emit = Some(tx);
    assert!(
        f.synoik().brightness.global_scale().is_none(),
        "no backlight"
    );

    f.synoik_state()
        .step_brightness(crate::brightness::Step::Up, false);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, SynoikToBrightness::UserChanged)),
        "a key press with nothing to move must not emit BrightnessChanged"
    );

    // The `-monitor` variant with the pointer on a monitor that has no backlight is the same
    // case: gnome-shell's lookup simply misses.
    let snapshot = crate::backlight::BacklightSnapshot {
        outputs: vec![crate::backlight::OutputBacklight {
            connector: "headless-1".to_owned(),
            display_name: "Built-in display".to_owned(),
            range: crate::backlight::BacklightRange { min: 0, max: 100 },
            brightness: 100,
        }],
    };
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik().backlight = snapshot;
    f.add_output(2, (1920, 1080));
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    let _: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    f.synoik_state()
        .step_brightness(crate::brightness::Step::Up, true);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, SynoikToBrightness::UserChanged)),
        "the -monitor key on a monitor without a backlight must not emit"
    );
}

/// The popover opens and closes with an animation (gnome-shell's `BoxPointer` fade):
/// opening starts a running animation; dismissing does NOT drop the popover instantly
/// but keeps it visible (fading) with an ongoing animation until it settles.
#[test]
fn panel_popover_open_and_close_are_animated() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Open the calendar (click the clock).
    let x = clock_center_x(&mut f, 1920.);
    pointer_motion_to(&mut f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.synoik().panel_popover.is_open());
    assert!(
        f.synoik().panel_popover.are_animations_ongoing(),
        "opening must start a fade animation"
    );

    // Once settled, it's still open but no longer animating.
    f.settle_animations();
    assert!(f.synoik().panel_popover.is_open());
    assert!(!f.synoik().panel_popover.are_animations_ongoing());

    // Dismiss with Escape: the popover must NOT vanish instantly — it stays visible,
    // fading out, with an ongoing animation.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    assert!(
        f.synoik().panel_popover.is_open(),
        "the close must be animated, not instant — the popover stays visible while fading"
    );
    assert!(
        f.synoik().panel_popover.are_animations_ongoing(),
        "closing must run a fade-out animation"
    );

    // After the fade-out settles, it's gone.
    f.settle_animations();
    assert!(!f.synoik().panel_popover.is_open());
    assert!(!f.synoik().panel_popover.are_animations_ongoing());
}

/// A client must not be able to kill the compositor with a malformed `wl_region`.
///
/// `wl_region.add` takes plain ints and the protocol does not forbid a negative extent, so clients
/// send one: Firefox emits a `-1` height while being resized. Smithay fed it straight to `Size`,
/// whose `debug_assert!` fired *inside* the libwayland request callback — a panic that cannot
/// unwind, so it aborted the whole session. Any client could take the desktop down; this one did,
/// by accident, on a window resize.
///
/// The compositor must instead ignore the empty rectangle and keep serving. Reaching the asserts
/// below at all is the test: before the fix, the `roundtrip` aborts the process.
#[test]
fn negative_wl_region_does_not_kill_the_compositor() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(200, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Exactly what Firefox sent: a sane width, a negative height.
    f.client(id).set_opaque_region(&surface, 0, 0, 997, -1);
    f.double_roundtrip(id);

    // Still alive, still serving this client, and the window is still mapped.
    assert_eq!(
        f.synoik().layout.windows().count(),
        1,
        "the window must survive a negative opaque region",
    );

    // And a subsequent, well-formed region is still honoured.
    f.client(id).set_opaque_region(&surface, 0, 0, 200, 200);
    f.double_roundtrip(id);
}

/// gsd-power drives screen dim, blank, and auto-suspend through `org.gnome.Mutter.IdleMonitor`,
/// which reports how long the user has been idle. Input activity must reset that clock, or the
/// screen would blank mid-use. The watch-firing semantics are unit-tested in `crate::idle_monitor`;
/// this pins the compositor wiring from real synthetic input to the monitor.
#[test]
fn idle_monitor_input_activity_resets_idle_time() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // With no input since construction, idle time grows with elapsed monotonic time.
    let base = f.synoik().clock.now_unadjusted();
    assert!(
        f.synoik()
            .idle_monitor
            .idletime_ms(base + Duration::from_secs(600))
            >= 600_000,
        "idle time must grow while the user is inactive",
    );

    // Any input resets it (input `should_notify_activity` -> `Synoik::notify_activity`).
    f.key_press(KEY_A);
    f.key_release(KEY_A);

    let now = f.synoik().clock.now_unadjusted();
    assert!(
        f.synoik().idle_monitor.idletime_ms(now) < 1000,
        "input activity must reset the idle time to near zero",
    );
}

/// The `org.gnome.Mutter.IdleMonitor` D-Bus methods land on the compositor via
/// `on_idle_monitor_msg`. Drive that entry point: an idle watch registered through it fires once
/// its interval elapses, and a `ResetIdletime` (what gsd sends, and what activity does) re-arms it
/// for the next period.
///
/// The clock is pinned through the `ResetIdletime` handler so the deadlines are deterministic —
/// `refresh` is then called directly rather than waiting on the real-time calloop timer.
#[test]
fn idle_monitor_dbus_idle_watch_fires_and_rearms() {
    use crate::dbus::mutter_idle_monitor::IdleMonitorToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Pin the idle clock to a known instant via ResetIdletime, then register a 5s watch.
    let t0 = Duration::from_secs(10_000);
    f.synoik().clock.set_unadjusted(t0);
    f.synoik_state()
        .on_idle_monitor_msg(IdleMonitorToSynoik::ResetIdletime);

    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_idle_monitor_msg(IdleMonitorToSynoik::AddIdleWatch {
            interval: 5000,
            owner: ":1.gsd".to_owned(),
            reply,
        });
    let id = rx.try_recv().expect("AddIdleWatch must reply with an id");
    assert!(id > 0, "watch ids are greater than zero");

    assert!(
        f.synoik()
            .idle_monitor
            .refresh(t0 + Duration::from_millis(4999))
            .is_empty(),
        "must not fire before the interval elapses",
    );
    let fired = f
        .synoik()
        .idle_monitor
        .refresh(t0 + Duration::from_millis(5000));
    assert_eq!(fired.len(), 1, "fires once at the interval");
    assert_eq!(fired[0].id, id);
    assert_eq!(
        fired[0].owner, ":1.gsd",
        "WatchFired is unicast to the owner"
    );

    // A ResetIdletime (gsd's "user is active") re-arms it for the next idle period.
    let t1 = t0 + Duration::from_secs(20);
    f.synoik().clock.set_unadjusted(t1);
    f.synoik_state()
        .on_idle_monitor_msg(IdleMonitorToSynoik::ResetIdletime);
    assert!(
        f.synoik()
            .idle_monitor
            .refresh(t1 + Duration::from_millis(4999))
            .is_empty(),
        "the just-reset watch must not fire early",
    );
    assert_eq!(
        f.synoik()
            .idle_monitor
            .refresh(t1 + Duration::from_millis(5000))
            .len(),
        1,
        "the watch must fire again after activity re-armed it",
    );
}

/// gnome-session drives logout/shutdown/restart by calling `EndSessionDialog.Open` on the shell and
/// waiting for a `Confirmed*` (or `Canceled`) signal. Drive that entry point (`on_end_session_msg`)
/// and the input-side confirm/cancel: `Open` raises both the lifecycle state and the visible
/// dialog; confirming closes both and would emit the type's confirm signal; cancelling and
/// gnome-session's own `Close` also close it. The countdown auto-confirm and signal-name mapping
/// are unit-tested in `crate::end_session`.
#[test]
fn end_session_dialog_open_confirm_and_cancel() {
    use crate::dbus::gnome_session::EndSessionDialogToSynoik;
    use crate::end_session::EndSessionType;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // gnome-session raises the shutdown dialog with a 60s countdown.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 1,
            seconds: 60,
        });
    assert!(
        f.synoik().end_session.is_open(),
        "Open must raise the end-session lifecycle",
    );
    assert!(
        f.synoik().end_session_dialog.is_open(),
        "Open must raise the visible dialog too",
    );
    assert_eq!(
        f.synoik().end_session.kind(),
        Some(EndSessionType::Shutdown)
    );
    assert_eq!(
        f.synoik().end_session.kind().unwrap().confirmed_signal(),
        "ConfirmedShutdown",
        "confirming a shutdown dialog must emit ConfirmedShutdown",
    );

    // Confirming (Enter / clicking Power Off) closes the dialog; gnome-session then powers off.
    f.synoik_state().synoik.confirm_end_session();
    assert!(!f.synoik().end_session.is_open(), "confirm must close it");
    assert!(!f.synoik().end_session_dialog.is_open());

    // A fresh dialog can be cancelled (Esc / Cancel), which aborts the request.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 0,
            seconds: 60,
        });
    assert_eq!(f.synoik().end_session.kind(), Some(EndSessionType::Logout));
    f.synoik_state().synoik.cancel_end_session();
    assert!(!f.synoik().end_session.is_open(), "cancel must close it");

    // gnome-session withdrawing the request (Close) also dismisses the dialog.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 2,
            seconds: 60,
        });
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Close);
    assert!(
        !f.synoik().end_session.is_open(),
        "gnome-session's Close must dismiss the dialog",
    );
}

// RecordArea screencast: an area is recorded from a single output (the one it overlaps most),
// cropped to the recorded rectangle. See docs/fork/panel-status-port.md (slice 1, Half A).

#[test]
fn screencast_area_resolves_to_the_containing_output() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::synoik::CastTarget;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    // A rect fully inside the output resolves to it, at 1:1 physical size (headless scale 1).
    let rect = Rectangle::new(Point::from((100, 100)), Size::from((300, 200)));
    let (target, size, _refresh) = f
        .synoik()
        .cast_params_for_area(rect)
        .expect("a rect inside an output must resolve");
    assert_eq!(size, Size::from((300, 200)));
    // The resolved target is matched by its output, so stopping the output stops this area
    // cast (guards the zombie-on-output-removal path that would leave R1 ticking).
    assert!(target.matches_output(&out.downgrade()));
    let CastTarget::Area {
        name, rect: got, ..
    } = target
    else {
        panic!("expected an Area cast target");
    };
    assert_eq!(name, out.name());
    assert_eq!(got, rect);

    // A rect off every output resolves to nothing (mutter fails the stream likewise).
    let off = Rectangle::new(Point::from((10_000, 10_000)), Size::from((100, 100)));
    assert!(f.synoik().cast_params_for_area(off).is_none());
}

/// The offline-update checkbox, driven through the real entry points: gnome-session's `Open`, the
/// asynchronous answer from gnome-software, the user toggling the box, and confirming.
///
/// The bus is not involved — `on_offline_update_state` is exactly what the D-Bus reply lands on, so
/// the corpus can hand it any answer including ones a real gnome-software would rarely produce.
#[test]
fn end_session_dialog_offers_pending_updates() {
    use crate::dbus::gnome_session::EndSessionDialogToSynoik;
    use crate::end_session::{OfflineUpdateState, PostUpdateAction, UpdateDecision};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A power-off dialog opens with no checkbox: gnome-software has not answered yet.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 1,
            seconds: 60,
        });
    assert_eq!(f.synoik().update_checkbox(), None, "nothing offered yet");

    // It answers: an update is downloaded and waiting. The box appears, already ticked.
    f.synoik()
        .on_offline_update_state(OfflineUpdateState::Prepared);
    assert_eq!(f.synoik().update_checkbox(), Some(true));

    // Unticking it is remembered, and confirming then asks gnome-software to drop the update.
    f.synoik().toggle_install_updates();
    assert_eq!(f.synoik().update_checkbox(), Some(false));
    assert_eq!(
        f.synoik().end_session.confirm().unwrap().updates,
        UpdateDecision::Discard,
    );

    // Nothing pending: no box, whatever the dialog type.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 2,
            seconds: 60,
        });
    f.synoik().on_offline_update_state(OfflineUpdateState::None);
    assert_eq!(f.synoik().update_checkbox(), None);

    // Logging out never offers it, even with an update ready to go.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 0,
            seconds: 60,
        });
    f.synoik()
        .on_offline_update_state(OfflineUpdateState::Scheduled);
    assert_eq!(
        f.synoik().update_checkbox(),
        None,
        "logout installs nothing, so it must not offer to",
    );

    // A restart with the box left ticked schedules the update and reboots into it.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 2,
            seconds: 60,
        });
    f.synoik()
        .on_offline_update_state(OfflineUpdateState::Prepared);
    assert_eq!(
        f.synoik().end_session.confirm().unwrap().updates,
        UpdateDecision::Install(PostUpdateAction::Reboot),
    );
}

/// gnome-shell's fourth dialog *presentation*, `UPDATE_RESTART` (`endSessionDialog.js:684-687`):
/// gnome-session never sends type 3, the shell promotes a restart to it when gnome-software says an
/// update is already scheduled. The promotion happens under a dialog that is already on screen,
/// because we ask gnome-software asynchronously — so this drives the real `Open` and the real
/// reply.
#[test]
fn end_session_dialog_promotes_a_restart_with_a_scheduled_update() {
    use crate::dbus::gnome_session::EndSessionDialogToSynoik;
    use crate::end_session::{OfflineUpdateState, PostUpdateAction, Presentation, UpdateDecision};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A restart opens as a plain restart — gnome-software has not answered yet.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 2,
            seconds: 60,
        });
    assert_eq!(
        f.synoik().end_session.presentation(),
        Some(Presentation::Restart)
    );

    // The answer lands: the update is already scheduled, so the dialog re-presents itself under the
    // user rather than growing a checkbox to ask a question they have already answered.
    f.synoik()
        .on_offline_update_state(OfflineUpdateState::Scheduled);
    assert_eq!(
        f.synoik().end_session.presentation(),
        Some(Presentation::UpdateRestart)
    );
    assert_eq!(
        f.synoik().update_checkbox(),
        None,
        "the promoted dialog asks with its title, not a checkbox",
    );

    // Confirming still schedules the reboot: `GetState` never says *which* action was scheduled, so
    // the dialog re-asserts it instead of reading it back.
    let c = f.synoik().end_session.confirm().unwrap();
    assert_eq!(c.updates, UpdateDecision::Install(PostUpdateAction::Reboot));
    assert_eq!(c.signal(true), "ConfirmedReboot");
    assert_eq!(c.signal(false), "ConfirmedReboot");

    // Power off does NOT promote: GNOME has that half drafted but unshipped (the
    // `unusedFuture*ForTranslation` strings, `:120-121`), so a scheduled update still gets the
    // checkbox — and with it the fallback for a `SetAction` gnome-software refuses.
    f.synoik_state()
        .on_end_session_msg(EndSessionDialogToSynoik::Open {
            kind: 1,
            seconds: 60,
        });
    f.synoik()
        .on_offline_update_state(OfflineUpdateState::Scheduled);
    assert_eq!(
        f.synoik().end_session.presentation(),
        Some(Presentation::Shutdown)
    );
    assert_eq!(f.synoik().update_checkbox(), Some(true));
}

#[test]
fn screencast_area_picks_the_largest_intersection_output() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::synoik::CastTarget;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    // Order the two outputs left→right by their global position.
    let mut outs = [f.synoik_output(1), f.synoik_output(2)];
    outs.sort_by_key(|o| f.synoik().global_space.output_geometry(o).unwrap().loc.x);
    let right_geo = f.synoik().global_space.output_geometry(&outs[1]).unwrap();
    let seam = right_geo.loc.x;

    // Straddle the seam: 40px on the left output, 200px on the right → the right output wins.
    let rect = Rectangle::new(
        Point::from((seam - 40, right_geo.loc.y + 100)),
        Size::from((240, 100)),
    );
    let (target, _, _) = f
        .synoik()
        .cast_params_for_area(rect)
        .expect("a straddling rect still resolves");
    let CastTarget::Area { name, .. } = target else {
        panic!("expected an Area cast target");
    };
    assert_eq!(
        name,
        outs[1].name(),
        "the output with the larger intersection must win"
    );
}

#[test]
fn area_crop_offset_accounts_for_the_output_origin() {
    use smithay::utils::{Point, Rectangle, Scale, Size};

    use crate::screencasting::area_crop_offset;

    // An output away from the global origin: the crop offset is the area's top-left relative to
    // the output, not to the stage. Dropping the `- output_geo.loc` term would give (2020, 190).
    let output_geo = Rectangle::new(Point::from((1920, 40)), Size::from((1920, 1080)));
    let rect = Rectangle::new(Point::from((2020, 190)), Size::from((300, 200)));
    assert_eq!(
        area_crop_offset(rect, output_geo, Scale::from(1.0)),
        Point::from((100, 150)),
    );
    // Scale doubles the physical offset.
    assert_eq!(
        area_crop_offset(rect, output_geo, Scale::from(2.0)),
        Point::from((200, 300)),
    );
}

// The R1 screen-recording indicator: a recording surfaces a right-box indicator that ticks its
// M:SS label, and clicking it stops the recording. See docs/fork/panel-status-port.md (slice 1).

#[test]
fn screen_recording_indicator_appears_and_ticks() {
    use crate::ui::panel::{PanelBox, WorkspaceState, ROLE_SCREEN_RECORDING};
    use crate::utils::CastSessionId;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Pin the clock so the elapsed label is deterministic (avoids the clock-trap).
    let mut clock = f.synoik().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let ow = 1920.;
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };

    // Nothing recording → no indicator.
    assert!(f
        .synoik()
        .panel
        .items(ow, ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));

    let id = CastSessionId::next();
    f.synoik().screen_recording_started(id);

    // Shows at 0:00, as a right-box item.
    assert_eq!(f.synoik().panel.recording_label(), Some("0:00"));
    assert!(f
        .synoik()
        .panel
        .items(ow, ws)
        .iter()
        .any(|i| i.role == ROLE_SCREEN_RECORDING && i.r#box == PanelBox::Right));

    // Re-ticking the label (the seam the 1 s recording timer calls) tracks elapsed time.
    clock.set_unadjusted(t0 + Duration::from_secs(65));
    assert!(f.synoik().panel.update_recording_label());
    assert_eq!(f.synoik().panel.recording_label(), Some("1:05"));

    clock.set_unadjusted(t0 + Duration::from_secs(600));
    assert!(f.synoik().panel.update_recording_label());
    assert_eq!(f.synoik().panel.recording_label(), Some("10:00"));
}

#[test]
fn screen_recording_indicator_click_stops_the_recording() {
    use crate::ui::panel::{WorkspaceState, ROLE_SCREEN_RECORDING};
    use crate::utils::CastSessionId;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut clock = f.synoik().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let id = CastSessionId::next();
    f.synoik().screen_recording_started(id);
    assert!(!f.synoik().casting.recordings.is_empty());

    // Click the indicator's center (top panel band).
    let r1 = f.synoik().panel.screen_recording_rect(1920.);
    let cx = r1.loc.x + r1.size.w / 2.;
    f.pointer_motion(cx, 16.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // Clicking it stops the recording through the real hit-test → stop_cast path, and the
    // indicator disappears.
    assert!(
        f.synoik().casting.recordings.is_empty(),
        "clicking the indicator stops the recording",
    );
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };
    assert!(f
        .synoik()
        .panel
        .items(1920., ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));
}

#[test]
fn native_screen_recording_registers_and_stops() {
    use crate::screencasting::RecordingKind;
    use crate::ui::panel::{WorkspaceState, ROLE_SCREEN_RECORDING};

    if !super::have_ffmpeg() {
        return;
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let mut clock = f.synoik().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let output = f.synoik().global_space.outputs().next().cloned().unwrap();
    let path = std::env::temp_dir().join(format!("synoik-native-rec-{}.webm", std::process::id()));

    // Starting registers a Native recording and shows the R1 pill.
    f.synoik()
        .start_native_recording(&output, path.clone(), 30, true, None)
        .unwrap();
    assert!(f
        .synoik()
        .casting
        .recordings
        .iter()
        .any(|r| matches!(r.kind, RecordingKind::Native(_))));
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };
    assert!(f
        .synoik()
        .panel
        .items(1920., ws)
        .iter()
        .any(|i| i.role == ROLE_SCREEN_RECORDING));

    // Clicking the pill runs the real hit-test → stop_screen_recordings → finalize-encoder path;
    // the ledger clears and the indicator disappears (regardless of the zero-frame file).
    let r1 = f.synoik().panel.screen_recording_rect(1920.);
    let cx = r1.loc.x + r1.size.w / 2.;
    f.pointer_motion(cx, 16.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.synoik().casting.recordings.is_empty(),
        "clicking the indicator stops the native recording",
    );
    assert!(f
        .synoik()
        .panel
        .items(1920., ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));

    std::fs::remove_file(&path).ok();
}

#[cfg(feature = "xdp-gnome-screencast")]
#[test]
fn shell_screencast_dbus_start_and_stop() {
    use crate::dbus::gnome_shell_screencast::ScreencastToSynoik;
    use crate::screencasting::RecordingKind;

    if !super::have_ffmpeg() {
        return;
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Land the recording under a temp dir via an absolute template (no XDG env dependency).
    let dir = std::env::temp_dir().join(format!("synoik-shell-sc-{}", std::process::id()));
    let template = dir.join("clip %%").to_string_lossy().into_owned();

    let start = |f: &mut Fixture, template: String| {
        let (reply, rx) = async_channel::bounded(1);
        f.synoik()
            .on_shell_screencast_msg(ScreencastToSynoik::Start {
                area: None,
                template,
                draw_cursor: true,
                framerate: 30,
                reply,
            });
        rx.recv_blocking().unwrap()
    };

    // A D-Bus Start registers a Native recording and replies with the resolved absolute path.
    let path = start(&mut f, template.clone()).expect("start should succeed");
    assert_eq!(path, dir.join("clip %.webm").to_string_lossy());
    assert!(f
        .synoik()
        .casting
        .recordings
        .iter()
        .any(|r| matches!(r.kind, RecordingKind::Native(_))));

    // A second Start while recording is declined (one recording at a time).
    assert!(
        start(&mut f, template.clone()).is_err(),
        "already recording"
    );

    // Stop reports that a recording was torn down and clears the ledger.
    let (reply, rx) = async_channel::bounded(1);
    f.synoik()
        .on_shell_screencast_msg(ScreencastToSynoik::Stop { reply });
    assert!(rx.recv_blocking().unwrap(), "stop found a live recording");
    assert!(f.synoik().casting.recordings.is_empty());

    // A ScreencastArea request records a region of the output (a later slice used to decline it).
    let (reply, rx) = async_channel::bounded(1);
    f.synoik()
        .on_shell_screencast_msg(ScreencastToSynoik::Start {
            area: Some((100, 100, 640, 480)),
            template: dir.join("area %%").to_string_lossy().into_owned(),
            draw_cursor: false,
            framerate: 30,
            reply,
        });
    let area_path = rx.recv_blocking().unwrap().expect("area recording starts");
    assert_eq!(area_path, dir.join("area %.webm").to_string_lossy());
    assert!(f
        .synoik()
        .casting
        .recordings
        .iter()
        .any(|r| matches!(r.kind, RecordingKind::Native(_))));

    let (reply, rx) = async_channel::bounded(1);
    f.synoik()
        .on_shell_screencast_msg(ScreencastToSynoik::Stop { reply });
    assert!(rx.recv_blocking().unwrap());

    std::fs::remove_dir_all(&dir).ok();
}

/// A notification presents as its **app** when one resolves: the app's name and icon replace the
/// `app_name`/`app_icon` call parameters (`FdoNotificationDaemonSource`,
/// `js/ui/notificationDaemon.js:396-399`, fed by `_getApp` at `:74-86`).
///
/// This is the path a browser's web notification takes. Firefox and Chromium send an **empty
/// `app_icon`** and identify themselves only through the `desktop-entry` hint, so a card built
/// from the call parameters alone has no icon to show and falls back to the generic executable
/// glyph — which is what the notification header did until the app resolution landed.
///
/// Driven through `on_notifications_msg` rather than the store, because the resolution happens in
/// the compositor (that is where the app catalog is) and the store is a plain-data seam.
#[test]
fn a_notification_takes_its_source_identity_from_the_resolved_app() {
    use crate::app_system::{AppEntry, AppIconRef, AppSystem, FakeCatalog};
    use crate::notifications::{NotificationsToSynoik, NotifyRequest, Urgency};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut entry = AppEntry::fake("firefox.desktop", "Firefox");
    entry.icon = AppIconRef::Themed(vec!["firefox".to_owned()]);
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![entry])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let web_notification = |app_name: &str, hint: Option<&str>| NotifyRequest {
        sender: Some(":1.9".to_owned()),
        pid: 100,
        app_name: app_name.to_owned(),
        replaces_id: 0,
        desktop_entry: hint.map(str::to_owned),
        // What a browser actually sends for a web notification: nothing.
        source_icon: None,
        app_icon: None,
        title: "davidwalsh.name".to_owned(),
        body: "body".to_owned(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        urgency: Urgency::Normal,
        resident: false,
        transient: false,
    };

    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify {
            req: web_notification("Firefox", Some("firefox")),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));

    let source = &f.synoik().notifications.sources[0];
    assert_eq!(
        source.app_icon,
        Some(AppIconRef::Themed(vec!["firefox".to_owned()])),
        "the resolved app's icon reaches the source, with no app_icon parameter to fall back on"
    );
    assert_eq!(
        source.title, "Firefox",
        "and its name, which is the app's, not the caller's app_name"
    );

    // An app that does not resolve leaves the source on the call parameters — the source is then
    // whatever the sender claimed, and the header falls back to the executable glyph.
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify {
            req: web_notification("Some Unknown App", Some("not-installed")),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(2));
    let unresolved = f
        .synoik()
        .notifications
        .sources
        .iter()
        .find(|s| s.title == "Some Unknown App")
        .expect("the unresolved source keeps the caller's app_name");
    assert_eq!(unresolved.app_icon, None);
}

/// The `org.freedesktop.Notifications` request path (`js/ui/notificationDaemon.js`
/// `NotifyAsync`/`CloseNotification` + the fdo proxy's per-sender id checks,
/// `js/dbusServices/notifications/notificationDaemon.js:76-90`), driven straight
/// through `on_notifications_msg` the way the calloop channel would deliver it.
#[test]
fn notifications_notify_replace_and_close_via_handler() {
    use crate::notifications::{NotificationsToSynoik, NotifyRequest, Urgency};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let req = |app: &str, sender: &str, replaces: u32| NotifyRequest {
        sender: Some(sender.to_owned()),
        pid: 100,
        app_name: app.to_owned(),
        replaces_id: replaces,
        desktop_entry: None,
        source_icon: None,
        app_icon: None,
        title: "title".to_owned(),
        body: "body".to_owned(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        urgency: Urgency::Normal,
        resident: false,
        transient: false,
    };

    // Notify allocates ids from 1 and stores the notification.
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify {
            req: req("app", ":1.7", 0),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));
    assert_eq!(f.synoik().notifications.sources.len(), 1);
    assert_eq!(f.synoik().notifications.find(1).unwrap().title, "title");

    // Replace (same sender) mutates in place, same id, no new notification.
    let mut update = req("app", ":1.7", 1);
    update.title = "updated".to_owned();
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify { req: update, reply });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));
    assert_eq!(f.synoik().notifications.sources[0].notifications.len(), 1);
    assert_eq!(f.synoik().notifications.find(1).unwrap().title, "updated");

    // Replace from a different sender is rejected (the fdo proxy's
    // "Invalid notification ID").
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify {
            req: req("evil", ":1.66", 1),
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());

    // CloseNotification: foreign sender rejected, own sender destroys and the
    // owed NotificationClosed emission (reason 3 = the app asked) reaches the
    // server's emit channel.
    let (to_notifications, emitted) = async_channel::unbounded();
    f.synoik_state().synoik.notifications_emit = Some(to_notifications);

    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Close {
            id: 1,
            sender: ":1.66".to_owned(),
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());
    assert!(f.synoik().notifications.find(1).is_some());

    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Close {
            id: 1,
            sender: ":1.7".to_owned(),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(()));
    assert!(f.synoik().notifications.find(1).is_none());
    assert!(
        f.synoik().notifications.sources.is_empty(),
        "a source with zero notifications removes itself",
    );
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::Closed { id, reason, sender } => {
            assert_eq!(id, 1);
            assert_eq!(reason.wire_code(), 3);
            assert_eq!(sender.as_deref(), Some(":1.7"));
        }
        _ => panic!("expected a Closed emission"),
    }
}

/// Sender-vanish teardown (`js/ui/notificationDaemon.js:340-348`): only
/// app-keyed (desktop-entry) sources die with their sender; pid-keyed
/// `notify-send`-style sources survive.
#[test]
fn notifications_sender_vanish_via_handler() {
    use crate::notifications::{NotificationsToSynoik, NotifyRequest, SourceKey, Urgency};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut app = NotifyRequest {
        sender: Some(":1.9".to_owned()),
        pid: 100,
        app_name: "App".to_owned(),
        replaces_id: 0,
        desktop_entry: Some("org.example.App".to_owned()),
        source_icon: None,
        app_icon: None,
        title: "t".to_owned(),
        body: String::new(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        urgency: Urgency::Normal,
        resident: false,
        transient: false,
    };
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify {
            req: app.clone(),
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    app.desktop_entry = None;
    app.app_name = "notify-send".to_owned();
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::Notify { req: app, reply });
    rx.recv_blocking().unwrap().unwrap();

    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::SenderVanished(":1.9".to_owned()));
    let sources = &f.synoik().notifications.sources;
    assert_eq!(sources.len(), 1);
    assert!(matches!(sources[0].key, SourceKey::PidName(..)));
}

/// The `org.gtk.Notifications` request path (`js/ui/notificationDaemon.js`
/// `GtkNotificationDaemon` — `AddNotification`/`RemoveNotification` keyed by
/// `(app_id, gtk_id)`, action routing by `app.` prefix), driven through
/// `on_notifications_msg`. `.desktop` resolution is server-side and tested in
/// `dbus::gtk_notifications`; here the request arrives already resolved.
#[test]
fn notifications_gtk_add_action_and_remove_via_handler() {
    use crate::notifications::{
        GtkNotifyRequest, GtkToNotifications, NotificationsToSynoik, SourceKey, Urgency,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let gtk_req = |gtk_id: &str| GtkNotifyRequest {
        app_id: "org.example.Chat".to_owned(),
        gtk_id: gtk_id.to_owned(),
        app_title: "Chat".to_owned(),
        app_icon: None,
        title: "title".to_owned(),
        body: "body".to_owned(),
        icon: None,
        actions: vec![("reply".to_owned(), "Reply".to_owned())],
        default_action: Some("app.open".to_owned()),
        urgency: Urgency::Normal,
    };

    // Add: a GtkApp source appears, keyed by application-id, with the
    // server-resolved display name.
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::AddGtk {
            req: gtk_req("msg-1"),
        });
    assert_eq!(f.synoik().notifications.sources.len(), 1);
    let source = &f.synoik().notifications.sources[0];
    assert!(matches!(&source.key, SourceKey::GtkApp(a) if a == "org.example.Chat"));
    assert_eq!(source.title, "Chat");
    let id = source.notifications[0].id;

    // Add with the same (app_id, gtk_id) replaces in place — no second card.
    let mut update = gtk_req("msg-1");
    update.title = "updated".to_owned();
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::AddGtk { req: update });
    assert_eq!(f.synoik().notifications.sources[0].notifications.len(), 1);
    assert_eq!(f.synoik().notifications.find(id).unwrap().title, "updated");

    // A non-`app.` action routes to the Gtk emit channel (NOT the fdo one).
    let (to_gtk, gtk_emitted) = async_channel::unbounded();
    f.synoik_state().synoik.gtk_notifications_emit = Some(to_gtk);
    let (to_fdo, fdo_emitted) = async_channel::unbounded();
    f.synoik_state().synoik.notifications_emit = Some(to_fdo);

    assert!(
        f.synoik_state()
            .synoik
            .emit_notification_action(id, "reply".to_owned()),
        "the shell activates the app itself on the Gtk path, so it reports the overview should go"
    );
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::ActionInvoked {
            app_id,
            gtk_id,
            action,
            ..
        } => {
            assert_eq!(app_id, "org.example.Chat");
            assert_eq!(gtk_id, "msg-1");
            assert_eq!(action, "reply");
        }
        _ => panic!("expected ActionInvoked"),
    }
    assert!(
        fdo_emitted.try_recv().is_err(),
        "a Gtk notification must not emit on the fdo channel"
    );

    // The body-click pseudo-key resolves to the payload's default-action.
    assert!(f
        .synoik_state()
        .synoik
        .emit_notification_action(id, "default".to_owned()));
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::ActionInvoked { action, .. } => assert_eq!(action, "app.open"),
        _ => panic!("expected ActionInvoked"),
    }

    // Remove destroys it and emits no fdo NotificationClosed (no sender).
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::RemoveGtk {
            app_id: "org.example.Chat".to_owned(),
            gtk_id: "msg-1".to_owned(),
        });
    assert!(f.synoik().notifications.find(id).is_none());
    assert!(f.synoik().notifications.sources.is_empty());
    assert!(
        fdo_emitted.try_recv().is_err(),
        "removing a Gtk notification emits no NotificationClosed"
    );
}

/// A body click on a Gtk notification with NO default action runs `open()` =
/// the app's `Activate` (`js/ui/notificationDaemon.js:539`).
#[test]
fn notifications_gtk_body_click_without_default_activates_app() {
    use crate::notifications::{
        GtkNotifyRequest, GtkToNotifications, NotificationsToSynoik, Urgency,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state()
        .on_notifications_msg(NotificationsToSynoik::AddGtk {
            req: GtkNotifyRequest {
                app_id: "org.example.App".to_owned(),
                gtk_id: "n1".to_owned(),
                app_title: "App".to_owned(),
                app_icon: None,
                title: "t".to_owned(),
                body: "b".to_owned(),
                icon: None,
                actions: Vec::new(),
                default_action: None,
                urgency: Urgency::Normal,
            },
        });
    let id = f.synoik().notifications.sources[0].notifications[0].id;

    let (to_gtk, gtk_emitted) = async_channel::unbounded();
    f.synoik_state().synoik.gtk_notifications_emit = Some(to_gtk);
    assert!(f.synoik_state().synoik.open_notification_app(id));
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::Activate { app_id, .. } => assert_eq!(app_id, "org.example.App"),
        _ => panic!("expected Activate"),
    }
}

/// The quick-settings system rows ask `org.gnome.SessionManager` **directly** — gnome-shell's
/// `activateLogout` / `activatePowerOff` / `activateRestart` are `LogoutAsync(0)` /
/// `ShutdownAsync(0)` / `RebootAsync()` on the session proxy (`systemActions.js:483-501`), and
/// `activateSuspend` is `SuspendAsync()` on the *same* proxy (`:509`) rather than a call to logind.
/// We used to spawn `gnome-session-quit` and `systemctl suspend` instead, which put a whole process
/// start in front of every logout: measured on the seat (journal, 2026-08-03) at 0.69-1.54 s before
/// the session even began to end.
///
/// The asymmetry is gnome-shell's and is the reason this is pinned: **only logout hides the
/// overview** (`Main.overview.hide()` at `:487`). Power-off, restart and suspend do not — the
/// machine is going away, so there is nothing to reveal, and hiding it would only put a frame of
/// desktop on screen on the way out.
#[test]
fn quick_settings_system_rows_call_gnome_session_directly() {
    use crate::end_session::SessionRequest;
    use crate::ui::popover::PopoverAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let open_overview = |f: &mut Fixture| {
        f.synoik_state().do_action(Action::OpenOverview, false);
        f.synoik_complete_animations();
        assert!(f.synoik().layout.is_overview_open());
    };

    // Logout hides it.
    open_overview(&mut f);
    f.synoik_state()
        .apply_popover_action(PopoverAction::SessionRequest(SessionRequest::Logout));
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "the Log Out row must hide the overview first"
    );

    // The other three do not.
    for request in [
        SessionRequest::PowerOff,
        SessionRequest::Reboot,
        SessionRequest::Suspend,
    ] {
        open_overview(&mut f);
        f.synoik_state()
            .apply_popover_action(PopoverAction::SessionRequest(request));
        f.synoik_complete_animations();
        assert!(
            f.synoik().layout.is_overview_open(),
            "{request:?} must leave the overview alone, as gnome-shell does"
        );
    }
}

/// Every shell surface that starts an app leaves the overview first. gnome-shell
/// writes `Main.overview.hide()` into each such handler by hand — the quick-settings
/// system rows (`js/ui/status/system.js:53-57,150-154`), `addSettingsAction`
/// (`js/ui/popupMenu.js:709-720`), the dateMenu cards (`js/ui/dateMenu.js:300-302,
/// 376-381,597-600`) — and ours all resolve to one `PopoverAction::Spawn`.
///
/// The two neighbours are pinned here too, because both look like they should hide it
/// and gnome-shell says otherwise: `Main.screenshotUI.open()` carries no hide at all
/// (`js/ui/status/system.js:120-127`), and the fdo notification path only emits
/// `ActionInvoked` — the hide lives in the *Gtk* daemon's `activateAction`
/// (`js/ui/notificationDaemon.js:512-519`), which has really activated the app.
#[test]
fn overview_closes_when_a_panel_button_launches_an_app() {
    use crate::ui::popover::PopoverAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let open_overview = |f: &mut Fixture| {
        f.synoik_state().do_action(Action::OpenOverview, false);
        f.synoik_complete_animations();
        assert!(f.synoik().layout.is_overview_open());
    };

    // An *empty* command: `spawn` returns early on it (`utils::spawning`), so this
    // exercises the choke point without a test really launching gnome-control-center.
    open_overview(&mut f);
    f.synoik_state()
        .apply_popover_action(PopoverAction::Spawn(Vec::new()));
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a panel/quick-settings button that starts an app must leave the overview"
    );

    // A toggle that changes a setting stays put — GNOME hides only for the rows that
    // raise a window.
    open_overview(&mut f);
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetDarkStyle(true));
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "a quick-settings toggle must not close the overview"
    );

    // An fdo notification action is a signal to the app, not an activation by us.
    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.synoik_state()
        .apply_popover_action(PopoverAction::InvokeNotificationAction {
            id,
            key: "reply".to_owned(),
        });
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "the fdo path only emits ActionInvoked, so the overview stays up"
    );
}

// ---- Notification banner (slice 2) ----

fn banner_req(app: &str, sender: &str) -> crate::notifications::NotifyRequest {
    crate::notifications::NotifyRequest {
        sender: Some(sender.to_owned()),
        pid: 100,
        app_name: app.to_owned(),
        replaces_id: 0,
        desktop_entry: None,
        source_icon: None,
        app_icon: None,
        title: "title".to_owned(),
        body: "body".to_owned(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        urgency: crate::notifications::Urgency::Normal,
        resident: false,
        transient: false,
    }
}

fn banner_notify(f: &mut Fixture, req: crate::notifications::NotifyRequest) -> u32 {
    let (reply, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_notifications_msg(crate::notifications::NotificationsToSynoik::Notify { req, reply });
    rx.recv_blocking().unwrap().unwrap()
}

/// The banner's on-screen rect, asked of the banner itself. It hangs off the *right*
/// corner, under the clock whose popover owns its message list (see
/// [`crate::ui::notification_banner`]), so tests derive their sample points from here
/// instead of assuming where it lands.
fn banner_rect(
    f: &mut Fixture,
    output: u8,
) -> smithay::utils::Rectangle<f64, smithay::utils::Logical> {
    let output = f.synoik_output(output);
    f.synoik().notification_banner.shown_rect(&output).unwrap()
}

/// Pin the clock forward and advance — the banner's deadline authority is the
/// pinned clock, not the wake-up timer (see the headless-animation-clock trap).
fn tick(f: &mut Fixture, ms: u64) {
    let synoik = f.synoik();
    let now = synoik.clock.now_unadjusted();
    synoik.clock.set_unadjusted(now + Duration::from_millis(ms));
    synoik.advance_animations();
}

/// A grab owns the cursor until it ends. Nothing that merely *reacts* to pointer motion may take
/// it away — while an interactive resize is running, the cursor stays the resize cursor even if the
/// pointer wanders under a notification banner.
#[test]
fn a_resize_grab_keeps_its_cursor_under_a_banner() {
    use smithay::input::pointer::{
        CursorIcon, CursorImageStatus, GrabStartData as PointerGrabStartData,
    };
    use smithay::utils::Point;

    use crate::utils::ResizeEdge;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);
    let window = f.synoik().layout.windows().next().unwrap().1.window.clone();

    // Park the pointer somewhere harmless and put a banner up.
    pointer_motion_to(&mut f, 600., 600.);
    banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    assert!(f.synoik().notification_banner.is_visible());

    // Start a real interactive resize on the bottom-right corner, the way the button handler does.
    let edges = ResizeEdge::BOTTOM_RIGHT;
    assert!(f
        .synoik_state()
        .synoik
        .layout
        .interactive_resize_begin(window.clone(), edges));
    let state = f.synoik_state();
    let pointer = state.synoik.seat.get_pointer().unwrap();
    let start_data = PointerGrabStartData {
        focus: None,
        button: 0x110,
        location: pointer.current_location(),
    };
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    let grab = crate::input::resize_grab::ResizeGrab::new(start_data, window);
    pointer.set_grab(state, grab, serial, smithay::input::pointer::Focus::Clear);
    state
        .synoik
        .cursor_manager
        .set_cursor_image(CursorImageStatus::Named(edges.cursor_icon()));
    assert_eq!(
        *f.synoik().cursor_manager.cursor_image(),
        CursorImageStatus::Named(CursorIcon::SeResize),
        "the grab starts by advertising what it is resizing"
    );

    // Drag up into the banner's own rect, which is where the arrow used to take over.
    let banner = banner_rect(&mut f, 1);
    let on_banner = banner.loc + Point::from((banner.size.w / 2., 4.));
    pointer_motion_to(&mut f, on_banner.x, on_banner.y);
    let output = f.synoik_output(1);
    assert!(
        f.synoik()
            .notification_banner
            .pointer_inside(&output, on_banner),
        "the sample point must really be under the banner, or this proves nothing"
    );
    assert_eq!(
        *f.synoik().cursor_manager.cursor_image(),
        CursorImageStatus::Named(CursorIcon::SeResize),
        "the resize grab still owns the cursor; a banner the pointer passed under must not \
         replace it with an arrow mid-drag"
    );

    let _ = surface;
}

/// The tray's own timing (`js/ui/messageTray.js:19,1279-1292`): a banner shows on
/// notify, auto-hides after 4 s, and hiding destroys ONLY transient notifications.
#[test]
fn notification_banner_shows_and_expires() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Activity: the user is not idle at show time.
    f.pointer_motion(1., 1.);

    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(f.synoik().notification_banner.is_visible());
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(id));
    // Showing acknowledged it (`js/ui/messageTray.js:1167`).
    assert!(f.synoik().notifications.find(id).unwrap().acknowledged);
    // The deadline is armed at the Showing->Shown transition inside
    // advance_animations — the wake-up timer must be re-armed there too, or a
    // banner over a damage-free desktop would never wake the loop to expire.
    assert!(f.synoik().notification_banner_timer.is_some());

    // The 4 s timeout elapses -> hide -> the notification SURVIVES in the store.
    tick(&mut f, 4100);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_visible());
    assert!(f.synoik().notifications.find(id).is_some());

    // A transient notification is destroyed by its banner hiding (EXPIRED).
    // Fresh activity first: the pinned clock has drifted past the idle
    // threshold, which would otherwise idle-gate the expiry (a real behavior,
    // pinned by `notification_banner_idle_gates_expiry`). `notify_activity`
    // runs once per event-loop iteration; clear the guard by hand since no
    // real iteration boundary passes in this test.
    f.synoik().notified_activity_this_iteration = false;
    f.pointer_motion(1., 1.);
    let mut transient = banner_req("app", ":1.1");
    transient.transient = true;
    let tid = banner_notify(&mut f, transient);
    f.settle_animations();
    tick(&mut f, 4100);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_visible());
    assert!(f.synoik().notifications.find(tid).is_none());
}

/// LOW never banners; DND suppresses all but CRITICAL, and CRITICAL never
/// auto-expires (`js/ui/messageTray.js:932-936,1211-1214`).
#[test]
fn notification_banner_policy_low_dnd_critical() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let mut low = banner_req("app", ":1.1");
    low.urgency = crate::notifications::Urgency::Low;
    banner_notify(&mut f, low);
    assert!(!f.synoik().notification_banner.is_visible());

    f.synoik().gnome_settings.quick_toggles.do_not_disturb = true;
    banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(!f.synoik().notification_banner.is_visible());

    let mut critical = banner_req("app", ":1.1");
    critical.urgency = crate::notifications::Urgency::Critical;
    let cid = banner_notify(&mut f, critical);
    assert!(f.synoik().notification_banner.is_visible());
    f.settle_animations();
    // No deadline: still up long past the normal timeout.
    tick(&mut f, 60_000);
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(cid));
}

/// The queue drains highest-urgency-first once the current banner expires
/// (`js/ui/messageTray.js:951-953,1070-1086`).
#[test]
fn notification_banner_queue_drains_urgency_first() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let first = banner_notify(&mut f, banner_req("app", ":1.1"));
    banner_notify(&mut f, banner_req("app", ":1.1"));
    let mut critical = banner_req("app", ":1.1");
    critical.urgency = crate::notifications::Urgency::Critical;
    let crit = banner_notify(&mut f, critical);

    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(first));
    tick(&mut f, 4100);
    f.settle_animations();
    // The critical one jumped the queue.
    assert_eq!(f.synoik().notification_banner.content_id(), Some(crit));
}

/// A replace re-enters banner admission: a dismissed-and-acked notification
/// banners again (`js/ui/messageTray.js:589-595`), and replacing the shown one
/// refreshes it in place and re-acks (`:938-943,1166-1168`).
#[test]
fn notification_banner_replace_rebanners() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    tick(&mut f, 4100);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_visible());

    // Replace the now-hidden, acked notification: it banners again.
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    assert_eq!(banner_notify(&mut f, update), id);
    assert!(f.synoik().notification_banner.is_visible());
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(id));

    // Replace while showing: stays visible, re-acked (never counts unseen).
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    update.title = "updated".to_owned();
    assert_eq!(banner_notify(&mut f, update), id);
    assert!(f.synoik().notification_banner.is_visible());
    assert!(f.synoik().notifications.find(id).unwrap().acknowledged);
    assert_eq!(f.synoik().notifications.unseen_count(), 0);

    // Replace while the banner is mid-hide: "we stop hiding it and show it
    // again" (`js/ui/messageTray.js:938-943`). Fresh activity first so the
    // deadline is armed, then let it lapse to start the hide animation.
    f.synoik().notified_activity_this_iteration = false;
    f.pointer_motion(1., 1.);
    tick(&mut f, 2100);
    assert!(f.synoik().notification_banner.is_visible()); // Hiding, not yet gone
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    update.title = "updated again".to_owned();
    assert_eq!(banner_notify(&mut f, update), id);
    f.settle_animations();
    assert!(f.synoik().notification_banner.is_visible());
    assert_eq!(f.synoik().notification_banner.content_id(), Some(id));
}

/// Clicking the close button destroys DISMISSED and the owed NotificationClosed
/// emission reaches the server channel (`js/ui/messageList.js:725-728`).
#[test]
fn notification_banner_close_click_dismisses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, emitted) = async_channel::unbounded();
    f.synoik().notifications_emit = Some(tx);

    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();

    // Banner geometry: 34em wide in the top-right corner, y = panel(32) + margin(4);
    // the close circle (28px) sits PAD + its 3px margin from the right edge, centered
    // in the header row (`_message-list.scss:152-155`).
    let banner = banner_rect(&mut f, 1);
    let close_x = banner.loc.x + banner.size.w - 6. - 3. - 14.;
    let close_y = banner.loc.y + 6. + 12.;
    pointer_motion_to(&mut f, close_x, close_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(!f.synoik().notification_banner.is_visible());
    assert!(f.synoik().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::Closed {
            id: cid, reason, ..
        } => {
            assert_eq!(cid, id);
            assert_eq!(reason.wire_code(), 2, "close button = DISMISSED");
        }
        _ => panic!("expected a Closed emission"),
    }
}

/// Clicking an action button emits ActivationToken+ActionInvoked (as one paired
/// command with a real token) and destroys the non-resident notification
/// (`js/ui/notificationDaemon.js:218-241`, `js/ui/messageTray.js:431-447`).
/// The action row only exists once the banner expands — here via hover
/// (`js/ui/messageList.js:598-601`, `js/ui/messageTray.js:1102-1105`).
#[test]
fn notification_banner_action_click_emits_and_dismisses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, emitted) = async_channel::unbounded();
    f.synoik().notifications_emit = Some(tx);

    let mut req = banner_req("app", ":1.1");
    req.actions = vec![("ok".to_owned(), "OK".to_owned())];
    let id = banner_notify(&mut f, req);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_expanded());

    // Hovering the shown banner expands it, revealing the action row.
    let banner = banner_rect(&mut f, 1);
    let center_x = banner.loc.x + banner.size.w / 2.;
    pointer_motion_to(&mut f, center_x, banner.loc.y + 44.);
    assert!(f.synoik().notification_banner.is_expanded());

    // Single action button: centered in the action row below the body block.
    let action_y = banner.loc.y + 6. + 24. + 6. + 48. + 6. + 14.;
    pointer_motion_to(&mut f, center_x, action_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(f.synoik().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::ActionInvoked {
            id: aid,
            action,
            token,
            sender,
        } => {
            assert_eq!(aid, id);
            assert_eq!(action, "ok");
            assert!(!token.is_empty(), "a real activation token is minted");
            assert_eq!(sender.as_deref(), Some(":1.1"));
        }
        _ => panic!("expected an ActionInvoked emission"),
    }
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::Closed { reason, .. } => {
            assert_eq!(reason.wire_code(), 2);
        }
        _ => panic!("expected a Closed emission"),
    }
}

/// While a panel popover is open the banner is blocked; queued banners show
/// once it closes (GNOME blocks for the dateMenu box; blocking for QS too is a
/// recorded divergence).
#[test]
fn notification_banner_blocked_by_open_popover() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    assert!(f.synoik().notification_banner.is_visible());

    // Open the calendar via a clock click (panel y < banner y: no overlap).
    let x = clock_center_x(&mut f, 1920.);
    pointer_motion_to(&mut f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.synoik().panel_popover.is_open());
    f.settle_animations();
    f.settle_animations();
    assert!(
        !f.synoik().notification_banner.is_visible(),
        "banners are blocked while a popover is open"
    );

    // A notification arriving while blocked stays queued.
    let queued = banner_notify(&mut f, banner_req("other", ":1.2"));
    assert!(!f.synoik().notification_banner.is_visible());

    // Closing the popover drains the queue.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(queued));
}

/// A banner shown while the user is idle never expires until their first
/// activity, which arms a 2 s deadline (`js/ui/messageTray.js:1092-1133`).
#[test]
fn notification_banner_idle_gates_expiry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // No activity while the pinned clock runs 5 s ahead: the user is idle.
    tick(&mut f, 5000);
    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(id));

    // Long past the normal timeout, still up: waiting for the user.
    tick(&mut f, 30_000);
    f.settle_animations();
    assert!(f.synoik().notification_banner.is_visible());

    // First activity arms the 2 s deadline.
    f.pointer_motion(1., 1.);
    tick(&mut f, 2500);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_visible());
    assert!(f.synoik().notifications.find(id).is_some());
}

/// Activity while the banner is still sliding in also resolves the idle gate:
/// the Showing->Shown transition then arms the short 2 s timeout (GNOME's
/// user-active watch fires during the show animation just the same,
/// `js/ui/messageTray.js:1118-1122`).
#[test]
fn notification_banner_activity_during_show_arms_short_timeout() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Idle at show time; activity arrives while the banner is still Showing.
    tick(&mut f, 5000);
    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(f.synoik().notification_banner.is_visible());
    f.pointer_motion(1., 1.);
    f.settle_animations();
    assert_eq!(f.synoik().notification_banner.content_id(), Some(id));

    // The short timeout applies — without the fix this waited forever.
    tick(&mut f, 2500);
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_visible());
}

/// Open the calendar popover with a clock click.
fn open_calendar(f: &mut Fixture) {
    let x = clock_center_x(f, 1920.);
    pointer_motion_to(f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.synoik().panel_popover.is_open());
}

/// Calendar events flow into the store through `on_calendar_events_msg`, the way
/// the `org.gnome.Shell.CalendarServer` watcher would deliver them, and
/// `has_calendars` gates section visibility (DBusEventSource / `_sync`,
/// `js/ui/calendar.js`, `js/ui/dateMenu.js`).
#[test]
fn calendar_events_flow_into_the_store() {
    use crate::calendar_events::{CalendarEvent, CalendarToSynoik};
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let ev = |id: &str, start: i64, end: i64| CalendarEvent {
        id: id.into(),
        summary: "Meeting".into(),
        start,
        end,
    };

    // No calendars yet → section hidden.
    assert!(!f.synoik().calendar_events.has_calendars());
    f.synoik_state()
        .on_calendar_events_msg(CalendarToSynoik::HasCalendars(true));
    assert!(f.synoik().calendar_events.has_calendars());

    // A batch lands in the store.
    f.synoik_state()
        .on_calendar_events_msg(CalendarToSynoik::EventsAddedOrUpdated(vec![
            ev("uid\n1", 100, 200),
            ev("uid\n2", 300, 400),
        ]));
    assert_eq!(f.synoik().calendar_events.events_for(0, 1000).len(), 2);

    // A removal is a prefix delete.
    f.synoik_state()
        .on_calendar_events_msg(CalendarToSynoik::EventsRemoved(vec!["uid\n1".into()]));
    assert_eq!(f.synoik().calendar_events.events_for(0, 1000).len(), 1);

    // A range change wipes the cache (the watcher sends this before the new
    // range loads) but keeps `has_calendars`.
    f.synoik_state()
        .on_calendar_events_msg(CalendarToSynoik::CacheReset);
    assert!(f.synoik().calendar_events.events_for(0, 1000).is_empty());
    assert!(f.synoik().calendar_events.has_calendars());

    // The server vanishing clears the store and hides the section.
    f.synoik_state()
        .on_calendar_events_msg(CalendarToSynoik::OwnerVanished);
    assert!(!f.synoik().calendar_events.has_calendars());
    assert!(f.synoik().calendar_events.events_for(0, 1000).is_empty());
}

/// Opening the calendar asks the CalendarServer watcher to load exactly the
/// shown month's 42-cell grid range (`js/ui/calendar.js:748` — the per-rebuild
/// `requestRange`; paging re-runs the same `sync_calendar_range`).
#[test]
fn opening_the_calendar_requests_its_grid_range() {
    use crate::calendar_events::SynoikToCalendar;
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let (tx, rx) = async_channel::unbounded();
    f.synoik_state().synoik.calendar_range_emit = Some(tx);

    open_calendar(&mut f);

    let expected = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .calendar
        .grid_range();
    // The open path issued a range request for the shown grid.
    let mut last = None;
    while let Ok(SynoikToCalendar::SetRange { since, until }) = rx.try_recv() {
        last = Some((since, until));
    }
    assert_eq!(
        last,
        Some(expected),
        "opening requests the shown month's grid range"
    );
}

/// Opening the calendar message list acknowledges the whole store exactly
/// once (`js/ui/messageList.js:1193-1199`) and drops queued banners
/// (`js/ui/messageTray.js:1070-1078`); notifications arriving while it is
/// open are pushed into the list but stay unseen; closing never re-acks.
#[test]
fn calendar_message_list_acks_on_open_once() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    banner_notify(&mut f, banner_req("app-a", ":1.1"));
    banner_notify(&mut f, banner_req("app-b", ":1.2"));
    f.settle_animations();
    // The first banner showed (acked); the second sits queued, unseen.
    assert_eq!(f.synoik().notifications.unseen_count(), 1);
    assert!(!f.synoik().notifications.banner_queue.is_empty());

    open_calendar(&mut f);
    assert_eq!(
        f.synoik().notifications.unseen_count(),
        0,
        "opening the list acknowledges everything"
    );
    assert!(
        f.synoik().notifications.banner_queue.is_empty(),
        "acked notifications drop out of the banner queue"
    );
    assert_eq!(
        f.synoik().panel_popover.date_menu().unwrap().list().len(),
        2,
        "the list snapshots the whole store"
    );

    // A notification arriving while open lands in the list WITHOUT an ack.
    let id3 = banner_notify(&mut f, banner_req("app-c", ":1.3"));
    assert_eq!(
        f.synoik().panel_popover.date_menu().unwrap().list().len(),
        3
    );
    assert_eq!(
        f.synoik().notifications.unseen_count(),
        1,
        "arrivals while open stay unseen"
    );

    // Closing does not acknowledge: the still-unseen notification banners as
    // soon as the popover unblocks the tray.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    f.settle_animations();
    assert!(!f.synoik().panel_popover.is_open());
    assert_eq!(
        f.synoik().notification_banner.content_id(),
        Some(id3),
        "the unseen notification banners after close"
    );
}

/// A panel popover grabs input modally: while it is open no window under it may
/// receive pointer focus, so the app can't keep driving the cursor image (a
/// maximized terminal was leaving its I-beam over the clock popover). Mirrors how
/// the screenshot UI / MRU already suppress the underlying surface in
/// `contents_under`.
#[test]
fn open_popover_suppresses_underlying_pointer_focus() {
    use smithay::utils::{Logical, Point};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    let id = f.add_client();
    // A large floating window covering the top-center where the popover opens.
    let _w = map_window_sized(&mut f, id, (1800, 1000), None);

    // A point inside the window, before the popover opens, focuses the window.
    let over_window = Point::<f64, Logical>::from((900., 120.));
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.synoik().contents_under(over_window).surface.is_some(),
        "the window under the pointer normally receives pointer focus"
    );
    assert!(
        f.synoik()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_some(),
        "the seat pointer focuses the window"
    );

    open_calendar(&mut f);

    // A point over the open popover (which sits over the window) must NOT focus
    // the window, so the app can't set the cursor image there.
    let origin = f.synoik().panel_popover.content_location(&output);
    let over_popover = origin + Point::from((50., 50.));
    assert!(
        f.synoik().panel_popover.contains(&output, over_popover),
        "the sampled point is inside the popover content"
    );
    pointer_motion_to(&mut f, over_popover.x, over_popover.y);
    assert!(
        f.synoik().contents_under(over_popover).surface.is_none(),
        "no window under the open popover receives pointer focus"
    );
    assert!(
        f.synoik()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "the seat pointer focus is cleared while the popover is open"
    );
}

/// A panel menu releases its modal grab at the TOP of the close, not at the end of the
/// fade-out. gnome-shell's `PopupMenu.close()` merely *starts* the box-pointer ease and then
/// synchronously emits `open-state-changed, false` (`js/ui/popupMenu.js:1081-1096`), which
/// `PopupMenuManager` turns into `Main.popModal` (`js/ui/popupMenu.js:1487`) — dismissing the
/// `Clutter.Grab`, so mutter re-runs `get_focus_surface` off `notify::is-grabbed`
/// (`meta-wayland-input.c:112-133`) and the window gets `wl_keyboard.enter` back while the menu
/// is still visibly fading. Gating focus on `is_open` (true for the whole fade) instead left the
/// client with no keyboard focus — and no keys and no clicks — for the 150 ms of the fade.
#[test]
fn closing_popover_restores_keyboard_focus_before_the_fade_ends() {
    use crate::synoik::KeyboardFocus;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);
    f.refresh();

    let focused_surface = |f: &mut Fixture| match &f.synoik().keyboard_focus {
        KeyboardFocus::Layout { surface } => surface.clone(),
        other => panic!("expected the layout to hold the keyboard, got {other:?}"),
    };
    let window_surface = focused_surface(&mut f);
    assert!(
        window_surface.is_some(),
        "the mapped window starts with keyboard focus"
    );

    // Opening takes the keyboard away — like `pushModal` at the top of the open animation.
    open_calendar(&mut f);
    f.refresh();
    assert!(
        matches!(f.synoik().keyboard_focus, KeyboardFocus::Popover),
        "the open menu holds the modal grab: {:?}",
        f.synoik().keyboard_focus,
    );

    // Escape starts the close. Do NOT settle: the assertion is about the fade window.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.refresh();

    assert!(
        f.synoik().panel_popover.is_open(),
        "the popover is still on screen, fading out"
    );
    assert_eq!(
        focused_surface(&mut f),
        window_surface,
        "the window has its keyboard focus back before the fade has finished"
    );
    assert!(
        !f.synoik().panel_popover.grabs_input(),
        "and the fading menu no longer holds the grab"
    );

    // And it stays there once the fade settles — the restore is not undone by the settle.
    f.settle_animations();
    f.refresh();
    assert!(!f.synoik().panel_popover.is_open());
    assert_eq!(focused_surface(&mut f), window_surface);
}

/// The panel MessagesIndicator dot (`js/ui/dateMenu.js:787-798`): lit when
/// banners are enabled and there are unseen notifications not still queued for a
/// banner, hidden under DND, and cleared by opening the calendar (which acks
/// everything on map, `js/ui/messageList.js:1193-1199`).
#[test]
fn messages_indicator_reflects_unseen_and_dnd() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    // A LOW notification never banners, so it stays unseen and unqueued — the
    // dot lights immediately (unseen − queued = 1 > 0).
    let mut low = banner_req("app", ":1.1");
    low.urgency = crate::notifications::Urgency::Low;
    banner_notify(&mut f, low);
    assert!(!f.synoik().notification_banner.is_visible());
    assert!(
        f.synoik().panel.messages_indicator_visible(),
        "an unseen low notification lights the dot"
    );

    // Opening the calendar acknowledges everything → the dot clears.
    open_calendar(&mut f);
    assert!(
        !f.synoik().panel.messages_indicator_visible(),
        "opening the list clears the dot"
    );
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();

    // Under DND, an unseen notification does NOT light the dot — GNOME gates the
    // indicator on `show-banners` (`js/ui/dateMenu.js:796-797`).
    f.synoik().gnome_settings.quick_toggles.do_not_disturb = true;
    banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(f.synoik().notifications.unseen_count() > 0);
    assert!(
        !f.synoik().panel.messages_indicator_visible(),
        "DND hides the dot even with unseen notifications"
    );
}

/// Message-list card interactions, end to end through real pointer clicks:
/// the close button dismisses one notification, a body click activates
/// (default action → ActionInvoked; none + resident → survives), and Clear
/// empties the store — all with the popover staying open and the list
/// snapshot tracking every change.
#[test]
fn calendar_message_list_click_close_body_and_clear() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, emitted) = async_channel::unbounded();
    f.synoik().notifications_emit = Some(tx);

    // Three sources with distinct timestamps: a resident one, one with a
    // default action, one plain (newest).
    let mut resident = banner_req("app-a", ":1.1");
    resident.resident = true;
    let rid = banner_notify(&mut f, resident);
    tick(&mut f, 1000);
    let mut with_default = banner_req("app-b", ":1.2");
    with_default.has_default_action = true;
    let did = banner_notify(&mut f, with_default);
    tick(&mut f, 1000);
    let pid = banner_notify(&mut f, banner_req("app-c", ":1.3"));
    f.settle_animations();

    open_calendar(&mut f);
    let output = f.synoik_output(1);
    let origin = f.synoik().panel_popover.content_location(&output);
    // All three cards are reachable now (the list scrolls); they render
    // newest-first.
    let cards = f.synoik().panel_popover.date_menu().unwrap().card_rects();
    assert_eq!(
        cards.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        vec![pid, did, rid],
        "sources render newest-first; scrolling keeps every card reachable"
    );
    assert_eq!(
        f.synoik().panel_popover.date_menu().unwrap().list().len(),
        3,
        "the snapshot still holds everything"
    );

    let click = |f: &mut Fixture, pos: smithay::utils::Point<f64, smithay::utils::Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let rect_center = |rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        smithay::utils::Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
    };

    // Close the newest card: dismissed, gone from store and list; still open.
    let (_, _, close_rect) = cards[0];
    click(&mut f, rect_center(close_rect));
    assert!(f.synoik().notifications.find(pid).is_none());
    assert_eq!(
        f.synoik().panel_popover.date_menu().unwrap().list().len(),
        2
    );
    assert!(f.synoik().panel_popover.is_open(), "the popover stays open");
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::Closed { id, reason, .. } => {
            assert_eq!((id, reason.wire_code()), (pid, 2), "Dismissed on the wire");
        }
        _ => panic!("expected a Closed emission"),
    }

    // Body-click the default-action card: ActionInvoked('default') unicast +
    // destroyed (non-resident) — and the popover CLOSES (activation drops the
    // menu, `js/ui/notificationDaemon.js:370-382`).
    let cards = f.synoik().panel_popover.date_menu().unwrap().card_rects();
    let (_, card, _) = cards[0];
    click(
        &mut f,
        smithay::utils::Point::from((card.loc.x + 30., card.loc.y + card.size.h - 10.)),
    );
    assert!(f.synoik().notifications.find(did).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::ActionInvoked {
            id, action, sender, ..
        } => {
            assert_eq!(id, did);
            assert_eq!(action, "default");
            assert_eq!(sender.as_deref(), Some(":1.2"));
        }
        _ => panic!("expected an ActionInvoked emission"),
    }
    let _ = emitted.recv_blocking().unwrap(); // its Closed
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "activating a notification closes the calendar"
    );

    // Body-click the resident card (no default action): `source.open()`
    // destroys only non-resident notifications — it survives; the popover
    // closes here too.
    open_calendar(&mut f);
    let origin = f.synoik().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: smithay::utils::Point<f64, smithay::utils::Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let cards = f.synoik().panel_popover.date_menu().unwrap().card_rects();
    let (_, card, _) = cards[0];
    click(
        &mut f,
        smithay::utils::Point::from((card.loc.x + 30., card.loc.y + card.size.h - 10.)),
    );
    assert!(
        f.synoik().notifications.find(rid).is_some(),
        "a resident notification survives activation"
    );
    f.settle_animations();
    assert!(!f.synoik().panel_popover.is_open());

    // Clear: everything (resident included) closes; the placeholder is up.
    open_calendar(&mut f);
    let pill = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .clear_pill_rect()
        .unwrap();
    click(&mut f, rect_center(pill));
    assert!(f.synoik().notifications.sources.is_empty());
    assert!(f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .list()
        .is_empty());
    assert!(
        f.synoik().panel_popover.is_open(),
        "Clear keeps the popover open"
    );
}

/// The list card's expand caret (`js/ui/messageList.js:521-538,614-666`):
/// collapsed cards show one body line and no action row; clicking the caret
/// wraps the body and reveals the actions; expansion survives a snapshot push
/// (with its line budget clamped to the remaining space); clicking again
/// collapses; an expanded card's action button emits
/// ActivationToken+ActionInvoked and destroys the notification, closing the
/// popover.
#[test]
fn calendar_message_list_caret_expands_and_actions_invoke() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, emitted) = async_channel::unbounded();
    f.synoik().notifications_emit = Some(tx);

    let mut req = banner_req("app-a", ":1.1");
    req.body = "a long body ".repeat(40).trim_end().to_owned();
    req.actions = vec![
        ("ok".to_owned(), "OK".to_owned()),
        ("no".to_owned(), "No".to_owned()),
    ];
    let id = banner_notify(&mut f, req);
    f.settle_animations();

    open_calendar(&mut f);
    let output = f.synoik_output(1);
    let origin = f.synoik().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: smithay::utils::Point<f64, smithay::utils::Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let rect_center = |rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        smithay::utils::Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
    };
    let dm = |f: &mut Fixture| {
        let card = f.synoik().panel_popover.date_menu().unwrap().card_rects()[0];
        let expand = f
            .synoik()
            .panel_popover
            .date_menu()
            .unwrap()
            .card_expand_rect(card.0);
        let actions = f
            .synoik()
            .panel_popover
            .date_menu()
            .unwrap()
            .card_action_rects(card.0);
        (card, expand, actions)
    };

    // Collapsed: a one-icon-row card, live caret, no action row. The height is the reference box
    // model (`_message-list.scss:83,118-120,160`), not a number to re-baseline when it moves.
    let collapsed_h = {
        use crate::ui::notification_card::{header_band, BODY_ICON, BORDER, PAD};
        BORDER + PAD + header_band() + PAD + BODY_ICON + PAD * 2. + BORDER
    };
    let ((cid, card, _), expand, actions) = dm(&mut f);
    assert_eq!(cid, id);
    assert_eq!(card.size.h, collapsed_h);
    assert!(actions.is_empty(), "actions hidden until expanded");
    let caret = expand.expect("a long body makes the caret live");

    // Caret click: the body wraps to its six-line budget and the action row
    // appears; the popover stays open, the store is untouched.
    click(&mut f, rect_center(caret));
    assert!(f.synoik().panel_popover.is_open());
    assert!(f.synoik().notifications.find(id).is_some());
    let ((_, card, _), expand, actions) = dm(&mut f);
    assert_eq!(
        card.size.h,
        collapsed_h + 5. * 18. + 28. + 6.,
        "six body lines + the action row"
    );
    assert_eq!(actions.len(), 2);
    assert!(expand.is_some(), "the caret stays live to collapse");

    // A snapshot push while open (new notification) keeps the card expanded at
    // its full budget — the list scrolls to fit rather than clamping the body.
    banner_notify(&mut f, banner_req("app-b", ":1.2"));
    let rects = f.synoik().panel_popover.date_menu().unwrap().card_rects();
    assert_eq!(rects.len(), 2, "both cards visible");
    let (aid, a_card, _) = rects[1];
    assert_eq!(aid, id);
    assert_eq!(
        a_card.size.h,
        collapsed_h + 5. * 18. + 28. + 6.,
        "still fully expanded (six lines); the list scrolls, no clamp"
    );

    // Caret click again: collapse back to the flat card.
    let caret = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(id)
        .unwrap();
    click(&mut f, rect_center(caret));
    let rects = f.synoik().panel_popover.date_menu().unwrap().card_rects();
    assert_eq!(rects[1].1.size.h, collapsed_h);
    assert!(f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_action_rects(id)
        .is_empty());

    // Expand once more and invoke the second action: ActionInvoked('no')
    // unicast with a real token, the notification destroyed (non-resident),
    // and the popover closes (the app it raised takes over).
    let caret = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(id)
        .unwrap();
    click(&mut f, rect_center(caret));
    // The expanded card's action row now sits below the second card, past the
    // fold — scroll the list down to bring it into view (as a user would).
    f.synoik().panel_popover.pointer_scroll(
        &output,
        origin + smithay::utils::Point::from((30., 30.)),
        1000.,
    );
    let actions = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_action_rects(id);
    click(&mut f, rect_center(actions[1]));
    assert!(f.synoik().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::ActionInvoked {
            id: aid,
            action,
            token,
            sender,
        } => {
            assert_eq!(aid, id);
            assert_eq!(action, "no");
            assert!(!token.is_empty());
            assert_eq!(sender.as_deref(), Some(":1.1"));
        }
        _ => panic!("expected an ActionInvoked emission"),
    }
    match emitted.recv_blocking().unwrap() {
        crate::notifications::SynoikToNotifications::Closed { reason, .. } => {
            assert_eq!(reason.wire_code(), 2);
        }
        _ => panic!("expected a Closed emission"),
    }
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "invoking an action closes the calendar"
    );
}

/// Notifications from ONE source group into a fanned stack
/// (`NotificationMessageGroup`): collapsed it shows no per-card rects, a click
/// expands it into a vertical list, the header collapse button fans it back,
/// and closing the collapsed stack closes the WHOLE group
/// (`js/ui/messageList.js:1106-1118,1236-1242`). End to end through real clicks.
#[test]
fn calendar_message_list_groups_and_group_close() {
    use smithay::utils::{Logical, Point, Rectangle};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    // Two notifications from the SAME app + sender → one source → one group.
    let id1 = banner_notify(&mut f, banner_req("chat", ":1.5"));
    let id2 = banner_notify(&mut f, banner_req("chat", ":1.5"));
    f.settle_animations();

    open_calendar(&mut f);
    let output = f.synoik_output(1);
    let origin = f.synoik().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: Point<f64, Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let rect_center = |r: Rectangle<f64, Logical>| {
        Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
    };
    let groups = |f: &mut Fixture| f.synoik().panel_popover.date_menu().unwrap().group_rects();

    // Collapsed: one group, not expanded, no per-card interactive rects.
    let g = groups(&mut f);
    assert_eq!(g.len(), 1, "both notifications collapse into one stack");
    assert!(!g[0].2, "the stack starts collapsed");
    assert!(f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_rects()
        .is_empty());

    // A click clear of the top card's close button expands the group.
    let bounds = g[0].1;
    click(
        &mut f,
        Point::from((bounds.loc.x + 20., bounds.loc.y + bounds.size.h - 6.)),
    );
    assert!(f.synoik().panel_popover.is_open());
    assert!(groups(&mut f)[0].2, "clicking the stack expanded it");
    assert_eq!(
        f.synoik()
            .panel_popover
            .date_menu()
            .unwrap()
            .card_rects()
            .len(),
        2,
        "expanded: both cards individually interactive"
    );
    // Expanding is pure UI — the store is untouched.
    assert!(f.synoik().notifications.find(id1).is_some());
    assert!(f.synoik().notifications.find(id2).is_some());

    // The header collapse button fans it back to a stack.
    let key = groups(&mut f)[0].0.clone();
    let collapse = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .group_collapse_rect(&key)
        .expect("expanded group has a collapse button");
    click(&mut f, rect_center(collapse));
    assert!(
        !groups(&mut f)[0].2,
        "the collapse button re-fanned the stack"
    );

    // Closing the collapsed stack closes the WHOLE group.
    let key = groups(&mut f)[0].0.clone();
    let stack_close = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .stack_close_rect(&key)
        .expect("collapsed stack has a top-card close");
    click(&mut f, rect_center(stack_close));
    assert!(f.synoik().notifications.find(id1).is_none());
    assert!(f.synoik().notifications.find(id2).is_none());
    assert!(
        f.synoik()
            .panel_popover
            .date_menu()
            .unwrap()
            .list()
            .is_empty(),
        "closing the group empties the list"
    );
    assert!(
        f.synoik().panel_popover.is_open(),
        "a group close keeps the popover open"
    );
}

/// Hover expands the shown banner (`js/ui/messageTray.js:1102-1105`) — unless
/// it popped up under the pointer, in which case the pointer must leave and
/// come back first (`:978-991`).
#[test]
fn notification_banner_hover_expand_and_under_pointer_guard() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Park the pointer where the banner is about to land — top-right corner, a
    // banner's width in from the edge.
    f.pointer_motion(1., 1.);
    let x = 1920. - 100.;
    pointer_motion_to(&mut f, x, 80.);
    banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    assert!(!f.synoik().notification_banner.is_expanded());
    assert!(
        banner_rect(&mut f, 1).contains(smithay::utils::Point::from((x, 80.))),
        "the parked pointer must really be under the banner, or the guard proves nothing"
    );

    // Hovering in place (it popped up under us) must NOT expand.
    pointer_motion_to(&mut f, x + 1., 80.);
    assert!(
        !f.synoik().notification_banner.is_expanded(),
        "popped-under-pointer: hover without leaving first doesn't expand"
    );

    // Leave, come back: now it expands.
    pointer_motion_to(&mut f, x, 400.);
    pointer_motion_to(&mut f, x, 80.);
    assert!(f.synoik().notification_banner.is_expanded());
}

/// A pointer that moves onto the banner DURING the slide-in registers as
/// hover (GNOME tracks the banner bin from SHOWING, `js/ui/messageTray.js:970-996`)
/// and expands at the SHOWN transition (`:1102-1105`).
#[test]
fn notification_banner_hover_during_slide_expands_when_shown() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    banner_notify(&mut f, banner_req("app", ":1.1"));
    // Mid-slide: move onto the banner's area and stop.
    let banner = banner_rect(&mut f, 1);
    pointer_motion_to(
        &mut f,
        banner.loc.x + banner.size.w / 2.,
        banner.loc.y + 44.,
    );
    assert!(!f.synoik().notification_banner.is_expanded());

    f.settle_animations();
    assert!(
        f.synoik().notification_banner.is_expanded(),
        "the settled pointer expands the banner at the Showing→Shown transition"
    );
}

/// CRITICAL banners auto-expand at show (`js/ui/messageTray.js:1170-1174`):
/// the action row is clickable without any hover.
#[test]
fn notification_banner_critical_auto_expands() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let mut req = banner_req("app", ":1.1");
    req.urgency = crate::notifications::Urgency::Critical;
    req.actions = vec![("ok".to_owned(), "OK".to_owned())];
    banner_notify(&mut f, req);
    assert!(
        f.synoik().notification_banner.is_expanded(),
        "critical expands at show, before any hover"
    );
    f.settle_animations();

    // The action row is present in the hit-test (short body: one line, so the
    // row sits right below the 48px body block).
    let banner = banner_rect(&mut f, 1);
    let output = f.synoik_output(1);
    let action_pos = banner.loc + smithay::utils::Point::from((banner.size.w / 2., 104.));
    assert_eq!(
        f.synoik().notification_banner.hit_test(&output, action_pos),
        Some(crate::ui::notification_banner::BannerHit::Action(0))
    );
}

/// The banner's own buttons light on hover, like the message list's
/// (`%notification_button:hover`, `_drawing.scss:228`): the close circle and each
/// action button, one at a time, while the card body stays hovered throughout.
#[test]
fn notification_banner_buttons_light_under_the_pointer() {
    use crate::ui::notification_card::CardZone;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let mut req = banner_req("app", ":1.1");
    req.urgency = crate::notifications::Urgency::Critical; // auto-expands: the action row is up
    req.actions = vec![("ok".to_owned(), "OK".to_owned())];
    banner_notify(&mut f, req);
    f.settle_animations();
    let banner = banner_rect(&mut f, 1);
    assert_eq!(f.synoik().notification_banner.hovered_zone(), None);

    // Onto the action button (the row below the 48px body block).
    pointer_motion_to(
        &mut f,
        banner.loc.x + banner.size.w / 2.,
        banner.loc.y + 104.,
    );
    assert_eq!(
        f.synoik().notification_banner.hovered_zone(),
        Some(CardZone::Action(0)),
        "the action button under the pointer lights up"
    );

    // Onto the close circle: the highlight moves, it does not stay behind.
    pointer_motion_to(
        &mut f,
        banner.loc.x + banner.size.w - 6. - 3. - 14.,
        banner.loc.y + 6. + 12.,
    );
    assert_eq!(
        f.synoik().notification_banner.hovered_zone(),
        Some(CardZone::Close)
    );

    // On the body but no button: the card darkens, nothing lights.
    pointer_motion_to(&mut f, banner.loc.x + 20., banner.loc.y + 60.);
    assert_eq!(f.synoik().notification_banner.hovered_zone(), None);

    // Off the banner entirely.
    pointer_motion_to(&mut f, 400., 600.);
    assert_eq!(f.synoik().notification_banner.hovered_zone(), None);
}

/// **Divergence from GNOME's centered `_bannerBin`** (`js/ui/messageTray.js:731-736`):
/// the banner hangs off the top-*right* corner, because that is where our dateMenu
/// lives — and its popover is what hosts the message list the banner belongs to. Its
/// right edge lines up with that popover's, so the two come out of the same corner.
#[test]
fn notification_banner_hangs_off_the_right_corner() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    let banner = banner_rect(&mut f, 1);
    assert!(
        banner.loc.x > 1920. / 2.,
        "the banner starts in the right half, not centered: {banner:?}"
    );

    open_calendar(&mut f);
    f.settle_animations();
    let popover_x = popover_origin(&mut f).x;
    let popover_w = f.synoik().panel_popover.content_size().unwrap().w;
    assert_eq!(
        banner.loc.x + banner.size.w,
        popover_x + popover_w,
        "banner and calendar share a right edge"
    );
}

/// The overview app catalog is wired but inert in headless mode: a fresh `Fixture`
/// has a *disconnected* `AppSystem` (nothing installed — the corpus never touches
/// the host app database), and fakes are injectable via `with_parts` with a launch
/// reaching the recorder. This pins the wiring and documents the injection idiom
/// the S3 (dash) and S4 (overview search) tests will use.
#[test]
fn app_system_is_disconnected_and_injectable_headless() {
    use crate::app_system::{
        AppEntry, AppSystem, FakeCatalog, LaunchMode, RecordingLauncher, ResolvedLaunch,
    };

    let mut f = Fixture::new();
    assert_eq!(
        f.synoik().app_system.installed().count(),
        0,
        "headless AppSystem must be inert"
    );
    assert!(f.synoik().app_system.favorites().is_empty());

    let recorder = RecordingLauncher::default();
    let catalog = FakeCatalog::new(vec![AppEntry::fake("org.example.App.desktop", "App")]);
    f.synoik().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder.clone()));

    f.synoik()
        .app_system
        .launch(
            "org.example.App.desktop",
            LaunchMode::Activate,
            &crate::app_system::LaunchContext::bare(get_monotonic_time()),
        )
        .expect("launch reaches the recorder");

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.id, "org.example.App.desktop");
    assert_eq!(calls[0].1, ResolvedLaunch::Default);
}

/// S3 dash: a fixture with a 1920×1080 output, an injected fake `AppSystem` whose
/// catalog holds one entry per `favorites` id, those ids synced into the dash, and
/// the overview open. Returns the launch recorder so a test can assert what a dash
/// click launched. This is the injection idiom of `app_system_is_disconnected_…`
/// wired through to the dash (`sync_dash_favorites`).
fn dash_fixture(favorites: &[&str]) -> (Fixture, crate::app_system::RecordingLauncher) {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let recorder = seed_favorites(&mut f, favorites);

    f.synoik_state().do_action(Action::OpenOverview, false);
    assert!(f.synoik().layout.is_overview_open(), "overview must open");

    (f, recorder)
}

/// Give a fixture's dash some favorites to draw, and a launcher that records what they do.
/// The half of [`dash_fixture`] that doesn't assume the overview — the dock shows the same
/// dash with the overview shut.
fn seed_favorites(f: &mut Fixture, favorites: &[&str]) -> crate::app_system::RecordingLauncher {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let recorder = RecordingLauncher::default();
    let apps = favorites
        .iter()
        .map(|id| AppEntry::fake(id, id))
        .collect::<Vec<_>>();
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.synoik()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.synoik().sync_dash_favorites();

    recorder
}

/// The overview chrome's allocated boxes on output 1 — the same
/// `ControlsManagerLayout` the render and input paths consume.
fn overview_controls(f: &mut Fixture) -> crate::ui::overview_layout::ControlsLayout {
    let output = f.synoik_output(1);
    f.synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor")
}

/// The center of dash tile `i` in output 1's logical coords.
fn dash_tile_center(
    f: &mut Fixture,
    i: usize,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let area = overview_controls(f).dash;
    f.synoik()
        .dash
        .tile_center(i, area)
        .expect("tile index in range")
}

/// `org.gnome.ScreenSaver.Lock` puts the shield down, and `SetActive(false)` raises it.
///
/// Driven through `State::on_screen_saver_msg` — the same entry point the bus task calls — rather
/// than against the model, so the D-Bus plumbing is in the loop
/// ([[test-the-code-not-a-reimplementation]]).
///
/// The screensaver half and the lock half are *different states*: `activate` never sets `locked`
/// (`screenShield.js:586-616`), which is what a blanked screen with `lock-enabled = false` is.
#[test]
fn the_screen_saver_bus_calls_drive_the_shield() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(!f.synoik().screen_shield.is_active(), "starts up");

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(true));
    assert!(
        f.synoik().screen_shield.is_active(),
        "SetActive(true) blanks"
    );
    assert!(
        !f.synoik().screen_shield.is_locked(),
        "...but the screensaver is not a lock"
    );

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(false));
    assert!(!f.synoik().screen_shield.is_active());

    // `Lock` also puts the shield down — the difference is what it takes to raise it.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    assert!(f.synoik().screen_shield.is_active(), "Lock blanks too");

    // The snapshot the bus reads deliberately lags the model: `active` is not published until the
    // curtain has landed, so the slide is not replaced by whatever gsd-power does on
    // `ActiveChanged` (our divergence from GNOME's beat — see `lock-screen-backlog.md` item H).
    assert!(
        !f.synoik().shield_snapshot.lock().unwrap().active,
        "GetActive must not claim the screensaver is up while the curtain is still sliding"
    );

    f.synoik().lock_screen.settle();
    f.synoik().publish_shield_active();
    assert!(
        f.synoik().shield_snapshot.lock().unwrap().active,
        "...and must say so the moment it lands, or gsd never blanks at all"
    );

    // Raising it publishes at once — there is nothing to wait for on the way out.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(false));
    assert!(
        !f.synoik().shield_snapshot.lock().unwrap().active,
        "unlocking must stop claiming the screensaver is on immediately"
    );
}

/// gnome-session saying the seat went idle covers the screen and arms the delayed lock; saying it
/// came back takes both away.
///
/// Driven through `State::on_presence_msg`, the entry point the presence watcher calls, so the
/// timer plumbing is in the loop and not just the model
/// ([[test-the-code-not-a-reimplementation]]). The observable for "a lock is pending" is
/// `lock_timer`, because there is nothing else to look at until it fires — and a leaked timer is
/// exactly the bug that locks a desktop the user is sitting at.
#[test]
fn going_idle_arms_the_lock_and_coming_back_disarms_it() {
    use crate::dbus::gnome_session_presence::{PresenceStatus, PresenceToSynoik};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_presence_msg(PresenceToSynoik::StatusChanged(PresenceStatus::Idle));
    assert!(
        !f.synoik().screen_shield.is_active(),
        "idle fades to black first; it does not cover"
    );
    assert!(f.synoik().fade_timer.is_some(), "the fade is running");
    assert!(
        f.synoik().lock_timer.is_some(),
        "and so is the grace period"
    );

    f.synoik_state()
        .on_presence_msg(PresenceToSynoik::StatusChanged(PresenceStatus::Available));
    assert!(
        f.synoik().fade_timer.is_none(),
        "coming back drops the fade..."
    );
    assert!(
        f.synoik().lock_timer.is_none(),
        "...and the pending lock with it, or the desktop locks under the user"
    );
}

/// A lock with nobody to ask stays a dismissible screensaver, immediately.
///
/// The gate makes the shield undismissible while it waits for gdm, so a request that can never be
/// answered would be a *worse* lockout than the lock it stood in for: covered, unlockable, and
/// unraisable. The headless fixture has no gdm client, which is the same shape as a build without
/// D-Bus or a gdm that failed to start.
#[test]
fn a_lock_with_no_verifier_to_ask_stays_a_screensaver() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // What a build without D-Bus, or a gdm client that failed to start, actually looks like.
    f.synoik_state().synoik.gdm_requests = None;

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    assert!(
        f.synoik().screen_shield.is_active(),
        "the screen is covered"
    );
    assert!(!f.synoik().screen_shield.is_locked(), "but never locked");
    assert!(
        f.synoik().screen_shield.is_dismissible(),
        "and raising it must not wait on an answer nobody will send"
    );
}

/// A status we do not recognise is not idleness.
///
/// gnome-session can grow a new `PresenceStatus`, and mapping an unknown one onto idle would blank
/// the screen for a reason nobody chose.
#[test]
fn an_unknown_presence_status_does_not_blank_the_screen() {
    use crate::dbus::gnome_session_presence::{PresenceStatus, PresenceToSynoik};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_presence_msg(PresenceToSynoik::StatusChanged(PresenceStatus::Unknown(42)));
    assert!(!f.synoik().screen_shield.is_active());
    assert!(f.synoik().lock_timer.is_none());
    assert!(f.synoik().fade_timer.is_none(), "and nothing starts fading");
}

/// logind's `PrepareForSleep(true)` locks before the machine goes down, with no grace period.
///
/// The delay inhibitor is what buys the time to do this at all, so the assertion that matters is
/// that the shield is *covered and asking to lock* by the time the handler returns — anything
/// deferred to a timer would run after the suspend.
#[test]
fn suspending_covers_the_screen_immediately() {
    use crate::dbus::freedesktop_login1::Login1ToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_login1_msg(Login1ToSynoik::PrepareForSleep(true));
    assert!(f.synoik().screen_shield.is_active());
    assert!(
        f.synoik().lock_timer.is_none(),
        "a suspend has no grace period"
    );

    // Resuming wakes the screen but leaves the shield where it is.
    f.synoik_state()
        .on_login1_msg(Login1ToSynoik::PrepareForSleep(false));
    assert!(f.synoik().screen_shield.is_active());
}

/// `disable-lock-screen` makes `Lock` a no-op — the shield does not even go down.
///
/// GNOME returns *before* `activate` (`screenShield.js:638-641`), so a locked-down session does
/// not get a blanked screen out of a `Lock` either. Getting that order wrong blanks a machine
/// whose administrator disabled locking.
#[test]
fn lockdown_makes_the_screen_saver_lock_a_no_op() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik()
        .screen_shield
        .set_settings(crate::screen_shield::ShieldSettings {
            disable_lock_screen: true,
            ..Default::default()
        });

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    assert!(!f.synoik().screen_shield.is_active());
    assert!(!f.synoik().shield_snapshot.lock().unwrap().active);

    // `SetActive` is a different call and is *not* gated by lockdown — the screensaver still
    // blanks, it just never becomes a lock.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(true));
    assert!(f.synoik().screen_shield.is_active());
}

/// Input while the shield is down raises it, and goes no further.
///
/// GNOME's curtain swallows the interaction that dismisses it — the click gesture and the key
/// handler raise the prompt rather than forwarding anything (`unlockDialog.js:570-572`). Letting
/// it through would run the desktop's binds from behind the lock screen, so the discriminating
/// assertion is not "the shield went up" (which a bare `deactivate` would also satisfy) but "and
/// the Super tap did not open the overview".
#[test]
fn input_raises_the_shield_instead_of_reaching_the_desktop() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(true));
    assert!(f.synoik().screen_shield.is_active());

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().screen_shield.is_active(),
        "a key press raises the shield"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and the key that raised it must not also have reached the desktop's binds"
    );
    assert!(
        !f.synoik().shield_snapshot.lock().unwrap().active,
        "GetActive follows the dismissal too"
    );

    // A second tap now behaves normally — the shield is not swallowing input forever.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "with the shield up, Super works again"
    );
}

/// A click raises the shield too, and **both** button edges are swallowed.
///
/// Forwarding the release alone would hand whatever is under the pointer a button-up it never saw
/// pressed — which is how a dismissing click ends up activating a panel button behind the curtain.
#[test]
fn a_click_raises_the_shield_and_neither_edge_reaches_the_desktop() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Park the pointer over the panel's Activities corner, whose click opens the overview — the
    // observable thing a leaked edge would trigger.
    pointer_motion_to(&mut f, 10., 10.);
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(true));

    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        !f.synoik().screen_shield.is_active(),
        "a click raises the shield"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and neither the press nor the release reached the panel behind it"
    );
}

/// The whole locked flow, driven through the real input path: lock, type, answer, unlock.
///
/// This is the test that would have caught every wiring mistake in the slice — it goes through
/// `State::on_shield_key`, so the keys are the ones a seat would deliver, and the verdict comes
/// from `State::on_verifier_event`, so it is gdm's word that unlocks and nothing else.
#[test]
fn a_locked_shield_takes_a_password_and_gdm_decides() {
    use crate::dbus::gdm::{VerifierEvent, VerifierRequest};
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::unlock_dialog::{Page, Status};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // `Lock` covers the screen at once, but must NOT claim to be locked — nothing can unlock it
    // until gdm answers.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    assert!(
        f.synoik().screen_shield.is_active(),
        "the screen is covered"
    );
    assert!(
        !f.synoik().screen_shield.is_locked(),
        "not locked until a verifier exists"
    );

    // gdm opens the channel. `epoch` is whatever the shield asked under; the model owns it, so
    // read it back rather than assuming 1.
    let epoch = 1;
    f.synoik_state()
        .on_verifier_event(VerifierEvent::Ready(epoch));
    assert!(
        f.synoik().screen_shield.is_locked(),
        "a live channel is what locks it"
    );

    // ...and it asks for the password.
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // A key on the clock page raises the prompt AND is kept.
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Clock);
    tap(&mut f, KEY_A);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Prompt);
    assert_eq!(
        f.synoik().unlock_dialog.entry_display(),
        "\u{25cf}",
        "the first keystroke is not eaten by the page flip, and it is masked"
    );

    tap(&mut f, KEY_T);
    assert_eq!(f.synoik().unlock_dialog.entry_display(), "\u{25cf}\u{25cf}");

    // Backspace, then Return sends the answer.
    tap(&mut f, KEY_BACKSPACE);
    assert_eq!(f.synoik().unlock_dialog.entry_display(), "\u{25cf}");
    tap(&mut f, KEY_ENTER);
    assert_eq!(f.synoik().unlock_dialog.status(), Status::Answered);
    assert_eq!(
        f.synoik().unlock_dialog.entry_display(),
        "",
        "the buffer does not outlive the answer"
    );

    // A refusal keeps the shield down and lets the user try again.
    f.synoik_state().on_verifier_event(VerifierEvent::Failed);
    assert!(f.synoik().screen_shield.is_locked(), "still locked");
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    assert!(f.synoik().unlock_dialog.is_entry_live(), "and can retry");

    // Only gdm's verdict raises it.
    f.synoik_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.synoik().screen_shield.is_locked());
    assert!(!f.synoik().screen_shield.is_active(), "the shield is up");
    let _ = VerifierRequest::Cancel;
}

/// The way back from the prompt animates, and unlocking does not snap the page out from under the
/// slide.
///
/// GNOME runs one `_adjustment` for the crossfade: `_showClock` eases it to 0 exactly as
/// `_showPrompt` eases it to 1 (`unlockDialog.js:786-810`), and Escape reaches it through
/// `cancelled` → `_fail` (`:755`, `:846`). So leaving the prompt is the same animation backwards,
/// not a cut. Nothing calls `_showClock` on a *successful* unlock, either — the shield slides away
/// still showing the prompt you authenticated with.
///
/// Both halves were broken by deriving the page from `is_locked()` at render time: `locked` is
/// false while the clock is coming back, and false again the instant gdm accepts.
#[test]
fn leaving_the_prompt_animates_and_survives_the_unlock() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Onto the prompt, and let the crossfade finish so the return trip starts from a clean 1.
    tap(&mut f, KEY_A);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Prompt);
    f.synoik().lock_screen.settle_page();
    let now = crate::utils::get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.page_progress(now),
        1.,
        "on the prompt"
    );

    // Escape goes back to the clock — as an animation, not a jump.
    tap(&mut f, KEY_ESC);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Clock);
    let now = crate::utils::get_monotonic_time();
    assert!(
        f.synoik().lock_screen.page_is_animating(now),
        "the way back owes frames"
    );
    let back = f.synoik().lock_screen.page_progress(now);
    assert!(
        back > 0.,
        "the clock fades in from where the prompt was, it does not cut: {back}"
    );

    // Now unlock from the prompt. The page must stay put: the curtain carries it out.
    tap(&mut f, KEY_A);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Prompt);
    f.synoik().lock_screen.settle_page();
    f.synoik_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.synoik().screen_shield.is_active(), "the shield is up");

    let now = crate::utils::get_monotonic_time();
    assert!(
        f.synoik().lock_screen.is_covering(now),
        "but the curtain is still sliding away"
    );
    assert_eq!(
        f.synoik().lock_screen.page_progress(now),
        1.,
        "and it takes the prompt with it, rather than flipping to the clock mid-slide"
    );
}

/// `Lock` does not answer until the shield is actually on screen — and always answers.
///
/// GNOME defers its reply on `lock-screen-shown` (`shellDBus.js:538-545`), emitted when the
/// curtain's slide completes (`screenShield.js:474-493`). Ours is level-triggered instead of
/// edge-triggered, which is what makes the second and third cases here answer at all: GNOME's own
/// `LockAsync` hangs on both, since `_resetLockScreen` returns early unless the shield is hidden
/// (`:440-445`) and a refused lock never reaches the emit.
#[test]
fn lock_answers_its_caller_only_once_the_shield_is_up() {
    use crate::dbus::gnome_screen_saver::{LockReply, ScreenSaverToSynoik};

    /// Poll the caller's side of the reply channel without blocking.
    fn answered(rx: &async_channel::Receiver<()>) -> bool {
        !matches!(rx.try_recv(), Err(async_channel::TryRecvError::Empty))
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // --- A lock from a bare screen waits for the curtain to land. ---
    let (tx, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(Some(LockReply::for_test(tx))));
    assert!(
        !answered(&rx),
        "answered while the curtain was still on its way down"
    );

    // The slide finishing is what answers. Settling stands in for the 250 ms.
    f.synoik().lock_screen.settle();
    f.synoik().settle_lock_replies();
    assert!(
        answered(&rx),
        "the shield is up and the caller is still waiting"
    );

    // --- A second lock, with the screen already covered, answers at once. ---
    let (tx2, rx2) = async_channel::bounded(1);
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(Some(LockReply::for_test(tx2))));
    assert!(
        answered(&rx2),
        "a lock at an already-covered screen must not wait for an edge that cannot come"
    );

    // --- A refused lock answers rather than hanging. ---
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let mut settings = f.synoik().screen_shield.settings();
    settings.disable_lock_screen = true;
    f.synoik().screen_shield.set_settings(settings);

    let (tx3, rx3) = async_channel::bounded(1);
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(Some(LockReply::for_test(tx3))));
    assert!(!f.synoik().screen_shield.is_active(), "lockdown refused it");
    assert!(
        answered(&rx3),
        "a refused lock left its caller waiting for a screen that will never be covered"
    );
}

/// Caps lock raises a warning on the password prompt, and only there.
///
/// GNOME shows `CapsLockWarning` (`shellEntry.js:162-218`) whenever the outstanding question is a
/// **secret** one — `this._capsLockWarningLabel.visible = secret` (`authPrompt.js:414`). A username
/// question gets none, and neither does the clock page, where there is no entry to mangle.
///
/// Driven by tapping the real Caps Lock key through the input path, so the state comes from xkb
/// exactly as it would on a seat — a test that set the flag by hand could not fail for the thing
/// most likely to break, which is the modifier branch never reporting it
/// ([[test-the-code-not-a-reimplementation]]).
#[test]
fn caps_lock_warns_on_the_password_prompt_only() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Caps lock on the *clock* page warns about nothing: there is no entry yet.
    tap(&mut f, KEY_CAPSLOCK);
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        0.,
        "no warning on the clock page"
    );

    // Onto the prompt. The warning is owed the moment it appears, without another caps press —
    // the state is already on, and GNOME reads the keymap rather than waiting for an event.
    tap(&mut f, KEY_A);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt
    );
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        1.,
        "caps is on and the question is secret"
    );

    // Turning it off takes the warning with it.
    tap(&mut f, KEY_CAPSLOCK);
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        0.,
        "caps lock is off"
    );

    // A non-secret question gets no warning even with caps on.
    tap(&mut f, KEY_CAPSLOCK);
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Username:".to_owned(),
            secret: false,
        });
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        0.,
        "a username question cannot be mangled by caps lock"
    );
}

/// The caps warning is right when the prompt is raised by a *click*, and across a re-lock.
///
/// The state is not carried in on the keystroke: GNOME reads the keymap every time it syncs
/// (`shellEntry.js:192`). Reading a value cached from the shield's own key path instead is wrong
/// for every other way the prompt goes up — lock with caps already on and click, and the warning
/// is missing; lock, unlock, turn caps off, lock again and click, and it is there when it should
/// not be.
#[test]
fn the_caps_warning_is_right_without_a_keystroke() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Caps lock goes on while the session is *unlocked*, so the shield never sees the key.
    tap(&mut f, KEY_CAPSLOCK);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Raise the prompt by clicking, not typing.
    f.synoik_state()
        .on_shield_click(smithay::utils::Point::from((960., 540.)));
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt
    );
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        1.,
        "caps was on before the shield existed, and clicking is not a keystroke"
    );

    // Unlock, turn caps off while unlocked, lock again, and click.
    f.synoik_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.synoik().screen_shield.is_active());
    tap(&mut f, KEY_CAPSLOCK);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(2));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state()
        .on_shield_click(smithay::utils::Point::from((960., 540.)));
    f.synoik().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.synoik().lock_screen.caps_alpha(now),
        0.,
        "caps went off while unlocked; a warning here is a lie"
    );
}

/// Shift and caps lock do not raise the prompt; anything else does.
///
/// GNOME returns early for exactly those four keysyms and lets everything else through to
/// `_showPrompt()` (`unlockDialog.js:677-682`). They are the keys you press *before* the one you
/// meant, so waking the prompt on them would eat the modifier of the first character.
#[test]
fn shift_and_caps_do_not_wake_the_prompt() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    tap(&mut f, KEY_LEFTSHIFT);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Clock, "shift waits");
    tap(&mut f, KEY_CAPSLOCK);
    assert_eq!(f.synoik().unlock_dialog.page(), Page::Clock, "so does caps");

    // Ctrl is not in GNOME's list: it raises the prompt like any other key.
    tap(&mut f, KEY_LEFTCTRL);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        Page::Prompt,
        "ctrl is not one of the four, so it wakes the prompt"
    );
}

/// Keys typed at a **locked** shield must not raise it, and must not reach the desktop.
///
/// The unlocked shield raises on anything (it is a screensaver). The locked one must not — and the
/// discriminating half is that a bind like Super does not fire behind it either, which a test that
/// only checks `is_locked` would miss entirely.
#[test]
fn a_locked_shield_swallows_keys_instead_of_raising() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.synoik().screen_shield.is_locked());

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();

    assert!(
        f.synoik().screen_shield.is_active(),
        "a locked shield does not raise on a keypress"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and the key did not reach the desktop behind it"
    );
}

/// Ctrl+Alt+F<n> must reach the VT switch from a locked screen.
///
/// This is the escape hatch from a lock screen that has gone wrong, so it has to work *before*
/// anything in the unlock dialog can fail. Swallowing it — which the first cut of the shield's key
/// handling did — turns any compositor bug behind the curtain into an unrecoverable session, and
/// "open a second VT first" is no help when the key that reaches it is eaten.
///
/// The observable is the *page*: a key the shield consumes raises the prompt (that is the whole
/// point of `on_shield_key`), so a shield that stays on the clock is one that let the key past.
#[test]
fn a_locked_shield_still_lets_ctrl_alt_fn_through() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.synoik().screen_shield.is_locked());

    f.key_press(KEY_LEFTCTRL);
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F2);
    f.key_release(KEY_LEFTALT);
    f.key_release(KEY_LEFTCTRL);

    assert!(
        f.synoik().screen_shield.is_locked(),
        "still locked, of course"
    );
    assert_eq!(
        f.synoik_state().backend.headless().last_vt(),
        Some(2),
        "Ctrl+Alt+F2 must reach the VT switch from behind the curtain"
    );

    // The control: an ordinary key does NOT get through — it is typed at the shield instead.
    // Without this the assertion above would pass for a shield that forwarded everything.
    tap(&mut f, KEY_A);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        Page::Prompt,
        "an ordinary key is still swallowed by the shield"
    );
}

/// logind's `Unlock` raises the shield — this is how gdm's own login screen unlocks you.
///
/// You switch to gdm's VT, authenticate there, gdm tells logind, and logind signals the session.
/// Without this the VT switches back and the shield is still up, with no way to tell it what
/// happened. `loginctl lock-session` / `unlock-session` are the same two signals by hand.
#[test]
fn logind_lock_and_unlock_drive_the_shield() {
    use crate::dbus::freedesktop_login1::Login1ToSynoik;
    use crate::dbus::gdm::VerifierEvent;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_login1_msg(Login1ToSynoik::SessionLock(true));
    assert!(
        f.synoik().screen_shield.is_active(),
        "Lock covers the screen"
    );
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.synoik().screen_shield.is_locked());

    // ...and gdm, having authenticated on its own VT, unlocks us.
    f.synoik_state()
        .on_login1_msg(Login1ToSynoik::SessionLock(false));
    assert!(
        !f.synoik().screen_shield.is_locked(),
        "Unlock must actually unlock — otherwise gdm authenticates you into a locked screen"
    );
    assert!(
        !f.synoik().screen_shield.is_active(),
        "and raise the shield"
    );
}

/// If the unlock channel dies, the lock drops to a dismissible screensaver rather than trapping
/// the session.
///
/// A locked shield whose verifier is gone can never be answered: gdm's `answer_query` no-ops on a
/// dead conversation *after* replying successfully, so the user gets no error and no progress.
/// Dropping the lock is a deliberate divergence — GNOME leaves the dialog stuck — and it is not a
/// weakening, since anyone who can kill gdm as root can already read the session.
#[test]
fn losing_the_unlock_channel_does_not_trap_the_session() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.synoik().screen_shield.is_locked());

    f.synoik_state().on_verifier_event(VerifierEvent::Lost);
    assert!(!f.synoik().screen_shield.is_locked(), "the lock is dropped");
    assert!(
        f.synoik().screen_shield.is_active(),
        "the screen stays covered"
    );

    // ...and it can now be dismissed, which is the whole point.
    tap(&mut f, KEY_A);
    assert!(!f.synoik().screen_shield.is_active());
}

/// A dash click does three different things depending on the app's state, and only one of them
/// is a launch — `AppIcon.activate` (`appDisplay.js:3056-3071`) over `shell_app_activate_full`
/// (`shell-app.c:497-535`).
///
/// STOPPED launches; STARTING does nothing at all (`shell-app.c:526-527` is an empty arm);
/// RUNNING activates the app's most recently used window. Ctrl- and middle-click ask a *running*
/// app for a new window instead, gated on `can_open_new_window`.
///
/// Relaunching a running app is the bug this pins: it opens a startup sequence, which is what
/// put a busy cursor behind every dash click on an app that was already open. It is also why
/// dropping the Ctrl modifier *looked* fine on a terminal — whose plain activation opens a window
/// anyway — and did nothing on Files, whose D-Bus `Activate` presents the window it already has.
#[test]
fn a_dash_click_launches_focuses_or_opens_a_new_window_by_state() {
    use crate::app_system::{
        AppEntry, AppSystem, DesktopAction, FakeCatalog, RecordingLauncher, ResolvedLaunch,
    };

    let files = AppEntry {
        actions: vec![DesktopAction {
            id: "new-window".to_owned(),
            name: "New Window".to_owned(),
        }],
        ..AppEntry::fake("org.example.Files.desktop", "Files")
    };
    let calc = AppEntry::fake("org.example.Calc.desktop", "Calc");

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let recorder = RecordingLauncher::default();
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![files, calc])),
        Box::new(recorder.clone()),
    );
    f.synoik().app_system.set_favorites(vec![
        "org.example.Files.desktop".to_owned(),
        "org.example.Calc.desktop".to_owned(),
    ]);
    f.synoik().sync_dash_favorites();

    // Files runs with two windows; Calc stays stopped.
    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.Files");
    let older = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Files");
    let newer = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    let click = |f: &mut Fixture, i: usize| {
        f.synoik_state().do_action(Action::OpenOverview, false);
        let c = dash_tile_center(f, i);
        pointer_motion_to(f, c.x, c.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let verbs = |r: &RecordingLauncher| {
        r.calls
            .borrow()
            .iter()
            .map(|(entry, verb, _)| (entry.id.clone(), verb.clone()))
            .collect::<Vec<_>>()
    };

    // Focus the older window, so "most recently used" is a real choice and not just "the one
    // that happens to be focused".
    // The focus *timestamp* is what orders the tab list, and it is stamped when the seat's
    // keyboard focus actually moves — so this needs a real focus round trip, not just a call.
    let older_window = f.synoik().find_window_by_id(older).unwrap();
    f.synoik_state().focus_window(&older_window);
    f.synoik_state().update_keyboard_focus();
    f.double_roundtrip(client);
    f.synoik_complete_animations();
    assert_eq!(f.synoik().layout.focus().unwrap().id(), older);
    let _ = newer;

    // RUNNING, no modifier: focus its most recent window, and *do not launch*.
    click(&mut f, 0);
    f.synoik_complete_animations();
    assert!(
        recorder.calls.borrow().is_empty(),
        "a running app must not be relaunched — that is what opens the spurious startup sequence"
    );
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        older,
        "it activates the app's most recently used window"
    );
    assert_eq!(
        f.synoik().app_system.app_state("org.example.Files.desktop"),
        crate::app_system::AppState::Running,
        "and leaves it RUNNING, not STARTING — no busy cursor"
    );

    // RUNNING + Ctrl: the app's `new-window` desktop action.
    f.key_press(KEY_LEFTCTRL);
    click(&mut f, 0);
    f.key_release(KEY_LEFTCTRL);
    assert_eq!(
        verbs(&recorder),
        vec![(
            "org.example.Files.desktop".to_owned(),
            ResolvedLaunch::Action("new-window".to_owned())
        )],
        "Ctrl-click on a running app asks for a new window"
    );
    recorder.calls.borrow_mut().clear();

    // STOPPED + Ctrl: a plain launch — launching *is* opening the window.
    f.key_press(KEY_LEFTCTRL);
    click(&mut f, 1);
    f.key_release(KEY_LEFTCTRL);
    assert_eq!(
        verbs(&recorder),
        vec![(
            "org.example.Calc.desktop".to_owned(),
            ResolvedLaunch::Default
        )],
        "a stopped app ignores the modifier"
    );
    recorder.calls.borrow_mut().clear();

    // STARTING: nothing at all. Calc is mid-launch after the click above.
    assert_eq!(
        f.synoik().app_system.app_state("org.example.Calc.desktop"),
        crate::app_system::AppState::Starting
    );
    click(&mut f, 1);
    assert!(
        recorder.calls.borrow().is_empty(),
        "clicking an app that is already coming up must not start it again"
    );
}

/// `can_open_new_window` honours an app's `SingleMainWindow` / `X-GNOME-SingleWindow`
/// declaration.
///
/// `shell_app_can_open_new_window` (`shell-app.c:601-672`) reaches for the key with
/// `g_desktop_app_info_has_key` and only then reads it, so a declared `false` and an absent key
/// are different answers: the first is a positive yes, the second carries on down the ladder.
///
/// **That difference is not observable yet**, and this test does not pretend otherwise — the
/// rungs below the key currently bottom out in GNOME's own "err on the side of yes" default, so
/// absent and `false` both end at yes. The tri-state is modelled now because the rung that will
/// make them diverge — a unique GtkApplication with no new-window action, which is what answers
/// *no* for apps like System Monitor — needs `gtk_shell1.set_dbus_properties`, a protocol we do
/// not serve. Collapsing the key to a plain boolean today would be invisible and wrong later.
#[test]
fn a_single_window_app_cannot_be_asked_for_a_new_window() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let single = AppEntry {
        single_main_window: Some(true),
        ..AppEntry::fake("org.example.Single.desktop", "Single")
    };
    let declared_multi = AppEntry {
        single_main_window: Some(false),
        ..AppEntry::fake("org.example.Multi.desktop", "Multi")
    };
    let silent = AppEntry::fake("org.example.Silent.desktop", "Silent");

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![single, declared_multi, silent])),
        Box::new(RecordingLauncher::default()),
    );

    let client = f.add_client();
    for app in [
        "org.example.Single",
        "org.example.Multi",
        "org.example.Silent",
    ] {
        map_window_for_app(&mut f, client, app);
    }
    f.synoik_complete_animations();

    let can = |f: &mut Fixture, id: &str| f.synoik().app_system.can_open_new_window(id);
    assert!(
        !can(&mut f, "org.example.Single.desktop"),
        "SingleMainWindow=true is a declaration that there is no new window to open"
    );
    assert!(
        can(&mut f, "org.example.Multi.desktop"),
        "a declared `false` is a positive yes"
    );
    assert!(
        can(&mut f, "org.example.Silent.desktop"),
        "and an app that declares nothing falls through to the compatibility default — which \
         today gives the same answer, see the note above"
    );
}

/// A left-click on a dash favorite launches it (plain `Activate` — all our apps
/// are stopped in S3, `appDisplay.js:3060`) and closes the overview, GNOME's
/// dash-icon behavior (`dash.js`/`appDisplay.js` `activate` → `_animateOverview`).
#[test]
fn overview_dash_favorite_click_launches_and_closes() {
    use crate::app_system::ResolvedLaunch;

    let (mut f, recorder) = dash_fixture(&["a.desktop", "b.desktop"]);
    let center = dash_tile_center(&mut f, 1);

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one favorite launched");
    assert_eq!(calls[0].0.id, "b.desktop", "the clicked favorite launched");
    assert_eq!(calls[0].1, ResolvedLaunch::Default);
    assert!(
        !f.synoik().layout.is_overview_open(),
        "launching a favorite closes the overview"
    );
}

/// The overview's icons are St.Buttons, so the launch happens on the *release*,
/// not the press: `ClutterClickGesture` only completes when the button is lifted
/// (`clutter-click-gesture.c:68-81`; StButton leaves `recognize-on-press` off,
/// `st-button.c:429-435`). The press alone must do nothing — that is what leaves
/// room for a press to start a drag instead.
#[test]
fn overview_dash_favorite_launches_on_release_not_press() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.synoik_complete_animations();
    assert!(
        recorder.calls.borrow().is_empty(),
        "the press alone must not launch"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "and must not close the overview"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        recorder.calls.borrow().len(),
        1,
        "the release completes the click"
    );
    assert!(!f.synoik().layout.is_overview_open());
}

/// ...and the release only counts if it lands on the same widget: lift the button
/// somewhere else and the click is cancelled (`clutter-click-gesture.c:74-79`,
/// which cancels when the press gesture is no longer pressed on the actor).
#[test]
fn overview_dash_release_off_the_icon_does_not_launch() {
    let (mut f, recorder) = dash_fixture(&["a.desktop", "b.desktop"]);
    let pressed = dash_tile_center(&mut f, 0);
    let other = dash_tile_center(&mut f, 1);

    pointer_motion_to(&mut f, pressed.x, pressed.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, other.x, other.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "releasing over a different icon launches nothing"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "and leaves the overview open"
    );
}

/// A fixture whose catalog has more apps than the dash pins, so the app grid has
/// something to drag *into* the dash.
fn favorites_and_grid_fixture(
    all: &[&str],
    favorites: &[&str],
) -> (Fixture, crate::app_system::RecordingLauncher) {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let recorder = RecordingLauncher::default();
    let apps = all
        .iter()
        .map(|id| AppEntry::fake(id, id))
        .collect::<Vec<_>>();
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.synoik()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.synoik().sync_dash_favorites();
    f.synoik().sync_app_grid();

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik().layout.toggle_app_grid();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");
    f.settle_animations();

    (f, recorder)
}

/// The favourites, in dash order — what `favorite-apps` would be written as.
fn dash_favorites(f: &mut Fixture) -> Vec<String> {
    f.synoik()
        .app_system
        .favorite_ids()
        .iter()
        .map(|s| s.to_owned())
        .collect()
}

/// Dropping an app grid icon on the dash pins it, at the slot it was dropped on —
/// gnome-shell's `Dash.acceptDrop` calls `addFavoriteAtPos` with the placeholder's
/// index (`dash.js:942-987`).
#[test]
fn overview_dragging_a_grid_icon_onto_the_dash_pins_it_at_that_slot() {
    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop"],
    );
    // The grid holds exactly the non-favourite.
    assert_eq!(
        f.synoik().app_grid.entry_id(0),
        Some("c.desktop"),
        "the grid should hold the one app that is not pinned"
    );

    let grid_area = overview_controls(&mut f).app_display;
    let from = f
        .synoik()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    // Drag onto the *first* dash tile: dropping on its left half aims at slot 0.
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x - 20., first.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.synoik().dash.drop_slot(),
        Some(0),
        "hovering the front of the dash must open the gap at slot 0"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(f.synoik().dash.drop_slot(), None, "the drop closes the gap");
    assert_eq!(
        dash_favorites(&mut f),
        vec!["c.desktop", "a.desktop", "b.desktop"],
        "the dropped app must be pinned at the slot it was dropped on"
    );
}

/// Search results are drag sources too — they are the same `AppIcon` the grid uses.
#[test]
fn overview_dragging_a_search_result_onto_the_dash_pins_it() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let recorder = RecordingLauncher::default();
    let catalog = FakeCatalog::new(vec![
        AppEntry::fake("a.desktop", "a.desktop"),
        AppEntry::fake("c.desktop", "c.desktop"),
    ]);
    *catalog.search_result.borrow_mut() = vec![vec!["c.desktop".to_owned()]];
    f.synoik().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder));
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".to_owned()]);
    f.synoik().sync_dash_favorites();

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    tap(&mut f, KEY_A);
    f.settle_animations();
    assert_eq!(
        f.synoik().overview_search.result_id(0),
        Some("c.desktop"),
        "the search must list the app we are about to drag"
    );

    let area = overview_controls(&mut f).into();
    let from = f
        .synoik()
        .overview_search
        .result_center(0, area)
        .expect("result tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x - 20., first.y);
    assert!(
        f.synoik().app_drag.is_some(),
        "a search result must be draggable — it was not a drag source at all before"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        dash_favorites(&mut f),
        vec!["c.desktop", "a.desktop"],
        "a search result dropped on the dash must be pinned like a grid icon"
    );
}

/// A favourite dropped immediately before or after itself is a no-op, not a reorder
/// (`dash.js:909-913` clears the placeholder for those two positions).
#[test]
fn overview_dropping_a_favorite_next_to_itself_changes_nothing() {
    let (mut f, _recorder) =
        favorites_and_grid_fixture(&["a.desktop", "b.desktop"], &["a.desktop", "b.desktop"]);
    let before = dash_favorites(&mut f);

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Move enough to start the drag, but stay over its own tile.
    pointer_motion_to(&mut f, first.x + 20., first.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.synoik().dash.drop_slot(),
        None,
        "no gap opens before or after the dragged favourite itself"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        dash_favorites(&mut f),
        before,
        "dropping a favourite next to itself must not reorder it"
    );
}

/// Reordering within the dash: an app already pinned moves rather than being pinned
/// twice (`Dash.acceptDrop` picks `moveFavoriteToPos` for a source that is already a
/// favourite, `dash.js:979-983`). Removing it first shifts the tail down one, which is
/// why the target index is not simply the slot.
#[test]
fn overview_dragging_a_favorite_across_the_dash_reorders_it() {
    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop", "c.desktop"],
    );
    assert_eq!(
        dash_favorites(&mut f),
        vec!["a.desktop", "b.desktop", "c.desktop"]
    );

    // Pick up the first favourite and drop it past the last one. That takes two moves:
    // the strip past the final tile only exists once a gap has widened the box (see
    // `Dash::drop_slot_at`), so hover a middle tile first and *then* slide right.
    let first = dash_tile_center(&mut f, 0);
    let past_end = dash_tile_center(&mut f, 3);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    let middle = dash_tile_center(&mut f, 2);
    pointer_motion_to(&mut f, middle.x, middle.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.synoik().dash.drop_slot(),
        Some(2),
        "hovering the third tile opens the gap before it"
    );
    // The gap eases open, and the strip past the last tile is made *of* that width —
    // so it isn't there to aim at until the animation lands.
    f.settle_animations();
    // Slightly left of where the show-apps button *was*: opening the gap widened the
    // pill, and a centered pill grows both ways, so everything slid half a tile left.
    pointer_motion_to(&mut f, past_end.x - 20., past_end.y);
    assert_eq!(
        f.synoik().dash.drop_slot(),
        Some(3),
        "past the last favourite clamps to the end of them, not into the running zone"
    );
    assert!(
        !f.synoik().app_drag.as_ref().unwrap().unpin,
        "the open gap pushed the show-apps button right, so this is the strip, not it"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["b.desktop", "c.desktop", "a.desktop"],
        "the dragged favourite must land at the end, not be duplicated or dropped"
    );

    // And a drop *between* two others lands between them: removing the dragged app
    // first shifts the tail down, so slot 2 of [b, c, a] is index 1.
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    let middle = dash_tile_center(&mut f, 2);
    pointer_motion_to(&mut f, middle.x - 10., middle.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        dash_favorites(&mut f),
        vec!["c.desktop", "b.desktop", "a.desktop"],
        "a drop between two favourites must land between them"
    );
}

/// Right-clicking an app icon pops up its context menu (`AppIcon.popupMenu`,
/// `appDisplay.js:3027`), and the menu's rows are the app's: the launch verbs, then the
/// favourite toggle labelled for what it would do.
#[test]
fn overview_right_clicking_an_icon_opens_its_context_menu() {
    use crate::ui::dash::DashHit;

    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop"],
    );

    // On a pinned app, the toggle offers to unpin.
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    assert!(
        f.synoik().panel_popover.is_app_menu(),
        "a right-click must open the menu on the PRESS, not wait for the release \
         (`recognize_on_press: true`, `appDisplay.js:2981-2986`)"
    );
    assert_eq!(
        f.synoik().panel_popover.app_menu().unwrap().labels(),
        vec!["New Window", "Unpin"],
    );
    f.pointer_button(BTN_RIGHT, ButtonState::Released);
    assert!(
        f.synoik().panel_popover.is_app_menu(),
        "and the release must not take it away again"
    );

    // The menu grabs, so nothing under it hovers — except its own icon, which stays
    // highlighted for as long as the menu is up.
    let second = dash_tile_center(&mut f, 1);
    pointer_motion_to(&mut f, second.x, second.y);
    assert_eq!(
        f.synoik().dash.hovered_for_test(),
        Some(DashHit::App(0)),
        "the icon whose menu is open keeps its highlight, and the icon the pointer \
         moved to must NOT take it (the menu holds a grab)"
    );

    // A dash icon's menu opens *upward* — the dash is at the bottom of the screen, so
    // `popupMenuSide: St.Side.BOTTOM` (`dash.js:27`) puts the arrow under the box.
    let output = f.synoik().global_space.outputs().next().unwrap().clone();
    let menu = f.synoik().panel_popover.content_location(&output);
    let menu_h = f
        .synoik()
        .panel_popover
        .app_menu()
        .unwrap()
        .logical_size()
        .h;
    let dash_area = overview_controls(&mut f).dash;
    let tile = f.synoik().dash.tile_rect(0, dash_area).unwrap();
    assert!(
        menu.y + menu_h <= tile.loc.y,
        "the dash menu must sit entirely above its icon (bottom {}, icon top {})",
        menu.y + menu_h,
        tile.loc.y
    );

    // Dismiss, then the same on an app that is not pinned. The settle matters: a
    // popover stays `is_open` while it fades out, and a press during that window is a
    // dismissal (it lands on the still-grabbing menu), not a new menu.
    f.synoik().panel_popover.close();
    f.settle_animations();
    let grid_area = overview_controls(&mut f).app_display;
    let unpinned = f
        .synoik()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, unpinned.x, unpinned.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    assert_eq!(
        f.synoik().panel_popover.app_menu().unwrap().labels(),
        vec!["New Window", "Pin to Dash"],
        "an app that is not pinned is offered the pin"
    );

    // A grid icon takes `AppIcon`'s default `St.Side.LEFT`, so its menu opens to the
    // icon's right instead (`appDisplay.js:2928`).
    let menu = f.synoik().panel_popover.content_location(&output);
    let tile = f.synoik().app_grid.entry_rect(0, grid_area).unwrap();
    assert!(
        menu.x >= tile.loc.x + tile.size.w,
        "the grid menu must sit to the right of its icon (menu left {}, icon right {})",
        menu.x,
        tile.loc.x + tile.size.w
    );
}

/// **Lifecycle L4.** A *running* app's menu grows the rows only a running app has:
/// the "Open Windows" section, one row per window labelled with its title
/// (`_updateWindowsSection`, `appMenu.js:262-291`), and "Quit" (`_updateQuitItem`,
/// `:136-138`). Picking a window row raises it and leaves the overview
/// (`Main.activateWindow`, `:285`); "Quit" closes the app's windows and does *not*
/// leave the overview — gnome-shell's handler is bare (`:99-100`).
#[test]
fn overview_the_context_menu_of_a_running_app_lists_its_windows_and_offers_quit() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".into()]);
    f.synoik().sync_dash_favorites();

    // Two windows of the app, the second titled.
    let mut surfaces = Vec::new();
    for title in ["First doc", ""] {
        let window = f.client(client).create_window();
        let surface = window.surface.clone();
        window.set_app_id("a");
        if !title.is_empty() {
            window.set_title(title);
        }
        window.commit();
        f.roundtrip(client);
        let window = f.client(client).window(&surface);
        window.attach_new_buffer();
        window.set_size(400, 300);
        window.ack_last_and_commit();
        f.double_roundtrip(client);
        surfaces.push(surface);
    }
    assert_eq!(f.synoik().app_system.running()[0].n_windows(), 2);

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let labels = f.synoik().panel_popover.app_menu().unwrap().labels();
    assert_eq!(
        labels[0], "Open Windows",
        "the window section leads the menu, headed by its labelled separator"
    );
    assert!(
        labels.contains(&"First doc"),
        "a window row is labelled with its title, got {labels:?}"
    );
    assert!(
        labels.iter().filter(|l| **l == "A").count() == 1,
        "an untitled window falls back to the app's name, got {labels:?}"
    );
    assert_eq!(
        labels.last(),
        Some(&"Quit"),
        "Quit closes the menu, and only a running app has it"
    );

    // Quit closes every window of the app, and stays in the overview.
    let output = f.synoik().global_space.outputs().next().unwrap().clone();
    let origin = f.synoik().panel_popover.content_location(&output);
    let row = f
        .synoik()
        .panel_popover
        .app_menu()
        .unwrap()
        .row_center("Quit")
        .expect("the menu has a Quit row");
    let at = origin + row;
    pointer_motion_to(&mut f, at.x, at.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(client);

    for surface in &surfaces {
        assert!(
            f.client(client).window(surface).close_requested,
            "Quit must ask every window of the app to close"
        );
    }
    assert!(
        f.synoik().layout.is_overview_open(),
        "Quit does not leave the overview — gnome-shell's handler has no hide()"
    );
}

/// An "Open Windows" row raises that window and leaves the overview —
/// `Main.activateWindow(window)` (`appMenu.js:284-286`), which is one of the
/// `AppMenu` handlers that does hide it.
#[test]
fn overview_the_context_menu_raises_the_window_row_you_pick() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".into()]);
    f.synoik().sync_dash_favorites();

    let mut windows = Vec::new();
    for title in ["First doc", "Second doc"] {
        let window = f.client(client).create_window();
        let surface = window.surface.clone();
        window.set_app_id("a");
        window.set_title(title);
        window.commit();
        f.roundtrip(client);
        let window = f.client(client).window(&surface);
        window.attach_new_buffer();
        window.set_size(400, 300);
        window.ack_last_and_commit();
        f.double_roundtrip(client);
        windows.push(f.synoik().layout.focus().unwrap().window.clone());
    }
    // The second one is focused; the row must move focus to the first.
    assert_eq!(f.synoik().layout.focus().unwrap().window, windows[1]);

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle_animations();
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let output = f.synoik().global_space.outputs().next().unwrap().clone();
    let origin = f.synoik().panel_popover.content_location(&output);
    let row = f
        .synoik()
        .panel_popover
        .app_menu()
        .unwrap()
        .row_center("First doc")
        .expect("the menu lists the first window");
    let at = origin + row;
    pointer_motion_to(&mut f, at.x, at.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();

    assert_eq!(
        f.synoik().layout.focus().unwrap().window,
        windows[0],
        "the row must raise the window it names"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "raising a window leaves the overview"
    );
}

/// A *stopped* app's menu has neither of those rows: Quit is hidden below RUNNING
/// (`appMenu.js:137`), and there are no windows to list.
#[test]
fn overview_the_context_menu_of_a_stopped_app_has_no_windows_and_no_quit() {
    let (mut f, _recorder) = favorites_and_grid_fixture(&["a.desktop"], &["a.desktop"]);

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let labels = f.synoik().panel_popover.app_menu().unwrap().labels();
    assert!(
        !labels.contains(&"Open Windows"),
        "a stopped app has no window section, got {labels:?}"
    );
    assert!(
        !labels.contains(&"Quit"),
        "and nothing to quit, got {labels:?}"
    );
}

/// Picking the favourite toggle pins or unpins, and — unlike the launch rows — leaves
/// the overview up (`appMenu.js:74-80` has no `Main.overview.hide()`).
#[test]
fn overview_the_context_menu_pins_and_unpins() {
    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop"],
    );
    let output = f.synoik().global_space.outputs().next().unwrap().clone();

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let origin = f.synoik().panel_popover.content_location(&output);
    let row = f
        .synoik()
        .panel_popover
        .app_menu()
        .unwrap()
        .row_center("Unpin")
        .expect("the menu has an Unpin row");
    let at = origin + row;
    pointer_motion_to(&mut f, at.x, at.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["b.desktop"],
        "the row must unpin the app"
    );
    assert!(
        !f.synoik().panel_popover.is_open(),
        "activating any popup-menu item closes the menu"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "but pinning must not leave the overview — only the launch rows do that"
    );
}

/// An empty dash is a bare show-apps button, with no run to aim a drop at. gnome-shell
/// reserves a placeholder-sized target for the duration of the drag
/// (`EmptyDropTargetItem`, `dash.js:410-414`) and forces the slot to 0
/// (`dash.js:894-895`), which is what makes pinning the *first* favourite possible at
/// all.
#[test]
fn overview_an_empty_dash_reserves_a_drop_target_while_dragging() {
    let (mut f, _recorder) = favorites_and_grid_fixture(&["a.desktop"], &[]);
    assert_eq!(
        f.synoik().dash.item_id(0),
        None,
        "the dash must start empty for this to be the empty-dash path"
    );

    let area = overview_controls(&mut f).dash;
    let idle_w = f.synoik().dash.pill_box(area).size.w;

    let grid_area = overview_controls(&mut f).app_display;
    let from = f
        .synoik()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x, from.y + 40.);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");

    let pill = f.synoik().dash.pill_box(area);
    assert_eq!(
        pill.size.w - idle_w,
        32.,
        "the drag must reserve `$dash_placeholder_size` of run for the drop target"
    );

    // Anywhere in that reserved run is slot 0.
    pointer_motion_to(&mut f, pill.loc.x + 15., pill.loc.y + 50.);
    assert_eq!(
        f.synoik().dash.drop_slot(),
        Some(0),
        "an empty dash always drops at the start"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["a.desktop"],
        "the drop must pin the first favourite"
    );
    assert_eq!(
        f.synoik().dash.pill_box(area).size.w,
        idle_w + 80.,
        "and the target must be released, leaving the pill one tile wider than empty"
    );
}

/// The show-apps button doubles as the unpin target for the duration of a drag:
/// gnome-shell relabels it and hovers it (`ShowAppsIcon.setDragApp`, `dash.js:236-247`)
/// and its `acceptDrop` removes the favourite (`dash.js:256-270`). Dropping there is
/// the only way to unpin by dragging — a drag that merely leaves the dash puts the icon
/// back.
#[test]
fn overview_dropping_a_favorite_on_the_show_apps_button_unpins_it() {
    use crate::ui::dash::DashHit;

    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop"],
    );

    let first = dash_tile_center(&mut f, 0);
    let show_apps = dash_tile_center(&mut f, 2);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, show_apps.x, show_apps.y);

    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert!(
        f.synoik().app_drag.as_ref().unwrap().unpin,
        "the show-apps button must arm as the unpin target"
    );
    assert_eq!(
        f.synoik().dash.drop_slot(),
        None,
        "the dash must not offer to pin and to unpin at once (`dash.js:444-445`)"
    );
    assert_eq!(
        f.synoik().dash.hovered_for_test(),
        Some(DashHit::ShowApps),
        "the armed button lights up, which is the only feedback that it will remove"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["b.desktop"],
        "the drop must unpin the dragged app"
    );
    assert!(
        (0..3).any(|i| f.synoik().app_grid.entry_id(i) == Some("a.desktop")),
        "and the unpinned app must come back to the grid"
    );
}

/// ...but only for an app that is pinned: `_canRemoveApp` requires `isFavorite`
/// (`dash.js:224-234`), so dragging a fresh app from the grid onto the button neither
/// arms it nor does anything on drop.
#[test]
fn overview_dropping_a_grid_app_on_the_show_apps_button_does_nothing() {
    let (mut f, _recorder) = favorites_and_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["a.desktop", "b.desktop"],
    );
    let before = dash_favorites(&mut f);

    let grid_area = overview_controls(&mut f).app_display;
    let from = f
        .synoik()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    let show_apps = dash_tile_center(&mut f, 2);
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, show_apps.x, show_apps.y);

    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert!(
        !f.synoik().app_drag.as_ref().unwrap().unpin,
        "an app that is not pinned cannot be unpinned"
    );
    assert_eq!(
        f.synoik().dash.hovered_for_test(),
        None,
        "so the button must not light up either — a drag grabs the pointer, and only \
         the unpin arming lights anything up (`dash.js:447-450`)"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        dash_favorites(&mut f),
        before,
        "dropping a non-favourite on the button must change nothing"
    );
}

/// Dragging a dash icon onto a workspace launches the app *there*: gnome-shell's
/// `Workspace.acceptDrop` calls `source.app.open_new_window(workspaceIndex)`
/// (`workspace.js:1429-1434`). The drag starts once the pointer leaves the
/// `drag-threshold` box (`st-dnd-start-gesture.c:73-90`), which also cancels the
/// click the press would otherwise have completed.
#[test]
fn overview_dragging_a_dash_icon_to_a_workspace_launches_it_there() {
    use crate::app_system::ResolvedLaunch;

    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    f.settle_animations();
    let center = dash_tile_center(&mut f, 0);

    // Pick the icon up and drag it onto the workspace above the dash.
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, center.x, center.y - 40.);
    assert!(
        f.synoik().app_drag.is_some(),
        "leaving the press box must start the drag"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "dragging must not launch on the way"
    );

    let ws = f.synoik().layout.active_workspace().unwrap().id();
    pointer_motion_to(&mut f, 960., 400.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(f.synoik().app_drag.is_none(), "the drop ends the drag");
    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "the drop launches the app");
    assert_eq!(calls[0].0.id, "a.desktop");
    assert_eq!(
        calls[0].1,
        ResolvedLaunch::Default,
        "a drop asks for a new window; our fake app has no new-window action, \
         so it resolves to a plain launch"
    );
    drop(calls);

    // And the app's first window opens on the workspace it was dropped on: the
    // drop opened a startup sequence carrying that workspace.
    assert_eq!(
        f.synoik()
            .app_system
            .complete_startup(Some("a"), None, Duration::ZERO),
        Some(ws),
        "the launch must claim the workspace it was dropped on"
    );
}

/// The other half of the drop: the app's first window really does open on the
/// workspace it was dropped on. gnome-shell hands the workspace index to
/// `open_new_window`, which carries it through the startup-notification launch
/// context; ours opens a startup sequence and the mapping window completes it.
#[test]
fn overview_launch_on_workspace_places_the_first_window() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")])),
        Box::new(RecordingLauncher::default()),
    );

    // A second workspace, and the target is *not* the active one.
    let _first = map_window_sized(&mut f, id, (800, 600), None);
    let first_win = f.synoik().layout.focus().unwrap().window.clone();
    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    f.synoik_complete_animations();
    let target = f.synoik().layout.active_workspace().unwrap().id();
    f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    f.synoik_complete_animations();
    assert_ne!(
        f.synoik().layout.active_workspace().unwrap().id(),
        target,
        "the target workspace must not be the active one"
    );

    f.synoik()
        .app_system
        .begin_startup("a.desktop", None, Some(target), get_monotonic_time());

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("a");
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    let win = f
        .synoik()
        .layout
        .windows()
        .map(|(_, m)| m.window.clone())
        .find(|w| *w != first_win)
        .expect("the second window must be mapped");
    let landed = f
        .synoik()
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&win))
        .map(|(_, _, ws)| ws.id())
        .expect("the window must be on a workspace");
    assert_eq!(
        landed, target,
        "the launched window must open on the workspace it was dropped on"
    );

    // The sequence is one-shot: a second window of the same app opens wherever it
    // would have anyway.
    assert_eq!(
        f.synoik()
            .app_system
            .complete_startup(Some("a"), None, get_monotonic_time()),
        None
    );
}

/// A drag that ends over the overview's own chrome drops nothing: gnome-shell's
/// dash and app display are drop targets in their own right (favorites
/// reordering), not workspaces.
#[test]
fn overview_dropping_an_icon_on_the_dash_launches_nothing() {
    let (mut f, recorder) = dash_fixture(&["a.desktop", "b.desktop"]);
    f.settle_animations();
    let from = dash_tile_center(&mut f, 0);
    let onto = dash_tile_center(&mut f, 1);

    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, onto.x, onto.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "a drop on the dash itself launches nothing"
    );
    assert!(f.synoik().layout.is_overview_open());
}

/// A middle-click on a favorite also launches it (still `Activate`: `open_new_window`
/// is reserved for a *running* app, which S3 never tracks) and closes the overview.
#[test]
fn overview_dash_favorite_middle_click_launches() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_MIDDLE, ButtonState::Pressed);
    f.pointer_button(BTN_MIDDLE, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(recorder.calls.borrow().len(), 1, "middle-click launches");
    assert!(!f.synoik().layout.is_overview_open());
}

/// A right-click on a favorite is *consumed* — no launch, and critically it must
/// not fall through to the overview's right-drag workspace grab (that pan starts
/// on a right-press over empty overview space, `input/mod.rs`). The overview stays
/// open and no pointer grab begins.
#[test]
fn overview_dash_favorite_right_click_consumed() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);

    assert!(
        recorder.calls.borrow().is_empty(),
        "right-click must not launch"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "right-click on the dash leaves the overview open"
    );
    assert!(
        !f.synoik().seat.get_pointer().unwrap().is_grabbed(),
        "a right-click on the dash must not begin the overview pan grab"
    );
}

/// The trailing show-apps button toggles the overview's app grid (S8): no launch,
/// the overview stays open, and the app-grid state eases in (the picker shrinks and
/// the app-display box slides on-screen). Escape then returns to the window picker
/// without closing the overview (the grid tier of the overview Escape), and closing
/// the overview *from* the grid resets the state so the next open starts in the
/// picker.
#[test]
fn overview_show_apps_toggles_the_app_grid() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let i = f.synoik().dash.show_apps_index();
    let center = dash_tile_center(&mut f, i);
    // `pointer_motion` is relative; move onto the button once (from the origin) and
    // leave the pointer there — keyboard/actions below don't move it, so later
    // clicks land on the same spot without re-moving.
    f.pointer_motion(center.x, center.y);
    let click = |f: &mut Fixture| {
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.synoik_complete_animations();
    };

    // Click show-apps → the app grid opens (no launch, overview stays open).
    click(&mut f);
    assert!(
        recorder.calls.borrow().is_empty(),
        "show-apps must not launch an app"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "show-apps keeps the overview open"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "show-apps opens the app grid"
    );
    // The app grid slid on-screen (parked below the work area at 1080 otherwise).
    assert!(
        overview_controls(&mut f).app_display.loc.y < 1080.,
        "the app grid must slide up on screen in the app-grid state"
    );

    // Escape returns to the picker without closing the overview, and the grid parks.
    // (The harness only sets overview keyboard focus on demand — the live loop does
    // it every iteration.)
    f.synoik_state().update_keyboard_focus();
    tap(&mut f, KEY_ESC);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_app_grid_open(),
        "Escape returns to the window picker"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "…without closing the overview"
    );
    assert!(
        overview_controls(&mut f).app_display.loc.y >= 1080.,
        "the app grid must park below the work area again"
    );

    // Reopen the grid, then close the overview from it → the state resets on hide,
    // so reopening the overview starts in the window picker.
    click(&mut f);
    assert!(f.synoik().layout.is_app_grid_open());

    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();
    assert!(!f.synoik().layout.is_overview_open());

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_app_grid_open(),
        "reopening the overview starts in the window picker, not the app grid"
    );
}

/// Like [`dash_fixture`], but also installs non-favorite apps so the app grid has
/// tiles, and opens the grid (state = APP_GRID, settled). `favorites` seed the dash;
/// `others` populate the grid (installed minus favorites).
fn app_grid_fixture(
    favorites: &[&str],
    others: &[&str],
) -> (Fixture, crate::app_system::RecordingLauncher) {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let recorder = RecordingLauncher::default();
    let apps = favorites
        .iter()
        .chain(others)
        .map(|id| AppEntry::fake(id, id))
        .collect::<Vec<_>>();
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.synoik()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.synoik().sync_dash_favorites();
    f.synoik().sync_app_grid();

    f.synoik_state().do_action(Action::OpenOverview, false);
    assert!(f.synoik().layout.is_overview_open(), "overview must open");
    f.synoik().layout.toggle_app_grid();
    f.synoik_complete_animations();
    assert!(f.synoik().layout.is_app_grid_open(), "app grid must open");

    (f, recorder)
}

/// A left-click on an app-grid tile launches the app (`Activate`) and closes the
/// overview (`AppIcon.activate` → `Main.overview.hide`, `appDisplay.js:3060,3077`).
/// The grid follows the user's saved arrangement, not our own idea of an order:
/// `AppDisplay._compareItems` (`appDisplay.js:1475-1490`) sorts by the `(page, position)`
/// each app has in `org.gnome.shell app-picker-layout`, and drops everything unplaced in
/// *after* those, by name. A profile that has never rearranged the grid has an empty
/// layout, which is why the fallback looks alphabetical.
#[test]
fn overview_app_grid_follows_the_saved_arrangement() {
    use std::collections::HashMap;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    let ids = |f: &mut Fixture| -> Vec<String> {
        (0..3)
            .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };
    assert_eq!(
        ids(&mut f),
        vec!["a.desktop", "m.desktop", "z.desktop"],
        "with no saved layout the order is by name"
    );

    // Place two of them, out of name order and on separate pages; leave one unplaced.
    f.synoik().gnome_settings.app_picker_layout = HashMap::from([
        ("z.desktop".to_owned(), (0, 7)),
        ("m.desktop".to_owned(), (1, 0)),
    ]);
    f.synoik().sync_app_grid();
    assert_eq!(
        ids(&mut f),
        vec!["z.desktop", "m.desktop", "a.desktop"],
        "placed apps come first in (page, position) order — page 0 before page 1, and \
         the position is an ordering key, not an index — then the unplaced ones by name"
    );
}

/// Dragging a grid icon onto the leading edge of a later one reorders the grid
/// (`_maybeMoveItem` → `_moveItem`, `appDisplay.js:768-810,1203-1209`). The drop
/// commits a move the 200 ms timer had not reached yet (`acceptDrop`, `:1014-1020`),
/// which is what this drives — the timer itself is real-time and its own concern.
///
/// (The other half — a drag nobody accepts putting the grid back — is
/// `a_cancelled_reorder_restores_the_order` in `ui::app_grid`.)
#[test]
fn overview_dragging_a_grid_icon_reorders_the_grid() {
    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    let ids = |f: &mut Fixture| -> Vec<String> {
        (0..3)
            .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };
    assert_eq!(ids(&mut f), vec!["a.desktop", "m.desktop", "z.desktop"]);

    let area = overview_controls(&mut f).app_display;
    let start = f.synoik().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.synoik().app_grid.entry_rect(2, area).expect("tile 2");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Aim just inside the leading edge of the third tile — within the 20px divider
    // leeway, so it is an insertion point and not the icon's body.
    pointer_motion_to(&mut f, third.loc.x + 5., third.loc.y + third.size.h / 2.);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        ids(&mut f),
        vec!["m.desktop", "a.desktop", "z.desktop"],
        "the dragged app must land where the pointer was, pushing the rest along"
    );
}

/// Dropping a dragged icon on the *body* of another app icon folds the two into a new
/// folder (`AppIcon.acceptDrop` → `AppDisplay.createFolder`, `appDisplay.js:3152-3160`,
/// `:1699-1751`). The hovered icon is the folder's first app and gives it its slot; the
/// name falls back to "Unnamed Folder" when the two share no category with a
/// `.directory` title (`_findBestFolderName`, `:114-144`).
///
/// This is the drop half only. The 500 ms preview that offers it is real-time and is
/// pinned in `ui::app_grid` instead.
#[test]
fn overview_dropping_a_grid_icon_on_another_makes_a_folder() {
    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);

    let area = overview_controls(&mut f).app_display;
    let start = f.synoik().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.synoik().app_grid.entry_center(2, area).expect("tile 2");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // The centre of the third tile: its body, not the divider a reorder would take.
    pointer_motion_to(&mut f, third.x, third.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.synoik().app_grid.entry_id(0),
        Some("m.desktop"),
        "the app that took no part stays where it was"
    );
    let members: Vec<&str> = f
        .synoik()
        .app_grid
        .entry_folder(1)
        .expect("the folder took the hovered icon's slot, less the source pulled out ahead of it")
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(
        members,
        vec!["z.desktop", "a.desktop"],
        "the hovered app comes first, then the dragged one (`[this.id, source.id]`)"
    );
    assert_eq!(f.synoik().app_grid.entry_name(1), Some("Unnamed Folder"));
    assert_eq!(
        f.synoik().app_grid.entry_id(2),
        None,
        "both apps left the top level"
    );
}

/// Dropping an app icon on a *folder* tile puts it in that folder (`FolderIcon.acceptDrop`
/// -> `FolderView.addApp`, `appDisplay.js:2400-2408,2223-2236`): it appends to the folder's
/// members and leaves the top level. Unlike the fold, the folder tile takes the `:drop`
/// state at once — there is nothing to preview and nothing to wait for.
///
/// A *folder* dragged onto an app is not a drop at all: both `_canAccept`s take only an
/// `AppIcon` (`:3118-3124`, `:2386-2398`), so it falls through to the reorder, which has
/// nothing to do over an icon's body either.
#[test]
fn overview_dropping_a_grid_icon_on_a_folder_joins_it() {
    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();
    assert_eq!(f.synoik().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.synoik().app_grid.entry_id(1), Some("Utilities"));

    let area = overview_controls(&mut f).app_display;
    let app = f.synoik().app_grid.entry_center(0, area).expect("tile 0");
    let folder = f.synoik().app_grid.entry_center(1, area).expect("tile 1");

    // The folder onto the app: no drop, no reorder, nothing.
    pointer_motion_to(&mut f, folder.x, folder.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, app.x, app.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.synoik().app_grid.drop_hover(),
        None,
        "a folder is not something another icon can swallow"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(f.synoik().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.synoik().app_grid.entry_id(1), Some("Utilities"));

    // The app onto the folder: a join.
    pointer_motion_to(&mut f, app.x, app.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, folder.x, folder.y);
    assert_eq!(
        f.synoik().app_grid.drop_hover(),
        Some(1),
        "a folder lights up the moment the drag reaches it — no 500 ms preview"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let members: Vec<&str> = f
        .synoik()
        .app_grid
        .entry_folder(0)
        .expect("the folder took the slot the app left")
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(
        members,
        vec!["m.desktop", "z.desktop", "a.desktop"],
        "the joined app appends, as `addApp` pushes onto `apps`"
    );
    assert_eq!(
        f.synoik().app_grid.entry_id(1),
        None,
        "and it is gone from the top level"
    );
}

/// Dropping a dragged icon on a page-preview band sends it to that page and follows it
/// there (`acceptDrop`'s hint branch, `appDisplay.js:1004-1013`). Stepping past the last
/// page is allowed — that is how a new page gets made.
#[test]
fn overview_dropping_a_grid_icon_on_a_preview_band_changes_its_page() {
    let apps: Vec<String> = (0..30).map(|i| format!("app{i:02}.desktop")).collect();
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    let (mut f, _recorder) = app_grid_fixture(&[], &refs);

    let area = overview_controls(&mut f).app_display;
    assert_eq!(f.synoik().app_grid.page_count(area), 2, "30 apps paginate");
    assert_eq!(f.synoik().app_grid.entry_id(0), Some("app00.desktop"));

    let start = f.synoik().app_grid.entry_center(0, area).expect("tile 0");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Out to the right band. It only becomes a target once the previews have slid in.
    let right = area.loc.x + area.size.w - 20.;
    pointer_motion_to(&mut f, right, start.y);
    f.settle_animations();
    pointer_motion_to(&mut f, right - 1., start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "the view must follow the app to its new page"
    );
    assert_eq!(
        f.synoik().app_grid.entry_id(29),
        Some("app00.desktop"),
        "the app appends to the page it was dropped onto"
    );
}

/// The grid holds the installed apps minus favorites, name-sorted.
#[test]
fn overview_app_grid_click_launches_and_closes() {
    let (mut f, recorder) = app_grid_fixture(&["a.desktop"], &["m.desktop", "z.desktop"]);
    let area = overview_controls(&mut f).app_display;
    // Tile 0 is the first non-favorite in name order ("m.desktop"); "a.desktop" is a
    // favorite and lives in the dash, not the grid.
    let center = f
        .synoik()
        .app_grid
        .tile_center(0, area)
        .expect("grid tile 0 in range");

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, "m.desktop", "the clicked grid app launched");
    assert!(
        !f.synoik().layout.is_overview_open(),
        "launching from the grid closes the overview"
    );
}

/// An app folder is a grid slot of its own, and its members stop appearing at the top
/// level: `_redisplay` (`appDisplay.js:1508-1533`) pushes each `FolderIcon` into the
/// same list as the app icons, collects `appsInsideFolders`, and filters the app list
/// against it. So a folder sorts by `app-picker-layout` under its `folder-children` id
/// exactly like an app does — on a real profile `'Utilities'` holds a position in that
/// dict. A folder that resolves to nothing is not displayed at all
/// (`appDisplay.js:1523-1527`), and clicking a folder opens it rather than launching
/// anything (`FolderIcon.vfunc_clicked`, `appDisplay.js:2343`).
#[test]
fn overview_app_grid_folds_a_folders_apps_out_of_the_top_level() {
    use std::collections::HashMap;

    use crate::gnome::AppFolder;

    let (mut f, recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    let ids = |f: &mut Fixture| -> Vec<String> {
        (0..4)
            .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };

    f.synoik().gnome_settings.app_folders = vec![
        AppFolder {
            id: "Utilities".to_owned(),
            name: "Utilities".to_owned(),
            apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
            ..Default::default()
        },
        AppFolder {
            id: "Empty".to_owned(),
            name: "Empty".to_owned(),
            apps: vec!["nothing-installed.desktop".to_owned()],
            ..Default::default()
        },
    ];
    f.synoik().sync_app_grid();

    assert_eq!(
        ids(&mut f),
        vec!["a.desktop", "Utilities"],
        "the folder's two apps left the top level, the folder took one slot, and the \
         folder that resolved to nothing is not displayed"
    );
    assert_eq!(
        f.synoik()
            .app_grid
            .entry_folder(1)
            .expect("tile 1 is the folder")
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        vec!["m.desktop", "z.desktop"],
        "the folder carries its members, in the order it lists them"
    );
    assert!(
        f.synoik().app_grid.entry_folder(0).is_none(),
        "an app tile is not a folder"
    );

    // The folder id sorts through the same saved arrangement as a desktop id.
    f.synoik().gnome_settings.app_picker_layout = HashMap::from([("Utilities".to_owned(), (0, 0))]);
    f.synoik().sync_app_grid();
    assert_eq!(ids(&mut f), vec!["Utilities", "a.desktop"]);

    // Clicking it launches nothing and leaves the overview up — it opens instead.
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(0, area)
        .expect("the folder tile is in range");
    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "a folder launches nothing"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "clicking a folder must not close the overview"
    );
    assert_eq!(
        f.synoik().folder_dialog.folder_id(),
        Some("Utilities"),
        "clicking a folder opens its dialog (`FolderIcon.vfunc_clicked`)"
    );
}

/// Renaming a folder (`_addFolderNameEntry` + `_maybeUpdateFolderName`,
/// `appDisplay.js:2531-2657`): the edit button swaps the label for an entry with the whole
/// name selected, typing replaces it, and Enter commits — the label follows at once and the
/// `name` key is written with `translate` off. An empty entry, or one still holding the old
/// name, writes nothing.
#[test]
fn overview_folder_dialog_renames_the_folder() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(1, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(f.synoik().folder_dialog.folder_id(), Some("Utilities"));

    // The edit button opens the entry, with the name selected whole.
    let edit = crate::ui::folder_dialog::layout(view).edit_button;
    let edit_center: Point<f64, smithay::utils::Logical> =
        Point::from((edit.loc.x + edit.size.w / 2., edit.loc.y + edit.size.h / 2.));
    pointer_motion_to(&mut f, edit_center.x, edit_center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.synoik().folder_dialog.is_renaming());
    assert_eq!(f.synoik().folder_dialog.rename_text(), Some("Utilities"));
    // Without this the overview never holds the key focus and the whole ladder is dead.
    f.synoik_state().update_keyboard_focus();

    // Typing over the selection replaces the whole name, and the keys never reach the
    // search entry behind.
    tap(&mut f, KEY_T);
    tap(&mut f, KEY_O);
    assert_eq!(f.synoik().folder_dialog.rename_text(), Some("to"));
    assert!(
        !f.synoik().overview_search.is_active(),
        "the rename entry holds the key focus, so the search never engages"
    );

    // Enter commits: the label follows immediately.
    tap(&mut f, KEY_ENTER);
    assert!(!f.synoik().folder_dialog.is_renaming());
    assert_eq!(f.synoik().app_grid.entry_name(1), Some("Utilities"));
    assert_eq!(
        f.synoik().folder_dialog.folder_name(),
        Some("to"),
        "the dialog shows the new name at once; the grid tile follows the settings reload"
    );
}

/// Dragging an app out of the open folder takes it out of the folder
/// (`AppFolderDialog.acceptDrop` -> `FolderView.removeApp`, `appDisplay.js:2857-2865`,
/// `:2239-2272`): the dialog pops down, the app becomes a top-level tile, and the folder
/// keeps the rest. The tile it becomes is GNOME's placeholder, added to the grid when the
/// drag began (`_ensurePlaceholder`, `:1434-1448`).
///
/// Dropping it back *inside* the panel is not a removal: that drop belongs to the folder's own
/// view, which reorders its members (see `overview_dragging_inside_a_folder_reorders_its_members`)
/// — and a drop on the slot the icon started in moves nothing.
#[test]
fn overview_dragging_an_app_out_of_a_folder_removes_it() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let open_it = |f: &mut Fixture| {
        let area = overview_controls(f).app_display;
        let center = f
            .synoik()
            .app_grid
            .tile_center(1, area)
            .expect("folder tile");
        pointer_motion_to(f, center.x, center.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.synoik_complete_animations();
    };
    let panel = crate::ui::folder_dialog::layout(view).panel;
    let outside: Point<f64, smithay::utils::Logical> =
        Point::from((panel.loc.x - 40., panel.loc.y + panel.size.h / 2.));

    // First, a drag that ends back inside the panel: nothing moves.
    open_it(&mut f);
    let member = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("member tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        Some(2),
        "the placeholder joins the grid for the duration of the drag"
    );
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        None,
        "a drop back inside the folder withdraws the placeholder"
    );
    assert_eq!(f.synoik().folder_dialog.member_count(), 2);

    // Then the real thing.
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.synoik().folder_dialog.is_open(),
        "the dialog takes the drop and pops down"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        Some(2),
        "the app is a top-level tile now, where its placeholder sat"
    );
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(2),
        "and it takes the key focus (`selectApp`)"
    );
    let members: Vec<&str> = f
        .synoik()
        .app_grid
        .entry_folder(1)
        .expect("the folder is still there with what is left")
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(members, vec!["z.desktop"]);

    // Taking the last app out takes the folder with it (`removeApp`'s empty branch).
    open_it(&mut f);
    let member = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("the one member left");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.synoik().app_grid.index_of("Utilities"),
        None,
        "an emptied folder is deleted, not shown empty"
    );
    assert_eq!(f.synoik().app_grid.index_of("z.desktop"), Some(2));
}

/// A drag that stays inside the folder reorders its members: `FolderView` is a
/// `BaseAppView`, so it inherits the same delayed move the app display uses, and its
/// `acceptDrop` writes the new order straight back to the folder's `apps`
/// (`appDisplay.js:2213-2221`).
#[test]
fn overview_dragging_inside_a_folder_reorders_its_members() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(
        &[],
        &[
            "a.desktop",
            "m.desktop",
            "n.desktop",
            "o.desktop",
            "z.desktop",
        ],
    );
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec![
            "m.desktop".to_owned(),
            "n.desktop".to_owned(),
            "o.desktop".to_owned(),
            "z.desktop".to_owned(),
        ],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(1, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik().folder_dialog.member_ids(),
        vec!["m.desktop", "n.desktop", "o.desktop", "z.desktop"]
    );

    let at = |f: &mut Fixture, i: usize| {
        f.synoik()
            .folder_dialog
            .entry_center(i, view)
            .expect("member tile")
    };
    let (first, second, third) = (at(&mut f, 0), at(&mut f, 1), at(&mut f, 2));
    let pitch = second.x - first.x;

    // The first member, dropped just past the third: it lands *after* it.
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x + pitch * 0.4, third.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert!(
        f.synoik().folder_pending_move.is_some(),
        "the folder arms the same delayed move the grid does"
    );
    // A drop that beats the 200 ms timer still commits the move, as it does in the grid.
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.synoik().folder_dialog.is_open(),
        "a drop inside the panel is the folder's, so the dialog stays up"
    );
    assert_eq!(
        f.synoik().folder_dialog.member_ids(),
        vec!["n.desktop", "o.desktop", "m.desktop", "z.desktop"],
        "the dragged member took its new place"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        None,
        "and the drag placeholder is gone from the grid behind"
    );

    // A drag that goes nowhere puts them back: `_onDragCancelled` → `_redisplay`.
    let (first, second) = (at(&mut f, 0), at(&mut f, 1));
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, second.x, second.y);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.synoik().folder_dialog.member_ids(),
        vec!["n.desktop", "o.desktop", "m.desktop", "z.desktop"],
        "a drag back to where it started changes nothing"
    );

    // A drop that beat the 200 ms timer must *unregister* it, not merely forget it: a
    // one-shot left registered fires against the next drag and commits its armed move on
    // the spot, which is the delayed move failing open.
    let (first, third) = (at(&mut f, 0), at(&mut f, 2));
    let pitch = at(&mut f, 1).x - first.x;
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x + pitch * 0.4, third.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    let after_first = f.synoik().folder_dialog.member_ids();

    // Wait out most of that timer's delay, so its firing lands *inside* the window the
    // second drag is watched over.
    f.dispatch_until(Duration::from_millis(150), |_| false);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x + pitch * 0.4, third.y);
    assert!(f.synoik().folder_pending_move.is_some(), "a move is armed");
    let moved_early = f.dispatch_until(Duration::from_millis(100), |state| {
        state.synoik.folder_dialog.member_ids() != after_first
    });
    assert!(
        !moved_early,
        "nothing moves before the delay is out: {:?} became {:?}",
        after_first,
        f.synoik().folder_dialog.member_ids()
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);
}

/// **Adaptive overview chrome** (`docs/fork/adaptive-overview-chrome.md`, approved
/// 2026-07-26). gnome-shell's overview chrome is fixed logical constants — a 64px dash
/// icon (`dash.js:321`), a 30px workspace-background radius (`workspace.js:30`), 24..80
/// picker-gap clamps (`workspacesView.js:22-23`), a 24em search entry — and they read
/// correctly only because it assumes a canvas of roughly 1280x800 or more. On the
/// 1024x665 canvas this fork actually runs on, they produce a dash wider than half the
/// screen over an app grid whose own icons have laddered down, a near-circular corner on
/// a short preview, and a gap pegged at its 80px maximum.
///
/// Two things are asserted, and the first matters as much as the second: on a canvas at
/// or above the reference **nothing changes** — the divergence only ever shrinks.
#[test]
fn overview_chrome_ramps_down_on_a_small_canvas() {
    use crate::ui::dash::Dash;

    // (dash icon, entry pill width, workspace background radius, picker gap)
    let measure = |size: (u16, u16)| -> (f64, f64, f64, f64) {
        let mut f = Fixture::new();
        f.add_output(1, size);
        let output = f.synoik_output(1);
        f.synoik_state().do_action(Action::OpenOverview, false);
        f.synoik_complete_animations();

        let controls = f
            .synoik()
            .layout
            .controls_layout_for_output(&output)
            .expect("the output has a monitor");
        let icon = Dash::metrics(controls.dash).icon_px;
        // The ramp sizes the *open* pill; at rest the entry is a collapsed puck whose width
        // is a fixed circle and would ramp with nothing.
        let entry = f
            .synoik()
            .overview_search
            .expanded_entry_pill(controls.into())
            .size
            .w;
        let mon = f
            .synoik()
            .layout
            .monitor_for_output(&output)
            .expect("the output has a monitor");
        // Un-divide the zoom the accessor applies, so this is the radius as drawn.
        let radius = mon.workspace_background_radius() * mon.overview_zoom();
        let gap = mon.workspace_gap_for_test();
        (icon, entry, radius, gap)
    };

    // 1920x1080 — above the reference canvas, so every number is GNOME's own.
    let (icon, entry, radius, gap) = measure((1920, 1080));
    assert_eq!(icon, 64., "the dash keeps GNOME's icon on a normal canvas");
    assert_eq!(entry, 352., "…and the entry its 24em");
    assert_eq!(radius, 30., "…and the preview its 30px corner");
    assert!(gap <= 80., "…and the gap stays inside GNOME's clamps");

    // 1024x665 (2048x1330 @ 2) — the canvas this divergence was written for. Ramp 0.8.
    let (s_icon, s_entry, s_radius, s_gap) = measure((1024, 665));
    assert_eq!(
        s_icon, 48.,
        "the dash steps down a ladder rung (64 x 0.8 = 51.2 -> 48)"
    );
    assert_eq!(s_entry, 282., "the entry keeps its share of the screen");
    assert!(
        s_gap < 80.,
        "the gap is no longer pegged at the un-ramped maximum: {s_gap}"
    );
    // The corner ramps here: the always-shown thumbnails band (divergence, see
    // `docs/fork/dynamic-workspaces-divergence.md`) takes its share of this canvas, so the
    // preview is under the 520px the flat 30 is written for. The rule is unchanged; what
    // moved is the canvas at which it starts biting.
    assert!(
        s_radius < radius && s_radius >= 8.,
        "the corner follows the preview down but stays a corner: {s_radius} vs {radius}"
    );

    // 900x600, and it keeps following rather than stepping once and stopping.
    let (_, _, t_radius, _) = measure((900, 600));
    assert!(
        t_radius < s_radius && t_radius >= 8.,
        "the corner keeps following the preview: {t_radius} after {s_radius}"
    );
    // …and it keeps following, rather than stepping once and stopping.
    let (_, _, u_radius, _) = measure((800, 500));
    assert!(
        u_radius < t_radius && u_radius >= 8.,
        "the corner keeps following the preview: {u_radius} after {t_radius}"
    );
}

/// Dropping a dash favourite into the app grid unpins it: the grid excludes pinned apps,
/// so `AppDisplay.acceptDrop` calls `removeFavorite` for the same reason it calls
/// `view.removeApp` for a folder member (`appDisplay.js:1680-1697`). The drag also needs a
/// placeholder to reorder against, since the app has no tile in the grid to begin with
/// (`_onDragBegin`, `:1646-1656`).
#[test]
fn overview_dropping_a_favourite_on_the_grid_unpins_it() {
    let (mut f, _recorder) = app_grid_fixture(
        &["a.desktop", "b.desktop", "c.desktop"],
        &["m.desktop", "z.desktop"],
    );
    assert!(
        f.synoik().app_system.is_favorite("a.desktop"),
        "a.desktop starts pinned"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("a.desktop"),
        None,
        "…and so is not in the grid"
    );

    let controls = overview_controls(&mut f);
    let (area, dash) = (controls.app_display, controls.dash);
    let from = f.synoik().dash.tile_center(0, dash).expect("the dash tile");
    let onto = f
        .synoik()
        .app_grid
        .entry_rect(1, area)
        .expect("grid tile 1");

    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // The leading edge of a tile: an insertion point, not the body a fold would take.
    pointer_motion_to(&mut f, onto.loc.x + 5., onto.loc.y + onto.size.h / 2.);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert!(
        f.synoik().app_grid.index_of("a.desktop").is_some(),
        "a placeholder joins the grid for the duration of the drag"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.synoik().app_system.is_favorite("a.desktop"),
        "the drop unpinned it"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("a.desktop"),
        Some(1),
        "and it stays in the slot it was dropped in, not at the name-ordered tail"
    );

    // Folding a favourite into a new folder unpins it too (`AppIcon.acceptDrop` reaches
    // the same `removeFavorite` via `AppDisplay.createFolder`, `appDisplay.js:1699-1751`).
    let from = f.synoik().dash.tile_center(0, dash).expect("the dash tile");
    let onto = f
        .synoik()
        .app_grid
        .entry_center(0, area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, onto.x, onto.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        !f.synoik().app_system.is_favorite("b.desktop"),
        "the fold unpinned it"
    );
    // Only the unpin is asserted here: unpinning re-derives the grid from the settings
    // model, and this fixture has no settings writer for the new folder to come back
    // from. The fold itself is pinned by `overview_dropping_a_grid_icon_on_another_…`.

    // A drag that ends nowhere leaves the dash alone and withdraws the placeholder.
    let from = f.synoik().dash.tile_center(0, dash).expect("the dash tile");
    let pinned = f
        .synoik()
        .dash
        .item_id(0)
        .map(str::to_owned)
        .expect("a tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, 4., 4.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.synoik().app_system.is_favorite(&pinned),
        "a drop on nothing keeps it pinned: {pinned}"
    );
    assert_eq!(
        f.synoik().app_grid.index_of(&pinned),
        None,
        "…and its placeholder is withdrawn"
    );
}

/// Escape during an item drag cancels the *drag* and nothing else: the icon goes home,
/// the grid keeps its old order, and the app grid stays open (`_onEvent` → `_cancelDrag`,
/// `dnd.js:567-573`). It used to fall through to the overview's own Escape, so one press
/// left the grid mid-drag and a second closed the overview.
#[test]
fn overview_escape_during_a_drag_cancels_the_drag() {
    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    let area = overview_controls(&mut f).app_display;
    let before: Vec<String> = (0..3)
        .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
        .collect();

    let first = f.synoik().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.synoik().app_grid.entry_center(2, area).expect("tile 2");
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x, third.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");

    tap(&mut f, KEY_ESC);
    assert!(f.synoik().app_drag.is_none(), "the drag is cancelled");
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "…and the grid it was over stays open — Escape went no further"
    );
    let after: Vec<String> = (0..3)
        .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
        .collect();
    assert_eq!(
        after, before,
        "the order the drag was reflowing is put back"
    );

    // The button is still down; releasing it must not now act as a drop.
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    let after: Vec<String> = (0..3)
        .filter_map(|i| f.synoik().app_grid.entry_id(i).map(str::to_owned))
        .collect();
    assert_eq!(after, before, "and the release that follows is not a drop");
}

/// Every app-icon surface draws from **one** GPU upload map, as gnome-shell keeps one
/// Cogl texture per gicon+size shell-wide (`st-texture-cache.c:998`). Ours had one per
/// surface, so a search result re-uploaded, at the same size, an icon the grid already
/// had resident.
///
/// Pinned by identity rather than by counting uploads: whether a *particular* icon is
/// shared depends on the sizes each surface happens to ask for, but the wiring — that
/// they are the same map at all — is the thing that can silently come undone.
#[test]
fn overview_app_icon_uploads_are_one_shared_map() {
    let (mut f, _recorder) = app_grid_fixture(&["a.desktop"], &["m.desktop"]);
    let synoik = f.synoik();
    let dash = synoik.dash.icon_uploads();
    for (name, other) in [
        ("the app grid", synoik.app_grid.icon_uploads()),
        ("the search results", synoik.overview_search.icon_uploads()),
        ("the drag proxy", synoik.app_icon_uploads.clone()),
    ] {
        assert!(
            std::rc::Rc::ptr_eq(&dash, &other),
            "{name} shares the dash's upload map"
        );
    }

    // A folder's view is built when it opens, so it can only inherit the map if the
    // dialog was told about it — the one path that is not wired at construction.
    synoik
        .folder_dialog
        .popup("Utilities", "Utilities", Vec::new());
    let folder = synoik
        .folder_dialog
        .icon_uploads()
        .expect("an open folder has a view");
    assert!(
        std::rc::Rc::ptr_eq(&dash, &folder),
        "a folder's view shares it too"
    );
}

/// The grid's name fallback is a *collation*, not a byte or case-folded compare: an
/// accented initial belongs with its base letter, which is what `localeCompare`
/// (`_compareItems`, `appDisplay.js:1475-1490`) gives GNOME and what a `to_lowercase()`
/// compare does not — it puts every accented name after Z.
#[test]
fn overview_app_grid_sorts_names_by_collation() {
    let (mut f, _recorder) = app_grid_fixture(&[], &[]);
    // The fixture pins the collation locale; skip where the machine has no dictionary
    // collation to pin it to (`C.UTF-8` sorts by codepoint, which would fail this for a
    // reason that is not ours). Detected by asking, not by locale name.
    use gio::glib::CollationKey;
    if CollationKey::from("Écran") > CollationKey::from("Zip") {
        eprintln!("skipping overview_app_grid_sorts_names_by_collation: codepoint collation");
        return;
    }
    {
        use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
        let named = [
            ("a.desktop", "Archive Manager"),
            ("e.desktop", "Écran"),
            ("z.desktop", "Zip"),
            ("d.desktop", "disks"),
        ];
        let apps: Vec<AppEntry> = named
            .iter()
            .map(|(id, name)| AppEntry::fake(id, name))
            .collect();
        f.synoik().app_system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(apps)),
            Box::new(RecordingLauncher::default()),
        );
        f.synoik().sync_app_grid();
    }

    let order: Vec<String> = (0..4)
        .filter_map(|i| f.synoik().app_grid.entry_name(i).map(str::to_owned))
        .collect();
    assert_eq!(
        order,
        vec!["Archive Manager", "disks", "Écran", "Zip"],
        "Écran sorts with the E's and case is ignored, as a collation does"
    );
}

/// A folder with more than one page takes a drag onto its *other* page: `FolderView`
/// inherits `BaseAppView`'s page-switch machinery whole — the preview bands, the edge
/// bump, and `acceptDrop`'s band branch (`appDisplay.js:827-959,1004-1013`). Ours wired
/// all three to the top-level grid only, so a member could not leave page 1.
#[test]
fn overview_dragging_a_member_onto_a_folder_page_band_moves_it_there() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;
    use crate::ui::folder_dialog::FolderDialog;

    let apps: Vec<String> = (0..12).map(|i| format!("m{i:02}.desktop")).collect();
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    let (mut f, _recorder) = app_grid_fixture(&[], &refs);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: apps.clone(),
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(0, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik().folder_dialog.page_count(view),
        2,
        "twelve members make two pages of the 3x3 folder grid"
    );

    // Pick the first member up and hold it over the next-page band.
    let member = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    let grid = FolderDialog::view_area(view);
    // Inside the preview band but clear of the 20 px edge-bump strip, so this exercises
    // the band drop rather than the bump.
    let band: Point<f64, smithay::utils::Logical> = Point::from((
        grid.loc.x + grid.size.w - 30.,
        grid.loc.y + grid.size.h / 2.,
    ));
    pointer_motion_to(&mut f, band.x, band.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    // The bands slide in over 150 ms and are not a drop target until they are there
    // (`hint_at` reads the peek). `synoik_complete_animations` will not do: it flips the
    // clock's complete-instantly flag back off, so the peek reads 0 again the moment it
    // returns — the animation clock has to really move.
    f.settle_animations();
    assert_eq!(
        f.synoik().folder_dialog.current_page(),
        0,
        "hovering a band does not switch the page on its own — that takes a beat"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.synoik().folder_dialog.is_open(),
        "the folder took the drop, so the dialog stays up"
    );
    assert_eq!(
        f.synoik().folder_dialog.current_page(),
        1,
        "and follows the member to the page it was sent to"
    );
    let members = f.synoik().folder_dialog.member_ids();
    assert_eq!(
        members.last().map(String::as_str),
        Some("m00.desktop"),
        "the member moved to the end, i.e. onto the second page: {members:?}"
    );
}

/// The folder's **view** is what takes a drop, not the panel around it: the name row has no
/// delegate of its own, so a drop there bubbles to the dialog actor — which covers the whole
/// monitor — and `AppFolderDialog.acceptDrop` pops down and removes the app
/// (`appDisplay.js:2857-2865`), exactly as a drop on the shade does.
#[test]
fn overview_dropping_a_member_on_the_folder_name_row_takes_it_out() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(1, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    let l = crate::ui::folder_dialog::layout(view);
    let name = Point::from((
        l.name_band.loc.x + l.name_band.size.w / 2.,
        l.name_band.loc.y + l.name_band.size.h / 2.,
    ));
    assert!(
        l.panel.contains(name) && !l.grid_area.contains(name),
        "the name row is on the panel but off the view"
    );

    let member = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, name.x, name.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.synoik().folder_dialog.is_open(),
        "the dialog takes the drop and pops down"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        Some(2),
        "the app is a top-level tile now"
    );
}

/// The grid behind the open dialog takes no part in a drag that never leaves the panel:
/// `AppDisplay._onDragMotion` returns `CONTINUE` while `_currentDialog`
/// (`appDisplay.js:1658-1663`). The motion path honoured that from the start, but the
/// frame the drag *begins* on did not — so picking an icon up inside a folder and moving
/// it a little reflowed the icons underneath, which the user then saw shift.
#[test]
fn overview_a_drag_inside_the_folder_leaves_the_grid_behind_alone() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f
        .synoik()
        .app_grid
        .tile_center(1, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    // Pick a member up and cross the drag threshold *inside* the panel.
    let from = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("tile 0");
    let to = f
        .synoik()
        .folder_dialog
        .entry_center(1, view)
        .expect("tile 1");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, to.x, to.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    assert!(
        f.synoik().folder_dialog.is_open(),
        "and the dialog is still up"
    );
    assert!(
        f.synoik().grid_pending_move.is_none(),
        "the covered grid arms no move"
    );
    assert_eq!(
        f.synoik().app_grid.drop_hover(),
        None,
        "and offers no folder of its own"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);
}

/// Dragging an app from one folder into another **moves** it: whatever accepts the drop,
/// `AppDisplay.acceptDrop` also calls `view.removeApp` on the folder the icon came from
/// (`appDisplay.js:1680-1697`). The join path used not to, so the app ended up in both.
///
/// The drag also has to leave the first dialog for the grid to see it at all, so this is
/// the one test that waits on the real `POPDOWN_DIALOG_TIMEOUT` timer.
#[test]
fn overview_dragging_an_app_between_folders_moves_it() {
    use std::time::Duration;

    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, _recorder) =
        app_grid_fixture(&[], &["a.desktop", "m.desktop", "q.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![
        AppFolder {
            id: "Utilities".to_owned(),
            name: "Utilities".to_owned(),
            apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
            ..Default::default()
        },
        AppFolder {
            id: "Office".to_owned(),
            name: "Office".to_owned(),
            apps: vec!["q.desktop".to_owned()],
            ..Default::default()
        },
    ];
    f.synoik().sync_app_grid();
    assert_eq!(f.synoik().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.synoik().app_grid.entry_id(1), Some("Office"));
    assert_eq!(f.synoik().app_grid.entry_id(2), Some("Utilities"));

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let office = f
        .synoik()
        .app_grid
        .entry_center(1, area)
        .expect("Office tile");

    // Open Utilities and pick its first member up.
    let center = f
        .synoik()
        .app_grid
        .tile_center(2, area)
        .expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    let member = f
        .synoik()
        .folder_dialog
        .entry_center(0, view)
        .expect("member tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    // Out of the panel, and hold there until the dialog gives up and pops down.
    pointer_motion_to(&mut f, office.x, office.y);
    assert!(f.synoik().app_drag.is_some(), "the drag must have started");
    let popped = f.dispatch_until(Duration::from_millis(2000), |state| {
        !state.synoik.folder_dialog.is_open()
    });
    assert!(
        popped,
        "the dialog pops down 500 ms after the drag leaves it"
    );
    // The pointer has not moved since, so nudge it to let the grid resolve the target it
    // has been ignoring.
    pointer_motion_to(&mut f, office.x, office.y);
    assert_eq!(
        f.synoik().app_grid.drop_hover(),
        f.synoik().app_grid.index_of("Office"),
        "Office is armed as the drop target"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let members = |f: &mut Fixture, id: &str| -> Vec<String> {
        let i = f
            .synoik()
            .app_grid
            .index_of(id)
            .expect("the folder is there");
        f.synoik()
            .app_grid
            .entry_folder(i)
            .expect("…and it is a folder")
            .iter()
            .map(|e| e.id.clone())
            .collect()
    };
    assert_eq!(
        members(&mut f, "Office"),
        vec!["q.desktop", "m.desktop"],
        "the app joined the folder it was dropped on"
    );
    assert_eq!(
        members(&mut f, "Utilities"),
        vec!["z.desktop"],
        "and left the one it came out of — a move, not a copy"
    );
    assert_eq!(
        f.synoik().app_grid.index_of("m.desktop"),
        None,
        "the placeholder is gone from the top level too"
    );
}

/// The app-folder dialog (`AppFolderDialog`, `appDisplay.js:2463-2916`): opening a folder
/// puts its apps in their own view, launching one from inside works exactly as it does at
/// the top level, and the dialog is *modal* — a click anywhere off the 720² panel pops it
/// down rather than reaching the grid, and Escape closes it before it closes anything else.
#[test]
fn overview_folder_dialog_opens_launches_and_pops_down() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::gnome::AppFolder;

    let (mut f, recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let open_it = |f: &mut Fixture| {
        let area = overview_controls(f).app_display;
        // The folder sorts after "a.desktop" by name, so it is tile 1.
        let center = f
            .synoik()
            .app_grid
            .tile_center(1, area)
            .expect("the folder tile is in range");
        pointer_motion_to(f, center.x, center.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.synoik_complete_animations();
    };

    open_it(&mut f);
    assert_eq!(f.synoik().folder_dialog.folder_id(), Some("Utilities"));
    assert_eq!(
        (0..2)
            .filter_map(|i| f.synoik().folder_dialog.entry_id(i).map(str::to_owned))
            .collect::<Vec<_>>(),
        vec!["m.desktop", "z.desktop"],
        "the dialog shows the folder's members"
    );

    // A click off the panel pops the dialog down and does NOT fall through to the grid
    // tile that happens to be under it.
    let panel = crate::ui::folder_dialog::layout(view).panel;
    let outside: Point<f64, smithay::utils::Logical> =
        Point::from((panel.loc.x - 40., panel.loc.y + panel.size.h / 2.));
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().folder_dialog.is_open(),
        "a click outside pops down"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "the modal swallowed the click; nothing under it launched"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "the grid is still there"
    );

    // Escape closes the folder first, leaving the grid open — the innermost tier of the
    // overview's Escape ladder. (The overview has to actually hold keyboard focus for the
    // ladder to be reachable at all.)
    open_it(&mut f);
    f.synoik_state().update_keyboard_focus();
    assert!(f.synoik().keyboard_focus.is_overview());
    tap(&mut f, KEY_ESC);
    assert!(
        !f.synoik().folder_dialog.is_open(),
        "Escape pops the folder down"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "…and stops there rather than also leaving the grid"
    );

    // Launching from inside behaves exactly like a top-level tile: activate, then hide.
    open_it(&mut f);
    let grid_area = crate::ui::folder_dialog::layout(view).grid_area;
    let member = f
        .synoik()
        .folder_dialog
        .tile_center(1, grid_area)
        .expect("the second member is in range");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(
        calls[0].0.id, "z.desktop",
        "the app inside the folder launched"
    );
    assert_eq!(calls[0].1, crate::app_system::ResolvedLaunch::Default);
    drop(calls);
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and the overview closed"
    );
    assert!(!f.synoik().folder_dialog.is_open(), "with the folder");
}

/// Dragging a folder tile carries the *folder*, not one of the apps inside it.
/// `FolderIcon.getDragActor` builds a `BaseIcon` from the folder's own `_createIcon` with
/// its `overview-tile app-folder` style class (`appDisplay.js:2286,2368-2379`), so the
/// proxy is the composed 2×2 over the raised fill. Ours carried `members[0].icon`, which
/// looked exactly like dragging the folder's first app out of it.
#[test]
fn overview_dragging_a_folder_carries_the_folder_not_its_first_app() {
    use crate::gnome::AppFolder;

    let (mut f, _recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();

    let grid_area = overview_controls(&mut f).app_display;
    let members: Vec<crate::app_system::AppIconRef> = f
        .synoik()
        .app_grid
        .entry_folder(1)
        .expect("tile 1 is the folder")
        .iter()
        .map(|m| m.icon.clone())
        .collect();
    assert_eq!(members.len(), 2);

    // Pick the folder tile up and move far enough to pass the drag threshold.
    let from = f
        .synoik()
        .app_grid
        .entry_center(1, grid_area)
        .expect("the folder tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x + 120., from.y);
    let drag = f
        .synoik()
        .app_drag
        .as_ref()
        .expect("the drag must have started");
    assert_eq!(drag.id, "Utilities");
    assert_eq!(
        drag.folder.as_deref(),
        Some(members.as_slice()),
        "the proxy composes the folder's members, rather than standing in as one of them"
    );

    // An ordinary app tile stays a plain single icon.
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    let from = f
        .synoik()
        .app_grid
        .entry_center(0, grid_area)
        .expect("the app tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x + 120., from.y);
    let drag = f
        .synoik()
        .app_drag
        .as_ref()
        .expect("the drag must have started");
    assert_eq!(drag.id, "a.desktop");
    assert_eq!(drag.folder, None, "an app is not a folder");
}

/// An open folder takes the arrows for itself — it is its own focus group in gnome-shell
/// (`global.focus_manager.add_group(this)` + `navigate_from_event`,
/// `appDisplay.js:2516,2788-2789`), so navigation stays inside the dialog and never reaches
/// the grid behind it, and Enter launches the member it lands on.
#[test]
fn overview_folder_dialog_navigates_with_the_keyboard() {
    use crate::gnome::AppFolder;

    let (mut f, recorder) = app_grid_fixture(&[], &["a.desktop", "m.desktop", "z.desktop"]);
    f.synoik().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();
    f.synoik_state().update_keyboard_focus();
    assert!(f.synoik().keyboard_focus.is_overview());

    // Reach the folder tile by keyboard too: it sorts after "a.desktop", so one Right
    // from the first tile lands on it, and Enter opens it rather than launching.
    tap(&mut f, KEY_RIGHT);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.synoik().app_grid.focused(), Some(1));
    tap(&mut f, KEY_ENTER);
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik().folder_dialog.folder_id(),
        Some("Utilities"),
        "Enter on a folder tile opens it"
    );
    assert!(recorder.calls.borrow().is_empty(), "…and launches nothing");

    // The arrows now belong to the dialog: the grid behind keeps the focus it had.
    let before = f.synoik().app_grid.focused();
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.synoik().folder_dialog.focused(), Some(0));
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.synoik().folder_dialog.focused(), Some(1));
    assert_eq!(
        f.synoik().app_grid.focused(),
        before,
        "the grid behind the modal did not move"
    );
    // Enter launches the focused member and takes the overview with it.
    tap(&mut f, KEY_ENTER);
    f.synoik_complete_animations();
    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, "z.desktop", "the focused member launched");
    drop(calls);
    assert!(!f.synoik().layout.is_overview_open());
    assert!(!f.synoik().folder_dialog.is_open());
}

/// The app grid navigates by keyboard. gnome-shell has no keynav code of its own here:
/// the arrows are St's *spatial* focus navigation over the `can_focus` icons — everything
/// strictly in the direction asked for, nearest by midpoint distance
/// (`filter_by_position` + `sort_by_distance`, `st-widget.c:1932-2030`) — moving focus
/// pages the view to follow it (`key-focus-in` → `_ensureItemIsVisible`,
/// `iconGrid.js:1196-1208`), Enter is `St.Button`'s activation, and `Page_Up`/`Page_Down`/
/// `Home`/`End` are `AppDisplay._onKeyPressEvent` (`appDisplay.js:1599-1618`).
#[test]
fn overview_app_grid_navigates_with_the_keyboard() {
    let ids: Vec<String> = (0..30).map(|i| format!("o{i:02}.desktop")).collect();
    let others: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (mut f, recorder) = app_grid_fixture(&[], &others);
    // The overview must actually hold keyboard focus or none of its key tiers run.
    f.synoik_state().update_keyboard_focus();
    assert!(f.synoik().keyboard_focus.is_overview());
    let area = overview_controls(&mut f).app_display;
    assert_eq!(
        f.synoik().app_grid.page_count(area),
        2,
        "30 apps span two pages"
    );
    let center = |f: &mut Fixture, i: usize| {
        f.synoik()
            .app_grid
            .entry_center(i, area)
            .expect("the tile is on the visible page")
    };

    // Nothing is lit until a key asks for it, and the first arrow takes the page's first
    // tile whichever way it points — our divergence: GNOME reaches the grid from the
    // search entry through a stage-wide focus chain we do not have.
    assert_eq!(f.synoik().app_grid.focused(), None);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.synoik().app_grid.focused(), Some(0));

    // Right moves along the row; Down drops a row in the same column.
    tap(&mut f, KEY_RIGHT);
    let right = f
        .synoik()
        .app_grid
        .focused()
        .expect("Right moved the focus");
    assert!(right > 0);
    assert_eq!(
        center(&mut f, right).y,
        center(&mut f, 0).y,
        "Right stays in the row"
    );
    assert!(center(&mut f, right).x > center(&mut f, 0).x);

    tap(&mut f, KEY_DOWN);
    let down = f.synoik().app_grid.focused().expect("Down moved the focus");
    assert_eq!(
        center(&mut f, down).x,
        center(&mut f, right).x,
        "Down stays in the column"
    );
    assert!(center(&mut f, down).y > center(&mut f, right).y);

    // …and back the way we came.
    tap(&mut f, KEY_UP);
    assert_eq!(f.synoik().app_grid.focused(), Some(right));
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.synoik().app_grid.focused(), Some(0));
    // Nothing lies left of the first column of the first page. The key is still consumed
    // — it must not fall through to the window binds behind the grid.
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.synoik().app_grid.focused(), Some(0));
    assert!(f.synoik().layout.is_app_grid_open());

    // Right off the end of a row crosses to the next page in the *same row*: the pages
    // sit side by side in one viewport, so that tile really is the nearest one to the
    // right. The view pages over to follow the focus.
    let row_y = center(&mut f, 0).y;
    for _ in 0..12 {
        tap(&mut f, KEY_RIGHT);
        if f.synoik().app_grid.current_page() == 1 {
            break;
        }
    }
    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "the page followed the focus across"
    );
    let crossed = f.synoik().app_grid.focused().unwrap();
    let per_page = f.synoik().app_grid.items_per_page(area);
    assert!(crossed >= per_page, "…onto the second page");
    assert_eq!(center(&mut f, crossed).y, row_y, "…staying in its row");

    // The paging keys move the page and leave the focus where it was.
    tap(&mut f, KEY_PAGEUP);
    assert_eq!(f.synoik().app_grid.current_page(), 0);
    assert_eq!(f.synoik().app_grid.focused(), Some(crossed));
    tap(&mut f, KEY_END);
    assert_eq!(f.synoik().app_grid.current_page(), 1);
    tap(&mut f, KEY_HOME);
    assert_eq!(f.synoik().app_grid.current_page(), 0);
    tap(&mut f, KEY_PAGEDOWN);
    assert_eq!(f.synoik().app_grid.current_page(), 1);

    // Enter launches the focused tile and closes the overview, exactly as a click does.
    tap(&mut f, KEY_ENTER);
    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, ids[crossed], "the focused app launched");
    drop(calls);
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and the overview closed"
    );
}

/// With more apps than fit one page, the grid paginates: a wheel scroll over it and a
/// click on a page-indicator dot move between pages, and a fresh overview open resets
/// to the first page (`Main.overview 'hidden'` → `goToPage(0)`, `appDisplay.js:1342`).
#[test]
fn overview_app_grid_paginates_and_navigates() {
    let ids: Vec<String> = (0..30).map(|i| format!("o{i:02}.desktop")).collect();
    let others: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (mut f, _recorder) = app_grid_fixture(&["a.desktop"], &others);
    let area = overview_controls(&mut f).app_display;
    assert_eq!(
        f.synoik().app_grid.page_count(area),
        2,
        "30 apps span two pages"
    );
    assert_eq!(f.synoik().app_grid.current_page(), 0);

    // A wheel notch over the grid pages forward.
    let tile = f.synoik().app_grid.tile_center(0, area).unwrap();
    f.pointer_motion(tile.x, tile.y);
    f.scroll_wheel();
    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "a wheel notch pages the grid forward"
    );

    // Clicking the first page-indicator dot returns to page 0.
    let dot0 = f.synoik().app_grid.indicator_center(0, area).unwrap();
    f.pointer_motion(dot0.x - tile.x, dot0.y - tile.y); // relative from the tile
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "clicking a dot jumps to its page"
    );

    // Clicking the next navigation arrow steps forward one page.
    use crate::ui::app_grid::PageArrow;
    let next = f
        .synoik()
        .app_grid
        .arrow_center(PageArrow::Next, area)
        .unwrap();
    f.pointer_motion(next.x - dot0.x, next.y - dot0.y); // relative from the dot
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "clicking the next arrow advances a page"
    );
    // On page 1 the previous arrow exists; clicking it steps back to page 0.
    let prev = f
        .synoik()
        .app_grid
        .arrow_center(PageArrow::Prev, area)
        .unwrap();
    f.pointer_motion(prev.x - next.x, prev.y - next.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "clicking the previous arrow steps back a page"
    );

    // A fresh overview open resets to page 0.
    f.synoik().app_grid.set_page(1, area);
    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();
    f.synoik().refresh_overview_search_state(); // falling edge
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik().refresh_overview_search_state(); // rising edge → reset
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "a fresh overview open starts on page 0"
    );
}

/// The startup icon prewarm enqueues the dash + grid icon decodes off-thread (so the
/// first overview open doesn't rasterize them on the opening frame), gated on the
/// worker being wired, and dedups repeat requests.
#[test]
fn overview_prewarm_requests_dash_and_grid_icon_decodes() {
    let (mut f, _r) = app_grid_fixture(&["fav.desktop"], &["a.desktop", "b.desktop"]);

    // Without a worker wired, prewarm must be a no-op (else it would decode inline on
    // the main thread — the stall it exists to avoid).
    assert!(!f.synoik().app_icon_cache.has_worker());
    f.synoik().prewarm_app_icons(); // no panic, nothing to observe

    // Wire the async path to a test channel and prewarm: every fake app icon is
    // `Fallback`, so the requests dedup to one per surface size — the dash's 64px and
    // the grid's 96px.
    let rx = f.synoik().app_icon_cache.wire_test_channel();
    assert!(f.synoik().app_icon_cache.has_worker());
    f.synoik().prewarm_app_icons();

    let mut sizes: Vec<u32> = rx
        .try_iter()
        .map(|req| req.logical_px().round() as u32)
        .collect();
    sizes.sort_unstable();
    assert_eq!(
        sizes,
        vec![64, 96],
        "prewarm requests the dash (64) and grid (96) icon decodes"
    );

    // A second prewarm enqueues nothing new — the keys are in flight (nothing was
    // applied to resolve them).
    f.synoik().prewarm_app_icons();
    assert_eq!(
        rx.try_iter().count(),
        0,
        "in-flight keys are not re-requested"
    );
}

/// A scale change re-warms the icon decodes.
///
/// Gustavo, 2026-07-28: after changing the resolution or scale, the first app-grid open
/// showed icons appearing a few at a time. Every decode is keyed on the icon's *physical*
/// size, so the startup warm is worthless at the new scale and each icon decodes on the
/// frame it is first drawn.
#[test]
fn overview_prewarm_follows_a_scale_change() {
    let (mut f, _r) = app_grid_fixture(&["fav.desktop"], &["a.desktop", "b.desktop"]);

    let rx = f.synoik().app_icon_cache.wire_test_channel();
    f.synoik().prewarm_app_icons();
    let warmed: Vec<f64> = rx.try_iter().map(|req| req.scale()).collect();
    assert!(!warmed.is_empty(), "the fixture warms at its own scale");
    assert!(warmed.iter().all(|scale| *scale == 1.));

    let output = f.synoik_output(1);
    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(2.)),
        None,
    );
    f.synoik().output_resized(&output);

    let rewarmed: Vec<f64> = rx.try_iter().map(|req| req.scale()).collect();
    assert!(
        !rewarmed.is_empty(),
        "a scale change re-warms; without this the grid decodes each icon as it draws it"
    );
    assert!(
        rewarmed.iter().all(|scale| *scale == 2.),
        "…at the new scale: {rewarmed:?}"
    );
}

/// The dash is only live while the overview is open: a click at a favorite's
/// position with the overview closed passes through to the windows/workspace, never
/// launching the app.
#[test]
fn overview_dash_ignored_when_overview_closed() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();
    assert!(!f.synoik().layout.is_overview_open());

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        recorder.calls.borrow().is_empty(),
        "a closed overview has no dash to click"
    );
}

/// The dash intercept fires only while the dash is actually *visible*. A surface
/// that renders over a still-open overview without closing it (the screenshot UI
/// here; a lock surface shares the identical guard) hides the dash — so a click at a
/// favorite's position must NOT launch it, or the invisible dash would eat clicks
/// (and, for the lock case, launch apps into a locked session). GNOME avoids the
/// state entirely by dropping the overview from those session modes.
#[test]
fn overview_dash_inert_behind_screenshot_ui() {
    // The screenshot UI captures the screen through the renderer, so this one needs a
    // Vulkan device; skip cleanly without one (like the `vulkan_*` render tests).
    if crate::render_helpers::vulkan::VulkanRenderer::new().is_err() {
        eprintln!("skipping overview_dash_inert_behind_screenshot_ui: no Vulkan device");
        return;
    }

    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    let center = dash_tile_center(&mut f, 0);

    // Give the cursor an output (screenshot capture is per-output-under-cursor), clear
    // of the dash so no hover is set yet.
    f.pointer_motion(960., 540.);
    // Raise the screenshot UI over the open overview (it doesn't close the overview).
    f.synoik_state().open_screenshot_ui(None);
    assert!(
        f.synoik().screenshot_ui.is_open(),
        "screenshot UI must open"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "the overview stays open behind the screenshot UI"
    );

    // Now move onto the (hidden) dash favorite and click.
    f.pointer_motion(center.x - 960., center.y - 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    assert!(
        recorder.calls.borrow().is_empty(),
        "the dash is hidden behind the screenshot UI — its click must not launch an app"
    );
    // And the hover tracker leaves the tile unhovered while the dash is hidden.
    assert_eq!(f.synoik().dash.hovered_for_test(), None);
}

/// Pointer motion over the dash tracks the hovered tile (the `.overview-icon:hover`
/// fill target); leaving the dash clears it. Only while the overview is open.
#[test]
fn overview_dash_hover_tracks_tile() {
    use crate::ui::dash::DashHit;

    let (mut f, _recorder) = dash_fixture(&["a.desktop", "b.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    f.pointer_motion(center.x, center.y);
    assert_eq!(
        f.synoik().dash.hovered_for_test(),
        Some(DashHit::App(0)),
        "hovering a favorite marks it hovered"
    );

    // Move well clear of the dash (top-left corner): hover clears.
    f.pointer_motion(-center.x + 5., -center.y + 5.);
    assert_eq!(
        f.synoik().dash.hovered_for_test(),
        None,
        "leaving the dash clears the hover"
    );
}

// ---- S4: overview search ----

/// A fixture with a 1920×1080 output, an injected fake `AppSystem` whose catalog
/// resolves each id in `groups` and returns `groups` from `search` (the fake ignores
/// the query — seed exactly the relevance tiers you want), the overview open, and the
/// keyboard focused on it (so typing engages the search). Returns the launch recorder.
fn search_overview(groups: &[&[&str]]) -> (Fixture, crate::app_system::RecordingLauncher) {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let recorder = RecordingLauncher::default();
    let ids: Vec<&str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    let apps = ids
        .iter()
        .map(|id| AppEntry::fake(id, id))
        .collect::<Vec<_>>();
    let catalog = FakeCatalog::new(apps);
    *catalog.search_result.borrow_mut() = groups
        .iter()
        .map(|g| g.iter().map(|s| s.to_string()).collect())
        .collect();
    f.synoik().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder.clone()));

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    assert!(
        f.synoik().keyboard_focus.is_overview(),
        "the overview must hold keyboard focus so typing engages search"
    );
    (f, recorder)
}

/// The resting entry is a puck; typing (or a click on it) grows it to GNOME's pill, and
/// clearing puts it back. The divergence, driven through the real key path.
#[test]
fn overview_search_entry_rests_collapsed_and_expands_on_typing() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    assert!(
        !f.synoik().overview_search.is_expanded(),
        "the entry rests as a puck until something asks for it"
    );
    tap(&mut f, KEY_A);
    assert!(
        f.synoik().overview_search.is_expanded(),
        "the first keystroke grows it into the pill"
    );

    tap(&mut f, KEY_ESC);
    assert!(
        !f.synoik().overview_search.is_expanded(),
        "clearing collapses it again — an empty open pill is a state with no meaning"
    );
}

/// GNOME's editing combos reach the query through the real key path — the whole point of
/// routing entries through the shared `TextEdit`. Ctrl-BackSpace used to be refused outright
/// (every modified key was), so this would have left the query untouched.
#[test]
fn overview_search_ctrl_backspace_deletes_a_word() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    for key in [KEY_A, KEY_W, KEY_SPACE, KEY_E, KEY_R] {
        tap(&mut f, key);
    }
    assert_eq!(f.synoik().overview_search.query(), "aw er");

    f.key_press(KEY_LEFTCTRL);
    tap(&mut f, KEY_BACKSPACE);
    f.key_release(KEY_LEFTCTRL);
    assert_eq!(
        f.synoik().overview_search.query(),
        "aw ",
        "Ctrl-BackSpace deletes the previous word, not one character"
    );
}

// ---- Clipboard (`st-entry.c:656-740`) ----

const KEY_C: u32 = 46;
const KEY_X: u32 = 45;
const KEY_INSERT: u32 = 110;

/// Tap `key` with Ctrl held.
fn ctrl_tap(f: &mut Fixture, key: u32) {
    f.key_press(KEY_LEFTCTRL);
    tap(f, key);
    f.key_release(KEY_LEFTCTRL);
}

/// What the compositor currently owns the clipboard with, as text — `None` when a client owns
/// it or it is empty.
fn clipboard_text(f: &mut Fixture) -> Option<String> {
    use smithay::wayland::selection::data_device::current_data_device_selection_userdata;

    let bytes = current_data_device_selection_userdata(&f.synoik().seat)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Put text on the clipboard the way a copy does, so a paste has something to find.
fn seed_clipboard(f: &mut Fixture, text: &str) {
    let mime = crate::clipboard::TEXT_MIME_TYPES
        .iter()
        .map(|m| (*m).to_owned())
        .collect();
    f.synoik()
        .set_clipboard(mime, text.as_bytes().to_vec().into());
}

/// `Ctrl-c` copies the selection and leaves the entry alone; `Ctrl-x` copies and deletes it.
/// Both offer GNOME's three text mime types (`st-clipboard.c:49-53`).
#[test]
fn ctrl_c_copies_and_ctrl_x_cuts_the_selection() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);

    for key in [KEY_A, KEY_W] {
        tap(&mut f, key);
    }
    assert_eq!(f.synoik().run_dialog.entry(), "aw");

    ctrl_tap(&mut f, KEY_A); // select all
    ctrl_tap(&mut f, KEY_C);
    assert_eq!(clipboard_text(&mut f).as_deref(), Some("aw"));
    assert_eq!(
        f.synoik().clipboard_mime_types,
        crate::clipboard::TEXT_MIME_TYPES,
        "a copy offers the mime types GNOME's own clipboard does"
    );
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "aw",
        "a copy must not touch the text"
    );

    ctrl_tap(&mut f, KEY_X);
    assert_eq!(clipboard_text(&mut f).as_deref(), Some("aw"));
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "",
        "a cut deletes what it copied"
    );
}

/// With nothing selected there is nothing to copy, and the clipboard is left as it was —
/// St's `if (text && strlen (text))` guard.
#[test]
fn a_copy_with_no_selection_leaves_the_clipboard_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);
    seed_clipboard(&mut f, "keep me");

    tap(&mut f, KEY_A);
    ctrl_tap(&mut f, KEY_C);
    assert_eq!(clipboard_text(&mut f).as_deref(), Some("keep me"));
    ctrl_tap(&mut f, KEY_X);
    assert_eq!(clipboard_text(&mut f).as_deref(), Some("keep me"));
    assert_eq!(f.synoik().run_dialog.entry(), "a", "and nothing was cut");
}

/// `Ctrl-v` and `Shift-Insert` both paste, through the entry's own text path.
#[test]
fn ctrl_v_and_shift_insert_paste_into_an_entry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);
    seed_clipboard(&mut f, "gedit");

    ctrl_tap(&mut f, KEY_V);
    assert_eq!(f.synoik().run_dialog.entry(), "gedit");

    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_INSERT);
    f.key_release(KEY_LEFTSHIFT);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "geditgedit",
        "Shift-Insert is a paste too (st-entry.c:669-670)"
    );
}

/// A paste replaces the selection, the way typing does — `st_entry_clipboard_callback` deletes
/// the selection before inserting (`st-entry.c:610-611`).
#[test]
fn a_paste_replaces_the_selection() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);

    for key in [KEY_A, KEY_W] {
        tap(&mut f, key);
    }
    ctrl_tap(&mut f, KEY_A);
    seed_clipboard(&mut f, "zz");
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(f.synoik().run_dialog.entry(), "zz");
}

/// Our divergence: a paste is capped at [`PASTE_LIMIT`] and only its first line goes in. These
/// entries hold a query, a command, a folder name or a password — never a document.
#[test]
fn a_paste_is_capped_and_single_line() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);

    seed_clipboard(&mut f, "first\nsecond\nthird");
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "first",
        "lines after the first are dropped, not glued on"
    );

    f.synoik().run_dialog.close();
    f.synoik_state().do_action(Action::ShowRunDialog, false);
    let huge = "x".repeat(crate::clipboard::PASTE_LIMIT * 2);
    seed_clipboard(&mut f, &huge);
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(
        f.synoik().run_dialog.entry().len(),
        crate::clipboard::PASTE_LIMIT,
        "a runaway clipboard is truncated at the cap"
    );
}

/// A masked field never yields its contents to the clipboard
/// (`clutter_text_get_password_char () == 0` guards both copy and cut, `st-entry.c:692,717`)
/// — but pasting *into* one is unguarded, because that is how a password manager is used.
#[test]
fn a_password_entry_refuses_copy_and_cut_but_accepts_a_paste() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state().update_keyboard_focus();

    seed_clipboard(&mut f, "hunter2");
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(
        f.synoik().unlock_dialog.entry().text(),
        "hunter2",
        "a paste into a password field is not guarded"
    );

    ctrl_tap(&mut f, KEY_A); // select all
    seed_clipboard(&mut f, "unchanged");
    ctrl_tap(&mut f, KEY_C);
    assert_eq!(
        clipboard_text(&mut f).as_deref(),
        Some("unchanged"),
        "a copy out of a password field must be refused"
    );
    ctrl_tap(&mut f, KEY_X);
    assert_eq!(
        clipboard_text(&mut f).as_deref(),
        Some("unchanged"),
        "and so must a cut"
    );
    assert_eq!(
        f.synoik().unlock_dialog.entry().text(),
        "hunter2",
        "a refused cut must not delete either — it is claimed, and inert"
    );
}

/// The real path: the clipboard belongs to a *client*, so the data only arrives over a pipe the
/// compositor hands out and then reads asynchronously. Everything above short-circuits through
/// the compositor-owned selection instead and would not notice this breaking.
#[test]
fn a_paste_reads_a_clients_selection_over_the_pipe() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    map_focused_window(&mut f, id);
    f.client(id).get_keyboard();
    f.roundtrip(id);

    // The client must hold keyboard focus to set the selection, so this happens before the
    // modal dialog opens.
    f.client(id)
        .offer_clipboard(&["text/plain;charset=utf-8"], b"from the client\n");
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().clipboard_mime_types,
        vec!["text/plain;charset=utf-8".to_owned()],
        "the compositor caches what the new owner offers"
    );

    f.synoik_state().do_action(Action::ShowRunDialog, false);
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(
        f.synoik().run_dialog.entry(),
        "",
        "nothing lands synchronously — the client has not written yet"
    );

    // Pump both sides until the transfer completes: the client has to receive `send`, write and
    // close, and only then does the compositor's fd source see the data and the EOF.
    for _ in 0..50 {
        f.double_roundtrip(id);
        if !f.synoik().run_dialog.entry().is_empty() {
            break;
        }
    }
    assert_eq!(f.synoik().run_dialog.entry(), "from the client");
    assert_eq!(
        f.client(id).state.selection_sends,
        vec!["text/plain;charset=utf-8".to_owned()],
        "and it asked for the mime type it picked"
    );
    assert!(
        !f.synoik().clipboard_paste_pending,
        "the in-flight flag must be cleared, or every later paste is dropped"
    );
}

/// Pasting an image (the clipboard a screenshot leaves behind) into a text entry does nothing:
/// there is no text mime type to ask for. GNOME's `pick_mimetype` returns NULL the same way.
#[test]
fn a_non_text_clipboard_pastes_nothing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state().do_action(Action::ShowRunDialog, false);

    f.synoik()
        .set_clipboard(vec!["image/png".to_owned()], b"\x89PNG".to_vec().into());
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(f.synoik().run_dialog.entry(), "");
}

/// The clipboard reaches the two entries that sit at the bottom of a fall-through ladder too —
/// they never go through `deliver_shell_key`, so they ask for the bindings themselves.
#[test]
fn the_overview_search_copies_and_pastes() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    seed_clipboard(&mut f, "aw");
    // The entry is closed and empty, so a Ctrl-v falls through to the overview's own binds —
    // gnome-shell's entry has no key focus yet either.
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(f.synoik().overview_search.query(), "");

    tap(&mut f, KEY_A);
    ctrl_tap(&mut f, KEY_V);
    assert_eq!(
        f.synoik().overview_search.query(),
        "aaw",
        "once the search is engaged the entry takes the paste"
    );

    ctrl_tap(&mut f, KEY_A); // select all
    ctrl_tap(&mut f, KEY_X);
    assert_eq!(clipboard_text(&mut f).as_deref(), Some("aaw"));
    assert_eq!(f.synoik().overview_search.query(), "");
}

/// Home/arrows move a real caret, so typing lands mid-string instead of always appending.
#[test]
fn overview_search_caret_moves_and_types_mid_string() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    for key in [KEY_A, KEY_W] {
        tap(&mut f, key);
    }
    assert_eq!(f.synoik().overview_search.query(), "aw");
    tap(&mut f, KEY_HOME);
    tap(&mut f, KEY_Z);
    assert_eq!(
        f.synoik().overview_search.query(),
        "zaw",
        "Home put the caret at the start; the key typed there"
    );
}

/// Typing a printable engages search and lists the provider's results (in group
/// order); the entry becomes active.
#[test]
fn overview_search_types_query_and_lists_results() {
    let (mut f, _rec) = search_overview(&[&["a.desktop", "b.desktop"]]);

    assert!(!f.synoik().overview_search.is_active());
    tap(&mut f, KEY_A);
    assert!(
        f.synoik().overview_search.is_active(),
        "a printable key must start a search"
    );
    assert_eq!(f.synoik().overview_search.result_id(0), Some("a.desktop"));
    assert_eq!(f.synoik().overview_search.result_id(1), Some("b.desktop"));
    assert_eq!(f.synoik().overview_search.result_id(2), None);
}

/// Results are filtered to `should_show` apps and capped at `MAX_RESULTS`.
#[test]
fn overview_search_filters_hidden_and_caps_results() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // 8 shown + 1 hidden; expect the hidden one filtered and the rest capped at 6.
    let mut apps: Vec<AppEntry> = (0..8)
        .map(|i| AppEntry::fake(&format!("app{i}.desktop"), &format!("App {i}")))
        .collect();
    apps.push(AppEntry {
        should_show: false,
        ..AppEntry::fake("hidden.desktop", "Hidden")
    });
    let mut group: Vec<String> = vec!["hidden.desktop".to_owned()];
    group.extend((0..8).map(|i| format!("app{i}.desktop")));
    let catalog = FakeCatalog::new(apps);
    *catalog.search_result.borrow_mut() = vec![group];
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();

    tap(&mut f, KEY_A);
    // hidden.desktop filtered → first result is app0; capped at 6.
    assert_eq!(
        f.synoik().overview_search.result_id(0),
        Some("app0.desktop")
    );
    assert_eq!(
        f.synoik().overview_search.result_id(6),
        None,
        "results are capped at MAX_RESULTS (6)"
    );
    assert_eq!(
        f.synoik().overview_search.result_id(5),
        Some("app5.desktop")
    );
}

/// Enter launches the default (first) result and closes the overview, clearing search.
#[test]
fn overview_search_enter_launches_selected_and_closes() {
    let (mut f, recorder) = search_overview(&[&["a.desktop", "b.desktop"]]);

    tap(&mut f, KEY_A);
    tap(&mut f, KEY_ENTER);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "a.desktop",
        "Enter launches the first result"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "launching from search closes the overview"
    );
    assert!(
        !f.synoik().overview_search.is_active(),
        "the search is cleared on activate"
    );
}

/// The Right arrow moves the selection; Enter then launches that result.
#[test]
fn overview_search_arrow_then_enter_launches_second() {
    let (mut f, recorder) = search_overview(&[&["a.desktop", "b.desktop"]]);

    tap(&mut f, KEY_A);
    tap(&mut f, KEY_RIGHT);
    tap(&mut f, KEY_ENTER);

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "selection moved to the second result"
    );
}

/// Tab walks the search results forward, Shift+Tab back.
///
/// The one that was broken is Shift+Tab: `ISO_Left_Tab` is the *modified* keysym, and the
/// key path hands surfaces the **raw** one, so what arrives is a plain `Tab` and the
/// select-prev arm matching `ISO_Left_Tab` was dead for real input. Only a test that goes
/// through the input path can see that — calling `handle_key(ISO_Left_Tab, …)` directly
/// passes against the bug.
#[test]
fn overview_search_shift_tab_steps_back() {
    let (mut f, recorder) = search_overview(&[&["a.desktop", "b.desktop", "c.desktop"]]);

    tap(&mut f, KEY_A);
    tap(&mut f, KEY_TAB);
    tap(&mut f, KEY_TAB);
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    tap(&mut f, KEY_ENTER);

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "two forward then one back lands on the second result"
    );
}

/// A click on a result tile launches it and closes the overview.
#[test]
fn overview_search_click_result_launches() {
    let (mut f, recorder) = search_overview(&[&["a.desktop", "b.desktop"]]);
    tap(&mut f, KEY_A);

    let area = overview_controls(&mut f).into();
    let center = f
        .synoik()
        .overview_search
        .result_center(1, area)
        .expect("result tile 1");
    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "clicking a tile launches that app"
    );
    assert!(!f.synoik().layout.is_overview_open());
}

/// Enter with an active query but zero results is consumed — it must NOT fall through
/// to the hardcoded Return→ToggleOverview bind and close the overview.
#[test]
fn overview_search_enter_with_no_results_keeps_overview_open() {
    let (mut f, recorder) = search_overview(&[]); // no groups → no results

    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());
    assert_eq!(f.synoik().overview_search.result_id(0), None);
    tap(&mut f, KEY_ENTER);

    assert!(recorder.calls.borrow().is_empty(), "nothing to launch");
    assert!(
        f.synoik().layout.is_overview_open(),
        "Enter with no results must not close the overview"
    );
}

/// Escape while active clears the search (overview stays open); a second Escape (now
/// inactive) falls through to the hardcoded bind and closes the overview.
#[test]
fn overview_search_escape_clears_then_closes() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());

    tap(&mut f, KEY_ESC);
    assert!(
        !f.synoik().overview_search.is_active(),
        "first Escape clears"
    );
    assert!(
        f.synoik().layout.is_overview_open(),
        "clearing the search leaves the overview open"
    );

    tap(&mut f, KEY_ESC);
    f.synoik_complete_animations();
    assert!(
        !f.synoik().layout.is_overview_open(),
        "a second Escape (inactive) closes the overview via the hardcoded bind"
    );
}

/// Backspacing the query to empty deactivates the search (results cleared).
#[test]
fn overview_search_backspace_empties_deactivates() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());
    tap(&mut f, KEY_BACKSPACE);
    assert!(!f.synoik().overview_search.is_active());
    assert_eq!(f.synoik().overview_search.result_id(0), None);
}

/// A space as the first key does not engage search (the query tokenizes to empty),
/// mirroring GNOME's `_shouldTriggerSearch`.
#[test]
fn overview_search_space_first_stays_inactive() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_SPACE);
    assert!(
        !f.synoik().overview_search.is_active(),
        "a leading space must not start a search"
    );
}

/// gnome-shell cross-fades the search over the window picker (`_onSearchChanged`,
/// `overviewControls.js:609-643`) and makes the covered picker inert, so a click
/// beside the results neither activates a preview hiding under them nor reads as
/// "clicked the empty desktop" and leaves the overview.
#[test]
fn overview_search_makes_the_picker_inert() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    let catalog = FakeCatalog::new(vec![AppEntry::fake("a.desktop", "a.desktop")]);
    *catalog.search_result.borrow_mut() = vec![vec!["a.desktop".to_string()]];
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();

    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let center = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);

    // Not searching: the preview is live and the fade is fully off.
    assert_eq!(f.synoik().overview_search_fade(), 0.);
    pointer_motion_to(&mut f, center.0, center.1);
    assert!(
        f.synoik().window_under_cursor().is_some(),
        "a preview must be clickable while not searching"
    );

    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());
    assert!(
        f.synoik().window_under_cursor().is_none(),
        "a preview under the search results must not activate"
    );

    // The fade eases rather than snapping: armed on one frame, mid-way on the next.
    f.synoik().advance_animations();
    {
        let synoik = f.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik.clock.set_unadjusted(now + Duration::from_millis(60));
        synoik.advance_animations();
    }
    let mid = f.synoik().overview_search_fade();
    assert!(mid > 0. && mid < 1., "the search fade must ease, got {mid}");
    f.settle_animations();
    assert_eq!(f.synoik().overview_search_fade(), 1.);

    // A click out on the covered picker (the pointer has not moved) is consumed by
    // the results strip.
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "clicking the covered picker must not leave the overview"
    );
    assert!(
        f.synoik().overview_search.is_active(),
        "and must not discard the search"
    );

    // Clearing brings the picker back — and its reactivity with it.
    tap(&mut f, KEY_ESC);
    f.settle_animations();
    assert!(!f.synoik().overview_search.is_active());
    assert_eq!(f.synoik().overview_search_fade(), 0.);
    assert!(
        f.synoik().window_under_cursor().is_some(),
        "clearing the search must make the picker live again"
    );
}

/// The thumbnail strip fades and goes inert with the picker (gnome-shell forces
/// its opacity to 0 whenever `searchActive`, `overviewControls.js:550-580`), so a
/// click where a thumbnail used to be must not switch workspace.
#[test]
fn overview_search_makes_the_thumbnail_strip_inert() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);

    let catalog = FakeCatalog::new(vec![AppEntry::fake("a.desktop", "a.desktop")]);
    *catalog.search_result.borrow_mut() = vec![vec!["a.desktop".to_string()]];
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();

    let (tx, ty) = thumbnail_center(&mut f, 0);
    let active = f.synoik().layout.active_workspace().unwrap().id();

    // Sanity: the same click switches workspace when nothing covers the strip —
    // otherwise this test could pass by simply missing the thumbnail.
    pointer_motion_to(&mut f, tx, ty);
    assert!(
        f.synoik().thumbnail_workspace_under_cursor().is_some(),
        "the probe must actually be over a thumbnail"
    );

    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());

    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert_eq!(
        f.synoik().layout.active_workspace().unwrap().id(),
        active,
        "a thumbnail under the search results must not switch workspace"
    );
    assert!(f.synoik().layout.is_overview_open());
}

/// The idle entry pill is drawn too, so it consumes its clicks the same way: a
/// fall-through would land on the workspace behind it and leave the overview.
/// (gnome-shell focuses the entry on that click; we have no click-to-focus, but
/// the click must still not escape.)
#[test]
fn overview_search_idle_entry_body_consumes_clicks() {
    let (mut f, recorder) = search_overview(&[&["a.desktop"]]);
    assert!(!f.synoik().overview_search.is_active());

    let area = overview_controls(&mut f).into();
    let pill = f.synoik().overview_search.entry_pill(area);
    let center = (pill.loc.x + pill.size.w / 2., pill.loc.y + pill.size.h / 2.);

    f.pointer_motion(center.0, center.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.synoik_complete_animations();

    assert!(
        f.synoik().layout.is_overview_open(),
        "a click on the idle entry must not fall through and close the overview"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "clicking the entry must not launch anything"
    );
}

/// The entry pill is an opaque drawn control, so a click on its body must be
/// CONSUMED — never fall through to the workspace behind it, which would leave the
/// overview and discard the search.
#[test]
fn overview_search_active_entry_body_consumes_clicks() {
    let (mut f, recorder) = search_overview(&[&["a.desktop"]]);
    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());

    let area = overview_controls(&mut f).into();
    let pill = f.synoik().overview_search.entry_pill(area);
    let center = (pill.loc.x + pill.size.w / 2., pill.loc.y + pill.size.h / 2.);

    f.pointer_motion(center.0, center.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.synoik().layout.is_overview_open(),
        "a click on the active entry must not fall through and close the overview"
    );
    assert!(
        f.synoik().overview_search.is_active(),
        "the query must survive a click on the entry"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "the entry body launches nothing"
    );
}

/// Typing when the overview is closed does not engage search (keyboard focus isn't
/// Overview) — the search is inert outside the overview.
#[test]
fn overview_search_inert_when_overview_closed() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let catalog = FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")]);
    *catalog.search_result.borrow_mut() = vec![vec!["a.desktop".to_owned()]];
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.synoik_state().update_keyboard_focus();
    assert!(!f.synoik().keyboard_focus.is_overview());

    tap(&mut f, KEY_A);
    assert!(
        !f.synoik().overview_search.is_active(),
        "typing outside the overview must not start a search"
    );
}

/// Closing the overview while a search is up takes the search with it
/// (`prepareToLeaveOverview` → `_setSearchActive(false)`, plus the `reset()` on unmap,
/// `searchController.js:117-131`). It used to survive: the picker's alpha is
/// `1 - overview_search_fade()` with no overview term, so every window stayed at alpha 0
/// behind the shade, Escape had no search left to reach, and only re-opening healed it.
#[test]
fn overview_search_closing_the_overview_drops_the_search() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);
    // Prime the edge detector for the open state *before* typing: the rising edge is
    // itself a clear, and it has not been seen yet in this fixture.
    f.synoik().refresh_overview_search_state();
    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik().refresh_overview_search_state(); // falling edge → clear
    assert!(
        !f.synoik().overview_search.is_active(),
        "the search does not outlive the overview"
    );

    f.synoik_complete_animations();
    f.synoik().advance_animations();
    assert_eq!(
        f.synoik().overview_search_fade(),
        0.,
        "…so the window picker is at full alpha again, not hidden behind the shade"
    );
}

/// Re-opening the overview starts search fresh: a query left from a previous session
/// is cleared on the visibility rising edge (`refresh_overview_search_state`).
#[test]
fn overview_search_resets_on_reopen() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);
    tap(&mut f, KEY_A);
    assert!(f.synoik().overview_search.is_active());

    // Prime the edge detector for the open state, then close and re-open.
    f.synoik().refresh_overview_search_state();
    f.synoik_state().do_action(Action::CloseOverview, false);
    f.synoik_complete_animations();
    f.synoik().refresh_overview_search_state(); // falling edge
    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik().refresh_overview_search_state(); // rising edge → clear

    assert!(
        !f.synoik().overview_search.is_active(),
        "a re-opened overview starts with an empty search"
    );
}

/// **Lifecycle.** A launch puts the app in `STARTING` and its first window moves
/// it to `RUNNING` — `shell_app_activate_full`'s stopped branch opening a startup
/// sequence (`shell-app.c:508-521` via `meta-launch-context.c:158-184`), then
/// `_shell_app_handle_startup_sequence`'s completed branch (`shell-app.c:1190-1195`).
///
/// The intermediate state is the point: between the spawn and the map, GNOME shows
/// a running dot but offers neither Quit nor a new window.
#[test]
fn launching_an_app_marks_it_starting_until_its_window_maps() {
    use crate::app_system::{
        AppEntry, AppState, AppSystem, FakeCatalog, LaunchMode, RecordingLauncher,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let recorder = RecordingLauncher::default();
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(recorder.clone()),
    );
    assert_eq!(
        f.synoik().app_system.app_state("a.desktop"),
        AppState::Stopped
    );
    assert!(f.synoik().app_system.can_open_new_window("a.desktop"));

    f.synoik()
        .app_system
        .launch(
            "a.desktop",
            LaunchMode::Activate,
            &crate::app_system::LaunchContext {
                token: Some("tok-1".to_owned()),
                workspace: None,
                now: get_monotonic_time(),
            },
        )
        .expect("launch");

    assert_eq!(
        f.synoik().app_system.app_state("a.desktop"),
        AppState::Starting,
        "a launch opens a startup sequence"
    );
    assert!(
        f.synoik().app_system.shows_running_dot("a.desktop"),
        "a starting app already shows the running dot (`appDisplay.js:3007`)"
    );
    assert!(
        !f.synoik().app_system.can_open_new_window("a.desktop"),
        "a starting app cannot be asked for another window (`shell-app.c:606-611`)"
    );
    assert_eq!(
        recorder.calls.borrow()[0].2.as_deref(),
        Some("tok-1"),
        "the activation token reaches the launcher, which exports it to the child"
    );

    // Its first window completes the sequence.
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("a");
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.synoik().app_system.app_state("a.desktop"),
        AppState::Running,
        "the mapping window completes the sequence"
    );
    assert_eq!(
        f.synoik().app_system.starting_apps().count(),
        0,
        "the sequence is consumed, not left open"
    );
}

/// The dash's running dot reads `state !== STOPPED`, not "has windows"
/// (`AppIcon._updateRunningStyle`, `appDisplay.js:3007-3012`), so a favorite shows
/// one from the moment it is launched. That needs the *state* change to reach the
/// dash on its own: a launch touches no window, so the window snapshot alone would
/// leave the dot until the app mapped.
#[test]
fn a_launching_favorite_shows_its_running_dot_before_its_window_maps() {
    use crate::app_system::{
        AppEntry, AppSystem, FakeCatalog, LaunchContext, LaunchMode, RecordingLauncher,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".into()]);
    f.synoik().sync_dash_favorites();

    let dot = |f: &mut Fixture| f.synoik().dash.item_shows_running_dot(0).unwrap();
    assert!(!dot(&mut f), "a stopped favorite shows no dot");

    f.synoik()
        .app_system
        .launch(
            "a.desktop",
            LaunchMode::Activate,
            &LaunchContext::bare(get_monotonic_time()),
        )
        .expect("launch");
    assert!(
        f.synoik().sync_running_apps(),
        "a state change must report as a change, or the dash never redisplays"
    );
    f.synoik().sync_dash_favorites();
    assert!(
        dot(&mut f),
        "a STARTING favorite already shows the dot, before any window exists"
    );

    // And it survives the window arriving (STARTING -> RUNNING).
    let window = f.client(client).create_window();
    let surface = window.surface.clone();
    window.set_app_id("a");
    window.commit();
    f.roundtrip(client);
    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(client);
    assert!(dot(&mut f), "and stays lit once it is running");
}

/// Launch feedback: while any app is starting, the pointer shows `wait`
/// compositor-wide, whatever it happens to be over. That is mutter's, not
/// gnome-shell's — `meta_startup_notification_has_pending_sequences`
/// (`startup-notification.c:120-132`) drives `MetaCompositor:global_cursor`
/// (`compositor.c:1103-1117`), applied through the backend's `override-cursor` hook.
#[test]
fn a_starting_app_puts_a_wait_cursor_on_the_whole_compositor() {
    use smithay::input::pointer::CursorIcon;

    use crate::app_system::{
        AppEntry, AppSystem, FakeCatalog, LaunchContext, LaunchMode, RecordingLauncher,
        STARTUP_TIMEOUT,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );

    let cursor = |f: &mut Fixture| f.synoik().cursor_manager.global_override();
    f.synoik().sync_running_apps();
    assert_eq!(cursor(&mut f), None, "nothing starting, no override");

    f.synoik()
        .app_system
        .launch(
            "a.desktop",
            LaunchMode::Activate,
            &LaunchContext::bare(get_monotonic_time()),
        )
        .expect("launch");
    f.synoik().sync_running_apps();
    assert_eq!(
        cursor(&mut f),
        Some(CursorIcon::Wait),
        "a pending startup sequence shows the wait cursor"
    );

    // Its window arriving completes the sequence and takes the cursor away.
    let window = f.client(client).create_window();
    let surface = window.surface.clone();
    window.set_app_id("a");
    window.commit();
    f.roundtrip(client);
    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(client);
    f.synoik().sync_running_apps();
    assert_eq!(
        cursor(&mut f),
        None,
        "the window completing the sequence clears it"
    );

    // And a launch that never maps clears it on the timeout, rather than leaving the
    // pointer stuck as a watch forever.
    let now = get_monotonic_time();
    f.synoik()
        .app_system
        .begin_startup("a.desktop", None, None, now);
    f.synoik().sync_running_apps();
    assert_eq!(cursor(&mut f), Some(CursorIcon::Wait));
    f.synoik()
        .app_system
        .expire_startups(now + STARTUP_TIMEOUT + Duration::from_millis(1));
    f.synoik().sync_running_apps();
    assert_eq!(
        cursor(&mut f),
        None,
        "an expired sequence releases the cursor"
    );
}

/// A launch that never produces a window stops being `STARTING` after mutter's
/// `STARTUP_TIMEOUT_MS` (`startup-notification.c:38,483-512`) — otherwise a failed
/// spawn would leave a permanent running dot.
#[test]
fn a_startup_sequence_that_never_maps_expires() {
    use crate::app_system::{
        AppEntry, AppState, AppSystem, FakeCatalog, RecordingLauncher, STARTUP_TIMEOUT,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")])),
        Box::new(RecordingLauncher::default()),
    );

    let now = get_monotonic_time();
    f.synoik()
        .app_system
        .begin_startup("a.desktop", None, None, now);
    assert_eq!(
        f.synoik().app_system.app_state("a.desktop"),
        AppState::Starting
    );

    assert!(
        !f.synoik()
            .app_system
            .expire_startups(now + STARTUP_TIMEOUT - Duration::from_millis(1)),
        "the sequence outlives everything up to the timeout"
    );
    assert!(f
        .synoik()
        .app_system
        .expire_startups(now + STARTUP_TIMEOUT + Duration::from_millis(1)));
    assert_eq!(
        f.synoik().app_system.app_state("a.desktop"),
        AppState::Stopped,
        "an expired sequence leaves the app stopped, not starting forever"
    );
}

/// **S6 — running apps.** A mapped window makes its app *running*, through the
/// compositor's own bookkeeping: the real xdg-shell `app_id` crosses the
/// `sync_running_apps` seam in `State::refresh`, runs the `StartupWMClass` ladder
/// (`get_app_from_window_wmclass`, `shell-window-tracker.c:146`) and lands in
/// `get_running()` (`shell-app-system.c:508`). Unmapping the window clears it.
///
/// The matching *table* is pinned by unit tests in `app_system.rs`; what this
/// pins is that a real window reaches it at all, and that the running set follows
/// map/unmap without an explicit invalidation hook.
#[test]
fn overview_mapped_window_marks_its_app_running() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // An installed app that claims the window's app_id via StartupWMClass — the
    // rung that must beat a basename lookup (there is no `editor-instance.desktop`).
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "org.example.Editor.desktop",
            "Editor",
            "editor-instance",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    assert!(
        f.synoik().app_system.running().is_empty(),
        "nothing runs before a window maps"
    );

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("editor-instance");
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let running: Vec<String> = f
        .synoik()
        .app_system
        .running()
        .iter()
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(
        running,
        ["org.example.Editor.desktop"],
        "the mapped window's app_id resolved through StartupWMClass"
    );
    assert_eq!(f.synoik().app_system.running()[0].n_windows(), 1);
    assert!(f
        .synoik()
        .app_system
        .is_running("org.example.Editor.desktop"));

    // Unmap: the app stops running.
    let window = f.client(id).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);

    assert!(
        f.synoik().app_system.running().is_empty(),
        "unmapping the last window stops the app"
    );
    assert!(!f
        .synoik()
        .app_system
        .is_running("org.example.Editor.desktop"));
}

/// **S6 — running apps in the dash.** A running non-favorite joins the dash after
/// the favorites, behind a `.dash-separator` (`Dash._redisplay`, `dash.js:677-699`
/// and `806-808`), and clicking it launches like any other tile. Driven end to
/// end: a real window maps, `sync_running_apps` resolves it, and the dash
/// redisplays off that change.
#[test]
fn overview_dash_shows_running_apps_after_a_separator() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::ui::dash::DashHit;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    let recorder = RecordingLauncher::default();
    let apps = vec![
        AppEntry::fake("fav.desktop", "Favorite"),
        AppEntry::fake_with_wm_class("runner.desktop", "Runner", "runner"),
    ];
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.synoik()
        .app_system
        .set_favorites(vec!["fav.desktop".to_owned()]);
    f.synoik().sync_dash_favorites();
    f.synoik_state().do_action(Action::OpenOverview, false);

    let area = overview_controls(&mut f).dash;
    assert!(
        f.synoik().dash.separator_box(area).is_none(),
        "one favorite and nothing running draws no divider"
    );
    assert_eq!(
        f.synoik().dash.item_id(1),
        None,
        "only the favorite is listed"
    );

    // The non-favorite app opens a window.
    let window = f.client(client).create_window();
    let surface = window.surface.clone();
    window.set_app_id("runner");
    window.commit();
    f.roundtrip(client);
    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(client);

    assert_eq!(
        f.synoik().dash.item_id(1),
        Some("runner.desktop"),
        "the running non-favorite joins the dash after the favorites"
    );
    let area = overview_controls(&mut f).dash;
    let sep = f
        .synoik()
        .dash
        .separator_box(area)
        .expect("a favorite plus a running non-favorite draws the divider");

    // The divider sits between the two tiles and is itself inert.
    let fav = f.synoik().dash.tile_center(0, area).unwrap();
    let run = f.synoik().dash.tile_center(1, area).unwrap();
    assert!(sep.loc.x > fav.x && sep.loc.x < run.x);

    // Clicking the running app's tile is a live target — but it *activates* rather than
    // launching, since the app is RUNNING (`shell_app_activate_full`, `shell-app.c:528-530`).
    // This assertion used to expect a launch, which was the bug behind the busy cursor on every
    // dash click of an already-open app.
    pointer_motion_to(&mut f, run.x, run.y);
    assert_eq!(f.synoik().dash.hovered_for_test(), Some(DashHit::App(1)));
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        recorder.calls.borrow().is_empty(),
        "a running app is activated, not relaunched"
    );
    assert!(
        !f.synoik().layout.is_overview_open(),
        "and the click still took, closing the overview"
    );

    // Closing the window drops it back out of the dash, divider and all.
    let window = f.client(client).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(client);

    assert_eq!(f.synoik().dash.item_id(1), None);
    let area = overview_controls(&mut f).dash;
    assert!(f.synoik().dash.separator_box(area).is_none());
}

// Display config: live applies vs the monitors.xml store ---------------------------------------
//
// mutter's model (meta-monitor-manager.c, `meta_monitor_manager_apply_monitors_config` →
// `meta_monitor_config_manager_set_current`): an `ApplyMonitorsConfig` becomes the *current*
// session config immediately; `monitors.xml` is only written for persistence and read back at
// startup/hotplug — never re-read to override a live apply. Getting this backwards made GNOME
// Settings' scale changes land one try late (the reload raced the store write and resurrected
// the previous value).

/// Points the monitors.xml store at a private per-test file (see `monitors_xml::TEST_PATH`;
/// the whole fixture runs on the test's thread). Removes the file and the override on drop.
struct MonitorsXmlGuard {
    path: std::path::PathBuf,
}

impl MonitorsXmlGuard {
    fn install(xml: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "synoik-test-monitors-{}-{:?}.xml",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::write(&path, xml).unwrap();
        crate::monitors_xml::TEST_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));
        Self { path }
    }

    /// Overwrite the store, like the DBus handler's persist step does.
    fn write(&self, xml: &str) {
        std::fs::write(&self.path, xml).unwrap();
    }
}

impl Drop for MonitorsXmlGuard {
    fn drop(&mut self) {
        crate::monitors_xml::TEST_PATH.with(|p| *p.borrow_mut() = None);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn monitors_xml_with_scale(scale: f64) -> String {
    format!(
        r#"<monitors version="2">
  <configuration>
    <logicalmonitor>
      <x>0</x><y>0</y><scale>{scale}</scale>
      <monitor><monitorspec><connector>headless-1</connector></monitorspec></monitor>
    </logicalmonitor>
  </configuration>
</monitors>"#
    )
}

fn output_scale(f: &Fixture) -> f64 {
    f.synoik_output(1).current_scale().fractional_scale()
}

/// The `ApplyMonitorsConfig` config for headless-1 at `scale`, the way the DBus handler builds it.
fn dbus_scale_config(scale: f64) -> HashMap<String, Option<synoik_config::Output>> {
    HashMap::from([(
        "headless-1".to_owned(),
        Some(synoik_config::Output {
            off: false,
            name: "headless-1".to_owned(),
            scale: Some(synoik_config::FloatOrInt(scale)),
            position: Some(synoik_config::Position { x: 0, y: 0 }),
            ..Default::default()
        }),
    )])
}

/// A saved monitors.xml scale is honored from the first frame (store > KDL > DPI guess).
#[test]
fn monitors_xml_scale_applies_at_startup() {
    let _store = MonitorsXmlGuard::install(&monitors_xml_with_scale(2.0));

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert_eq!(
        output_scale(&f),
        2.0,
        "saved store scale wins over the guess"
    );
}

/// The regression: a scale applied via GNOME Settings (`ApplyMonitorsConfig`) takes effect
/// immediately — the reload must not re-read the store (whose persist write races behind, or
/// never happens for a TEMPORARY apply) and resurrect the previous value.
#[test]
fn settings_scale_apply_takes_effect_immediately() {
    let store = MonitorsXmlGuard::install(&monitors_xml_with_scale(1.0));

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert_eq!(output_scale(&f), 1.0);

    // First apply: the store still holds the old value (TEMPORARY applies never write it, and a
    // PERSISTENT write lands on the DBus thread after this runs). The new scale must win anyway.
    f.synoik_state()
        .apply_display_config(dbus_scale_config(2.0));
    assert_eq!(
        output_scale(&f),
        2.0,
        "the applied scale takes effect on the FIRST apply"
    );

    // Any later reload keeps the live-applied value; the store never overrides it.
    f.synoik_state().reload_output_config();
    assert_eq!(
        output_scale(&f),
        2.0,
        "a reload must not resurrect the stored scale"
    );

    // Second apply after the first one persisted: what applies is THIS value, not the file's.
    store.write(&monitors_xml_with_scale(2.0));
    f.synoik_state()
        .apply_display_config(dbus_scale_config(1.5));
    assert_eq!(
        output_scale(&f),
        1.5,
        "the second apply must not land the first apply's value"
    );
}

/// `synoik msg output set-scale` also outranks the store; `set-scale automatic` falls back to it.
#[test]
fn ipc_scale_beats_store_and_automatic_returns_to_it() {
    let _store = MonitorsXmlGuard::install(&monitors_xml_with_scale(2.0));

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert_eq!(output_scale(&f), 2.0);

    f.synoik_state().apply_transient_output_config(
        "headless-1",
        synoik_ipc::OutputAction::Scale {
            scale: synoik_ipc::ScaleToSet::Specific(1.5),
        },
    );
    assert_eq!(
        output_scale(&f),
        1.5,
        "a live IPC apply beats the saved store scale"
    );

    f.synoik_state().apply_transient_output_config(
        "headless-1",
        synoik_ipc::OutputAction::Scale {
            scale: synoik_ipc::ScaleToSet::Automatic,
        },
    );
    assert_eq!(
        output_scale(&f),
        2.0,
        "automatic falls back to the store, not the guess"
    );
}

/// The same two transitions with the active workspace in the *middle* of a longer
/// row — the shape that exposed the second half of this bug.
///
/// With two workspaces the active one is an end of the run, so the fit-all row and
/// the fit-single row want it in nearly the same place and a bad blend barely
/// moves it. Put it in the middle and the two rows disagree about every
/// workspace's position, so any endpoint that is itself a function of the running
/// progress bends the path: the row used to swing right and come back.
#[test]
fn overview_grid_transitions_are_monotonic_with_the_active_workspace_in_the_middle() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Two populated workspaces plus the trailing empty one, focused on the
    // middle: `[win] [win, active] [empty]`.
    let _first = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    f.settle_animations();
    let _second = map_window_sized(&mut f, id, (800, 600), None);
    f.settle_animations();

    let desktop = workspace_geo(&mut f);
    assert!(
        desktop.len() >= 3,
        "need a row long enough to have a middle"
    );

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    f.synoik().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "picker -> app grid (middle active)");

    f.settle_animations();
    let grid = workspace_geo(&mut f);

    // ...and back down the same leg, which is the direction that used to swing
    // left before settling right.
    f.synoik().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "app grid -> picker (middle active)");
    f.settle_animations();
    let picker = workspace_geo(&mut f);
    assert_geo_eq(
        &picker,
        &grid,
        "the picker must not change size across the app-grid leg — it fades, it does \
         not travel (divergence, 2026-08-03)",
    );

    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    f.synoik_state().do_action(Action::CloseOverview, false);
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "app grid -> desktop (middle active)");
    assert_geo_eq(
        samples.last().unwrap(),
        &desktop,
        "the close must land back on the desktop layout it started from",
    );
}

/// Closing the overview **from the app grid** unwinds in gnome-shell's order: the grid goes
/// away first, at a parked zoom, and only then does the overview zoom into the active
/// workspace. gnome-shell gets that for free from one adjustment travelling 2 -> 0 *through*
/// `WINDOW_PICKER` (`overviewControls.js:278-308`); we carry two scalars and reconstruct the
/// axis in `Monitor::overview_state`.
///
/// Gustavo, 2026-08-03: "going out of the app grid back to normal desktop, the animation is a
/// bit jarring". It was reading both blends off the *raw* show-apps scalar, which is frozen
/// across a close — so the grid stayed "fully in" for the whole animation, the window picker
/// behind it stayed at alpha 0, and the entire return to the desktop (the previews
/// un-spreading, the workspace zooming out) happened invisibly behind it before the desktop
/// popped in.
///
/// Pinned on the ordering rather than on the alphas, because the ordering is what was wrong:
/// while the grid is on its way out nothing else may move, and nothing may still be on its way
/// out once the zoom starts.
#[test]
fn overview_close_from_the_app_grid_unwinds_the_grid_before_it_zooms() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    let picker_zoom = {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap().overview_zoom()
    };
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();

    let output = f.synoik_output(1);
    f.synoik_state().do_action(Action::CloseOverview, false);
    let samples = f.sample_animation(Duration::from_millis(600), 48, |f| {
        let mon = f.synoik().layout.monitor_for_output(&output).unwrap();
        (mon.app_grid_leg(), mon.overview_zoom())
    });

    // Premise: the close really does start with the grid still up.
    assert!(
        samples[0].0 > 0.5,
        "the close must start from the app grid, got leg {}",
        samples[0].0
    );

    // The grid only ever goes away, and it is gone before the end.
    for pair in samples.windows(2) {
        assert!(
            pair[1].0 <= pair[0].0 + 1e-9,
            "the app grid must not come back mid-close: {:?} -> {:?}",
            pair[0],
            pair[1]
        );
    }
    let gone = samples
        .iter()
        .position(|(leg, _)| *leg <= 0.)
        .expect("the app grid must finish unwinding before the close does");

    // Nothing zooms while it is on its way out: the workspace sits at the picker's zoom for
    // every frame the grid is still visible. This is the assertion that fails if the two
    // blends are read off the same raw progress.
    for (i, (leg, zoom)) in samples.iter().enumerate().take(gone) {
        assert!(
            (zoom - picker_zoom).abs() <= 1e-6,
            "sample {i}: the zoom must be parked while the grid unwinds \
             (leg {leg}, zoom {zoom}, picker zoom {picker_zoom})"
        );
    }

    // ...and once it is gone, the zoom is what carries the rest of the way to the desktop.
    let end = samples.last().unwrap().1;
    assert!(
        end > picker_zoom,
        "the close must zoom out after the grid is gone, got {end} from {picker_zoom}"
    );
    assert!(
        samples[gone..].iter().any(|(_, zoom)| *zoom > picker_zoom),
        "the zoom must actually run during the second leg, not snap at the end"
    );
}

/// Closing from the app grid unwinds the picker cleanly, with the row never folding in on
/// itself.
///
/// This used to guard an ordering — the zoom parked across the app-grid leg so the row
/// re-fitted *before* it zoomed, because a fit-all row at a near-desktop zoom is degenerate
/// and blending toward it threw the row sideways. Since 2026-08-03 the picker does not
/// change fit mode at all (it fades away instead of travelling into the app-grid row), so
/// that state is unreachable by construction; what is left worth pinning is the invariant
/// it was measured by.
#[test]
fn overview_close_from_the_app_grid_never_folds_the_row() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    let grid = workspace_geo(&mut f);

    f.synoik_state().do_action(Action::CloseOverview, false);
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);

    // The active workspace never overlaps its neighbour: the row's pitch stays at
    // least its width, which is exactly what a degenerate fit-all blend breaks.
    for (i, row) in samples.iter().enumerate() {
        for pair in row.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            assert!(
                right.loc.x + 1. >= left.loc.x + left.size.w,
                "sample {i}: workspaces overlap ({left:?} then {right:?})",
            );
        }
    }

    // And by the time the active workspace is back to full width, the row is the desktop
    // row: one screen per workspace, with nothing left to snap.
    let full = samples
        .iter()
        .position(|row| row[0].size.w >= 1919.)
        .expect("the close must reach full width");
    let pitch = samples[full][1].loc.x - samples[full][0].loc.x;
    assert!(
        pitch >= 1919.,
        "at full width the row must be the desktop row, got pitch {pitch} \
         (it was {} in the grid)",
        grid[1].loc.x - grid[0].loc.x,
    );
}

/// A burst of `installed-changed` pings must not reload the catalog once per ping.
///
/// Installing one package writes many `.desktop` files and glib's monitors fire per
/// directory, so the pings arrive in clumps. Each reload re-enumerates every desktop entry
/// on disk, drops four icon caches, re-syncs three surfaces and forces a redraw — on the
/// compositor thread, and thrown away by the next ping milliseconds later. gnome-shell
/// coalesces them on a restarting 5s timer (`shell_app_cache_queue_update`,
/// `src/shell-app-cache.c:219-230`); this pins that we do too.
///
/// Observable without waiting the timer out: a catalog installed but never synced leaves
/// the app grid empty, and only a reload would fill it.
#[test]
fn an_installed_changed_burst_does_not_reload_the_catalog_per_ping() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("b.desktop", "B"),
        ])),
        Box::new(RecordingLauncher::default()),
    );
    assert!(
        f.synoik().app_grid.entry_id(0).is_none(),
        "the grid must start empty, or a reload proves nothing"
    );

    for _ in 0..8 {
        f.synoik().queue_app_catalog_reload();
    }
    f.dispatch();

    assert!(
        f.synoik().app_grid.entry_id(0).is_none(),
        "a ping reloaded the catalog inline instead of coalescing"
    );
    let first = f
        .synoik()
        .app_catalog_reload_at
        .expect("the burst queued no reload at all — the change would be lost");

    // A later ping pushes the deadline out rather than arming a second timer: the reload
    // lands once the writes stop, not once the first one starts.
    f.synoik().queue_app_catalog_reload();
    let second = f.synoik().app_catalog_reload_at.expect("still pending");
    assert!(
        second >= first,
        "a ping mid-wait must move the deadline forward, not backward"
    );
}

/// The icon prewarm has to warm the size the grid will actually *render*. The grid picks
/// its icon size from the band it is given — the largest of `ICON_SIZES` whose cells fit
/// the chosen mode (`iconGrid.js:395`) — so it is not 96 on every display: a 1280×800
/// screen renders at 48. The decode cache is keyed by logical px, so prewarming a size the
/// grid never draws warms an entry nothing asks for and leaves every icon to decode lazily
/// the first time its page is looked at, which is what a one-time blink on first reaching a
/// page looks like.
#[test]
fn overview_app_icon_prewarm_uses_the_size_the_grid_will_render() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    for (mode, expect_default) in [((1920, 1080), true), ((1280, 800), false)] {
        let mut f = Fixture::new();
        f.add_output(1, mode);
        f.synoik().app_system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "a")])),
            Box::new(RecordingLauncher::default()),
        );
        f.synoik().sync_app_grid();
        f.synoik_state().do_action(Action::OpenOverview, false);
        f.synoik().layout.toggle_app_grid();
        f.synoik_complete_animations();

        let area = overview_controls(&mut f).app_display;
        let rendered = f.synoik().app_grid.metrics_for(area).icon_px;
        let default = crate::ui::widget::TileMetrics::overview().icon_px;
        assert_eq!(
            rendered == default,
            expect_default,
            "{mode:?} should{} render at the default {default}, got {rendered}",
            if expect_default { "" } else { " NOT" }
        );

        let variants = f.synoik().prewarm_variants();
        assert_eq!(variants.len(), 1, "one output, one variant: {variants:?}");
        assert_eq!(
            variants[0].1, rendered,
            "{mode:?}: the prewarm must warm the rendered size, not {default}"
        );
    }
}

/// A touchpad swipe drags the app grid's pages 1:1 and settles on a page when the fingers
/// lift — GNOME gives `AppDisplay` its own `SwipeTracker` over the grid's scroll view
/// (`appDisplay.js:605-614,706-735`), and the value it drags is the very same scroll
/// adjustment `goToPage` eases, which is why the swipe and the slide are one state.
/// One page is `TOUCHPAD_BASE_WIDTH` 400 px of travel and a scroll delta counts ×10
/// (`swipeTracker.js:14,18,183`); a swipe crosses at most one page (`_getBounds`), and
/// the release threshold is 0.6 **pixels** per ms, not pages.
#[test]
fn overview_app_grid_swipes_between_pages() {
    let ids: Vec<String> = (0..30).map(|i| format!("o{i:02}.desktop")).collect();
    let others: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (mut f, _recorder) = app_grid_fixture(&[], &others);
    let area = overview_controls(&mut f).app_display;
    assert_eq!(f.synoik().app_grid.page_count(area), 2);

    // Park the pointer over the grid — the swipe is only live there.
    let center = f
        .synoik()
        .app_grid
        .entry_center(0, area)
        .expect("the first tile");
    pointer_motion_to(&mut f, center.x, center.y);

    // `n` scroll notches of `dx`, `gap` ms apart, then the fingers lift.
    let swipe = |f: &mut Fixture, n: usize, dx: f64, gap: u32| {
        for _ in 0..n {
            f.advance_input_time(gap);
            f.scroll_finger(dx, 0.);
        }
    };
    let lift = |f: &mut Fixture| {
        f.advance_input_time(1);
        f.scroll_finger(0., 0.);
        f.settle_animations();
    };

    // A slow drag: 8 notches of 2 is 160 px of travel (×10), two fifths of a page. The
    // view follows it 1:1 rather than snapping — that is the point of a 1:1 gesture.
    swipe(&mut f, 8, 2., 50);
    let dragged = f.synoik().app_grid.page_pos();
    assert!(
        (dragged - 0.4).abs() < 0.01,
        "the pages follow the finger 1:1 (160 px of 400), got {dragged}"
    );
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "…without committing to a page yet"
    );

    // Released slowly, it falls back to the page it is nearest.
    lift(&mut f);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "two fifths snaps back"
    );
    assert_eq!(f.synoik().app_grid.page_pos(), 0.);

    // Dragged past halfway just as slowly, it falls forward instead.
    swipe(&mut f, 13, 2., 50);
    lift(&mut f);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "past halfway, the nearest page is the next one"
    );

    // A flick: a short drag, but fast enough to clear the velocity threshold, so it
    // carries a whole page even though the drag itself covered a fifth of one.
    swipe(&mut f, 4, -2., 1);
    lift(&mut f);
    assert_eq!(
        f.synoik().app_grid.current_page(),
        0,
        "a flick advances a page the drag never reached"
    );
    assert_eq!(f.synoik().app_grid.page_pos(), 0.);

    // A vertical two-finger scroll is swallowed and moves nothing: GNOME's tracker is
    // horizontal, and letting it through would page the workspaces behind the grid.
    swipe(&mut f, 4, 0., 20);
    for _ in 0..4 {
        f.advance_input_time(20);
        f.scroll_finger(0., 4.);
    }
    assert_eq!(f.synoik().app_grid.page_pos(), 0.);
    assert_eq!(f.synoik().app_grid.current_page(), 0);
}

/// Dragging the app grid's background with the mouse pages it. gnome-shell's swipe
/// tracker attaches a `Clutter.PanGesture` with `min_n_points: 1` and `allowDrag` on by
/// default (`swipeTracker.js:367-404`), so a plain click-drag pans the same adjustment a
/// touchpad swipe does — which on a machine with no touchpad is the *only* way to swipe.
/// The pages follow the pointer, so the travel is the negation of it
/// (`_getGestureDirFactor` is -1 for LTR, `swipeTracker.js:689-695`).
#[test]
fn overview_app_grid_pages_by_dragging_its_background() {
    let ids: Vec<String> = (0..30).map(|i| format!("o{i:02}.desktop")).collect();
    let others: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (mut f, recorder) = app_grid_fixture(&[], &others);
    let area = overview_controls(&mut f).app_display;
    assert_eq!(f.synoik().app_grid.page_count(area), 2);

    // Grab the band well below the tiles — the background, not an icon: a press on an
    // icon belongs to that icon's own drag.
    // Near the right edge, so a long leftward drag does not run the pointer into the
    // side of the screen (which silently shortens the travel).
    let start_x = area.loc.x + area.size.w - 20.;
    let start_y = area.loc.y + area.size.h - 6.;
    assert!(
        f.synoik()
            .app_grid
            .hit_test((start_x, start_y).into(), area)
            .is_none(),
        "the grab point must not be on a tile"
    );

    // Drag left, slowly. A pointer drag is one *page width* per page — `_swipeBegin`
    // confirms the swipe with the grid's own allocation width, and `_updatePanGesture`
    // divides by that (`appDisplay.js:713-716`, `swipeTracker.js:578-585,710-711`) — so
    // the pages travel exactly as far as the pointer does. The touchpad's 400 px is an
    // override for a device whose physical size Clutter cannot know.
    let travel = 1100.;
    pointer_motion_to(&mut f, start_x, start_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    for i in 1..=10 {
        f.advance_input_time(50);
        pointer_motion_to(&mut f, start_x - travel / 10. * f64::from(i), start_y);
    }
    let dragged = f.synoik().app_grid.page_pos();
    let expected = travel / area.size.w;
    assert!(
        (dragged - expected).abs() < 0.01,
        "the pages follow the pointer 1:1 — {travel} px of a {} px page is {expected}, \
         and *towards* the next page when dragged left; got {dragged}",
        area.size.w
    );

    // Released past halfway, it settles on the next page.
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert_eq!(f.synoik().app_grid.current_page(), 1);
    assert_eq!(f.synoik().app_grid.page_pos(), 1.);
    assert!(
        recorder.calls.borrow().is_empty(),
        "a drag on the background launches nothing"
    );
    assert!(
        f.synoik().layout.is_app_grid_open(),
        "…and does not dismiss the grid"
    );

    // A press that never moves is just a click on the background: nothing happens.
    pointer_motion_to(&mut f, start_x, start_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert_eq!(
        f.synoik().app_grid.current_page(),
        1,
        "a click on the background does not page"
    );
    assert!(f.synoik().layout.is_app_grid_open());
}

/// Tab walks the app grid's icons in **child order**, wrapping — a different traversal
/// from the arrows' spatial one. `st_widget_real_navigate_focus` uses
/// `st_widget_get_focus_chain` for `TAB_FORWARD`/`TAB_BACKWARD` (`st-widget.c:2086-2103`)
/// and `st_widget_navigate_focus` retries from the start when it runs off the end
/// (`:2214-2224`; the focus manager sets `wrap_around` for Tab, `st-focus-manager.c:96-106`).
/// The grid is a focus group because `ctrlAltTabManager.addGroup` registers it
/// (`overviewControls.js:392`, `ctrlAltTab.js:43`). With nothing focused, Tab *enters* the
/// grid at the first icon and Shift+Tab at the last (`overviewControls.js:464-470`).
#[test]
fn overview_app_grid_tab_walks_the_icons_in_order() {
    let ids: Vec<String> = (0..30).map(|i| format!("o{i:02}.desktop")).collect();
    let others: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (mut f, _recorder) = app_grid_fixture(&[], &others);
    f.synoik_state().update_keyboard_focus();
    assert!(f.synoik().keyboard_focus.is_overview());
    let area = overview_controls(&mut f).app_display;
    let per_page = f.synoik().app_grid.items_per_page(area);
    assert_eq!(f.synoik().app_grid.page_count(area), 2);

    // Nothing focused: Tab enters at the very first icon.
    assert_eq!(f.synoik().app_grid.focused(), None);
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(0),
        "Tab enters at the start"
    );

    // …then steps one at a time, in catalog order — not spatially.
    tap(&mut f, KEY_TAB);
    assert_eq!(f.synoik().app_grid.focused(), Some(1));

    // Entering is from the *start of the grid*, not of the page you happen to be looking
    // at: `navigate_focus(null, TAB_FORWARD)` walks the focus chain from its beginning,
    // and the page then follows the focus back.
    f.synoik().app_grid.set_focused(None);
    assert!(f.synoik().app_grid.set_page(1, area));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(0),
        "Tab enters at the grid's first icon even from another page"
    );
    assert_eq!(f.synoik().app_grid.current_page(), 0, "…paging back to it");

    // Entering *backwards* takes the other end — `TAB_BACKWARD` reverses the focus chain
    // before taking its first entry (`st-widget.c:2089-2090`).
    f.synoik().app_grid.set_focused(None);
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(29),
        "Shift+Tab enters at the grid's last icon"
    );

    // Back to the front for the wrap checks below.
    f.synoik().app_grid.set_focused(Some(0));
    assert!(f.synoik().app_grid.set_page(0, area));

    // Shift+Tab steps back, and off the front it wraps to the very last icon — which
    // pages the view with it. (Focus is on the first icon after the entry above.)
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(29),
        "Shift+Tab off the front wraps to the last icon"
    );
    assert_eq!(
        f.synoik().app_grid.current_page(),
        29 / per_page,
        "…and the page follows the focus there"
    );

    // Forward off the end wraps back to the start, paging back with it.
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(0),
        "and forward wraps too"
    );
    assert_eq!(f.synoik().app_grid.current_page(), 0);

    // An open folder is its own focus group (`appDisplay.js:2516`), so Tab cycles inside
    // it and the grid behind keeps whatever focus it had.
    f.synoik().app_grid.set_focused(Some(0));
    f.synoik().gnome_settings.app_folders = vec![crate::gnome::AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec![ids[1].clone(), ids[2].clone()],
        ..Default::default()
    }];
    f.synoik().sync_app_grid();
    let folder = f
        .synoik()
        .app_grid
        .index_of("Utilities")
        .expect("the folder tile");
    f.synoik().app_grid.set_focused(Some(folder));
    tap(&mut f, KEY_ENTER);
    f.synoik_complete_animations();
    assert!(f.synoik().folder_dialog.is_open());

    tap(&mut f, KEY_TAB);
    assert_eq!(f.synoik().folder_dialog.focused(), Some(0));
    tap(&mut f, KEY_TAB);
    assert_eq!(f.synoik().folder_dialog.focused(), Some(1));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().folder_dialog.focused(),
        Some(0),
        "Tab wraps inside the folder — it does not escape into the grid"
    );
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(folder),
        "…and the grid behind the modal kept its own focus"
    );
    tap(&mut f, KEY_ESC);
    assert!(!f.synoik().folder_dialog.is_open());

    // Tab is a genuinely *different* traversal from the arrows, and the end of a row is
    // where they part: Tab takes the next icon in order (the row below), where Right
    // leaves for the same row of the next page.
    let row0_y = f.synoik().app_grid.entry_center(0, area).unwrap().y;
    let cols = (1..per_page)
        .find(|&i| f.synoik().app_grid.entry_center(i, area).unwrap().y != row0_y)
        .expect("the page has more than one row");
    let row_end = cols - 1;

    f.synoik().app_grid.set_focused(Some(row_end));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(row_end + 1),
        "Tab wraps onto the next row"
    );

    f.synoik().app_grid.set_focused(Some(row_end));
    f.synoik().app_grid.set_page(0, area);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(
        f.synoik().app_grid.focused(),
        Some(per_page),
        "…where Right from the same icon crosses to the next page instead"
    );
}

/// A mode change re-derives the scale.
///
/// Moving this VM between the laptop panel and the external monitor swaps the virtual
/// connector's mode in place. Both rungs of the scale chain below the live-applied override
/// are keyed on the mode — `monitors.xml` stores a setting per mode, and the DPI guess reads
/// the resolution — so the derivation has to be re-run, which is what the mode-change branch
/// in `Tty::on_output_config_changed` now does.
#[test]
fn output_scale_is_derived_from_the_current_mode() {
    use smithay::output::{Output, PhysicalProperties, Subpixel};
    use synoik_config::OutputName;

    use crate::synoik::AppliedDisplayConfig;

    let mut f = Fixture::new();

    // A 16" panel, so the mobile DPI target applies (utils/scale.rs, mutter's meta-monitor.c).
    let output = Output::new(
        "Virtual-1".to_owned(),
        PhysicalProperties {
            size: (344, 215).into(),
            subpixel: Subpixel::Unknown,
            make: "synoik".to_owned(),
            model: "test".to_owned(),
            serial_number: "1".to_owned(),
        },
    );
    output.user_data().insert_if_missing(|| OutputName {
        connector: "Virtual-1".to_owned(),
        make: None,
        model: None,
        serial: None,
    });

    let set_mode = |size: (i32, i32)| {
        output.change_current_state(
            Some(smithay::output::Mode {
                size: size.into(),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
    };

    set_mode((2048, 1330));
    let (hidpi, _) = f.synoik().derive_output_scale_transform(&output, None);

    set_mode((3840, 2160));
    let (uhd, _) = f.synoik().derive_output_scale_transform(&output, None);

    assert!(
        uhd > hidpi,
        "the same panel at a denser mode wants a bigger scale ({hidpi} -> {uhd})"
    );

    // The live-applied config (GNOME Settings' ApplyMonitorsConfig) still outranks the guess —
    // it is dropped on a *hardware* mode change, not consulted-and-ignored.
    f.synoik().applied_display_config.insert(
        "Virtual-1".to_owned(),
        AppliedDisplayConfig {
            scale: Some(1.),
            transform: None,
        },
    );
    let (applied, _) = f.synoik().derive_output_scale_transform(&output, None);
    assert_eq!(applied, 1.);

    f.synoik().applied_display_config.remove("Virtual-1");
    set_mode((2048, 1330));
    let (back, _) = f.synoik().derive_output_scale_transform(&output, None);
    assert_eq!(back, hidpi, "dropping the override re-derives for the mode");
}

/// With the app grid up, the workspace previews behind it are scenery, not a picker.
///
/// gnome-shell's workspace mode is 0 in the `APP_GRID` state (`workspacesView.js:236`), and a
/// window preview's overlay — the hover growth, the close button, the title — is enabled only at
/// mode 1 (`workspace.js:775-777` `_syncOverlay`); the keyboard focus chain is empty there too
/// (`workspace.js:889-891`). Gustavo, 2026-07-28: the small workspaces still raised windows on
/// hover.
#[test]
fn app_grid_makes_the_shrunken_workspaces_inert() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _win = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.synoik().layout.focus().unwrap().window.clone();

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();

    // In the picker, hovering a preview is live: it hovers, and a click would activate it.
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let center = rect.loc + rect.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    assert!(
        f.synoik().window_under_cursor().is_some(),
        "the picker must be live before the app grid opens"
    );
    // The overlay fades in, so it is only on screen once its animation has run — and settling has
    // to come after the last input roundtrip (the headless animation-clock trap).
    f.settle_animations();
    let hovered = |f: &mut Fixture| {
        let out = f.synoik_output(1);
        f.synoik()
            .layout
            .monitor_for_output(&out)
            .unwrap()
            .preview_overlays()
            .into_iter()
            .filter(|(_, _, hover)| *hover > 0.)
            .count()
    };
    assert_eq!(hovered(&mut f), 1, "the hovered preview shows its overlay");

    f.synoik().layout.toggle_app_grid();
    f.settle_animations();

    // Sample where the preview *now* is. The picker no longer travels into the app-grid
    // row — it fades away in place (divergence, 2026-08-03) — so this is very nearly where
    // it was, and the point is that it is inert rather than gone. The hover and the click
    // both resolve through `Layout::window_under` — not through `Synoik::window_under`, which
    // would answer None here anyway because the app grid covers the layout. This is the
    // path that was still handing a window over.
    let small = f.synoik().layout.expose_drawn_rect(&win).unwrap();
    let small_center = small.loc + small.size.downscale(2.).to_point();
    let out = f.synoik_output(1);
    assert!(
        f.synoik().layout.window_under(&out, small_center).is_none(),
        "a faded-out workspace must not hand a window to the pointer"
    );
    assert_eq!(
        hovered(&mut f),
        0,
        "the overlay must go with the state, even though the pointer never moved"
    );

    // …and it comes back when the app grid closes.
    f.synoik().layout.toggle_app_grid();
    f.settle_animations();
    pointer_motion_to(&mut f, center.x, center.y);
    let out = f.synoik_output(1);
    assert!(
        f.synoik().layout.window_under(&out, center).is_some(),
        "closing the app grid makes the picker live again"
    );
}

/// Clicking the accessibility indicator opens its menu, and clicking a switch row flips
/// the backing state **and closes the menu** — `PopupSwitchMenuItem.activate` toggles
/// and then falls through to `super.activate` for a pointer event
/// (`js/ui/popupMenu.js:539-550`).
#[test]
fn a11y_menu_row_toggles_the_setting_and_closes() {
    use crate::gnome::A11yToggle;
    use crate::ui::panel::ROLE_A11Y;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let ow = 1920.0_f64;

    // Pin the indicator on so it's clickable with nothing enabled.
    let mut a11y = f.synoik().gnome_settings.a11y;
    a11y.always_show = true;
    f.synoik().gnome_settings.a11y = a11y;
    f.synoik().panel.set_a11y(a11y);

    let anchor = f.synoik().panel.a11y_rect(ow).expect("indicator present");
    let click = |f: &mut Fixture, x: f64, y: f64| {
        pointer_motion_to(f, x, y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    click(
        &mut f,
        anchor.loc.x + anchor.size.w / 2.,
        anchor.loc.y + anchor.size.h / 2.,
    );
    assert!(
        f.synoik().panel_popover.is_open(),
        "clicking the a11y indicator opens its menu"
    );
    assert_eq!(f.synoik().panel_popover.open_role(), Some(ROLE_A11Y));

    // The first row is High Contrast (`accessibility.js:45-46`). Its center comes from the
    // menu itself rather than a copy of its metrics, so changing the padding can't
    // silently retarget this click at a different row.
    let out = f.synoik().global_space.outputs().next().unwrap().clone();
    let origin = f.synoik().panel_popover.content_location(&out);
    let row0 = f.synoik().panel_popover.a11y_row_center(0).unwrap();
    click(&mut f, origin.x + row0.x, origin.y + row0.y);

    assert!(
        f.synoik().gnome_settings.a11y.get(A11yToggle::HighContrast),
        "the row must flip the backing a11y state"
    );
    // Before the fade finishes, the clicked switch must already show its NEW state:
    // GNOME's rows are `settings.bind`-ed, so the switch travels as the menu closes
    // rather than fading out still showing the old position. The gsettings echo cannot
    // do this — it arrives after the close has begun.
    assert_eq!(
        f.synoik().panel_popover.a11y_row_state(0),
        Some(true),
        "the clicked switch must flip before the menu finishes closing"
    );

    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "a switch row closes the menu (popupMenu.js:539-550)"
    );

    // And the indicator's own predicate now holds without the pin.
    let mut a11y = f.synoik().gnome_settings.a11y;
    a11y.always_show = false;
    f.synoik().gnome_settings.a11y = a11y;
    f.synoik().panel.set_a11y(a11y);
    assert!(
        f.synoik().panel.a11y_rect(ow).is_some(),
        "High Contrast alone keeps the indicator up"
    );
}

// ---------------------------------------------------------------------------
// OSD (`js/ui/osdWindow.js`)
// ---------------------------------------------------------------------------

const VOL_ICON: &[&str] = &["audio-volume-high-symbolic"];

/// `show()` refuses without an icon (`js/ui/osdWindow.js:90-92`), and the 1500 ms
/// `HIDE_TIMEOUT` (`:10,104-107`) takes it away again — armed as a real wake-up,
/// since an OSD over a damage-free desktop generates no frames to expire on.
#[test]
fn osd_shows_and_expires() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    // No icon -> nothing at all.
    f.synoik()
        .osd
        .show_one(&out, &[], Some("Volume"), OsdLevel::new(0.5, 1.));
    assert!(f.synoik().osd.content(&out).is_none());

    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    assert!(f.synoik().osd.content(&out).is_some());
    // The 100 ms fade is running, so it is not yet opaque.
    assert!(f.synoik().osd.alpha(&out) < 1.);
    assert!(f.synoik().osd.are_animations_ongoing());

    tick(&mut f, 120);
    assert_eq!(f.synoik().osd.alpha(&out), 1.);
    // The deadline is armed at the Showing->Shown transition inside
    // advance_animations; the wake-up must be armed from the same place.
    assert!(f.synoik().osd_timer.is_some());

    // Still up just before the timeout — which started at `show()`, concurrently
    // with the fade (`osdWindow.js:107-110`) — and gone after it plus the fade out.
    tick(&mut f, 1300);
    assert!(f.synoik().osd.content(&out).is_some());
    tick(&mut f, 200);
    tick(&mut f, 200);
    assert!(f.synoik().osd.content(&out).is_none());
}

/// A second OSD while one is up replaces its content in place and re-arms the
/// timeout, with **no re-fade** — the fade only runs on the hidden->visible edge
/// (`js/ui/osdWindow.js:94-111`).
#[test]
fn osd_replace_in_place_never_refades() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.3, 1.));
    tick(&mut f, 120);
    assert_eq!(f.synoik().osd.alpha(&out), 1.);

    // Just short of expiry, a new level arrives.
    tick(&mut f, 1300);
    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.6, 1.));
    assert_eq!(
        f.synoik().osd.alpha(&out),
        1.,
        "replacing content must not restart the fade"
    );

    // The re-arm happens inside `show()`, between frames. Nothing else can notice
    // it, so the wake-up has to be re-armed against what the timer is actually set
    // to — otherwise the old timer fires early, drops itself, and the OSD hangs on a
    // damage-free desktop until unrelated damage happens by.
    let armed = f.synoik().osd_timer_at;
    tick(&mut f, 0);
    let (now_armed, deadline) = {
        let synoik = f.synoik();
        (synoik.osd_timer_at, synoik.osd.next_wakeup())
    };
    assert_eq!(
        now_armed, deadline,
        "the wake-up must follow a deadline re-armed by show()"
    );
    assert_ne!(armed, now_armed, "and it moved");

    // The re-arm bought another full 1500 ms from *now*.
    tick(&mut f, 1300);
    assert!(f.synoik().osd.content(&out).is_some());
    tick(&mut f, 200);
    tick(&mut f, 200);
    assert!(f.synoik().osd.content(&out).is_none());
}

/// The level *eases* when the OSD is already visible and *snaps* when it is not
/// (`js/ui/osdWindow.js:71-84`) — which is what makes a held volume key look like a
/// bar sliding rather than teleporting.
#[test]
fn osd_level_eases_only_when_already_visible() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    // First show: no ease, the bar is already at its value.
    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.2, 1.));
    assert_eq!(f.synoik().osd.displayed_level(&out), Some(0.2));
    tick(&mut f, 120);

    // A step up while visible eases across 100 ms. Sampled *mid-flight*: read at zero
    // elapsed time it would still be exactly 0.2, which an implementation that just
    // applies the new value one frame later would also pass.
    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.8, 1.));
    tick(&mut f, 50);
    let mid = f.synoik().osd.displayed_level(&out).unwrap();
    assert!(
        mid > 0.2 && mid < 0.8,
        "the bar should be strictly in flight at 50 ms, was at {mid}"
    );
    tick(&mut f, 120);
    assert_eq!(f.synoik().osd.displayed_level(&out), Some(0.8));
    assert!(!f.synoik().osd.are_animations_ongoing());

    // Up then straight back down inside one frame: the new target equals the value
    // the (stale) ease started from, so nothing new is armed — and if the old ease is
    // left running the bar climbs to 0.8 and then snaps back to 0.2.
    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.3, 1.));
    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.8, 1.));
    tick(&mut f, 50);
    assert_eq!(
        f.synoik().osd.displayed_level(&out),
        Some(0.8),
        "a superseded level ease must not keep running"
    );
}

/// The fade runs ONLY on the hidden->visible edge (`js/ui/osdWindow.js:94-105` guards
/// it with `if (!this.visible)`): a show landing mid-fade-in lets that fade finish
/// instead of snapping to opaque, and the 1500 ms timeout starts at `show()` time,
/// concurrently with the fade (`:107-110`) — so a still-fading OSD can expire.
#[test]
fn osd_show_during_the_fade_neither_snaps_nor_refades() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.4, 1.));
    tick(&mut f, 40);
    let mid = f.synoik().osd.alpha(&out);
    assert!(mid > 0. && mid < 1., "still fading in, alpha was {mid}");

    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    let after = f.synoik().osd.alpha(&out);
    assert_eq!(after, mid, "a show mid-fade must not snap the pill opaque");
    // ...and the fade still lands.
    tick(&mut f, 80);
    assert_eq!(f.synoik().osd.alpha(&out), 1.);
}

/// The alt-tab switcher becoming visible hides every OSD
/// (`js/ui/switcherPopup.js:170-178`) — driven through the real keybind, not by
/// calling `hide_all` by hand.
#[test]
fn osd_hidden_by_the_window_switcher() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);
    let id = f.add_client();
    let _first = map_focused_window(&mut f, id);
    let _second = map_focused_window(&mut f, id);

    f.synoik()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.synoik().osd.is_visible());

    // The real keybind, not a hand-call to `hide_all` — the wiring is the thing
    // under test.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.synoik().switcher.is_open(), "Alt+Tab opened it");

    // The OSD goes when the popup *appears*, not when the key is pressed: `_showImmediately`
    // calls `osdWindowManager.hideAll()` (`switcherPopup.js:178`), and that is 150 ms after the
    // press. So the first tick reveals the popup and starts the OSD's fade, and the second lets
    // the fade finish. (niri's MRU hid it at keypress, which is why this used to need one tick.)
    tick(&mut f, 200);
    assert!(
        f.synoik().switcher.is_visible(),
        "the popup is past its delay"
    );
    tick(&mut f, 200);
    assert!(
        !f.synoik().osd.is_visible(),
        "the switcher becoming visible hides the OSD"
    );
    f.key_release(KEY_LEFTALT);
}

/// `show(icon, label, levels)` cancels every output **absent** from the level map
/// (`js/ui/osdWindow.js:172-182`) — the behavior the brightness manager relies on to
/// flash only the monitors that actually changed.
#[test]
fn osd_show_cancels_outputs_absent_from_the_level_map() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let a = f.synoik_output(1);
    let b = f.synoik_output(2);

    f.synoik()
        .osd
        .show_all(VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.synoik().osd.content(&a).is_some());
    assert!(f.synoik().osd.content(&b).is_some());

    // Now show on `a` alone: `b` is cancelled, not left behind.
    f.synoik()
        .osd
        .show_one(&a, VOL_ICON, None, OsdLevel::new(0.9, 1.));
    tick(&mut f, 200);
    assert!(f.synoik().osd.content(&a).is_some());
    assert!(
        f.synoik().osd.content(&b).is_none(),
        "an output missing from the level map must be cancelled"
    );

    // hideAll takes the rest (`switcherPopup.js:178`).
    f.synoik().osd.hide_all();
    tick(&mut f, 200);
    assert!(!f.synoik().osd.is_visible());
}

/// An output that goes away takes its OSD with it (`js/ui/osdWindow.js:157-160`);
/// one that appears gets its own. Nothing migrates.
#[test]
fn osd_follows_outputs() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let a = f.synoik_output(1);
    f.synoik()
        .osd
        .show_all(VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.synoik().osd.is_visible());

    f.add_output(2, (1280, 720));
    let b = f.synoik_output(2);
    assert!(
        f.synoik().osd.content(&b).is_none(),
        "a new output starts with a hidden OSD, it does not inherit one"
    );

    f.remove_output(1);
    assert!(f.synoik().osd.content(&a).is_none());
    assert!(!f.synoik().osd.is_visible());
}

/// A serialized `GIcon` is not a bare icon name (`js/ui/shellDBus.js:140-142` runs
/// it through `Gio.Icon.new_for_string`): the multi-name themed form is what
/// `g_themed_icon_new_with_default_fallbacks` produces, and it maps straight onto
/// our first-that-resolves candidate list.
#[test]
fn osd_icon_candidates_parse_serialized_gicons() {
    use crate::ui::osd::icon_candidates;

    assert_eq!(
        icon_candidates("audio-volume-high-symbolic"),
        vec!["audio-volume-high-symbolic"]
    );
    assert_eq!(
        icon_candidates(". GThemedIcon audio-volume-high-symbolic audio-volume-high"),
        vec!["audio-volume-high-symbolic", "audio-volume-high"]
    );
    // GFileIcon / GBytesIcon / empty: no theme name to resolve, so no OSD.
    assert!(icon_candidates("/usr/share/pixmaps/x.png").is_empty());
    assert!(icon_candidates("file:///tmp/x.png").is_empty());
    assert!(icon_candidates(". GBytesIcon AAAA").is_empty());
    assert!(icon_candidates("").is_empty());
}

/// `ShowOSD` end to end from the D-Bus message: `connector` routes to one output
/// (and cancels the rest), an absent one goes to all, an absent `level` means no
/// bar rather than a bar at zero, and an absent `max_level` is 1
/// (`js/ui/shellDBus.js:143-152`, `js/ui/osdWindow.js:71-72,86-88`).
#[test]
fn osd_show_osd_routes_by_connector() {
    use crate::dbus::gnome_shell::GnomeShellToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let a = f.synoik_output(1);
    let b = f.synoik_output(2);
    let a_name = a.name();

    let show = |f: &mut Fixture, connector: Option<&str>, level: Option<f64>, max: Option<f64>| {
        f.synoik_state()
            .on_gnome_shell_msg(GnomeShellToSynoik::ShowOsd {
                connector: connector.map(str::to_owned),
                label: None,
                level,
                max_level: max,
                icon: Some(". GThemedIcon audio-volume-high-symbolic audio-volume-high".to_owned()),
            });
        tick(f, 120);
    };

    // No connector -> every monitor.
    show(&mut f, None, Some(0.5), None);
    assert!(f.synoik().osd.content(&a).is_some());
    assert!(f.synoik().osd.content(&b).is_some());
    let content = f.synoik().osd.content(&a).unwrap();
    assert_eq!(
        content.icon,
        vec!["audio-volume-high-symbolic", "audio-volume-high"],
        "the serialized GIcon becomes the candidate list"
    );
    assert_eq!(content.max_level, 1., "an absent max_level is 1");

    // A connector routes to that output alone, and cancels the other.
    show(&mut f, Some(&a_name), Some(0.5), None);
    tick(&mut f, 200);
    assert!(f.synoik().osd.content(&a).is_some());
    assert!(
        f.synoik().osd.content(&b).is_none(),
        "showOne cancels the monitors it did not name"
    );

    // An unknown connector is skipped, not applied to everything.
    f.synoik().osd.hide_all();
    tick(&mut f, 200);
    show(&mut f, Some("does-not-exist"), Some(0.5), None);
    assert!(!f.synoik().osd.is_visible());

    // Amplified volume: max_level > 1 is carried through.
    show(&mut f, None, Some(1.4), Some(1.5));
    assert_eq!(f.synoik().osd.content(&a).unwrap().max_level, 1.5);

    // No level at all -> the OSD shows, but with no bar.
    f.synoik().osd.hide_all();
    tick(&mut f, 200);
    show(&mut f, None, None, None);
    let content = f.synoik().osd.content(&a).unwrap();
    assert!(content.level.is_none(), "an absent level means no bar");
    assert!(f.synoik().osd.level_rect(&a).is_none());
}

/// An icon that is not a theme name leaves no candidates, and `show()` refuses
/// without an icon (`js/ui/osdWindow.js:90-92`) — so a ShowOSD carrying only a
/// file icon draws nothing rather than an empty pill.
#[test]
fn osd_show_osd_without_a_resolvable_icon_draws_nothing() {
    use crate::dbus::gnome_shell::GnomeShellToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.synoik_output(1);

    f.synoik_state()
        .on_gnome_shell_msg(GnomeShellToSynoik::ShowOsd {
            connector: None,
            label: Some("Volume".to_owned()),
            level: Some(0.5),
            max_level: None,
            icon: Some("/usr/share/pixmaps/whatever.png".to_owned()),
        });
    tick(&mut f, 120);
    assert!(f.synoik().osd.content(&out).is_none());

    // ...and with no icon key at all.
    f.synoik_state()
        .on_gnome_shell_msg(GnomeShellToSynoik::ShowOsd {
            connector: None,
            label: Some("Volume".to_owned()),
            level: Some(0.5),
            max_level: None,
            icon: None,
        });
    tick(&mut f, 120);
    assert!(f.synoik().osd.content(&out).is_none());
}

// ---------------------------------------------------------------------------
// MPRIS (`js/ui/mpris.js`)
// ---------------------------------------------------------------------------

/// A player state as the watcher would hand it over.
fn mpris_state(identity: &str, desktop_entry: Option<&str>) -> crate::mpris::PlayerState {
    crate::mpris::PlayerState {
        identity: identity.to_owned(),
        desktop_entry: desktop_entry.map(str::to_owned),
        can_play: true,
        title: "So What".into(),
        artists: vec!["Miles Davis".into()],
        ..crate::mpris::PlayerState::default()
    }
}

fn mpris_update(bus_name: &str, state: crate::mpris::PlayerState) -> crate::mpris::MprisToSynoik {
    crate::mpris::MprisToSynoik::PlayerUpdated {
        bus_name: bus_name.to_owned(),
        state: Box::new(state),
    }
}

/// Cover art is resolved when the **player** appears, not when the message list is opened.
/// gnome-shell constructs the `MediaMessage` — and with it the `Gio.FileIcon` its `Message.icon`
/// resolves — as the player is added to the view (`js/ui/messageList.js:1780-1784`), so the art is
/// already there the first time the popover is drawn. Loading lazily at render would instead show
/// the themed fallback for as long as the load takes, which on a remote cover is a network round
/// trip.
///
/// The popover is never opened in this test: that is the point.
#[test]
fn album_art_is_loaded_when_the_player_appears() {
    use crate::image_source::ImageSource;
    use crate::ui::notification_card::BODY_ICON;

    let dir = std::env::temp_dir().join(format!("gsrs-art-warm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = dir.join("first.png");
    let second = dir.join("second.png");
    for path in [&first, &second] {
        image::RgbaImage::from_pixel(64, 64, image::Rgba([200, 40, 40, 255]))
            .save(path)
            .unwrap();
    }
    let first_src = ImageSource::File(first.clone());
    let second_src = ImageSource::File(second.clone());

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut state = mpris_state("Rhythmbox", None);
    state.art = Some(first_src.clone());
    f.synoik_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", state));

    assert!(
        !f.synoik().panel_popover.is_open(),
        "the point of this test is that nothing has been opened"
    );
    assert!(
        f.synoik().image_cache.is_loaded(
            &first_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the cover must load as the player appears, not when the card is first drawn"
    );

    // The track changes: the new cover loads, and the old one stops being paid for. That eviction
    // is the cache's only bound — its key space is one entry per cover *played*.
    let mut next = mpris_state("Rhythmbox", None);
    next.title = "Blue in Green".into();
    next.art = Some(second_src.clone());
    f.synoik_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", next));

    assert!(
        f.synoik().image_cache.is_loaded(
            &second_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the new cover must load"
    );
    assert!(
        !f.synoik().image_cache.is_loaded(
            &first_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the previous cover must be evicted once no player claims it"
    );

    // And a player going away takes its cover with it.
    f.synoik_state()
        .on_mpris_msg(crate::mpris::MprisToSynoik::PlayerRemoved {
            bus_name: "org.mpris.MediaPlayer2.rb".to_owned(),
        });
    assert!(
        !f.synoik().image_cache.is_loaded(
            &second_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "a departed player's cover must be evicted"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The account picture is decoded when AccountsService answers, and a media change cannot evict it.
///
/// Two failures that both look like "the lock screen shows the default avatar", and neither leaves
/// a trace:
///
/// - a **lazy** decode. A cold key returns `None` and the prompt emits no picture at all, so the
///   first lock after login draws the themed glyph and only swaps to the photograph once some later
///   frame happens to be drawn ([[cold-cost-class]]).
/// - a **shared eviction**. The avatar lives in the same `ImageCache` as album art, whose `retain`
///   is the cache's only bound. Built from the live players alone, it drops the avatar on every
///   MPRIS change — which on a machine playing music is continuously.
#[test]
fn the_account_picture_is_decoded_up_front_and_outlives_a_track_change() {
    use crate::dbus::accounts_service::{AccountIcon, AccountsToSynoik, UserAccount};
    use crate::image_source::ImageSource;
    use crate::render_helpers::icon::ImageFit;
    use crate::ui::lock_screen::AVATAR_PX;
    use crate::ui::notification_card::BODY_ICON;

    let dir = std::env::temp_dir().join(format!("gsrs-avatar-warm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let face = dir.join("face.png");
    let cover = dir.join("cover.png");
    for path in [&face, &cover] {
        image::RgbaImage::from_pixel(64, 64, image::Rgba([200, 40, 40, 255]))
            .save(path)
            .unwrap();
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::UserChanged(UserAccount {
            real_name: "Test User".to_owned(),
            icon_file: AccountIcon::read(face.clone()),
            ..Default::default()
        }));

    let face_src = ImageSource::File(face.clone());
    assert!(
        f.synoik()
            .image_cache
            .is_loaded(&face_src, ImageFit::Cover, AVATAR_PX, 1.0),
        "the picture must decode as AccountsService answers, not on the frame that draws it"
    );

    // A player appears and then changes track: two `retain` passes over the shared cache.
    let mut state = mpris_state("Rhythmbox", None);
    state.art = Some(ImageSource::File(cover.clone()));
    f.synoik_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", state));
    let mut next = mpris_state("Rhythmbox", None);
    next.title = "Blue in Green".into();
    next.art = None;
    f.synoik_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", next));

    assert!(
        !f.synoik().image_cache.is_loaded(
            &ImageSource::File(cover.clone()),
            ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the cover no player claims must still be evicted — the bound has to keep working"
    );
    assert!(
        f.synoik()
            .image_cache
            .is_loaded(&face_src, ImageFit::Cover, AVATAR_PX, 1.0),
        "the account picture was evicted by a track change"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Changing your picture replaces it, even though the file keeps its name.
///
/// AccountsService writes every user's picture to one path
/// (`/var/lib/AccountsService/icons/<user>`) and emits an argument-less `Changed`, so the account
/// we read back is byte-identical to the one we hold and every cache downstream is keyed on that
/// same path. Two independent things therefore have to notice the swap, and if either does not the
/// old picture survives until the session restarts — with nothing in the logs, because nothing
/// failed.
#[test]
fn changing_the_account_picture_in_place_replaces_it() {
    use crate::dbus::accounts_service::{AccountIcon, AccountsToSynoik, UserAccount};
    use crate::image_source::ImageSource;
    use crate::render_helpers::icon::ImageFit;
    use crate::ui::lock_screen::AVATAR_PX;

    let dir = std::env::temp_dir().join(format!("gsrs-avatar-swap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The one path AccountsService will keep reporting.
    let face = dir.join("face.png");

    let write = |rgb: [u8; 3]| {
        image::RgbaImage::from_pixel(64, 64, image::Rgba([rgb[0], rgb[1], rgb[2], 255]))
            .save(&face)
            .unwrap();
    };
    let announce = |f: &mut Fixture| {
        f.synoik_state()
            .on_accounts_msg(AccountsToSynoik::UserChanged(UserAccount {
                real_name: "Test User".to_owned(),
                icon_file: AccountIcon::read(face.clone()),
                ..Default::default()
            }));
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    write([200, 40, 40]);
    announce(&mut f);

    let source = ImageSource::File(face.clone());
    let red = f
        .synoik()
        .image_cache
        .buffer(&source, ImageFit::Cover, AVATAR_PX, 1.0)
        .expect("the first picture decodes");
    assert!(
        red.data().chunks_exact(4).all(|p| p[0] > 150 && p[1] < 90),
        "the fixture decoded a red picture"
    );

    // The user picks a new picture. Same path, different bytes — and `modified()` has a coarse
    // resolution on some filesystems, so the length moves too (a solid green PNG compresses
    // differently) and the test does not depend on the clock.
    write([40, 200, 40]);
    announce(&mut f);

    let now = f
        .synoik()
        .image_cache
        .buffer(&source, ImageFit::Cover, AVATAR_PX, 1.0)
        .expect("the new picture decodes");
    assert!(
        now.data().chunks_exact(4).all(|p| p[1] > 150 && p[0] < 90),
        "the lock screen is still holding the previous picture"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every one of the four conditions can hide the "Log in as another user" button, on its own.
///
/// `_updateUserSwitchVisibility` (`unlockDialog.js:921-926`) ANDs four independent sources — a seat
/// that can host another session, another account to switch to, the user's preference, and the
/// administrator's lockdown. Missing one is not a visual nit: a button that appears when switching
/// is impossible does nothing when pressed, and one that appears under `disable-user-switching`
/// hands out a route past a policy somebody set deliberately. Each is asserted from a state where
/// everything *else* is satisfied, so a gate that is wired to the wrong field cannot hide behind
/// another gate that happens to be false too.
#[test]
fn each_condition_alone_hides_the_switch_user_button() {
    use crate::dbus::accounts_service::AccountsToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Nothing has answered yet: the fail-closed direction is hidden, since a button offering a
    // switch we have not established is possible is the one that does nothing.
    assert!(
        !f.synoik().switch_user_visible(),
        "the button must not appear before anything has said it can work"
    );

    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(true));
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(true));
    assert!(
        f.synoik().switch_user_visible(),
        "with a seat, another user, and default settings, the button shows"
    );

    // ...and each condition, alone, takes it away again.
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(false));
    assert!(
        !f.synoik().switch_user_visible(),
        "a seat that cannot host another session"
    );
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(true));

    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(false));
    assert!(
        !f.synoik().switch_user_visible(),
        "nobody else to log in as"
    );
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(true));

    let base = f.synoik().screen_shield.settings();
    let mut settings = base;
    settings.user_switch_enabled = false;
    f.synoik().screen_shield.set_settings(settings);
    assert!(
        !f.synoik().switch_user_visible(),
        "org.gnome.desktop.screensaver user-switch-enabled = false"
    );

    let mut settings = base;
    settings.disable_user_switching = true;
    f.synoik().screen_shield.set_settings(settings);
    assert!(
        !f.synoik().switch_user_visible(),
        "org.gnome.desktop.lockdown disable-user-switching = true"
    );

    f.synoik().screen_shield.set_settings(base);
    assert!(f.synoik().switch_user_visible(), "and back");
}

/// The switch-user button is clickable exactly while it is on screen — both edges of that.
///
/// GNOME gates `reactive` on the very number that drives `opacity` (`unlockDialog.js:811-821`), so
/// the two cannot disagree. Gating on the model's *page* instead splits them in both directions,
/// and each half is invisible on its own: the frame after the prompt is raised the page already
/// reads `Prompt` while the button is still at alpha 0, so a click lands on nothing anyone can see;
/// and for the whole 300 ms fade-out the page reads `Clock` while the button is still drawn, so a
/// click on a button plainly on screen re-raises the prompt instead.
///
/// The times here are picked either side of `CROSSFADE_TIME`, which is what makes this a test of
/// the *transition* rather than of the two resting states.
#[test]
fn the_switch_user_button_is_clickable_exactly_while_it_is_drawn() {
    use std::time::Duration;

    use crate::dbus::accounts_service::AccountsToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(true));
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(true));

    let t0 = Duration::from_secs(1_000);
    let mid = t0 + Duration::from_millis(150);
    let after = t0 + Duration::from_millis(400);

    // Resting on the clock: nothing drawn, nothing to click.
    f.synoik().lock_screen.set_page(false, t0);
    f.synoik().lock_screen.settle();
    assert!(
        !f.synoik().switch_user_reactive(t0),
        "the button must not be reactive with the clock up"
    );

    // The instant the prompt starts coming up, the button is still at alpha 0.
    f.synoik().lock_screen.set_page(true, t0);
    assert!(
        !f.synoik().switch_user_reactive(t0),
        "a click landed on the button on the frame it was still invisible"
    );
    assert!(
        f.synoik().switch_user_reactive(mid),
        "mid-crossfade the button is on screen and must take a click"
    );
    assert!(
        f.synoik().switch_user_reactive(after),
        "and once it settles"
    );

    // Going back: the page is already the clock, but the button is still fading out.
    f.synoik().lock_screen.set_page(false, after);
    assert!(
        f.synoik()
            .switch_user_reactive(after + Duration::from_millis(150)),
        "the button was still drawn but had stopped taking clicks"
    );
    assert!(
        !f.synoik()
            .switch_user_reactive(after + Duration::from_millis(400)),
        "and once it is gone it must stop"
    );
}

/// Clicking the button cancels the authentication in flight; clicking beside it does not.
///
/// `_otherUserClicked` (`unlockDialog.js:901-905`) cancels the prompt as well as leaving, and that
/// half is the one worth pinning: a conversation left running holds a PAM transaction open on a
/// session whose user has gone to the login screen. The switch itself is a system-bus round trip
/// with no observable state here, so what this asserts is the compositor's half — including that a
/// click a few pixels outside the circle still just raises the prompt, since the button is round
/// and its bounding box is a quarter larger than it is.
#[test]
fn clicking_the_switch_user_button_cancels_the_prompt() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::dbus::accounts_service::AccountsToSynoik;
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(true));
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(true));

    let raise = |f: &mut Fixture| {
        f.synoik_state()
            .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
        f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
        f.synoik_state()
            .on_verifier_event(VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            });
        f.synoik_state()
            .on_shield_key(None, Some('a'), Default::default());
    };
    raise(&mut f);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt,
        "the fixture is on the prompt page"
    );

    let monitor = Rectangle::from_size(Size::from((1920., 1080.)));
    let rect = crate::ui::lock_screen::switch_user_rect(monitor);
    let centre = Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));

    // A click just outside the circle but inside its box: the corner of the bounding square.
    f.synoik_state().on_shield_click(rect.loc);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt,
        "a click in the button's corner must not have counted as the button"
    );

    // ...and one on the button itself drops back to the clock, the prompt cancelled.
    f.synoik_state().on_shield_click(centre);
    assert_eq!(
        f.synoik().unlock_dialog.page(),
        crate::unlock_dialog::Page::Clock,
        "the click did not cancel the authentication in flight"
    );
    assert!(
        f.synoik().screen_shield.is_active(),
        "and it must certainly not have unlocked the screen"
    );
}

/// The compositor's half of `_updateState` (`js/ui/mpris.js:167-177`): `DesktopEntry` resolves
/// through the app system, and the card's source name is the app's name with `Identity` as the
/// fallback. Everything else about a player is what the watcher validated.
#[test]
fn mpris_players_resolve_their_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.gnome.Rhythmbox3.desktop",
            "Rhythmbox",
        )])),
        Box::new(RecordingLauncher::default()),
    );

    // `DesktopEntry` is the id WITHOUT `.desktop`, which is what gnome-shell appends
    // (`mpris.js:168`).
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.synoik_state().on_mpris_msg(mpris_update(
        bus,
        mpris_state("Rhythmbox 3", Some("org.gnome.Rhythmbox3")),
    ));
    let player = f
        .synoik()
        .mpris
        .get(bus)
        .expect("player is tracked")
        .clone();
    assert_eq!(
        player.app.as_ref().map(|a| a.id.as_str()),
        Some("org.gnome.Rhythmbox3.desktop")
    );
    assert_eq!(player.source_name(), "Rhythmbox", "the app's name wins");
    assert_eq!(player.artists_line(), "Miles Davis");

    // A player whose DesktopEntry matches nothing installed -- or that sends none at all -- falls
    // back to Identity, and is still shown.
    let other = "org.mpris.MediaPlayer2.mystery";
    f.synoik_state()
        .on_mpris_msg(mpris_update(other, mpris_state("Mystery Player", None)));
    let player = f.synoik().mpris.get(other).unwrap().clone();
    assert!(player.app.is_none());
    assert_eq!(player.source_name(), "Mystery Player");
    assert_eq!(f.synoik().mpris.visible().count(), 2);

    // A vanished bus name takes its player with it (`mpris.js:242-249`).
    f.synoik_state()
        .on_mpris_msg(crate::mpris::MprisToSynoik::PlayerRemoved {
            bus_name: bus.to_owned(),
        });
    assert!(f.synoik().mpris.get(bus).is_none());
    assert_eq!(f.synoik().mpris.visible().count(), 1);
}

/// `raise()` (`mpris.js:93-100`) prefers the app over the remote `Raise()`, because a remote raise
/// runs into focus-stealing prevention. With no resolvable app it falls back -- but only when the
/// player claims `CanRaise`.
#[test]
fn mpris_raise_prefers_the_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::mpris::SynoikToMpris;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let recorder = RecordingLauncher::default();
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.gnome.Rhythmbox3.desktop",
            "Rhythmbox",
        )])),
        Box::new(recorder.clone()),
    );

    // Stand in for the watcher's inbound half so the calls we would make are observable.
    let (tx, rx) = async_channel::unbounded();
    f.synoik().mpris_emit = Some(tx);

    // A resolvable app that is not running: activating it is a launch, and NOTHING goes on the bus.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.synoik_state().on_mpris_msg(mpris_update(
        bus,
        mpris_state("Rhythmbox 3", Some("org.gnome.Rhythmbox3")),
    ));
    f.synoik_state().raise_mpris_player(bus);
    assert_eq!(recorder.calls.borrow().len(), 1);
    assert_eq!(
        recorder.calls.borrow()[0].0.id,
        "org.gnome.Rhythmbox3.desktop"
    );
    assert!(rx.try_recv().is_err(), "the app path never calls Raise()");

    // No app, but CanRaise: the fallback goes out on the bus.
    let other = "org.mpris.MediaPlayer2.mystery";
    let mut state = mpris_state("Mystery Player", None);
    state.can_raise = true;
    f.synoik_state().on_mpris_msg(mpris_update(other, state));
    f.synoik_state().raise_mpris_player(other);
    assert_eq!(
        rx.try_recv().ok(),
        Some(SynoikToMpris::Raise(other.to_owned()))
    );

    // No app and no CanRaise: there is nothing to do, and we must not invent a launch.
    f.synoik_state()
        .on_mpris_msg(mpris_update(other, mpris_state("Mystery Player", None)));
    f.synoik_state().raise_mpris_player(other);
    assert!(rx.try_recv().is_err());
    assert_eq!(recorder.calls.borrow().len(), 1);

    // A player that is not tracked at all is a no-op, not a panic -- the card can outlive it.
    f.synoik_state()
        .raise_mpris_player("org.mpris.MediaPlayer2.gone");
    assert!(rx.try_recv().is_err());
}

/// The media card sits at the TOP of the message list, above every notification group: gnome-shell
/// inserts each player's message at index 0 of the `MessageView` (`js/ui/messageList.js:1780-1784`,
/// mpris set up before notifications at `:1516-1518`). It carries no close button and cannot be
/// cleared (`canClose() = false`, `:668-670`), so a list holding only players shows neither the
/// placeholder nor the Clear pill (`empty`/`canClear`, `:1521-1527`).
#[test]
fn media_cards_sit_above_the_notification_groups() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    // A player, then a notification, so their orders would disagree if the list appended.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.synoik_state()
        .on_mpris_msg(mpris_update(bus, mpris_state("Rhythmbox", None)));
    let nid = banner_notify(&mut f, banner_req("app-a", ":1.1"));
    f.settle_animations();

    open_calendar(&mut f);
    let dm = f.synoik().panel_popover.date_menu().unwrap();
    let media = dm.media_card_rects();
    let cards = dm.card_rects();
    assert_eq!(media.len(), 1, "one card per player");
    assert_eq!(media[0].0, bus);
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].0, nid);
    assert!(
        media[0].1.loc.y + media[0].1.size.h <= cards[0].1.loc.y,
        "the media card must be entirely above the notification card"
    );
    // The notification is what makes the pill appear...
    assert!(dm.clear_pill_rect().is_some());

    // ... and with it gone, a list holding only the player has no pill and no placeholder.
    f.synoik_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::ClearNotifications);
    let dm = f.synoik().panel_popover.date_menu().unwrap();
    assert!(dm.card_rects().is_empty());
    assert_eq!(
        dm.media_card_rects().len(),
        1,
        "the player card survives Clear"
    );
    assert!(
        dm.clear_pill_rect().is_none(),
        "a media card cannot be closed, so nothing can be cleared"
    );
    assert!(
        !dm.list().is_empty(),
        "the list is not empty, so no placeholder"
    );

    // A player that stops being playable takes its card with it (`mpris.js:217-223`).
    let mut stopped = mpris_state("Rhythmbox", None);
    stopped.can_play = false;
    f.synoik_state().on_mpris_msg(mpris_update(bus, stopped));
    let dm = f.synoik().panel_popover.date_menu().unwrap();
    assert!(dm.media_card_rects().is_empty());
    assert!(dm.list().is_empty(), "now the placeholder is back");
}

/// The card's controls drive the player (`js/ui/messageList.js:778-791` → `mpris.js:73-91`) and,
/// unlike a menu item, leave the popover open. Its body raises the player and closes it
/// (`MediaMessage.vfunc_clicked`, `:799-804`), and an insensitive skip button is `reactive = false`
/// (`:836-838`) — so a click on it falls through to the body rather than being swallowed.
#[test]
fn media_card_controls_drive_the_player() {
    use crate::mpris::SynoikToMpris;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, rx) = async_channel::unbounded();
    f.synoik().mpris_emit = Some(tx);

    // Next is allowed, Previous is not — the state the fall-through case needs.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    let mut state = mpris_state("Rhythmbox", None);
    state.can_go_next = true;
    state.can_go_previous = false;
    state.can_raise = true;
    f.synoik_state().on_mpris_msg(mpris_update(bus, state));

    open_calendar(&mut f);
    let output = f.synoik_output(1);
    let origin = f.synoik().panel_popover.content_location(&output);
    let (_, card, controls) = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .media_card_rects()
        .remove(0);

    let click = |f: &mut Fixture, rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        let cx = origin.x + rect.loc.x + rect.size.w / 2.;
        let cy = origin.y + rect.loc.y + rect.size.h / 2.;
        pointer_motion_to(f, cx, cy);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    // Play/pause and next go out on the bus, and the popover stays open.
    click(&mut f, controls[1]);
    assert_eq!(
        rx.try_recv().ok(),
        Some(SynoikToMpris::PlayPause(bus.to_owned()))
    );
    assert!(
        f.synoik().panel_popover.is_open(),
        "a control is not a menu item"
    );
    click(&mut f, controls[2]);
    assert_eq!(
        rx.try_recv().ok(),
        Some(SynoikToMpris::Next(bus.to_owned()))
    );

    // Previous is insensitive: the click reaches the message, which raises the player. With no
    // app resolved and CanRaise set, raising is the remote `Raise()`.
    click(&mut f, controls[0]);
    assert_eq!(
        rx.try_recv().ok(),
        Some(SynoikToMpris::Raise(bus.to_owned()))
    );
    // `close()` starts the fade; the popover is open until it finishes.
    f.settle_animations();
    assert!(
        !f.synoik().panel_popover.is_open(),
        "raising the player closes the calendar"
    );

    // The body does the same. Re-open and click the card away from every control.
    open_calendar(&mut f);
    click(
        &mut f,
        smithay::utils::Rectangle::new(card.loc, smithay::utils::Size::from((80., card.size.h))),
    );
    assert_eq!(
        rx.try_recv().ok(),
        Some(SynoikToMpris::Raise(bus.to_owned()))
    );
    f.settle_animations();
    assert!(!f.synoik().panel_popover.is_open());
}

/// Only the VOLUME icon takes the scroll. gnome-shell connects `scroll-event` to that one
/// indicator's actor (`js/ui/status/volume.js:434-437,470-472`), so its neighbours in the status
/// cluster have no scroll behavior and a wheel notch over them falls through to whatever else
/// wants it — here, a plain wheel bind.
#[test]
fn only_the_volume_icon_consumes_a_panel_scroll() {
    use synoik_config::Trigger;

    use crate::gnome::GnomeKeybinding;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // No-modifier scroll bindings, so "was the event consumed?" is observable without PipeWire.
    let bind = |trigger, action| GnomeKeybinding {
        action: KeybindingAction::Synoik(action),
        accels: vec![Accel {
            trigger: AccelTrigger::Device(trigger),
            mods: AccelMods::empty(),
        }],
        cooldown: None,
    };
    f.synoik().gnome_settings.keybindings = vec![
        bind(Trigger::WheelScrollDown, Action::FocusWorkspaceDown),
        // Up, so it is observable from where the wheel bind leaves us: with one window there are
        // only two workspaces, and workspace 1 is the last.
        bind(Trigger::TouchpadScrollDown, Action::FocusWorkspaceUp),
    ];
    // Without this the modifier fast path never lets the lookup happen at all.
    f.synoik().refresh_keybinding_state();

    // Two workspaces to move between, and a volume icon in the cluster to aim at.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.synoik_state()
        .on_audio_status(Some(crate::audio::AudioStatus {
            volume: 0.5,
            muted: false,
        }));
    let active = |f: &mut Fixture| {
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx()
    };
    assert_eq!(active(&mut f), 0);

    let volume = f.synoik().panel.volume_indicator_rect(1920.).unwrap();
    let centre_x = volume.loc.x + volume.size.w / 2.;

    // Over the volume icon: consumed, so the bind never runs. (Changing the volume itself needs a
    // PipeWire connection, which a headless fixture has none of -- see the OSD test below.)
    pointer_motion_to(&mut f, centre_x, 10.);
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        active(&mut f),
        0,
        "a scroll over the volume icon belongs to the volume, not to the wheel bind"
    );

    // Just outside it -- still on the panel, still on the status cluster -- the bind fires.
    pointer_motion_to(&mut f, volume.loc.x + volume.size.w + 4., 10.);
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        active(&mut f),
        1,
        "the icons beside the volume one have no scroll behavior of their own"
    );

    // A TOUCHPAD scroll over the icon is the volume's too: GNOME's SMOOTH branch turns the delta
    // into fractional steps (`volume.js:452-458`), where ours used to ignore anything but a wheel.
    pointer_motion_to(&mut f, centre_x, 10.);
    f.scroll_finger(0., 120.);
    f.synoik_complete_animations();
    assert_eq!(
        active(&mut f),
        1,
        "a touchpad scroll over the volume icon must be consumed too"
    );

    // ... and off the icon it reaches the touchpad bind, proving the fixture's finger scroll does
    // fire it and the assertion above is not vacuous.
    pointer_motion_to(&mut f, volume.loc.x + volume.size.w + 4., 10.);
    f.scroll_finger(0., 120.);
    f.synoik_complete_animations();
    assert_eq!(active(&mut f), 0, "off the icon, the touchpad bind runs");
}

/// What a scroll over the indicator decides to do, which is the half of the handler that does not
/// need a PipeWire connection. GNOME's `if (item.mapped || item.slider.step(nSteps))
/// item.showOSD()` (`js/ui/status/volume.js:457`) short-circuits on `mapped`: with the
/// quick-settings menu open its slider is on screen, so the scroll reports the volume instead of
/// changing it.
#[test]
fn a_scroll_with_the_quick_settings_open_reports_instead_of_stepping() {
    use crate::input::{volume_scroll_action, VolumeScroll};

    // Menu closed: a filled notch steps, an unfilled one does nothing at all.
    assert_eq!(volume_scroll_action(false, 1.), VolumeScroll::Step(1.));
    assert_eq!(
        volume_scroll_action(false, -0.25),
        VolumeScroll::Step(-0.25)
    );
    assert_eq!(volume_scroll_action(false, 0.), VolumeScroll::Ignore);

    // Menu open: never a step, and the OSD comes up even for a scroll too small to have moved
    // anything -- `mapped` short-circuits before `step()` is ever called.
    assert_eq!(volume_scroll_action(true, 1.), VolumeScroll::OsdOnly);
    assert_eq!(volume_scroll_action(true, 0.), VolumeScroll::OsdOnly);
}

/// `StreamSlider.showOSD` (`js/ui/status/volume.js:284-289`): a volume change made from the panel
/// shows the level bar on EVERY monitor, with no label and the icon the indicator itself shows.
/// The quick-settings drag deliberately shows none — the slider is on screen to speak for itself.
#[test]
fn a_panel_volume_change_shows_the_osd_everywhere() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));
    let one = f.synoik_output(1);
    let two = f.synoik_output(2);

    f.synoik_state()
        .show_volume_osd(&crate::audio::AudioStatus {
            volume: 0.5,
            muted: false,
        });
    let content = f
        .synoik()
        .osd
        .content(&one)
        .expect("output 1 shows the OSD");
    assert_eq!(content.icon, vec!["audio-volume-medium-symbolic"]);
    assert_eq!(content.label, None);
    assert_eq!(content.level, Some(0.5));
    assert_eq!(
        content.max_level,
        crate::audio::MAX_VOLUME,
        "the bar is scaled to the volume ceiling, not to 1.0 by accident"
    );
    assert!(
        f.synoik().osd.content(&two).is_some(),
        "showAll, not showOne: every monitor gets it"
    );

    // Muting swaps the glyph, as the indicator's own icon does.
    f.synoik_state()
        .show_volume_osd(&crate::audio::AudioStatus {
            volume: 0.5,
            muted: true,
        });
    assert_eq!(
        f.synoik().osd.content(&one).unwrap().icon,
        vec!["audio-volume-muted-symbolic"]
    );

    // The QS slider path is silent: it goes through `apply_popover_action`, which never asks for
    // an OSD.
    f.synoik().osd.hide_all();
    f.settle_animations();
    f.synoik_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetVolume(0.8));
    assert!(
        !f.synoik().osd.is_visible(),
        "dragging the visible slider must not also raise an OSD"
    );
}

/// The whole panel-scroll chain end to end — pointer over the volume icon, wheel notch, a real
/// write reaching the backend, and the OSD (`js/ui/status/volume.js:452-458`).
///
/// This is what the [`crate::audio::AudioBackend`] seam bought: with a concrete PipeWire handle on
/// `Synoik` the headless fixture had no backend at all, so the scroll path returned early and none
/// of this was observable — deleting the `show_volume_osd` call left the suite green.
#[test]
fn a_scroll_over_the_volume_icon_steps_the_backend_and_shows_the_osd() {
    use crate::audio::AudioWrite;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    let audio = f.install_stub_audio(0.5);

    let volume = f.synoik().panel.volume_indicator_rect(1920.).unwrap();
    let centre_x = volume.loc.x + volume.size.w / 2.;
    pointer_motion_to(&mut f, centre_x, 10.);

    // One notch down: a write of exactly one slider step, and an OSD saying so.
    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        audio.writes(),
        vec![AudioWrite::Volume(0.5 - crate::audio::SCROLL_STEP)],
        "a wheel notch is one SLIDER_SCROLL_STEP, written to the backend"
    );
    let content = f
        .synoik()
        .osd
        .content(&output)
        .expect("the scroll shows an OSD");
    assert_eq!(content.level, Some(0.5 - crate::audio::SCROLL_STEP));
    assert_eq!(content.icon, vec!["audio-volume-medium-symbolic"]);
    assert_eq!(
        f.synoik().audio.unwrap().volume,
        0.5 - crate::audio::SCROLL_STEP,
        "the model the panel icon reads follows the write, without waiting for an echo"
    );

    // At the ceiling the write still happens, but the value cannot move -- and GNOME gates the OSD
    // on `slider.step()` having returned true (`volume.js:457`), so a scroll that changes nothing
    // must not re-arm an OSD that says the same thing.
    f.synoik_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetVolume(
            crate::audio::MAX_VOLUME,
        ));
    f.synoik().osd.hide_all();
    f.settle_animations();
    audio.clear_writes();

    f.scroll_wheel_up();
    f.synoik_complete_animations();
    assert!(
        !f.synoik().osd.is_visible(),
        "scrolling up at the ceiling must not keep re-arming the OSD"
    );

    // With the quick settings open the slider is already on screen: `mapped` short-circuits, so the
    // scroll reports the volume and writes NOTHING (`volume.js:457`). The volume icon lives inside
    // the quick-settings cluster, so clicking where we just scrolled is what opens it.
    pointer_motion_to(&mut f, centre_x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        "the volume icon is part of the quick-settings button"
    );
    f.synoik().osd.hide_all();
    f.settle_animations();
    audio.clear_writes();

    f.scroll_wheel();
    f.synoik_complete_animations();
    assert_eq!(
        audio.writes(),
        vec![],
        "with its slider on screen, the scroll must not change the volume"
    );
    assert!(
        f.synoik().osd.is_visible(),
        "...but it still says what the volume is"
    );
}

/// Activating a port row writes the route, then the default node — gvc's `change_output` case 3
/// (`change_port` on the stream, then `set_default_sink`). A portless row writes only the default,
/// gvc's case 2.
#[test]
fn picking_an_output_port_writes_the_route_then_the_default() {
    use crate::audio::{AudioDeviceKey, AudioWrite};
    use crate::ui::popover::PopoverAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let audio = f.install_stub_audio(0.5);

    // Headphones on the same node as the current default: the route write is the whole switch, and
    // re-asserting the default is harmless (gvc does the same).
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetOutputDevice(AudioDeviceKey::Port {
            card_id: 42,
            route_index: 1,
            device: 0,
            node: Some("alsa_output.pci".to_owned()),
        }));
    assert_eq!(
        audio.writes(),
        vec![
            AudioWrite::Route {
                card_id: 42,
                device: 0,
                route_index: 1
            },
            AudioWrite::DefaultSink("alsa_output.pci".to_owned()),
        ],
        "the route comes first: making a node default before selecting its port would land on the \
         old port"
    );

    // A port with no node behind it still selects the route rather than doing nothing.
    audio.clear_writes();
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetOutputDevice(AudioDeviceKey::Port {
            card_id: 42,
            route_index: 2,
            device: 1,
            node: None,
        }));
    assert_eq!(
        audio.writes(),
        vec![AudioWrite::Route {
            card_id: 42,
            device: 1,
            route_index: 2
        }]
    );

    // A portless device (bluetooth): default only, no route.
    audio.clear_writes();
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetOutputDevice(AudioDeviceKey::Node(
            "bluez_output.AA".to_owned(),
        )));
    assert_eq!(
        audio.writes(),
        vec![AudioWrite::DefaultSink("bluez_output.AA".to_owned())]
    );

    // The input side takes the same two shapes, against the source setters.
    audio.clear_writes();
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetInputDevice(AudioDeviceKey::Port {
            card_id: 42,
            route_index: 3,
            device: 0,
            node: Some("alsa_input.pci".to_owned()),
        }));
    assert_eq!(
        audio.writes(),
        vec![
            AudioWrite::Route {
                card_id: 42,
                device: 0,
                route_index: 3
            },
            AudioWrite::DefaultSource("alsa_input.pci".to_owned()),
        ]
    );
}

/// Plugging headphones in: `OutputStreamSlider._portChanged` (`js/ui/status/volume.js:347-358`).
///
/// The suppression rule is the whole point. `initializing = this._hasHeadphones === undefined` is
/// once per shell lifetime — so the first answer sets the icon silently, and every change after it
/// shows the OSD. And `_hasHeadphones` survives a default-sink swap, so moving from a headphone
/// sink to a speaker sink is a change that *does* speak up.
#[test]
fn a_port_change_to_headphones_shows_the_osd_but_the_first_answer_is_silent() {
    use crate::audio::{AudioCard, AudioCards, PortDirection, RouteInfo, SinkCard, SinkInfo};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);
    f.install_stub_audio(0.5);

    // One card whose active output route is `port`.
    let cards = |port: &str| AudioCards {
        cards: vec![AudioCard {
            id: 42,
            description: "Built-in Audio".to_owned(),
            icon_name: Some("audio-card-analog".to_owned()),
            ports: vec![],
            active_profile: None,
            active: vec![RouteInfo {
                index: 0,
                direction: Some(PortDirection::Output),
                name: port.to_owned(),
                device: Some(1),
                ..RouteInfo::default()
            }],
        }],
    };
    let sinks = crate::audio::SinkList {
        sinks: vec![SinkInfo {
            name: "sink".to_owned(),
            description: "Built-in Audio".to_owned(),
            card: Some(SinkCard {
                card_id: 42,
                device: Some(1),
            }),
            form_factor: None,
        }],
        default_name: Some("sink".to_owned()),
    };

    // FIRST answer — speakers. The state is recorded, and NOTHING is shown.
    f.synoik_state().on_sink_list(sinks.clone());
    f.synoik_state().on_audio_cards(cards("analog-output"));
    assert_eq!(f.synoik().headphones, Some(false));
    assert!(
        !f.synoik().osd.is_visible(),
        "the initial sync must not raise an OSD (`initializing`)"
    );

    // Plug headphones in: a change, so the OSD comes up — showing the LEVEL glyph, never the
    // headphone one (`showOSD` uses `getIcon()`, `volume.js:283-288`).
    f.synoik_state()
        .on_audio_cards(cards("analog-output-headphones"));
    assert_eq!(f.synoik().headphones, Some(true));
    let content = f
        .synoik()
        .osd
        .content(&output)
        .expect("plugging headphones in shows the volume OSD");
    assert_eq!(
        content.icon,
        vec!["audio-volume-medium-symbolic"],
        "the OSD shows the level, not the headphone glyph"
    );
    assert_eq!(content.level, Some(0.5));

    // Re-publishing the same port is not a change: no second OSD.
    f.synoik().osd.hide_all();
    f.settle_animations();
    f.synoik_state()
        .on_audio_cards(cards("analog-output-headphones"));
    assert!(
        !f.synoik().osd.is_visible(),
        "an unchanged port must not re-arm the OSD"
    );

    // Unplugging is a change too, and the first answer's silence is long spent.
    f.synoik_state().on_audio_cards(cards("analog-output"));
    assert_eq!(f.synoik().headphones, Some(false));
    assert!(f.synoik().osd.is_visible(), "unplugging speaks up as well");

    // A bluetooth headset arriving as the new default: no card, but a form factor. GNOME does not
    // reset `_hasHeadphones` across a stream swap, so this is a change and shows the OSD.
    f.synoik().osd.hide_all();
    f.settle_animations();
    f.synoik_state().on_sink_list(crate::audio::SinkList {
        sinks: vec![SinkInfo {
            name: "bluez".to_owned(),
            description: "Bluetooth Headset".to_owned(),
            card: None,
            form_factor: Some("headset".to_owned()),
        }],
        default_name: Some("bluez".to_owned()),
    });
    assert_eq!(f.synoik().headphones, Some(true));
    assert!(
        f.synoik().osd.is_visible(),
        "a default-sink swap that changes the answer shows the OSD -- \
         `_hasHeadphones` is not reset per stream"
    );
}

/// The quick-settings audio controls reach the backend: the sliders write volume, the icons toggle
/// mute, and the device pickers set the default by `node.name`.
///
/// Setting a default is deliberately fire-and-forget — gvc's `change_output` has no corrective echo
/// for a rejected write, so nothing may move the picker's check optimistically.
#[test]
fn the_quick_settings_audio_controls_reach_the_backend() {
    use crate::audio::{AudioWrite, MicStatus};
    use crate::ui::popover::PopoverAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let audio = f.install_stub_audio(0.5);

    f.synoik_state()
        .apply_popover_action(PopoverAction::SetVolume(0.8));
    f.synoik_state()
        .apply_popover_action(PopoverAction::ToggleMute);
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetOutputDevice(
            crate::audio::AudioDeviceKey::Node(
                "alsa_output.pci-0000_00_1f.3.analog-stereo".to_owned(),
            ),
        ));
    assert_eq!(
        audio.writes(),
        vec![
            AudioWrite::Volume(0.8),
            AudioWrite::Muted(true),
            AudioWrite::DefaultSink("alsa_output.pci-0000_00_1f.3.analog-stereo".to_owned()),
        ]
    );
    assert!(
        f.synoik().audio.unwrap().muted,
        "the mute toggle updates the model the panel icon reads"
    );
    assert_eq!(
        f.synoik().sink_list.default_name,
        None,
        "the picker's check waits for the backend's echo -- a rejected write has none"
    );

    // The input side needs a bound source, exactly as the live backend does: with none, the mic
    // controls return None and the compositor leaves its model alone.
    audio.clear_writes();
    f.synoik_state()
        .apply_popover_action(PopoverAction::ToggleInputMute);
    assert_eq!(
        audio.writes(),
        vec![],
        "no source bound: nothing to control, and nothing written"
    );

    let audio = f.install_stub_audio(0.5);
    let audio = audio.with_mic(MicStatus {
        recording: true,
        muted: false,
        volume: 0.4,
        source_present: true,
    });
    f.synoik().audio_backend = Some(Box::new(audio.clone()));
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetInputVolume(0.6));
    f.synoik_state()
        .apply_popover_action(PopoverAction::ToggleInputMute);
    f.synoik_state()
        .apply_popover_action(PopoverAction::SetInputDevice(
            crate::audio::AudioDeviceKey::Node("alsa_input.usb".to_owned()),
        ));
    assert_eq!(
        audio.writes(),
        vec![
            AudioWrite::InputVolume(0.6),
            AudioWrite::InputMuted(true),
            AudioWrite::DefaultSource("alsa_input.usb".to_owned()),
        ]
    );
    assert!(
        f.synoik().mic.muted,
        "the mic mute updates the model the privacy indicator reads"
    );
}

/// Map a window carrying an `app_id`, so it resolves to an app in the switcher.
pub(super) fn map_window_for_app(f: &mut Fixture, id: ClientId, app_id: &str) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

/// Super-Tab raises the app switcher, and it starts on the *previous* app.
///
/// The initial selection is the whole contract: item 0 is the app you are already in, so a
/// forward switcher that started there would make tap-and-release do nothing at all
/// (`_initialSelection`, `switcherPopup.js:113-120`).
///
/// Driven through `do_action` rather than by hand-building the popup, so the app grouping, the
/// tab list and the state machine all run the way a keypress runs them.
#[test]
fn super_tab_opens_the_app_switcher_on_the_previous_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    assert!(
        f.synoik().switcher.is_open(),
        "Super-Tab raises the switcher"
    );
    assert_eq!(
        f.synoik().switcher.selected(),
        Some(1),
        "a forward switcher starts on the previous app, not the current one"
    );
}

/// A tap shorter than the open delay switches with no popup ever drawn.
///
/// Both halves matter and are asserted together: the switcher must have committed, *and* nothing
/// must ever have been visible. Asserting only the commit would pass an implementation that
/// flashes the popup for a frame, which is exactly what the 150 ms delay exists to prevent.
#[test]
fn a_quick_super_tab_switches_without_showing_the_popup() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    let first = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    // Hold Super for real, so the popup gets a modifier to commit on: driving the action with
    // nothing held makes it a *no-modifier* switcher, which commits on a timeout instead and
    // would quietly test the wrong path.
    const KEY_LEFTMETA: u32 = 125;
    f.key_press(KEY_LEFTMETA);

    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert!(f.synoik().switcher.is_open());
    assert!(
        !f.synoik().switcher.is_visible(),
        "nothing is drawn inside the open delay"
    );

    // Let go well inside the delay. The popup was never drawn, and the switch happens anyway --
    // the release reaches us because the grab was taken at open rather than at reveal.
    f.key_release(KEY_LEFTMETA);

    assert!(
        !f.synoik().switcher.is_open(),
        "the release ends the session"
    );
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        first,
        "the tap moved focus to the previously used app's window"
    );
}

/// Super-Tab's window sub-list: it pops up on its own, Down descends into it, and picking a
/// preview commits to *that* window instead of the app's most recent one.
///
/// The whole point of `ThumbnailSwitcher` is reaching an app's second window, so the assertion
/// that matters is the last one: the same app item, committed twice, activating different windows
/// depending on what the sub-list had picked.
///
/// The two ways it opens are deliberately both here, because they differ in a way that is easy to
/// get wrong. The 500ms timer opens it with **nothing** picked (`_timeoutPopupThumbnails`,
/// `altTab.js:359-364` never touches `_currentWindow`), so releasing then still activates what the
/// app row promised; Down opens it *on* window 0 (`:206`), which changes the target.
#[test]
fn super_tab_pops_up_an_apps_windows_and_commits_to_the_one_it_picks() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    // "One" gets two windows, so it has a sub-list; "Two" is the app the switcher starts on.
    // Each window maps focused, so the focus right after is that window's id.
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    // Rest on "One" (item 1) and let the popup timer run out.
    let open_on_one = |f: &mut Fixture| {
        f.key_press(KEY_LEFTMETA);
        f.synoik_state()
            .do_action(Action::SwitchApplications { backward: false }, false);
        assert_eq!(f.synoik().switcher.selected(), Some(1), "opens on \"One\"");
    };
    let rest = |f: &mut Fixture, by: Duration| {
        let mut clock = f.synoik().clock.clone();
        let now = clock.now_unadjusted();
        clock.set_unadjusted(now + by);
        f.synoik().advance_animations();
    };

    open_on_one(&mut f);
    assert!(
        !f.synoik().switcher.thumbnails_open(),
        "the sub-list is not instant — tabbing through a multi-window app must not flash it"
    );
    rest(&mut f, crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    assert!(
        f.synoik().switcher.thumbnails_open(),
        "resting on a multi-window app pops its windows up"
    );
    assert_eq!(
        f.synoik().switcher.thumbnail_selected(),
        None,
        "...with nothing picked in it"
    );

    // So the release still activates the app's most recent window.
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        b,
        "a sub-list nobody picked from commits to the app's first window"
    );

    // Now descend into it and take the *other* window.
    open_on_one(&mut f);
    tap(&mut f, KEY_DOWN);
    assert!(
        f.synoik().switcher.thumbnails_open(),
        "Down opens the sub-list at once"
    );
    assert_eq!(f.synoik().switcher.thumbnail_selected(), Some(0));

    tap(&mut f, KEY_RIGHT);
    assert_eq!(
        f.synoik().switcher.thumbnail_selected(),
        Some(1),
        "Right walks the previews, not the app row"
    );
    assert_eq!(
        f.synoik().switcher.selected(),
        Some(1),
        "and the app row stays where it was"
    );

    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        a,
        "committing with a preview picked activates that window"
    );
}

/// Up leaves the window sub-list, and moving to another app takes it down.
///
/// Both are `_select`'s doing rather than the sub-list's: `window == null` destroys it
/// (`altTab.js:328-331`), and `forceAppFocus` — which only Up passes — is what stops the 500ms
/// timer from putting it straight back (`:349-356`).
#[test]
fn the_window_sublist_closes_on_up_and_on_moving_to_another_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    tap(&mut f, KEY_DOWN);
    assert!(f.synoik().switcher.thumbnails_open());

    // Up hands the arrows back to the app row and does *not* re-open the sub-list on the timer.
    tap(&mut f, KEY_UP);
    assert!(!f.synoik().switcher.thumbnails_open(), "Up closes it");

    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.synoik().advance_animations();
    assert!(
        !f.synoik().switcher.thumbnails_open(),
        "and it stays closed — `forceAppFocus` does not re-arm the timer"
    );

    // With the arrows back on the row, Left/Right move apps again...
    tap(&mut f, KEY_LEFT);
    assert_eq!(
        f.synoik().switcher.selected(),
        Some(0),
        "the arrows are back on the app row"
    );

    // ...and landing on the single-window app arms nothing at all.
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.synoik().advance_animations();
    assert!(
        !f.synoik().switcher.thumbnails_open(),
        "a one-window app has no sub-list to show"
    );

    f.key_release(KEY_LEFTMETA);
}

/// DIVERGENCE: `switch-group` is the *window* switcher over one app, not the app switcher
/// pinned inside it.
///
/// GNOME opens `AppSwitcherPopup` on (app 0, window 1) with the thumbnail sub-list already up
/// (`_initialSelection`, `altTab.js:117-137`), which spends the top of the popup on an app row
/// you cannot usefully move in. Every item in this session belongs to one app by construction, so
/// we show that app's windows directly: same previews, same footer title, no row.
///
/// The two are told apart here by their *arrangement*, not by an internal flag — a window
/// switcher has one panel-wide title band and no sub-list, an app switcher has neither.
///
/// Driven through the real `Above_Tab` key, which is a **keycode** match: mutter special-cases its
/// fake keysym to `KEY_GRAVE + 8` before consulting any layout
/// (`src/core/keybindings.c:385-392`), so the binding is the physical key above Tab whatever it
/// happens to type.
#[test]
fn switch_group_walks_the_current_apps_windows_as_a_window_switcher() {
    const KEY_GRAVE: u32 = 41;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);

    let client = f.add_client();
    // "One" gets three windows so forward's "second" and backward's "last" are distinguishable.
    // Each maps focused, so within "One" the MRU order ends up [c, b, a].
    map_window_for_app(&mut f, client, "org.example.Two");
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let c = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    // Super + the key above Tab, held.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_GRAVE);

    assert!(
        f.synoik().switcher.is_open(),
        "Above_Tab raises the switcher"
    );
    assert_eq!(
        f.synoik().switcher.item_count(),
        Some(3),
        "over the focused app's windows alone — \"Two\" is not in the list"
    );
    assert!(
        f.synoik().switcher.footer_rect().is_some(),
        "laid out as window previews, with the title in the panel-wide footer band"
    );
    assert!(
        !f.synoik().switcher.thumbnails_open(),
        "there is no app row to descend out of, so there is no sub-list"
    );
    assert_eq!(
        f.synoik().switcher.selected(),
        Some(1),
        "starting on the app's *second* window — the one you are not in"
    );

    // A second press walks the same list; there is nothing else it could mean.
    tap(&mut f, KEY_GRAVE);
    assert_eq!(f.synoik().switcher.selected(), Some(2));

    // And releasing commits to the window it had picked.
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    let focused = f.synoik().layout.focus().unwrap().id();
    assert_eq!(focused, a, "committed to the picked window");
    assert_ne!(focused, b);
    assert_ne!(focused, c);
    assert_eq!(
        stack_order(&mut f).first().copied(),
        Some(a),
        "...and only that one came forward: a window switcher is not a group raise"
    );

    // Backward starts at the *end* of the app's windows instead.
    f.key_press(KEY_LEFTSHIFT);
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_GRAVE);
    assert_eq!(f.synoik().switcher.selected(), Some(2));
    f.key_release(KEY_LEFTMETA);
    f.key_release(KEY_LEFTSHIFT);
}

/// `cycle-windows` (`<Alt>Escape`) walks the same window list with **no popup at all**.
///
/// `WindowCyclerPopup` (`altTab.js:638-667`) extends `CyclerPopup`, whose `_switcherList` is a
/// `CyclerList` that draws nothing (`:472-484`). The selection is shown by raising the window and
/// framing it with `.cycler-highlight`, so this asserts on the highlight rather than on a panel —
/// and the highlight is up on the *first* press, without waiting out `POPUP_DELAY`, because
/// `_highlightItem` runs from `_initialSelection` inside `show()` and the delay only ever touched
/// the popup actor's opacity.
#[test]
fn alt_escape_cycles_windows_in_place_with_no_popup() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.A");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.B");
    let b = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_ESC);

    assert!(f.synoik().switcher.is_open(), "Alt+Escape opens a cycler");
    assert!(
        f.synoik().switcher.item_rect(0).is_none() && f.synoik().switcher.footer_rect().is_none(),
        "...which has no list, so it measures no panel to hit-test or draw"
    );
    // `_initialSelection`: forward starts at 1, the window you are *not* on.
    assert_eq!(f.synoik().switcher.cycler_window(), Some(a));
    let highlight = f.synoik().cycler_highlight.expect("the window is framed");
    assert!(
        highlight.size.w > 0. && highlight.size.h > 0.,
        "and framed somewhere real: {highlight:?}"
    );

    // `<Alt>F6` is the *other* cycler's binding: `_keyPressHandler` matches one action and
    // propagates the rest, so it does not cross-drive this one.
    tap(&mut f, KEY_F6);
    assert_eq!(
        f.synoik().switcher.cycler_window(),
        Some(a),
        "the group cycler's key does not drive the window cycler"
    );

    // A second press of its own key walks on; the frame follows.
    tap(&mut f, KEY_ESC);
    assert_eq!(f.synoik().switcher.cycler_window(), Some(b));
    assert_ne!(f.synoik().cycler_highlight, Some(highlight));

    // Releasing the modifier commits, like any other switcher.
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), b);
    assert!(
        f.synoik().cycler_highlight.is_none(),
        "and the frame goes with the session"
    );
}

/// A window closing under a running cycler removes it from the walk, and nothing else.
///
/// `_itemRemoved` (`switcherPopup.js:269-284`) is shared by every switcher, but a cycler is the one
/// with no `_switcherList` items to remove alongside — `CyclerList` draws nothing
/// (`altTab.js:472-484`). Ours used to drop the art entry regardless, which on a cycler is an
/// index into an empty vec. That path runs from the `xdg_toplevel` destructor, where a panic is an
/// abort, so the compositor went down with it.
#[test]
fn closing_a_window_under_a_cycler_does_not_take_the_compositor_with_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.A");
    let a = f.synoik().layout.focus().unwrap().id();
    let b_surface = map_window_for_app(&mut f, client, "org.example.B");
    let b = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_ESC);
    assert_eq!(f.synoik().switcher.cycler_window(), Some(a));

    // Walk onto B, then have its client take it away underneath the cycler.
    tap(&mut f, KEY_ESC);
    assert_eq!(f.synoik().switcher.cycler_window(), Some(b));

    let window = f.client(client).window(&b_surface);
    window.xdg_toplevel.destroy();
    window.xdg_surface.destroy();
    window.surface.destroy();
    f.double_roundtrip(client);

    // One window left, so the cycler is still up and pointing at it.
    assert!(
        f.synoik().switcher.is_open(),
        "the cycler survives the close"
    );
    assert_eq!(f.synoik().switcher.cycler_window(), Some(a));

    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    assert_eq!(f.synoik().layout.focus().unwrap().id(), a);
}

/// `cycle-group` (`<Alt>F6`) is the same listless cycler over the focused app's windows only.
///
/// `GroupCyclerPopup._getWindows` is `focus_app.get_windows()` (`altTab.js:557-570`), so a window
/// of another app is never reachable from it however long you hold F6 down — which is the whole
/// difference from `cycle-windows` and the thing a shared item list would silently lose.
#[test]
fn alt_f6_cycles_only_the_focused_apps_windows() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.Two");
    let other = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F6);
    assert!(f.synoik().switcher.is_open());
    assert_eq!(f.synoik().switcher.cycler_window(), Some(a));

    // "One" has exactly two windows, so any number of presses stays inside them.
    for expected in [b, a, b] {
        tap(&mut f, KEY_F6);
        assert_eq!(f.synoik().switcher.cycler_window(), Some(expected));
        assert_ne!(
            f.synoik().switcher.cycler_window(),
            Some(other),
            "the other app's window is not in this cycler at all"
        );
    }

    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(f.synoik().layout.focus().unwrap().id(), b);
}

/// An action the running cycler does not match falls through to the base — so `<Alt>Escape`
/// **abandons** an `<Alt>F6` cycler instead of driving it.
///
/// This is the seam the JS comments out loud: "pressing one of the below keys will destroy the
/// popup only if that key is not used by the active popup's keyboard shortcut"
/// (`switcherPopup.js:206-210`). `GroupCyclerPopup._keyPressHandler` matches `CYCLE_GROUP` and
/// propagates everything else (`altTab.js:571-580`), so the Escape keysym reaches the base and
/// cancels. Get the allowlist wrong — let every switch binding resolve while any popup is up —
/// and the key is swallowed by an action that does nothing, leaving no way out but the modifier.
#[test]
fn a_key_the_cycler_does_not_match_falls_through_and_abandons_it() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.A.desktop",
            "A",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.A");
    map_window_for_app(&mut f, client, "org.example.A");
    let before = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F6);
    assert!(f.synoik().switcher.is_open(), "the group cycler is up");
    assert_ne!(f.synoik().switcher.cycler_window(), Some(before));

    // Still holding Alt: `<Alt>Escape` is `cycle-windows`, which this popup does not match.
    tap(&mut f, KEY_ESC);
    assert!(
        !f.synoik().switcher.is_open(),
        "it abandons rather than cycles"
    );
    assert!(f.synoik().cycler_highlight.is_none());

    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        before,
        "an abandoned cycler leaves the focus where it was"
    );
}

/// Down on a sub-list the *timer* already opened only moves the highlight — it does not rebuild
/// the list, and so does not restart its fade.
///
/// `_select` destroys the thumbnails only when the app changed or the window is null
/// (`altTab.js:329-332`); with the same app and a real window it falls through to
/// `this._thumbnails.highlight(window, ...)` on the list that is already up (`:345-349`).
/// Rebuilding there restarts `THUMBNAIL_FADE_TIME` on a list that is fully on screen, which reads
/// on the seat as the sub-list blinking under the key that was supposed to move a highlight.
///
/// The fade assertion is the one that matters: every *value* here passes either way, so a
/// selection-only test would have shipped the blink.
#[test]
fn descending_into_an_open_sublist_moves_the_highlight_without_rebuilding_it() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    let mut clock = f.synoik().clock.clone();
    let rest = |f: &mut Fixture, clock: &mut crate::animation::Clock, by: Duration| {
        let now = clock.now_unadjusted();
        clock.set_unadjusted(now + by);
        f.synoik().advance_animations();
    };

    // Rest on the multi-window app until its sub-list pops up on the timer, then let the fade
    // finish so "still animating" below can only mean a *new* fade.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert_eq!(f.synoik().switcher.selected(), Some(1), "opens on \"One\"");
    rest(
        &mut f,
        &mut clock,
        crate::ui::switcher::thumbnails::POPUP_TIME * 2,
    );
    rest(
        &mut f,
        &mut clock,
        crate::ui::switcher::thumbnails::FADE_TIME * 2,
    );
    assert!(f.synoik().switcher.thumbnails_open());
    assert_eq!(f.synoik().switcher.thumbnail_selected(), None);
    assert!(
        !f.synoik().switcher.are_animations_ongoing(),
        "the timer-opened list has finished fading in"
    );

    tap(&mut f, KEY_DOWN);

    assert_eq!(
        f.synoik().switcher.thumbnail_selected(),
        Some(0),
        "Down picks the app's first window"
    );
    assert!(
        !f.synoik().switcher.are_animations_ongoing(),
        "and does it in place: a list already on screen must not fade in again"
    );

    f.key_release(KEY_LEFTMETA);
}

/// The window sub-list fades in rather than appearing — and keeps the compositor drawing while
/// it does.
///
/// `_createThumbnails` eases opacity 0 -> 255 over `THUMBNAIL_FADE_TIME` (`altTab.js:381-408`).
/// The second assertion is the one that would otherwise rot: a fade nothing is asking for frames
/// for only advances when some *other* event forces one, and on a switcher the next event is
/// usually the key that ends the session — so it would read as instant on the seat while every
/// value-based assertion here still passed.
#[test]
fn the_window_sublist_fades_in() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.One.desktop",
            "One",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    f.synoik_complete_animations();

    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    tap(&mut f, KEY_DOWN);

    assert!(f.synoik().switcher.thumbnails_open());
    let alpha = f
        .synoik()
        .switcher
        .thumbnail_alpha()
        .expect("an open sub-list");
    assert!(
        alpha < 1.,
        "the sub-list starts transparent and eases in, got {alpha}"
    );
    assert!(
        f.synoik().switcher.are_animations_ongoing(),
        "and it must keep the redraw loop alive, or the fade never runs"
    );

    // Past the fade, it is fully drawn and asks for nothing more.
    let mut clock = f.synoik().clock.clone();
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::FADE_TIME * 2);
    assert_eq!(f.synoik().switcher.thumbnail_alpha(), Some(1.));
    assert!(!f.synoik().switcher.are_animations_ongoing());

    f.key_release(KEY_LEFTMETA);
}

/// `w` closes the window the switcher is pointing at, and `q` quits the selected app — neither
/// ends the session.
///
/// Where they apply is the part worth pinning. Alt-Tab's `w` closes the selected window (`_
/// closeWindow`, `altTab.js:610-616`), but Super-Tab's only works **inside** the sub-list
/// (`:203-208` puts it in the `_thumbnailsFocused` branch), so `w` on the app row does nothing
/// rather than closing a window you cannot see. `q` is the opposite: app switcher only, and it
/// goes through the same `shell_app_request_quit` path as the app menu's Quit row.
#[test]
fn the_switchers_close_and_quit_keys_act_without_ending_the_session() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    const KEY_W: u32 = 17;
    const KEY_Q: u32 = 16;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    let one_a = map_window_for_app(&mut f, client, "org.example.One");
    let one_b = map_window_for_app(&mut f, client, "org.example.One");
    let two = map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    // How many of "One"'s windows have been asked to close, and whether "Two" ever was.
    let asked = |f: &mut Fixture| {
        f.double_roundtrip(client);
        let n = [&one_a, &one_b]
            .iter()
            .filter(|s| f.client(client).window(s).close_requested)
            .count();
        let other = f.client(client).window(&two).close_requested;
        (n, other)
    };

    // Super-Tab, resting on the two-window app.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert_eq!(f.synoik().switcher.selected(), Some(1), "opens on \"One\"");

    // `w` on the app row is a no-op: nothing is picked, so there is nothing to close.
    tap(&mut f, KEY_W);
    assert_eq!(
        asked(&mut f),
        (0, false),
        "`w` on the app row must not close anything — the key belongs to the sub-list"
    );
    assert!(
        f.synoik().switcher.is_open(),
        "and it certainly must not end the session"
    );

    // Nor does it once the sub-list has merely *popped up* on its timer: it is up with nothing
    // picked, and there is still no window the key names.
    let mut clock = f.synoik().clock.clone();
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.synoik().advance_animations();
    assert!(f.synoik().switcher.thumbnails_open());
    assert_eq!(f.synoik().switcher.thumbnail_selected(), None);
    tap(&mut f, KEY_W);
    assert_eq!(
        asked(&mut f),
        (0, false),
        "a sub-list with nothing picked names no window to close"
    );

    // Inside the sub-list it closes the picked window, and the popup stays up.
    tap(&mut f, KEY_DOWN);
    tap(&mut f, KEY_W);
    assert_eq!(
        asked(&mut f),
        (1, false),
        "`w` in the sub-list closes the one picked window"
    );
    assert!(f.synoik().switcher.is_open(), "without ending the session");

    // `q` quits the app: every window of it is asked to close, and no other app's.
    tap(&mut f, KEY_Q);
    assert_eq!(
        asked(&mut f),
        (2, false),
        "`q` asks every window of the selected app to close, and only that app's"
    );
    assert!(f.synoik().switcher.is_open(), "still without ending it");

    f.key_release(KEY_LEFTMETA);
}

/// An open switcher grabs the pointer too: no window under it may keep pointer focus.
///
/// Same rule and same symptom as [`open_popover_suppresses_underlying_pointer_focus`] — a client
/// that still has the pointer keeps setting the cursor image, so moving across the popup cycles
/// through every I-beam and resize arrow of whatever is behind it. `SwitcherPopup.show` takes its
/// modal grab **first** (`pushModal`, `switcherPopup.js:125`) and the open delay that follows only
/// sets opacity, so the suppression starts with the popup, not with its drawing.
#[test]
fn an_open_switcher_suppresses_underlying_pointer_focus() {
    use smithay::utils::{Logical, Point};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (1800, 1000), None);
    let _second = map_focused_window(&mut f, id);

    // The pointer rests over the window, which owns it.
    let over_window = Point::<f64, Logical>::from((900., 500.));
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.synoik().contents_under(over_window).surface.is_some(),
        "the window under the pointer normally receives pointer focus"
    );

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.synoik().switcher.is_open());

    // Still inside the popup's delay: the grab is already up, so the window is already cut off.
    assert!(
        !f.synoik().switcher.is_visible(),
        "sampled inside the open delay, where the popup draws nothing"
    );
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.synoik().contents_under(over_window).surface.is_none(),
        "no window under an open switcher receives pointer focus"
    );
    assert!(
        f.synoik()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "the seat pointer focus is cleared while the switcher holds its grab"
    );

    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();

    // ...and it comes back when the session ends.
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.synoik().contents_under(over_window).surface.is_some(),
        "the window takes the pointer back once the switcher is gone"
    );
}

/// The arrows walk the open switcher, Escape abandons it, and Return takes the selection.
///
/// These are the popup's *keysym* arms, the half of `_keyPressHandler` that does not go through a
/// keybinding: Left is `_previous` and Right is `_next` (`altTab.js:613-620`), Escape destroys and
/// Return finishes (`switcherPopup.js:206-217`). Driven through real key events, because the whole
/// point is that they reach the popup at all — the switcher holds a modal grab, so nothing routes
/// them for us.
#[test]
fn the_switchers_arrows_walk_it_escape_abandons_it_and_return_takes_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let _first = map_focused_window(&mut f, id);
    let _second = map_focused_window(&mut f, id);
    let _third = map_focused_window(&mut f, id);
    let before = f.synoik().layout.focus().unwrap().id();

    // Open on item 1 (the previously used window), then walk right and back.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.synoik().switcher.selected(),
        Some(1),
        "opens on the previous"
    );

    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.synoik().switcher.selected(), Some(2), "Right is _next");
    tap(&mut f, KEY_LEFT);
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.synoik().switcher.selected(), Some(0), "Left is _previous");

    // Escape abandons: the popup goes away and focus has not moved.
    tap(&mut f, KEY_ESC);
    assert!(!f.synoik().switcher.is_open(), "Escape destroys the popup");
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        before,
        "a cancelled switcher leaves focus where it was"
    );

    // Return commits without waiting for the modifier, which is what makes a no-modifier popup
    // usable at all.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.synoik().switcher.is_open());
    tap(&mut f, KEY_ENTER);
    assert!(!f.synoik().switcher.is_open(), "Return finishes the popup");
    f.key_release(KEY_LEFTALT);
    f.synoik_complete_animations();
    f.double_roundtrip(id);
    assert_ne!(
        f.synoik().layout.focus().unwrap().id(),
        before,
        "Return activated the selection"
    );
}

/// Alt-Tab titles the *selected* window across the whole panel; Super-Tab labels every item.
///
/// The two popups put their label in different places. `AppIcon` adds its label as a child of the
/// item (`altTab.js:682-686`), so an app name sits under its own icon. `WindowIcon` does **not**:
/// its label is only handed to `addItem` as the accessible `label_actor` (`switcherPopup.js:460`),
/// and `WindowSwitcher` owns one `St.Label` for the whole list (`altTab.js:1066-1070`) whose text
/// follows the selection (`highlight`, `:1130-1134`).
///
/// This is not cosmetics. A window title is arbitrary client text, often far wider than the 128px
/// preview it belongs to; per-item titles either overflow their slot or force every slot as wide
/// as the longest title. Asserting the *geometry* is what pins it: the item stays square and the
/// title band spans the panel.
#[test]
fn alt_tab_titles_the_selection_across_the_panel_and_super_tab_labels_each_item() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.One.desktop",
            "One",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    f.synoik_complete_animations();

    const KEY_LEFTALT: u32 = 56;
    const KEY_LEFTMETA: u32 = 125;

    f.key_press(KEY_LEFTALT);
    f.synoik_state()
        .do_action(Action::SwitchWindows { backward: false }, false);
    let item = f.synoik().switcher.item_rect(0).expect("an item");
    let footer = f
        .synoik()
        .switcher
        .footer_rect()
        .expect("the window switcher has a title band");
    let panel = f.synoik().switcher.panel_rect().expect("a panel");
    f.key_release(KEY_LEFTALT);

    // The preview slot carries no label strip of its own: it is exactly the 128px preview plus
    // `.item-box`'s padding. A per-item title would inflate the content height and, through
    // `squareItems`, the whole slot — so this one number fails the moment the label moves back in.
    use crate::ui::switcher::window_switcher::WINDOW_PREVIEW_SIZE;
    use crate::ui::switcher::ITEM_PADDING;
    let side = WINDOW_PREVIEW_SIZE + ITEM_PADDING * 2.;
    assert_eq!(
        item.size,
        smithay::utils::Size::from((side, side)),
        "a window switcher item is the bare preview square"
    );
    // ...and the title band is the panel's, not the item's.
    assert!(
        footer.size.w > item.size.w,
        "the title spans the panel ({}) rather than one 128px slot ({})",
        footer.size.w,
        item.size.w
    );
    assert!(
        footer.loc.y >= item.loc.y + item.size.h,
        "the title sits below the row, not inside it"
    );
    assert!(
        footer.loc.y + footer.size.h <= panel.loc.y + panel.size.h,
        "and inside the panel"
    );

    // The app switcher is the other arrangement: label per item, no band.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    let app_footer = f.synoik().switcher.footer_rect();
    f.key_release(KEY_LEFTMETA);

    assert!(
        app_footer.is_none(),
        "an app switcher has no panel-wide title band"
    );
}

/// Alt-Tab is workspace-local and Super-Tab is not — the two schemas' opposed defaults, driven
/// through the real popups.
///
/// `org.gnome.shell.app-switcher current-workspace-only` defaults **false** while
/// `org.gnome.shell.window-switcher current-workspace-only` defaults **true**. Reading either
/// from the wrong schema looks completely fine on a one-workspace machine, which is why this test
/// puts a window on a second workspace before asking.
#[test]
fn alt_tab_stays_on_this_workspace_and_super_tab_does_not() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");

    // A second window on the next workspace down.
    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    // Sanity: the two really are on different workspaces, so the assertions below can differ.
    assert_eq!(
        f.synoik().switcher_tab_list(false).len(),
        2,
        "both windows exist when nothing is filtered"
    );

    const KEY_LEFTALT: u32 = 56;
    const KEY_LEFTMETA: u32 = 125;

    f.key_press(KEY_LEFTALT);
    f.synoik_state()
        .do_action(Action::SwitchWindows { backward: false }, false);
    let alt_tab_items = f.synoik().switcher.item_count();
    f.key_release(KEY_LEFTALT);

    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    let super_tab_items = f.synoik().switcher.item_count();
    f.key_release(KEY_LEFTMETA);

    assert_eq!(
        alt_tab_items,
        Some(1),
        "stock Alt-Tab shows only this workspace's window"
    );
    assert_eq!(super_tab_items, Some(2), "stock Super-Tab spans workspaces");
}

/// Two apps with a fake catalog, the shape most switcher tests want.
fn switcher_apps(f: &mut Fixture) {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
            AppEntry::fake("org.example.Three.desktop", "Three"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );
}

/// Let the clock run without ending the session, so a timer inside the popup can fire.
fn switcher_rest(f: &mut Fixture, by: Duration) {
    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + by);
    f.synoik().advance_animations();
}

/// Rest long enough for the workspace preview to settle on the current selection.
///
/// Two beats, not one: the dwell is armed by the frame that *notices* the selection moved and
/// fires on a later one, so a single advance can only ever arm it. A live session queues a frame
/// on the keypress itself, which is what makes one beat's worth of latency invisible there.
///
/// The first beat is a whole [`POPUP_DELAY`](crate::ui::switcher::POPUP_DELAY) because a popup
/// still inside that delay previews nothing at all, so a shorter beat would arm nothing and the
/// second would find no dwell to fire.
fn settle_workspace_preview(f: &mut Fixture) {
    switcher_rest(f, crate::ui::switcher::POPUP_DELAY);
    switcher_rest(f, crate::ui::switcher::WORKSPACE_PREVIEW_DELAY * 2);
    f.synoik_complete_animations();
}

/// The active workspace's floating stack, topmost first.
fn stack_order(f: &mut Fixture) -> Vec<crate::window::mapped::MappedId> {
    f.synoik()
        .layout
        .active_workspace()
        .unwrap()
        .windows()
        .map(|w| w.id())
        .collect()
}

/// Committing on an app brings **all** of its windows forward, not just the one that takes focus.
///
/// `shell_app_activate_window` (`shell-app.c:413-425`) raises every window of the app on the
/// target's workspace before activating the target, so switching to a two-window editor does not
/// leave its second window buried under the app you just left. Ours only ever raised the one
/// window, which looks right in the common one-window case and is why it went unnoticed.
///
/// The reverse-order raise is half the contract and is asserted here too: the group arrives with
/// its own relative stacking intact rather than re-sorted, so `a` stays under `b`.
#[test]
fn super_tab_brings_the_whole_app_forward_not_just_its_focused_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    let c = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    assert_eq!(
        stack_order(&mut f),
        vec![c, b, a],
        "sanity: \"Two\" is on top, with \"One\"'s two windows buried under it"
    );

    // Super-Tab opens on item 1 — "One", the app we are not in — and releasing commits to it.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);

    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        b,
        "focus goes to the app's most recently used window"
    );
    assert_eq!(
        stack_order(&mut f),
        vec![b, a, c],
        "and its other window comes with it, above the app we left"
    );
}

/// DIVERGENCE: while a switcher is up, the windows it would raise are drawn on top — and put back
/// untouched when you abandon it.
///
/// GNOME previews nothing outside the cycler. The preview is deliberately *draw order only*: it
/// must never restack, so Escape is free. This asserts both halves, because a preview implemented
/// by really raising would pass the first one and silently destroy the second.
#[test]
fn a_switcher_shows_what_it_would_raise_and_puts_it_back_on_escape() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.synoik().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    let c = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    // `preview_raised` speaks the layout's window handles, so translate back to the ids the rest
    // of the test names windows by.
    let previewed = |f: &mut Fixture| {
        let raised: Vec<_> = f
            .synoik()
            .layout
            .active_workspace()
            .unwrap()
            .preview_raised()
            .to_vec();
        raised
            .iter()
            .filter_map(|w| {
                f.synoik()
                    .layout
                    .windows()
                    .find(|(_, m)| crate::layout::LayoutElement::id(*m) == w)
                    .map(|(_, m)| m.id())
            })
            .collect::<Vec<_>>()
    };

    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    assert!(
        previewed(&mut f).is_empty(),
        "nothing is previewed while the popup is still inside its open delay — a tap that shows \
         no UI must not shuffle the screen either"
    );

    switcher_rest(&mut f, crate::ui::switcher::POPUP_DELAY * 2);
    assert_eq!(
        previewed(&mut f),
        vec![b, a],
        "once it is on screen, the whole app it would commit to is drawn on top, in the order \
         the commit would leave it"
    );
    assert_eq!(
        stack_order(&mut f),
        vec![c, b, a],
        "but the real stack has not moved: the preview is draw order, not a raise"
    );

    // Escape abandons it.
    tap(&mut f, KEY_ESC);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);

    assert!(previewed(&mut f).is_empty(), "the preview is dropped");
    assert_eq!(
        stack_order(&mut f),
        vec![c, b, a],
        "and nothing about the stack changed"
    );
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        c,
        "nor about focus"
    );
}

/// DIVERGENCE: a selection whose window lives on another workspace takes you there while you
/// hold the switcher, and brings you back if you let it go.
///
/// The trip is on a dwell, so tabbing *through* an app that lives elsewhere costs nothing. Both
/// the going and the coming back are asserted: a preview that switched workspaces without
/// recording where it came from would pass the first half and strand you on the second.
#[test]
fn the_switcher_previews_another_workspace_and_gives_it_back() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);

    let client = f.add_client();

    // One app per workspace, walking *down* as each one is mapped: the strip only grows a
    // workspace once the one before it is occupied, so mapping first and moving second is the
    // only order that reliably reaches three of them.
    //
    // It also puts the MRU order where this test needs it. The app switcher's item order is pure
    // MRU by focus timestamp and switching workspaces does not re-stamp the window it lands on,
    // so mapping "One" last is what makes it unambiguously the app we are in, with "Two" one
    // workspace away and "Three" two.
    map_window_for_app(&mut f, client, "org.example.Three");
    let far = f.synoik().layout.focus().unwrap().id();

    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    map_window_for_app(&mut f, client, "org.example.Two");
    let there = f.synoik().layout.focus().unwrap().id();

    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    map_window_for_app(&mut f, client, "org.example.One");
    let here = f.synoik().layout.focus().unwrap().id();
    f.synoik_complete_animations();

    let ws = |f: &mut Fixture| {
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx()
    };
    assert_eq!(ws(&mut f), 2, "sanity: we start where \"One\" is");

    // Leg 1: open on "Two" and pass straight through — too quick to have gone anywhere.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    switcher_rest(&mut f, crate::ui::switcher::POPUP_DELAY);
    assert_eq!(
        ws(&mut f),
        2,
        "the dwell has not elapsed, so nothing has moved yet"
    );

    // Rest on it, and the screen follows the selection.
    settle_workspace_preview(&mut f);
    assert_eq!(
        ws(&mut f),
        1,
        "resting on a window that lives elsewhere shows you where it lives"
    );

    // Escape: we are owed the workspace we started on.
    tap(&mut f, KEY_ESC);
    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(ws(&mut f), 2, "abandoning it brings us back");
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        here,
        "with the focus we had"
    );

    // Leg 2: the same trip, but tabbing *on* to a third app two workspaces away, and committed
    // this time. Two hops, because one is what makes the bookmark assertion below vacuous — with
    // a single hop the workspace we came from and the workspace the session started on are the
    // same one, and a preview that trampled the bookmark would pass anyway.
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    settle_workspace_preview(&mut f);
    assert_eq!(ws(&mut f), 1, "the first stop");

    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    settle_workspace_preview(&mut f);
    assert_eq!(ws(&mut f), 0, "and on to the second");

    f.key_release(KEY_LEFTMETA);
    f.synoik_complete_animations();
    f.double_roundtrip(client);

    assert_eq!(ws(&mut f), 0, "committing keeps the workspace we ended on");
    assert_eq!(
        f.synoik().layout.focus().unwrap().id(),
        far,
        "and focuses the window we picked"
    );
    let _ = there;

    // "The previous workspace" is where the *session* started, not the last stop the preview made
    // on the way. Letting `activate_workspace` write the bookmark as the preview went would leave
    // this on workspace 1, which is somewhere the user only ever saw out of the corner of an eye.
    f.synoik_state()
        .do_action(Action::FocusWorkspacePrevious, false);
    f.synoik_complete_animations();
    assert_eq!(
        ws(&mut f),
        2,
        "the bookmark is the workspace the session started on, not a stop it passed through"
    );
}

/// A polkit request goes from polkitd to a prompt, takes a real password off the keyboard, and its
/// verdict is polkitd's alone.
///
/// This drives the entry points a live session drives — `State::on_polkit_msg` for the agent's
/// side, synthetic key events for the user's — so it fails for the wiring mistakes it exists to
/// catch: a dialog that never takes keyboard focus, keys that reach a window behind it, a password
/// that is not what gets sent, or a cancel that reaches polkitd as a failure instead of a
/// dismissal.
#[test]
fn a_polkit_request_becomes_a_prompt_and_polkitd_decides() {
    use crate::dbus::polkit_agent::{BeginRequest, PolkitRequest, PolkitToSynoik};
    use crate::synoik::KeyboardFocus;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Stand in for the agent, so what the dialog *sends* can be read back rather than assumed.
    let (to_agent, from_dialog) = async_channel::unbounded();
    f.synoik().polkit_requests = Some(to_agent);
    let sent = move || from_dialog.try_recv().ok();

    let begin = |user: &str| {
        PolkitToSynoik::Begin(Box::new(BeginRequest {
            action_id: "org.freedesktop.test.frobnicate".to_owned(),
            message: "Authentication is required to frobnicate".to_owned(),
            user_name: user.to_owned(),
            passwordless: false,
            avatar: None,
        }))
    };

    // polkitd asks. Nothing is on screen yet — PAM has not said it wants anything.
    f.synoik_state().on_polkit_msg(begin("root"));
    assert!(
        !f.synoik().polkit_is_open(),
        "the dialog must not appear before PAM asks"
    );
    assert!(
        matches!(sent(), Some(PolkitRequest::Initiate { .. })),
        "but the conversation has been started"
    );

    // PAM asks, and now it is on screen and holds the keyboard.
    f.synoik_state().on_polkit_msg(PolkitToSynoik::Request {
        prompt: "Password:".to_owned(),
        echo_on: false,
    });
    f.synoik().polkit_ui.settle();
    assert!(f.synoik().polkit_is_open());
    f.synoik_state().refresh_and_flush_clients();
    assert!(
        matches!(f.synoik().keyboard_focus, KeyboardFocus::PolkitDialog),
        "the dialog is modal, so it owns the keyboard: {:?}",
        f.synoik().keyboard_focus,
    );

    // A real password off a real keyboard, masked on the way in.
    tap(&mut f, KEY_A);
    tap(&mut f, KEY_T);
    assert_eq!(f.synoik().polkit_dialog.entry_display(), "\u{25cf}\u{25cf}");
    tap(&mut f, KEY_BACKSPACE);
    tap(&mut f, KEY_E);

    tap(&mut f, KEY_ENTER);
    match sent() {
        Some(PolkitRequest::Respond(response)) => {
            assert_eq!(response, "ae", "what was typed is what is sent")
        }
        other => panic!("expected a response, got {other:?}"),
    }
    assert_eq!(
        f.synoik().polkit_dialog.entry_display(),
        "",
        "the buffer does not outlive the answer"
    );

    // PAM refuses. The dialog stays up and another conversation starts.
    f.synoik_state()
        .on_polkit_msg(PolkitToSynoik::Completed(false));
    assert!(f.synoik().polkit_is_open(), "a refusal is not the end");
    assert!(matches!(sent(), Some(PolkitRequest::Initiate { .. })));

    // Escape is a dismissal, which is a different answer from a failure: it tells the program that
    // asked to stop, rather than to try again.
    tap(&mut f, KEY_ESC);
    assert!(
        matches!(sent(), Some(PolkitRequest::Done { dismissed: true })),
        "Escape must reach polkitd as a dismissal"
    );
    f.synoik().polkit_ui.settle();
    assert!(!f.synoik().polkit_is_open());
}

/// A request that arrives while the screen is locked waits for it, rather than drawing a password
/// box over the shield or answering polkitd without asking anyone.
///
/// GNOME defers it to the next session-mode change (`polkitAgent.js:439-450`). The failure this
/// pins is not cosmetic: a prompt stacked on the lock screen is a second password entry on a locked
/// machine, and there is no way for the person looking at it to tell which one is which.
#[test]
fn a_request_that_arrives_locked_waits_for_the_unlock() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::dbus::polkit_agent::{BeginRequest, PolkitRequest, PolkitToSynoik};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (to_agent, from_dialog) = async_channel::unbounded();
    f.synoik().polkit_requests = Some(to_agent);
    let sent = move || from_dialog.try_recv().ok();

    // Lock, with a live verifier behind it.
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.synoik().screen_shield.is_locked());

    f.synoik_state()
        .on_polkit_msg(PolkitToSynoik::Begin(Box::new(BeginRequest {
            action_id: "org.freedesktop.test.frobnicate".to_owned(),
            message: "Authentication is required to frobnicate".to_owned(),
            user_name: "root".to_owned(),
            passwordless: false,
            avatar: None,
        })));
    assert!(!f.synoik().polkit_is_open(), "not over a lock screen");
    assert!(
        sent().is_none(),
        "and no conversation is started behind it either"
    );
    assert!(
        f.synoik().polkit_deferred.is_some(),
        "the request is held, not dropped -- polkitd is still waiting on it"
    );

    // gdm accepts; the shield goes, and the held request gets its turn on the next refresh (which
    // a live compositor runs constantly).
    f.synoik_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.synoik().screen_shield.is_active());
    f.synoik_state().refresh_and_flush_clients();
    assert!(
        f.synoik().polkit_deferred.is_none(),
        "the held request has been run"
    );
    assert!(
        matches!(sent(), Some(PolkitRequest::Initiate { .. })),
        "and its conversation starts now"
    );
}

/// The portal's window list is built from the real window and app models, with the fields its
/// chooser reads (`GetWindows`, `introspect.js:135-182`).
///
/// This drives `State::on_introspect_msg` — the entry point the bus drives — so it fails for the
/// mistakes that matter: an `app-id` that is the raw Wayland id instead of the resolved desktop id
/// (which is why the chooser used to have no icons), a focus flag that never moves, or a window on
/// an inactive workspace reported as showing.
#[test]
fn the_portal_window_list_carries_what_its_chooser_reads() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::dbus::gnome_shell_introspect::{IntrospectToSynoik, SynoikToIntrospect};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.synoik().app_system = AppSystem::with_parts(
        // The desktop id deliberately is *not* `{app_id}.desktop`: that is the only shape that
        // tells the resolved id apart from the string concatenation this used to do.
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "org.example.Editor.desktop",
            "Editor",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );

    map_window_for_app(&mut f, client, "a");
    f.synoik().sync_running_apps();

    let (tx, rx) = async_channel::unbounded();
    f.synoik_state()
        .on_introspect_msg(&tx, IntrospectToSynoik::GetWindows);
    let SynoikToIntrospect::Windows(windows) = rx.try_recv().expect("a reply") else {
        panic!("wrong reply");
    };

    // The list also carries the dynamic-cast pseudo-window, which is not a window at all.
    #[cfg(feature = "xdp-gnome-screencast")]
    {
        let synthetic = windows
            .values()
            .find(|p| p.wm_class.is_none())
            .expect("the dynamic-cast entry");
        assert_eq!(
            synthetic.title.as_deref(),
            Some(crate::synoik::DYNAMIC_CAST_TARGET_LABEL),
            "the picker's label says what it does, not who made it"
        );
        assert_eq!(
            synthetic.title.as_deref(),
            Some("Dynamic Target"),
            "and the label itself is pinned: it is user-visible product text"
        );
    }

    let props = windows
        .values()
        .find(|p| p.wm_class.as_deref() == Some("a"))
        .expect("the mapped window");
    assert_eq!(
        props.app_id, "org.example.Editor.desktop",
        "the resolved desktop id, not the Wayland app id"
    );
    assert_eq!(
        props.wm_class.as_deref(),
        Some("a"),
        "and the raw id beside it"
    );
    assert!(props.has_focus, "the only window has focus");
    assert!(!props.is_hidden, "and it is on the active workspace");
    assert_eq!((props.width, props.height), (100, 100));

    // ...and the app list agrees about which app is active.
    f.synoik_state()
        .on_introspect_msg(&tx, IntrospectToSynoik::GetRunningApplications);
    let SynoikToIntrospect::RunningApplications(apps) = rx.try_recv().expect("a reply") else {
        panic!("wrong reply");
    };
    assert_eq!(
        apps.get("org.example.Editor.desktop")
            .and_then(|a| a.active_on_seats.as_deref()),
        Some(&[String::from("seat0")][..]),
        "the focused app is active on seat0"
    );
}

/// `SelectArea` answers its caller on every exit, not just the happy one.
///
/// A D-Bus caller that is not answered does not fail — it *hangs* until its timeout, with the
/// compositor looking perfectly healthy. Two exits are not the happy one: the picker refusing to
/// open (locked screen, or already up), and the user dismissing it.
///
/// Open the screenshot picker with **no Vulkan device**, laid out and clickable.
///
/// The two things that normally need a renderer are a frozen screen and a panel layout, and neither
/// has to: a `ScreenshotNeutral` is plain `MemoryBuffer` pixels, and `PanelLayout` is arithmetic
/// over measured captions. So this hand-builds a neutral per output, hands it to the real
/// `State::open_screenshot_ui_with`, and installs the layout the bake would otherwise produce.
/// Everything the tests then drive — the hit test, `activate`, the D-Bus contract, the cancellation
/// rules — is the production path.
///
/// What it does *not* get is pixels: with no texture the panel draws nothing. Any claim about how
/// something **looks** belongs in `src/tests/vulkan_render.rs`.
fn open_picker_headless(f: &mut Fixture) {
    use smithay::backend::allocator::Fourcc;
    use smithay::utils::{Size, Transform};

    use crate::render_helpers::memory::MemoryBuffer;
    use crate::render_helpers::RenderTarget;
    use crate::ui::screenshot_ui::{CaptionMetrics, ScreenshotNeutral};

    let outputs: Vec<_> = f.synoik().global_space.outputs().cloned().collect();
    let neutrals = outputs
        .into_iter()
        .map(|output| {
            let mode = output.current_mode().unwrap();
            let size = output.current_transform().transform_size(mode.size);
            let scale = output.current_scale().fractional_scale();
            let pixels = vec![0u8; (size.w * size.h * 4) as usize];
            let neutrals = std::array::from_fn(|_| ScreenshotNeutral {
                screen: Some(MemoryBuffer::new(
                    pixels.clone(),
                    Fourcc::Abgr8888,
                    Size::from((size.w, size.h)),
                    scale,
                    Transform::Normal,
                )),
                pointer: None,
            });
            let _: [ScreenshotNeutral; RenderTarget::COUNT] = neutrals;
            (output, neutrals)
        })
        .collect();

    // Window mode picks from frozen per-window copies, which are `MemoryBuffer` pixels too, so the
    // corpus can supply those as well rather than leave the Window button permanently insensitive.
    let outputs: Vec<_> = f.synoik().global_space.outputs().cloned().collect();
    let window_shots = outputs
        .into_iter()
        .map(|output| {
            let scale = output.current_scale().fractional_scale();
            let shots = f
                .synoik()
                .layout
                .active_workspace_windows_for_output(&output)
                .into_iter()
                .map(|(mapped, rect)| {
                    let size: Size<i32, smithay::utils::Physical> =
                        rect.size.to_physical_precise_round(scale);
                    let (w, h) = (size.w.max(1), size.h.max(1));
                    let neutral = MemoryBuffer::new(
                        vec![0u8; (w * h * 4) as usize],
                        Fourcc::Abgr8888,
                        Size::from((w, h)),
                        scale,
                        Transform::Normal,
                    );
                    crate::ui::screenshot_ui::WindowShot::new(mapped.id().get(), rect, neutral)
                })
                .collect::<Vec<_>>();
            (output, shots)
        })
        .collect();

    f.synoik_state()
        .open_screenshot_ui_with(neutrals, window_shots, None);
    assert!(
        f.synoik().screenshot_ui.is_open(),
        "the picker must open from hand-built neutrals; a renderer is not what opens it"
    );
    f.synoik()
        .screenshot_ui
        .lay_out_panels(CaptionMetrics::TEST);
}

/// Click a panel control by the rect the layout published for it, on output 1.
fn click_picker_control(
    f: &mut Fixture,
    rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>,
) -> crate::ui::screenshot_ui::PointerUp {
    use smithay::utils::{Logical, Point};

    let output = f.synoik_output(1);
    let panel = f
        .synoik()
        .screenshot_ui
        .panel_rect(&output)
        .expect("an open, laid-out picker has a panel rect");
    let scale = output.current_scale().fractional_scale();
    let point =
        Point::<f64, Logical>::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
            .to_physical(scale)
            .to_i32_round::<i32>()
            + panel.loc;

    let ui = &mut f.synoik_state().synoik.screenshot_ui;
    ui.pointer_motion(point, None);
    assert!(ui.pointer_down(output, point, None, false).is_some());
    ui.pointer_up(None)
        .expect("the release must land on a control")
}

/// **Our divergence, the delay.** Arming hands the whole capture to a timer and closes the picker,
/// so the two things that must not happen at that moment are answering the D-Bus caller — it has
/// been given nothing yet — and losing the capture with the picker that armed it.
#[test]
fn arming_a_delayed_capture_does_not_answer_its_caller() {
    use crate::ui::screenshot_ui::PointerUp;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::bounded(1);
    f.synoik().interactive_screenshot_reply = Some(tx);
    open_picker_headless(&mut f);

    let output = f.synoik_output(1);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    assert_eq!(
        f.synoik().screenshot_ui.delay(),
        None,
        "the delay starts off"
    );

    // Off -> 3s -> 10s -> off. The layout does not move as it cycles (the number replaces a glyph
    // inside the same circle), so one read of it is enough.
    click_picker_control(&mut f, layout.delay);
    assert_eq!(
        f.synoik().screenshot_ui.delay(),
        Some(Duration::from_secs(3)),
        "one click must arm the first stop"
    );
    click_picker_control(&mut f, layout.delay);
    assert_eq!(
        f.synoik().screenshot_ui.delay(),
        Some(Duration::from_secs(10))
    );
    click_picker_control(&mut f, layout.delay);
    assert_eq!(
        f.synoik().screenshot_ui.delay(),
        None,
        "the third click must wrap back to off"
    );

    // Back to 3s, then fire the shutter.
    click_picker_control(&mut f, layout.delay);
    assert_eq!(
        click_picker_control(&mut f, layout.capture),
        PointerUp::Capture
    );
    f.synoik_state()
        .handle_screenshot_ui_pointer_up(PointerUp::Capture);

    assert!(
        !f.synoik().screenshot_ui.is_open(),
        "arming must dismiss the picker — the delay exists to get the shell out of the shot"
    );
    assert!(
        f.synoik().pending_capture.is_some(),
        "the capture must survive the picker that armed it"
    );
    assert_eq!(
        rx.try_recv(),
        Err(async_channel::TryRecvError::Empty),
        "an armed capture has not failed; answering `None` here would tell the portal it was \
         cancelled while a shot is still coming"
    );

    // Escape has no bind to reach with the picker gone, so `cancel_pending_capture` is its route —
    // and cancelling *is* the dismissal the caller was spared above.
    assert!(f.synoik_state().cancel_pending_capture());
    assert!(f.synoik().pending_capture.is_none());
    assert_eq!(
        rx.try_recv(),
        Ok(None),
        "a cancelled countdown must answer the caller it was holding"
    );
}

/// A lock landing mid-countdown must take the capture with it: the delay was armed against a screen
/// the user could see, and firing into a lock screen would capture what the lock exists to hide.
#[test]
fn a_lock_mid_countdown_cancels_the_delayed_capture() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;
    use crate::ui::screenshot_ui::PointerUp;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::bounded(1);
    f.synoik().interactive_screenshot_reply = Some(tx);
    open_picker_headless(&mut f);

    let output = f.synoik_output(1);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_picker_control(&mut f, layout.delay);
    click_picker_control(&mut f, layout.capture);
    f.synoik_state()
        .handle_screenshot_ui_pointer_up(PointerUp::Capture);
    assert!(f.synoik().pending_capture.is_some());

    // A tick before the lock keeps counting — otherwise this would pass for the wrong reason.
    assert!(matches!(
        f.synoik_state().tick_pending_capture(),
        calloop::timer::TimeoutAction::ToDuration(_)
    ));
    assert!(f.synoik().pending_capture.is_some());

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    assert!(matches!(
        f.synoik_state().tick_pending_capture(),
        calloop::timer::TimeoutAction::Drop
    ));
    assert!(
        f.synoik().pending_capture.is_none(),
        "the locked screen must not be shot"
    );
    assert_eq!(
        rx.try_recv(),
        Ok(None),
        "and its caller must be told, not left waiting"
    );
}

/// Drive a press/drag/release on the area selector, in output-local physical coords.
fn drag_selection(f: &mut Fixture, from: (i32, i32), to: (i32, i32)) {
    use smithay::utils::{Physical, Point};

    let output = f.synoik_output(1);
    let from = Point::<i32, Physical>::from(from);
    f.synoik_state()
        .handle_screenshot_ui_pointer_down(output, from, None, false);
    // From wherever the press left the pointer — a resize warps it onto the side it grabbed.
    f.synoik_state()
        .handle_screenshot_ui_motion(Point::from(to), None);
    f.synoik_state().synoik.screenshot_ui.pointer_up(None);
}

fn selection_of(f: &mut Fixture) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
    f.synoik()
        .screenshot_ui
        .selection_rect_global()
        .expect("an open picker in Selection mode has a selection")
}

/// A press on a corner handle resizes from it, holding the opposite corner still.
#[test]
fn dragging_a_handle_resizes_from_the_opposite_corner() {
    use smithay::utils::{Point, Rectangle, Size};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);

    // A known rectangle to grab: 400x400 at (400, 400).
    drag_selection(&mut f, (400, 400), (799, 799));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((400, 400)), Size::from((400, 400)))
    );

    // Grab the top-left handle and pull it out to (200, 200). The bottom-right must not move.
    drag_selection(&mut f, (400, 400), (200, 200));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((200, 200)), Size::from((600, 600))),
        "the corner opposite the one dragged is the one that must stay put"
    );

    // Grab the bottom edge from just outside it — an edge is grabbable from up to 10px out — and
    // pull down. Only the height changes: a pure edge drag pins the other axis.
    drag_selection(&mut f, (500, 805), (500, 900));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((200, 200)), Size::from((600, 701))),
        "an edge drag must not move the axis it did not grab"
    );
}

/// Dragging a handle past the opposite side flips which handle it is, rather than collapsing the
/// rectangle or inverting it (`js/ui/screenshot.js:672-709`).
#[test]
fn a_handle_dragged_past_the_far_side_flips() {
    use smithay::input::pointer::CursorIcon;
    use smithay::utils::{Point, Rectangle, Size};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);
    drag_selection(&mut f, (400, 400), (599, 599));

    let output = f.synoik_output(1);
    // The pointer arrives on the handle before it presses, as a real one does.
    f.synoik_state()
        .handle_screenshot_ui_motion(Point::from((400, 400)), None);
    assert_eq!(
        f.synoik().screenshot_ui.cursor_icon(),
        CursorIcon::NwResize,
        "hovering the handle already advertises the grab"
    );
    // Grab it...
    f.synoik_state().handle_screenshot_ui_pointer_down(
        output,
        Point::from((400, 400)),
        None,
        false,
    );
    assert_eq!(
        f.synoik().screenshot_ui.cursor_icon(),
        CursorIcon::NwResize,
        "and holding it keeps it, with no motion in between"
    );

    // ...and drag it beyond the bottom-right one.
    f.synoik_state()
        .handle_screenshot_ui_motion(Point::from((800, 800)), None);
    assert_eq!(
        f.synoik().screenshot_ui.cursor_icon(),
        CursorIcon::SeResize,
        "past the far corner it is the south-east handle now, not a north-west one dragging \
         backwards"
    );
    f.synoik_state().synoik.screenshot_ui.pointer_up(None);

    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((599, 599)), Size::from((202, 202))),
        "the rectangle stays a rectangle, hinged on the corner that was standing still"
    );
}

/// A press inside the selection moves the whole thing, and it cannot be pushed off the output.
#[test]
fn dragging_inside_the_selection_moves_it() {
    use smithay::utils::{Point, Rectangle, Size};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);
    drag_selection(&mut f, (400, 400), (599, 599));

    drag_selection(&mut f, (500, 500), (700, 500));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((600, 400)), Size::from((200, 200))),
        "moving keeps the size and follows the pointer"
    );

    // Shove it hard into the left edge: it stops there with its size intact.
    drag_selection(&mut f, (700, 500), (-5000, 500));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((0, 400)), Size::from((200, 200))),
        "a move clamps to the output rather than sliding off it"
    );
}

/// A press outside the selection still drags a new one — the behaviour the handles must not eat.
#[test]
fn pressing_outside_the_selection_still_starts_a_new_one() {
    use smithay::utils::{Point, Rectangle, Size};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);
    drag_selection(&mut f, (400, 400), (599, 599));

    // Well clear of the rectangle and its grab bands.
    drag_selection(&mut f, (1000, 1000), (1199, 1039));
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((1000, 1000)), Size::from((200, 40)))
    );
}

/// A trip through Screen mode must not destroy the rectangle you dragged.
///
/// Ours reuses `selection` for Screen mode — it widens it to the whole output so the capture path
/// needs no special case — which quietly overwrote the area. Nothing can do that in GNOME:
/// `_areaSelector` keeps its own geometry and Screen mode draws `_screenSelectors`, a different
/// widget entirely (`js/ui/screenshot.js:1780-1800`).
#[test]
fn the_area_selection_survives_a_trip_through_screen_mode() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::ui::screenshot_ui::CaptureType;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);

    drag_selection(&mut f, (300, 300), (699, 599));
    let area = Rectangle::new(Point::from((300, 300)), Size::from((400, 300)));
    assert_eq!(selection_of(&mut f), area);

    f.synoik_state()
        .synoik
        .screenshot_ui
        .set_capture_type(CaptureType::Screen);
    assert_eq!(
        selection_of(&mut f),
        Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080))),
        "Screen mode still captures the whole output"
    );

    f.synoik_state()
        .synoik
        .screenshot_ui
        .set_capture_type(CaptureType::Selection);
    assert_eq!(
        selection_of(&mut f),
        area,
        "and coming back hands the rectangle back, rather than leaving the whole screen selected"
    );
}

/// The picker comes back the way you left it, for as long as the session lasts.
///
/// gnome-shell's `ScreenshotUI` is a singleton built at startup that merely hides on close
/// (`js/ui/screenshot.js:1727`), so its controls still hold last time's state when it opens again:
/// `_finishClosing` touches neither the capture type nor the Show Pointer toggle, and
/// `AreaSelector.reset()` (`:304`) explicitly preserves the rectangle. What it *does* reset is
/// shot-vs-cast (`_shotButton.checked = true`, `:1739`), so the picker never comes back armed to
/// record. None of it is stored: GNOME has no GSettings key for any of this, so a restart starts
/// over — and neither does ours.
#[test]
fn the_picker_remembers_its_controls_across_opens() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::ui::screenshot_ui::{CaptureMode, CaptureType};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    open_picker_headless(&mut f);

    assert!(
        !f.synoik().screenshot_ui.show_pointer(),
        "GNOME builds the button unchecked (`screenshot.js:1417-1421`), so a first picker hides the \
         pointer"
    );

    drag_selection(&mut f, (300, 300), (699, 599));
    let area = Rectangle::new(Point::from((300, 300)), Size::from((400, 300)));
    f.synoik().screenshot_ui.toggle_pointer();
    f.synoik().screenshot_ui.set_mode(CaptureMode::Cast);
    f.synoik()
        .screenshot_ui
        .set_capture_type(CaptureType::Screen);

    f.synoik().close_screenshot_ui();
    open_picker_headless(&mut f);

    assert!(
        f.synoik().screenshot_ui.show_pointer(),
        "the pointer toggle is the user's answer to a question we should only ask once"
    );
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Screen,
        "the capture type survives the close, as GNOME's checked button does"
    );
    assert_eq!(
        f.synoik().screenshot_ui.mode(),
        CaptureMode::Shot,
        "but shot-vs-cast does not: GNOME resets it on every close"
    );

    f.synoik()
        .screenshot_ui
        .set_capture_type(CaptureType::Selection);
    assert_eq!(
        selection_of(&mut f),
        area,
        "and the area is still the one dragged before the close — closing in Screen mode must not \
         remember the whole output as the selection"
    );
}

/// Window mode is not remembered into a picker with nothing to pick.
///
/// GNOME's own guard is at open, not at close: `_syncWindowButtonSensitivity` is followed by
/// `if (!this._windowButton.reactive) this._selectionButton.checked = true`
/// (`js/ui/screenshot.js:1662-1664`). Restoring the type through `set_capture_type` inherits that
/// refusal rather than restating it — but only if the restore actually goes through it.
#[test]
fn a_remembered_window_mode_falls_back_when_there_are_no_windows() {
    use crate::ui::screenshot_ui::CaptureType;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    open_picker_headless(&mut f);
    f.synoik()
        .screenshot_ui
        .set_capture_type(CaptureType::Window);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Window,
        "with a window mapped, Window mode is selectable"
    );
    f.synoik().close_screenshot_ui();

    open_picker_headless(&mut f);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Window,
        "and it is remembered while the window is still there"
    );
    f.synoik().close_screenshot_ui();

    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    open_picker_headless(&mut f);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection,
        "with the window gone, the remembered type falls back rather than arming an insensitive \
         button"
    );
}

/// The crosshair belongs to the area selector, not to the whole picker.
///
/// In GNOME the cursor is set on `_areaSelector` (`js/ui/screenshot.js:448`), so the panel's
/// buttons are siblings that inherit the default, and leaving Selection mode resets it outright
/// (`:1792`). A crosshair everywhere says "click to select an area" over chrome that does nothing
/// of the kind.
#[test]
fn the_crosshair_is_only_over_the_selectable_area() {
    use smithay::input::pointer::CursorIcon;
    use smithay::utils::{Logical, Physical, Point, Rectangle};

    use crate::ui::screenshot_ui::CaptureType;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_focused_window(&mut f, id);

    open_picker_headless(&mut f);
    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();
    let panel = f.synoik().screenshot_ui.panel_rect(&output).unwrap();
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();

    let physical = |r: Rectangle<f64, Logical>| {
        Point::<f64, Logical>::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
            .to_physical(scale)
            .to_i32_round::<i32>()
            + panel.loc
    };
    let move_to = |f: &mut Fixture, p: Point<i32, Physical>| {
        f.synoik_state().handle_screenshot_ui_motion(p, None);
        f.synoik().screenshot_ui.cursor_icon()
    };

    // Over the free screen: this is the selectable area.
    assert_eq!(
        move_to(&mut f, Point::from((40, 40))),
        CursorIcon::Crosshair
    );

    // Over a button.
    assert_eq!(
        move_to(&mut f, physical(layout.capture)),
        CursorIcon::Default,
        "a crosshair over the capture button offers a selection the button will not start"
    );

    // Over the panel's own background, between controls — chrome too, and the case a hit test that
    // only knew about *controls* would get wrong.
    let gap = Point::<f64, Logical>::from((
        layout.shot_cast.loc.x
            + layout.shot_cast.size.w
            + (layout.capture.loc.x - layout.shot_cast.loc.x - layout.shot_cast.size.w) / 2.,
        layout.capture.loc.y + layout.capture.size.h / 2.,
    ));
    assert_eq!(
        f.synoik()
            .screenshot_ui
            .panel_layout(&output)
            .unwrap()
            .control_at(gap),
        None,
        "the sample point must really be between controls, or this proves nothing"
    );
    assert_eq!(
        move_to(
            &mut f,
            gap.to_physical(scale).to_i32_round::<i32>() + panel.loc
        ),
        CursorIcon::Default
    );

    // Screen mode has nothing to drag out, so the crosshair goes even over open screen — and it
    // must go *without* the pointer moving, because a click is what changed the mode.
    let at_40 = Point::from((40, 40));
    move_to(&mut f, at_40);
    click_picker_control(&mut f, layout.type_buttons[1]);
    assert_eq!(f.synoik().screenshot_ui.capture_type(), CaptureType::Screen);
    assert_eq!(
        f.synoik().screenshot_ui.cursor_icon(),
        CursorIcon::Default,
        "switching out of Selection must drop the crosshair where the pointer already is"
    );

    // And Window mode likewise.
    click_picker_control(&mut f, layout.type_buttons[2]);
    assert_eq!(f.synoik().screenshot_ui.capture_type(), CaptureType::Window);
    assert_eq!(move_to(&mut f, at_40), CursorIcon::Default);

    // Back to Selection, and it returns.
    click_picker_control(&mut f, layout.type_buttons[0]);
    assert_eq!(move_to(&mut f, at_40), CursorIcon::Crosshair);
}

/// Cast mode takes Window mode with it, and gives it back. Recording a single window is not
/// something the recorder does, so GNOME greys the button rather than leaving a mode whose capture
/// button would silently do nothing (`_onCastButtonToggled`, `js/ui/screenshot.js:1880-1906`).
#[test]
fn cast_mode_refuses_window_capture() {
    use crate::ui::screenshot_ui::{CaptureMode, CaptureType};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_focused_window(&mut f, id);

    open_picker_headless(&mut f);
    assert_eq!(f.synoik().screenshot_ui.mode(), CaptureMode::Shot);
    assert!(
        f.synoik().screenshot_ui.window_enabled(),
        "there is a window, so Window mode is available in Shot mode"
    );

    let output = f.synoik_output(1);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_picker_control(&mut f, layout.type_buttons[2]);
    assert_eq!(f.synoik().screenshot_ui.capture_type(), CaptureType::Window);

    let cast = crate::ui::widget::Segmented::segment_rect(layout.shot_cast, 1);
    click_picker_control(&mut f, cast);
    assert_eq!(f.synoik().screenshot_ui.mode(), CaptureMode::Cast);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection,
        "cast mode must move off Window rather than leave a mode it will not act on"
    );
    assert!(
        !f.synoik().screenshot_ui.window_enabled(),
        "and it must stay unavailable while cast is checked, window or no window"
    );

    // A click that reaches the insensitive button anyway must still be refused.
    click_picker_control(&mut f, layout.type_buttons[2]);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection
    );

    let shot = crate::ui::widget::Segmented::segment_rect(layout.shot_cast, 0);
    click_picker_control(&mut f, shot);
    assert_eq!(f.synoik().screenshot_ui.mode(), CaptureMode::Shot);
    assert!(
        f.synoik().screenshot_ui.window_enabled(),
        "and switching back must give it up again"
    );
}

/// While an area recording runs, the rest of the screen is shaded so the user can see what is
/// being recorded (`_screencastAreaIndicator`, `js/ui/screenshot.js:1192-1207`). The shade covers
/// exactly the complement of the recorded rect, and — our fail-closed rule — never appears on a
/// capture target, so it cannot end up inside the very recording it describes.
#[test]
fn a_running_area_recording_shades_what_it_leaves_out() {
    use smithay::backend::renderer::element::Element as _;
    use smithay::utils::{Physical, Rectangle};

    use crate::render_helpers::RenderTarget;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let recorded = Rectangle::<i32, Physical>::new((100, 200).into(), (800, 600).into());
    f.synoik().cast_area_indicator.set(output.clone(), recorded);

    let shades = |f: &mut Fixture, target| {
        let mut geo = Vec::new();
        f.synoik()
            .cast_area_indicator
            .push(target, &output, |elem| geo.push(elem.geometry(1.0.into())));
        geo
    };

    for target in [RenderTarget::Screencast, RenderTarget::ScreenCapture] {
        assert!(
            shades(&mut f, target).is_empty(),
            "the shade must not reach {target:?}"
        );
    }

    let geo = shades(&mut f, RenderTarget::Output);
    assert_eq!(geo.len(), 4);
    for shade in &geo {
        assert!(
            shade.intersection(recorded).is_none(),
            "{shade:?} covers part of what is being recorded"
        );
    }
    // Every pixel outside the recorded rect belongs to one of the four.
    let outside = |x, y| {
        let point = Rectangle::<i32, Physical>::new((x, y).into(), (1, 1).into());
        geo.iter().any(|shade| shade.contains_rect(point))
    };
    for (x, y) in [
        (0, 0),
        (1919, 0),
        (0, 1079),
        (1919, 1079),
        (500, 100),
        (50, 500),
    ] {
        assert!(
            outside(x, y),
            "({x}, {y}) is outside the recording, unshaded"
        );
    }

    // And it stops the moment the recording does.
    f.synoik().cast_area_indicator.clear();
    assert!(shades(&mut f, RenderTarget::Output).is_empty());
}

/// Single keys drive the type row and the shot/cast pill, so the picker is usable without the
/// pointer (`vfunc_key_press_event`, `js/ui/screenshot.js:2207-2233`). The insensitive Window
/// button still refuses its key, as its click already does.
#[test]
fn single_keys_pick_the_capture_type_and_mode() {
    use crate::ui::screenshot_ui::{CaptureMode, CaptureType};

    const KEY_W: u32 = 17;
    const KEY_S: u32 = 31;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_focused_window(&mut f, id);

    open_picker_headless(&mut f);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection
    );

    let tap = |f: &mut Fixture, code| {
        f.key_press(code);
        f.key_release(code);
    };

    tap(&mut f, KEY_C);
    assert_eq!(f.synoik().screenshot_ui.capture_type(), CaptureType::Screen);
    tap(&mut f, KEY_W);
    assert_eq!(f.synoik().screenshot_ui.capture_type(), CaptureType::Window);
    tap(&mut f, KEY_S);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection
    );

    tap(&mut f, KEY_V);
    assert_eq!(f.synoik().screenshot_ui.mode(), CaptureMode::Cast);
    tap(&mut f, KEY_W);
    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection,
        "Window is insensitive under cast, and its key must be refused like its button"
    );
    tap(&mut f, KEY_V);
    assert_eq!(f.synoik().screenshot_ui.mode(), CaptureMode::Shot);

    // And none of it leaked out to the compositor underneath.
    assert!(f.synoik().screenshot_ui.is_open());
}

/// The headless corpus has no renderer, so the picker cannot freeze the screen and open here —
/// which makes this fixture exactly the refusal case, driven through the real
/// `State::on_screen_shot_msg`. The dismissal is driven through `Synoik::close_screenshot_ui`, the
/// one seam every dismissal path goes through.
#[test]
fn select_area_always_answers_its_caller() {
    use crate::dbus::gnome_shell_screenshot::ScreenshotToSynoik;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (to_screenshot, _from_niri) = async_channel::unbounded();

    // The picker never opens: the caller is answered anyway rather than left pending.
    let (tx, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_screen_shot_msg(&to_screenshot, ScreenshotToSynoik::SelectArea(tx));
    assert!(
        !f.synoik().screenshot_ui.is_open(),
        "no renderer here, so the picker cannot open"
    );
    // `try_recv`, not a blocking wait: the answer is synchronous, and a test that *hangs* when
    // this regresses is a test nobody can read the output of.
    assert_eq!(
        rx.try_recv(),
        Ok(None),
        "a picker that never opened must still answer"
    );

    // A dismissal reaches the caller too.
    let (tx, rx) = async_channel::bounded(1);
    f.synoik().select_area_reply = Some(tx);
    f.synoik().close_screenshot_ui();
    assert_eq!(rx.try_recv(), Ok(None), "a dismissal must reach the caller");

    // `InteractiveScreenshot` shares both exits, and its dismissal is a `None` URI rather than an
    // error — the portal reads the boolean instead of catching a fault.
    let (tx, rx) = async_channel::bounded(1);
    f.synoik_state()
        .on_screen_shot_msg(&to_screenshot, ScreenshotToSynoik::Interactive(tx));
    assert_eq!(
        rx.try_recv(),
        Ok(None),
        "an interactive picker that never opened must still answer"
    );

    let (tx, rx) = async_channel::bounded(1);
    f.synoik().interactive_screenshot_reply = Some(tx);
    f.synoik().close_screenshot_ui();
    assert_eq!(rx.try_recv(), Ok(None), "and a dismissal must reach it too");

    // NOT covered here: the save/close race on the *confirm* path. `save_screenshot` takes the
    // reply so the close that follows cannot answer it as a dismissal first — but reaching that
    // needs a real capture, and the headless corpus has no renderer. See the port doc.
}

/// The screenshot and recording keys come from `org.gnome.shell.keybindings`, not from niri's
/// config.
///
/// `Action::ToggleScreenRecord` had no default binding anywhere, so before this there was no key
/// that started a recording at all.
///
/// This covers adoption and the mapping, **not the effect**: every one of these actions opens the
/// picker or captures, and both need a renderer to freeze the screen, which the headless corpus
/// does not have.
#[test]
fn the_screenshot_keys_come_from_gnome_settings() {
    use crate::gnome::GnomeKeyAction;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // The four keys are adopted with GNOME's own defaults.
    let adopted: Vec<_> = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter()
        .filter_map(|kb| kb.action.gnome())
        .filter(|action| {
            matches!(
                action,
                GnomeKeyAction::ShowScreenshotUi
                    | GnomeKeyAction::Screenshot
                    | GnomeKeyAction::ScreenshotWindow
                    | GnomeKeyAction::ShowScreenRecordingUi
            )
        })
        .collect();
    assert_eq!(adopted.len(), 4, "all four keys adopted, got {adopted:?}");

    // ...with GNOME's own accelerators, and mapped to the actions those keys mean.
    let screenshot_accels = f
        .synoik()
        .gnome_settings
        .keybindings
        .iter()
        .find(|kb| kb.action.gnome() == Some(GnomeKeyAction::Screenshot))
        .map(|kb| kb.accels.len())
        .expect("adopted");
    assert_eq!(
        screenshot_accels, 1,
        "<Shift>Print, from GNOME's own default — plain Print opens the picker"
    );

    use crate::input::action_for_gnome;
    assert!(matches!(
        action_for_gnome(GnomeKeyAction::ShowScreenshotUi),
        Some(Action::Screenshot(None))
    ));
    assert!(matches!(
        action_for_gnome(GnomeKeyAction::Screenshot),
        Some(Action::ScreenshotScreen(true, true, None))
    ));
    assert!(matches!(
        action_for_gnome(GnomeKeyAction::ScreenshotWindow),
        Some(Action::ScreenshotWindow(true, true, None))
    ));
    assert!(
        matches!(
            action_for_gnome(GnomeKeyAction::ShowScreenRecordingUi),
            Some(Action::ToggleScreenRecord)
        ),
        "the interim mapping, to be replaced when the recording UI lands"
    );
}

/// The "Screenshot captured" notification carries the shot and a way into the file manager.
///
/// GNOME's has the image as its icon, a **Show in Files** button, and a body click that opens the
/// file (`js/ui/screenshot.js:2386-2420`). Ours used to be posted over
/// `org.freedesktop.Notifications` from the encoding thread — which made it a notification we sent
/// *to ourselves* from a connection that was dropped a moment later, so its buttons had nowhere to
/// route to and it carried the image in the `app_icon` slot (a small source badge) rather than as
/// the notification image.
#[test]
fn a_saved_screenshot_notifies_with_the_image_and_a_show_in_files_button() {
    use crate::notifications::{shell_action_for, NotificationIcon, ShellAction};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let path = std::path::PathBuf::from("/tmp/does-not-need-to-exist.png");
    // A real capture always has pixels; the icon is built from them on the encoding thread.
    let thumbnail = std::sync::Arc::new(crate::notifications::PixelIcon {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    });
    f.synoik_state()
        .show_screenshot_notification(Some(path.clone()), Some(thumbnail.clone()));

    let store = &f.synoik().notifications;
    let source = store
        .sources
        .iter()
        .find(|s| {
            s.key
                == crate::notifications::SourceKey::Shell(
                    crate::notifications::SHELL_SOURCE_SCREENSHOT,
                )
        })
        .expect("the screenshot notification must have its own source");
    assert_eq!(source.title, "Screenshot");

    let n = source
        .notifications
        .last()
        .expect("the source must hold the notification");
    assert_eq!(n.title, "Screenshot captured");
    assert_eq!(
        n.icon,
        Some(NotificationIcon::Pixels(thumbnail)),
        "the shot itself is the notification's image, not a themed badge"
    );

    // One button, and it resolves to the file manager.
    assert_eq!(n.actions.len(), 1);
    assert_eq!(n.actions[0].1, "Show in Files");
    assert_eq!(
        shell_action_for(&n.kind, &n.actions[0].0),
        Some(ShellAction::ShowInFiles(path.clone())),
        "the Show in Files button must resolve to the file manager"
    );

    // ...and a body click opens the file.
    assert!(n.has_default_action);
    assert_eq!(
        shell_action_for(&n.kind, "default"),
        Some(ShellAction::OpenFile(path))
    );
}

/// A clipboard-only capture keeps its image but loses the file buttons.
///
/// GNOME sets the notification's image unconditionally and gates only the "Show in Files" button
/// and the body click on `disableSaveToDisk` (`js/ui/screenshot.js:2397-2418`). Offering either for
/// a file that was never written is the failure this pins.
#[test]
fn a_clipboard_only_screenshot_notifies_without_a_file_button() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Clipboard-only still has pixels — GNOME sets the notification's image unconditionally and
    // gates only the button and the body click on `disableSaveToDisk` (`screenshot.js:2397-2400`).
    let thumbnail = std::sync::Arc::new(crate::notifications::PixelIcon {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    });
    f.synoik_state()
        .show_screenshot_notification(None, Some(thumbnail));

    let n = f
        .synoik()
        .notifications
        .sources
        .iter()
        .find(|s| {
            s.key
                == crate::notifications::SourceKey::Shell(
                    crate::notifications::SHELL_SOURCE_SCREENSHOT,
                )
        })
        .and_then(|s| s.notifications.last())
        .expect("the notification must still be posted");

    assert_eq!(n.title, "Screenshot captured");
    assert!(
        n.icon.is_some(),
        "a clipboard-only capture still has an image — it just has no file"
    );
    assert!(n.actions.is_empty(), "nothing to open in the file manager");
    assert!(!n.has_default_action);
}

/// `org.gnome.desktop.peripherals` reaches the compositor: a settings change lands in
/// `config.input`, which is where `apply_libinput_settings` and the key-repeat timer read it.
///
/// This drives `State::apply_peripherals`, the real entry point the live GSettings subscription
/// calls — the model itself is checked against real GSettings stores in
/// `crate::input::peripherals`. What is worth pinning here is the *hand-off*, because the
/// symptom of dropping it is silent: settings look right, devices keep the old behavior.
#[test]
fn peripherals_settings_reach_the_input_config() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(
        f.synoik().config.borrow().input.touchpad.tap,
        "the default is GNOME's tap-to-click"
    );

    let p = &mut f.synoik().gnome_settings.peripherals;
    p.touchpad.tap = false;
    p.touchpad.natural_scroll = false;
    p.mouse.left_handed = true;
    p.repeat_delay = 250;
    p.repeat_rate = 10;

    f.synoik_state().apply_peripherals();

    let config = f.synoik().config.borrow();
    let input = &config.input;
    assert!(!input.touchpad.tap);
    assert!(!input.touchpad.natural_scroll);
    assert!(input.mouse.left_handed);
    assert_eq!(input.keyboard.repeat_delay, 250);
    assert_eq!(input.keyboard.repeat_rate, 10);
}

// ---------------------------------------------------------------------------
// App indicators — the remote menu (`com.canonical.dbusmenu`)
//
// GNOME Shell has no equivalent, so these pin *our* behavior against the
// `gnome-shell-extension-appindicator` extension's; see
// `docs/fork/status-notifier-port.md`.
// ---------------------------------------------------------------------------

/// An indicator registered with a menu, ready and Active — the state a click acts on. `tweak`
/// shapes the properties the click ladder reads (`ItemIsMenu`, `Activate`, the menu path).
#[cfg(test)]
fn register_indicator_with(f: &mut Fixture, id: &str, tweak: impl FnOnce(&mut ItemProps)) {
    use crate::status_notifier::{ItemIcon, ItemStatus, RegisteredItem, StatusNotifierToSynoik};

    let item = RegisteredItem {
        id: id.to_owned(),
        unique_name: ":1.42".to_owned(),
        object_path: "/StatusNotifierItem".to_owned(),
    };
    let mut props = ItemProps {
        app_id: "test-indicator".to_owned(),
        title: "Test".to_owned(),
        status: ItemStatus::Active,
        icon: ItemIcon::Themed("folder".to_owned()),
        menu_path: Some("/MenuBar".to_owned()),
        // Introspection's answer for a well-behaved KDE item: it has `Activate` and does not have
        // the Ayatana spelling of the secondary one.
        supports_activation: true,
        ..ItemProps::default()
    };
    tweak(&mut props);
    f.synoik_state()
        .on_status_notifier_msg(StatusNotifierToSynoik::ItemUpdated {
            item,
            props: Box::new(props),
        });
}

/// The common case: an item that says a primary click opens its menu.
#[cfg(test)]
fn register_test_indicator(f: &mut Fixture, id: &str) {
    register_indicator_with(f, id, |props| props.item_is_menu = true);
}

/// Click the first indicator on the panel with `button`, and hand back the request channel the
/// watcher would be reading.
#[cfg(test)]
fn click_first_indicator(f: &mut Fixture, button: u32) {
    let anchor = f
        .synoik()
        .panel
        .app_indicator_rect(0, 1920.)
        .expect("the indicator is on the panel");
    pointer_motion_to(
        f,
        anchor.loc.x + anchor.size.w / 2.,
        anchor.loc.y + anchor.size.h / 2.,
    );
    f.pointer_button(button, ButtonState::Pressed);
    f.pointer_button(button, ButtonState::Released);
}

/// A client's layout, as `GetLayout` would deliver it: a root whose children are the rows.
#[cfg(test)]
fn test_menu_layout() -> crate::dbusmenu::MenuNode {
    use crate::dbusmenu::{MenuNode, NodeKind};

    let row = |id: i32, label: &str| MenuNode {
        label: label.to_owned(),
        ..MenuNode::new(id)
    };
    MenuNode {
        children: vec![
            row(1, "_Open Nextcloud"),
            MenuNode {
                kind: NodeKind::Separator,
                ..MenuNode::new(2)
            },
            row(3, "Settings"),
            row(4, "Quit"),
        ],
        ..MenuNode::new(0)
    }
}

/// Clicking an indicator opens its menu, which is **empty** until the client answers: a remote
/// menu's rows are a round trip away, so the box is on screen before its contents are.
#[test]
fn an_indicator_menu_opens_empty_and_fills_in() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    register_test_indicator(&mut f, "org.kde.StatusNotifierItem-1-1");

    click_first_indicator(&mut f, BTN_LEFT);

    assert_eq!(
        f.synoik().panel_popover.indicator_menu_item(),
        Some("org.kde.StatusNotifierItem-1-1"),
        "the click opens that indicator's menu"
    );
    assert!(
        f.synoik()
            .panel_popover
            .indicator_menu()
            .unwrap()
            .labels()
            .is_empty(),
        "the rows have not been asked for yet"
    );

    f.synoik_state().on_status_notifier_msg(
        crate::status_notifier::StatusNotifierToSynoik::MenuLayout {
            item_id: "org.kde.StatusNotifierItem-1-1".to_owned(),
            root: Box::new(test_menu_layout()),
        },
    );

    assert_eq!(
        f.synoik().panel_popover.indicator_menu().unwrap().labels(),
        // The mnemonic marker is gone, and the separator is not a row.
        vec!["Open Nextcloud", "Settings", "Quit"],
    );
}

/// A layout for an item whose menu is *not* the one on screen is dropped rather than shown.
/// Two indicators' menus are two different clients' node-id spaces, and drawing one into the
/// other's box would activate whatever row happened to share the number.
#[test]
fn a_layout_for_another_indicator_is_ignored() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    register_test_indicator(&mut f, "org.kde.StatusNotifierItem-1-1");
    click_first_indicator(&mut f, BTN_LEFT);

    f.synoik_state().on_status_notifier_msg(
        crate::status_notifier::StatusNotifierToSynoik::MenuLayout {
            item_id: "some.other.item".to_owned(),
            root: Box::new(test_menu_layout()),
        },
    );

    assert!(
        f.synoik()
            .panel_popover
            .indicator_menu()
            .unwrap()
            .labels()
            .is_empty(),
        "a layout that lost the race with a dismissal has nowhere to go"
    );
}

/// The watcher is told which menu is open, and — however the menu went away — that it closed.
///
/// The close matters more than the open: a client left believing its menu is still up stops
/// answering `AboutToShow` and, for some, stops updating its rows at all.
#[test]
fn the_client_is_told_its_menu_opened_and_closed() {
    use crate::status_notifier::SynoikToStatusNotifier as Req;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_test_indicator(&mut f, "org.kde.StatusNotifierItem-1-1");

    click_first_indicator(&mut f, BTN_LEFT);

    f.synoik_state().reconcile_indicator_menu();
    assert_eq!(
        rx.try_recv().ok(),
        Some(Req::OpenMenu {
            item_id: "org.kde.StatusNotifierItem-1-1".to_owned(),
            dest: ":1.42".to_owned(),
            item_path: "/StatusNotifierItem".to_owned(),
            menu_path: "/MenuBar".to_owned(),
        }),
        "the watcher is told where to read the menu from"
    );

    // Re-running the reconciler must not re-open: it runs every cycle.
    f.synoik_state().reconcile_indicator_menu();
    assert!(rx.try_recv().is_err(), "nothing changed, nothing sent");

    // Escape, not a click on a row — one of the several ways a popover is dismissed.
    f.synoik_state().on_status_notifier_msg(
        crate::status_notifier::StatusNotifierToSynoik::MenuLayout {
            item_id: "org.kde.StatusNotifierItem-1-1".to_owned(),
            root: Box::new(test_menu_layout()),
        },
    );
    f.synoik().panel_popover.close();
    f.synoik_state().reconcile_indicator_menu();
    assert_eq!(rx.try_recv().ok(), Some(Req::CloseMenu));
}

/// Clicking a row names it back to the client by the client's own node id — not by a row index,
/// which would drift the moment the client hid a row.
#[test]
fn activating_a_row_sends_the_clients_node_id() {
    use crate::status_notifier::SynoikToStatusNotifier as Req;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_test_indicator(&mut f, "org.kde.StatusNotifierItem-1-1");

    click_first_indicator(&mut f, BTN_LEFT);
    f.synoik_state().reconcile_indicator_menu();
    let _ = rx.try_recv();

    f.synoik_state().on_status_notifier_msg(
        crate::status_notifier::StatusNotifierToSynoik::MenuLayout {
            item_id: "org.kde.StatusNotifierItem-1-1".to_owned(),
            root: Box::new(test_menu_layout()),
        },
    );

    // "Quit" is the client's node 4 and our third row: the ids are the client's, not ours.
    let out = f.synoik().global_space.outputs().next().unwrap().clone();
    let origin = f.synoik().panel_popover.content_location(&out);
    let row = f
        .synoik()
        .panel_popover
        .indicator_menu()
        .unwrap()
        .row_center("Quit")
        .expect("the row is there");
    pointer_motion_to(&mut f, origin.x + row.x, origin.y + row.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // The token is minted per click, so match on the row rather than the whole message.
    assert!(
        matches!(rx.try_recv().ok(), Some(Req::MenuActivate { node_id: 4, token }) if !token.is_empty()),
        "the client's node id, with a real activation token"
    );

    // And the menu goes away, as activating any menu row does.
    f.settle_animations();
    assert!(!f.synoik().panel_popover.is_open());
    f.synoik_state().reconcile_indicator_menu();
    assert_eq!(rx.try_recv().ok(), Some(Req::CloseMenu));
}

/// The click ladder's first rung: an item that can be activated and did **not** ask for
/// menu-first behavior is activated by a left click, menu or no menu.
///
/// This is our divergence from the extension, which never reads `ItemIsMenu` and instead waits
/// out a double-click timeout on every primary click (`indicatorStatusIcon.js:375-445`).
#[test]
fn a_left_click_activates_an_activatable_indicator() {
    use crate::status_notifier::SynoikToStatusNotifier as Req;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = true;
    });

    click_first_indicator(&mut f, BTN_LEFT);

    assert!(
        matches!(rx.try_recv().ok(), Some(Req::Activate { item_id, token, .. })
            if item_id == "item" && !token.is_empty()),
        "a primary click activates, with a real activation token"
    );
    assert!(
        f.synoik().panel_popover.indicator_menu().is_none(),
        "and does not open the menu"
    );
}

/// `ItemIsMenu` flips that: the client says a primary click is a menu, so no `Activate` is sent
/// even though the item has one. This is what Plasma does and what these clients are tested
/// against.
#[test]
fn item_is_menu_makes_a_left_click_open_the_menu() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = true;
        props.supports_activation = true;
    });

    click_first_indicator(&mut f, BTN_LEFT);

    assert_eq!(f.synoik().panel_popover.indicator_menu_item(), Some("item"));
    assert!(
        rx.try_recv().is_err(),
        "nothing is called on the item itself"
    );
}

/// An item with no `Activate` falls back to its menu, and one with neither is simply not
/// clickable — there is nothing to invoke, which is the client's doing rather than a gap here.
#[test]
fn an_indicator_without_activate_falls_back_to_its_menu() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = false;
    });

    click_first_indicator(&mut f, BTN_LEFT);
    assert_eq!(f.synoik().panel_popover.indicator_menu_item(), Some("item"));
    assert!(rx.try_recv().is_err());

    // Now take the menu away too.
    f.synoik().panel_popover.close_immediately();
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = false;
        props.menu_path = None;
    });

    click_first_indicator(&mut f, BTN_LEFT);
    assert!(f.synoik().panel_popover.indicator_menu().is_none());
    assert!(rx.try_recv().is_err(), "there is nothing to call");
}

/// A right click is always the menu, even on an item a left click would activate — that is the
/// only way to reach the menu of an activatable item.
#[test]
fn a_right_click_always_opens_the_menu() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = true;
    });

    click_first_indicator(&mut f, BTN_RIGHT);

    assert_eq!(f.synoik().panel_popover.indicator_menu_item(), Some("item"));
    assert!(rx.try_recv().is_err(), "no Activate on a right click");
}

/// A middle click is `SecondaryActivate`, in whichever spelling introspection found — the Ayatana
/// one takes a timestamp and KDE's takes coordinates, and a client has one or the other
/// (`appIndicator.js:817-840`).
#[test]
fn a_middle_click_secondary_activates_in_the_clients_spelling() {
    use crate::status_notifier::SynoikToStatusNotifier as Req;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.has_ayatana_secondary_activate = true;
    });

    click_first_indicator(&mut f, BTN_MIDDLE);

    assert!(
        matches!(
            rx.try_recv().ok(),
            Some(Req::SecondaryActivate {
                ayatana_first: true,
                ..
            })
        ),
        "an Ayatana client is asked in its own spelling first"
    );
    assert!(
        f.synoik().panel_popover.indicator_menu().is_none(),
        "and the menu stays shut"
    );
}

/// An `Activate` that answers `UnknownMethod` is a discovery, not a failed click: the item
/// declared the method and does not have it, so it is demoted to menu-first for good
/// (`appIndicator.js:804-810`). Without this, every future click on that icon vanishes.
#[test]
fn a_declared_but_missing_activate_demotes_the_item() {
    use crate::status_notifier::{StatusNotifierToSynoik, SynoikToStatusNotifier as Req};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = true;
    });

    click_first_indicator(&mut f, BTN_LEFT);
    assert!(matches!(rx.try_recv().ok(), Some(Req::Activate { .. })));

    // The watcher reports what the call turned out to mean.
    f.synoik_state()
        .on_status_notifier_msg(StatusNotifierToSynoik::ActivationUnsupported {
            item_id: "item".to_owned(),
        });

    click_first_indicator(&mut f, BTN_LEFT);
    assert_eq!(
        f.synoik().panel_popover.indicator_menu_item(),
        Some("item"),
        "the next click opens the menu instead of vanishing"
    );
    assert!(rx.try_recv().is_err());
}

/// A wheel notch over an indicator is forwarded to its client with an axis name, and is consumed:
/// an indicator that ignores its scroll must not fall through to switching workspaces under the
/// pointer.
#[test]
fn scrolling_an_indicator_forwards_to_the_client() {
    use crate::status_notifier::{ScrollOrientation, SynoikToStatusNotifier as Req};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A mapped window gives the monitor a second workspace to (wrongly) scroll to.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_test_indicator(&mut f, "item");

    let anchor = f.synoik().panel.app_indicator_rect(0, 1920.).unwrap();
    pointer_motion_to(
        &mut f,
        anchor.loc.x + anchor.size.w / 2.,
        anchor.loc.y + anchor.size.h / 2.,
    );
    f.scroll_wheel();

    assert!(
        matches!(rx.try_recv().ok(), Some(Req::Scroll { delta, orientation: ScrollOrientation::Vertical, .. })
            if delta > 0),
        "the notch reaches the client as a vertical scroll"
    );
    f.synoik_complete_animations();
    assert_eq!(
        f.synoik()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0,
        "and does not also switch workspaces"
    );
}

/// A window a tray icon opened is placed **under that icon**, not wherever a floating window
/// happens to land.
///
/// This is ours to do or nobody's: a Wayland client cannot position its own toplevel, so the
/// spec's `Activate(x, y)` hint is unactionable at the other end. Matched by the activation token
/// we hand the item before `Activate` — the client PID cannot be used, because a sandboxed
/// client's bus traffic goes through `xdg-dbus-proxy` and the connection resolves to the proxy.
#[test]
fn a_window_opened_from_an_indicator_lands_under_its_icon() {
    use crate::status_notifier::SynoikToStatusNotifier as Req;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    let (tx, rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = true;
    });

    let anchor = f.synoik().panel.app_indicator_rect(0, 1920.).unwrap();
    click_first_indicator(&mut f, BTN_LEFT);

    let Some(Req::Activate { token, .. }) = rx.try_recv().ok() else {
        panic!("the click must activate the item");
    };

    // The client opens a window and passes our token along, which is the whole mechanism.
    let window = f.client(client).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(client);
    {
        let synoik = f.synoik();
        let unmapped = synoik.unmapped_windows.values_mut().next().unwrap();
        unmapped.activation_token = Some(token);
    }
    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(client);
    f.synoik_complete_animations();

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_windows())
        .unwrap();
    let (tile, pos, _) = ws.tiles_with_render_positions().next().unwrap();
    let width = tile.tile_size().w;

    assert!(
        (pos.x + width - (anchor.loc.x + anchor.size.w)).abs() < 1.,
        "the window's right edge lines up with the icon's: window right {} vs icon right {}",
        pos.x + width,
        anchor.loc.x + anchor.size.w,
    );
    assert!(
        pos.y > crate::ui::panel::panel_height(),
        "and it hangs below the panel, not under it: y={}",
        pos.y
    );
    assert!(
        pos.y < crate::ui::panel::panel_height() + 16.,
        "close under the panel, like the menu the other button opens: y={}",
        pos.y
    );
}

/// A window that arrives **without** our token is left where the layout put it. There is nothing
/// to fall back to: matching by PID would place an unrelated window under an icon whenever a
/// sandboxed client happened to open one, and the PID does not even identify the client.
#[test]
fn a_window_without_our_token_is_not_moved() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    let (tx, _rx) = async_channel::unbounded();
    f.synoik().status_notifier_emit = Some(tx);
    register_indicator_with(&mut f, "item", |props| {
        props.item_is_menu = false;
        props.supports_activation = true;
    });

    let anchor = f.synoik().panel.app_indicator_rect(0, 1920.).unwrap();

    // The activation is outstanding — this is the window that would be captured if we matched on
    // anything looser than the token.
    click_first_indicator(&mut f, BTN_LEFT);
    let _win = map_window_sized(&mut f, client, (400, 300), None);
    f.synoik_complete_animations();

    let synoik = f.synoik();
    let (_, _, ws) = synoik
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_windows())
        .unwrap();
    let (tile, pos, _) = ws.tiles_with_render_positions().next().unwrap();
    let right = pos.x + tile.tile_size().w;
    assert!(
        (right - (anchor.loc.x + anchor.size.w)).abs() > 1.,
        "an untagged window must not be pulled under the icon: right edge {right}"
    );
}

// ---------------------------------------------------------------------------
// xdg_session_management_v1 — see docs/fork/session-management-port.md.
//
// Slice 1 is the protocol skeleton: sessions live only as long as a client holds them and nothing
// is persisted, so these pin object lifetime, takeover and the error cases. Restore itself is
// pinned once slices 2-4 give it something to restore.
// ---------------------------------------------------------------------------

/// Creates a session and returns the id the compositor minted for it.
#[track_caller]
fn new_session(f: &mut Fixture, id: ClientId) -> (XdgSessionV1, String) {
    let session = f.client(id).get_session(Reason::Launch, None);
    f.roundtrip(id);

    let events = f.client(id).session_events();
    let [SessionEvent::Created(session_id)] = events else {
        panic!("expected exactly one created event, got {events:?}");
    };
    let session_id = session_id.clone();
    assert!(!session_id.is_empty(), "the session id must not be empty");
    (session, session_id)
}

/// mutter's `basic`: a fresh session reports `created`, and a toplevel merely *added* to it is
/// never reported as restored.
#[test]
fn a_new_session_is_created_and_adds_do_not_restore() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);

    let (session, _session_id) = new_session(&mut f, id);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(id).qh.clone();
    session.add_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);

    assert_eq!(
        f.client(id).session_events().len(),
        1,
        "adding a toplevel must not emit restored: {:?}",
        f.client(id).session_events()
    );
}

/// An id the compositor has never issued is treated as if NULL had been passed: a brand new
/// session, with a brand new id, and no `restored`.
#[test]
fn an_unknown_session_id_is_treated_as_a_new_session() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);

    let _session = f
        .client(id)
        .get_session(Reason::Launch, Some("not-a-session"));
    f.roundtrip(id);

    let events = f.client(id).session_events();
    let [SessionEvent::Created(session_id)] = events else {
        panic!("expected exactly one created event, got {events:?}");
    };
    assert_ne!(
        session_id, "not-a-session",
        "an unknown id must not be adopted; a fresh one is minted"
    );
}

/// mutter's `replace`: a second client asking for a live session takes it over. It is told the
/// session was `restored`; the first client is told it was `replaced`.
#[test]
fn another_client_taking_a_session_over_replaces_the_first() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let first = f.add_client();
    f.roundtrip(first);
    let (_session, session_id) = new_session(&mut f, first);

    let second = f.add_client();
    f.roundtrip(second);
    let _taken = f
        .client(second)
        .get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(second);
    f.roundtrip(first);

    assert_eq!(
        f.client(second).session_events(),
        [SessionEvent::Restored],
        "the taking client is told the session was restored, not created"
    );
    assert_eq!(
        f.client(first).session_events(),
        [SessionEvent::Created(session_id), SessionEvent::Replaced],
        "the losing client is told it was replaced"
    );
}

/// The *same* client asking twice for one live session is a protocol error, not a takeover.
#[test]
#[should_panic(expected = "Protocol error 1 on object xdg_session_manager_v1")]
fn re_requesting_a_live_session_is_in_use() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (_session, session_id) = new_session(&mut f, id);

    let _again = f.client(id).get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(id);
}

/// The loser of a takeover destroying its session object must not disturb the winner.
///
/// A takeover moves the id to the new client, leaving the old object inert; when that object is
/// eventually destroyed — which happens on its own schedule, and often long after — its destructor
/// must recognise that it no longer owns the id and do nothing. Without that check it drops the
/// *winner's* live session, and the id silently stops being held.
#[test]
#[should_panic(expected = "Protocol error 1 on object xdg_session_manager_v1")]
fn a_replaced_session_being_destroyed_leaves_the_winner_holding_the_id() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let loser = f.add_client();
    f.roundtrip(loser);
    let (losing_session, session_id) = new_session(&mut f, loser);

    let winner = f.add_client();
    f.roundtrip(winner);
    let _won = f
        .client(winner)
        .get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(winner);

    // The inert object goes away afterwards, as a real client's would.
    losing_session.destroy();
    f.double_roundtrip(loser);

    // The winner still holds it, so asking again is `in_use` — not a restore.
    let _again = f
        .client(winner)
        .get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(winner);
}

/// `restore_toplevel` changes the very first configure, so it is meaningless — and an error —
/// once the client has committed the surface.
#[test]
#[should_panic(expected = "Protocol error 2 on object xdg_session_v1")]
fn restoring_a_toplevel_after_its_first_commit_is_an_error() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    f.client(id).window(&surface).commit();
    f.roundtrip(id);

    let qh = f.client(id).qh.clone();
    session.restore_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    f.roundtrip(id);
}

/// Two toplevels cannot share a name within one session.
#[test]
#[should_panic(expected = "Protocol error 1 on object xdg_session_v1")]
fn two_toplevels_with_the_same_name_is_name_in_use() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let qh = f.client(id).qh.clone();
    for _ in 0..2 {
        let toplevel = f.client(id).create_window().xdg_toplevel.clone();
        session.add_toplevel(&toplevel, String::from("same"), &qh, String::from("same"));
    }
    f.roundtrip(id);
}

/// One toplevel cannot be added twice, even under a different name.
#[test]
#[should_panic(expected = "Protocol error 4 on object xdg_session_v1")]
fn adding_one_toplevel_twice_is_already_added() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let qh = f.client(id).qh.clone();
    let toplevel = f.client(id).create_window().xdg_toplevel.clone();
    session.add_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    session.add_toplevel(&toplevel, String::from("two"), &qh, String::from("two"));
    f.roundtrip(id);
}

/// `rename` frees the old name, so a later toplevel may take it.
#[test]
fn renaming_a_toplevel_session_frees_its_old_name() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let qh = f.client(id).qh.clone();
    let first = f.client(id).create_window().xdg_toplevel.clone();
    let handle = session.add_toplevel(&first, String::from("one"), &qh, String::from("one"));
    handle.rename(String::from("renamed"));

    // "one" is free again, so this must not raise name_in_use.
    let second = f.client(id).create_window().xdg_toplevel.clone();
    session.add_toplevel(&second, String::from("one"), &qh, String::from("one"));
    f.roundtrip(id);

    assert_eq!(
        f.client(id).session_events().len(),
        1,
        "only the original created event: {:?}",
        f.client(id).session_events()
    );
}

/// Renaming onto a name the session already holds is a protocol error.
#[test]
#[should_panic(expected = "Protocol error 1 on object xdg_session_v1")]
fn renaming_onto_a_taken_name_is_name_in_use() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let qh = f.client(id).qh.clone();
    let first = f.client(id).create_window().xdg_toplevel.clone();
    let handle = session.add_toplevel(&first, String::from("one"), &qh, String::from("one"));

    let second = f.client(id).create_window().xdg_toplevel.clone();
    session.add_toplevel(&second, String::from("two"), &qh, String::from("two"));

    handle.rename(String::from("two"));
    f.roundtrip(id);
}

/// `destroy` makes the session object inert but *keeps* the state, so the id stays known: asking
/// for it again is a restore, not a fresh session. That is the whole difference from `remove`.
#[test]
fn a_destroyed_session_goes_inert_but_stays_known() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);

    let qh = f.client(id).qh.clone();
    let toplevel = f.client(id).create_window().xdg_toplevel.clone();
    session.add_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    f.roundtrip(id);

    session.destroy();
    f.roundtrip(id);

    // No longer live, so this is not `in_use`; still known, so it is a restore.
    let _again = f.client(id).get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(id);

    assert_eq!(
        f.client(id).session_events(),
        [SessionEvent::Created(session_id), SessionEvent::Restored]
    );
}

/// Destroying a session must save what its still-mapped toplevels look like *now*.
///
/// The spec's word for `destroy` is "preserving the current state" — the state is frozen as of the
/// request, not left at whatever the last unmap happened to record. Our only save trigger is unmap,
/// so without this a session destroyed while its windows are up keeps a record from a previous run.
#[test]
fn destroying_a_session_saves_its_still_mapped_toplevels() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let _surface = map_session_window(&mut f, id, &session, "main");

    // Somewhere unmistakable, and somewhere that is not where a fresh window lands.
    f.synoik_state()
        .do_action(Action::MoveWindowToWorkspaceDown(true), false);
    f.synoik_complete_animations();

    session.destroy();
    f.double_roundtrip(id);

    let saved = f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .and_then(|record| record.toplevels.get("main"))
        .and_then(|toplevel| toplevel.workspace);
    assert_eq!(
        saved,
        Some(1),
        "destroying the session must preserve where the window is *now*"
    );
}

/// Ghost's shape: two windows, one torn down before the session and one after. The second one must
/// not be left holding a stale record — which is exactly how a window comes back on the desktop it
/// was on two runs ago.
#[test]
fn a_toplevel_outliving_its_session_still_saves() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let first = map_session_window(&mut f, id, &session, "first");
    let second = map_session_window(&mut f, id, &session, "second");

    // `second` has focus; move it one desktop down so the two differ.
    f.synoik_state()
        .do_action(Action::MoveWindowToWorkspaceDown(true), false);
    f.synoik_complete_animations();

    // Tear down in the order that loses state: one toplevel, then the session, then the rest.
    f.client(id).window(&first).attach_null_buffer();
    f.client(id).window(&first).commit();
    f.double_roundtrip(id);

    session.destroy();
    f.double_roundtrip(id);

    f.client(id).window(&second).attach_null_buffer();
    f.client(id).window(&second).commit();
    f.double_roundtrip(id);

    let saved = |f: &mut Fixture, name: &str| {
        f.synoik()
            .session_manager_state
            .store
            .get(&session_id)
            .and_then(|record| record.toplevels.get(name))
            .and_then(|toplevel| toplevel.workspace)
    };
    assert_eq!(saved(&mut f, "first"), Some(0), "`first` unmapped normally");
    assert_eq!(
        saved(&mut f, "second"),
        Some(1),
        "`second` outlived the session and must still have been saved"
    );
}

/// `remove` forgets the session, so the id stops being known and asking for it again mints a new
/// one.
#[test]
fn a_removed_session_is_forgotten() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);

    session.remove();
    f.roundtrip(id);

    let _again = f.client(id).get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(id);

    let events = f.client(id).session_events();
    let [SessionEvent::Created(first), SessionEvent::Created(second)] = events else {
        panic!("expected two created events, got {events:?}");
    };
    assert_eq!(first, &session_id);
    assert_ne!(first, second, "a removed id must not be handed back out");
}

/// An inert session — one another client took over — must not be able to delete the state it no
/// longer manages.
#[test]
fn removing_from_an_inert_session_does_not_forget_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let first = f.add_client();
    f.roundtrip(first);
    let (losing, session_id) = new_session(&mut f, first);

    let second = f.add_client();
    f.roundtrip(second);
    let taken = f
        .client(second)
        .get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(second);
    f.roundtrip(first);

    // The first client is now inert. Its `remove` must be a no-op.
    losing.remove();
    f.roundtrip(first);
    f.roundtrip(second);

    taken.destroy();
    f.roundtrip(second);

    let third = f.add_client();
    f.roundtrip(third);
    let _again = f
        .client(third)
        .get_session(Reason::Launch, Some(&session_id));
    f.roundtrip(third);

    assert_eq!(
        f.client(third).session_events(),
        [SessionEvent::Restored],
        "the inert client's remove must not have deleted the session"
    );
}

/// `restore_toplevel` for a name the session has never heard of degrades to `add_toplevel`, and
/// in particular sends **no** `restored` event. Nothing is persisted yet, so every name is
/// unknown; slice 4 is what makes the other branch reachable.
#[test]
fn restoring_an_unknown_name_adds_it_without_a_restored_event() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(id).qh.clone();
    session.restore_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);

    assert_eq!(
        f.client(id).session_events(),
        [SessionEvent::Created(_session_id)],
        "an unknown name must not be reported as restored"
    );

    // And the name really was taken, so a second toplevel cannot claim it — that is what makes
    // this an `add_toplevel` rather than a no-op.
    let second = f.client(id).create_window().xdg_toplevel.clone();
    session.add_toplevel(&second, String::from("one"), &qh, String::from("one"));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.roundtrip(id)));
    assert!(panicked.is_err(), "expected a name_in_use protocol error");
}

/// A toplevel that has already been mapped once cannot be restored, even though unmapping it puts
/// it back through the initial commit-configure sequence: `already_mapped` is pinned to the first
/// commit after the *toplevel* was created.
#[test]
#[should_panic(expected = "Protocol error 2 on object xdg_session_v1")]
fn restoring_a_remapped_toplevel_is_still_already_mapped() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, _session_id) = new_session(&mut f, id);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    window.commit();
    f.roundtrip(id);

    // Map, then unmap by attaching a null buffer, which puts the toplevel back in the unmapped
    // set as if it were new.
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    let qh = f.client(id).qh.clone();
    session.restore_toplevel(&toplevel, String::from("one"), &qh, String::from("one"));
    f.roundtrip(id);
}

/// A session id the store remembers from a previous run is *known* even though no client holds
/// it, so asking for it is a restore rather than a fresh session. This is what persistence buys.
#[test]
fn a_session_id_only_the_store_knows_is_restored() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    // Stand in for a store loaded off disk at startup.
    f.synoik()
        .session_manager_state
        .store
        .touch("from-a-previous-run");

    let id = f.add_client();
    f.roundtrip(id);
    let _session = f
        .client(id)
        .get_session(Reason::Launch, Some("from-a-previous-run"));
    f.roundtrip(id);

    assert_eq!(
        f.client(id).session_events(),
        [SessionEvent::Restored],
        "a remembered id must not be re-minted"
    );
}

/// Maps a window into `session` under `name`, and returns its surface.
#[track_caller]
fn map_session_window(
    f: &mut Fixture,
    id: ClientId,
    session: &XdgSessionV1,
    name: &str,
) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(id).qh.clone();
    session.add_toplevel(&toplevel, String::from(name), &qh, String::from(name));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(300, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

/// Unmapping a registered window writes its state, geometry and workspace into the store —
/// mutter's `on_window_unmanaging` (`meta-wayland-xdg-session.c:262-276`).
#[test]
fn unmapping_a_registered_window_saves_its_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let surface = map_session_window(&mut f, id, &session, "main");

    assert!(
        f.synoik()
            .session_manager_state
            .store
            .get(&session_id)
            .unwrap()
            .toplevels
            .is_empty(),
        "nothing is saved while the window is still mapped"
    );

    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    let record = f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .expect("the session must be in the store")
        .toplevels
        .get("main")
        .cloned()
        .expect("the unmapped toplevel must have a record");

    assert_eq!(record.restorable_state(), Some(WindowState::Floating));
    assert_eq!(record.workspace, Some(0));
    let rect = record.floating_rect.expect("a floated window has a rect");
    assert_eq!(
        [rect[2], rect[3]],
        [300, 200],
        "the saved size is the window's"
    );
    assert!(
        f.synoik().session_save_timer.is_some(),
        "the save must be debounced, not written inline"
    );
}

/// The saved rect is **global**: on a second output it carries that output's origin, so restore
/// can pick the output back out of it. And it clears the panel strut, since the working area does.
#[test]
fn the_saved_rect_is_in_global_coordinates() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    f.add_output(2, (1280, 720));

    let origin = {
        let synoik = f.synoik();
        let output = synoik
            .global_space
            .outputs()
            .find(|output| output.name() == "headless-2")
            .cloned()
            .expect("the second output must exist");
        synoik
            .global_space
            .output_geometry(&output)
            .expect("the second output must be in global space")
            .loc
    };
    assert_ne!(
        origin.x, 0,
        "the outputs must not overlap, or this proves nothing"
    );

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let surface = map_session_window(&mut f, id, &session, "main");

    // Put it on the second output, then close it.
    f.synoik_state()
        .do_action(Action::MoveWindowToMonitorRight, false);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    let rect = f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .unwrap()
        .toplevels["main"]
        .floating_rect
        .expect("a floated window has a rect");

    assert!(
        rect[0] >= origin.x,
        "the saved x ({}) must carry the second output's origin ({})",
        rect[0],
        origin.x
    );
    assert!(
        rect[1] >= crate::ui::panel::panel_height().round() as i32,
        "the saved y ({}) must clear the panel strut",
        rect[1]
    );
}

/// A maximized window saves `maximized` plus the rect it would go back to, not the maximized one —
/// mutter's `saved_rect`, which is the whole reason the two are kept apart.
#[test]
fn a_maximized_window_saves_its_pre_maximize_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let surface = map_session_window(&mut f, id, &session, "main");

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);

    // Actually take the maximized size. Without this the client stays 300x200 and the test would
    // pass even if the save read the *live* rect instead of the remembered floating one.
    let maximized = f
        .client(id)
        .window(&surface)
        .recent_configures()
        .last()
        .expect("maximize must configure the window")
        .size;
    assert_ne!(maximized, (300, 200), "maximize must change the size");
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(maximized.0 as u16, maximized.1 as u16);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    let record = f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .unwrap()
        .toplevels["main"]
        .clone();

    assert_eq!(record.restorable_state(), Some(WindowState::Maximized));
    let rect = record
        .floating_rect
        .expect("the pre-maximize rect is remembered");
    assert_eq!(
        [rect[2], rect[3]],
        [300, 200],
        "the remembered size is the floating one, not the maximized one"
    );
}

/// A window whose record was dropped by `remove_toplevel` must not come back when it unmaps: the
/// registration is gone, so there is nothing to save under.
#[test]
fn removing_a_toplevel_then_unmapping_it_saves_nothing() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let surface = map_session_window(&mut f, id, &session, "main");

    session.remove_toplevel(String::from("main"));
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);

    assert!(
        f.synoik()
            .session_manager_state
            .store
            .get(&session_id)
            .unwrap()
            .toplevels
            .is_empty(),
        "an unregistered toplevel must not be resurrected by unmapping"
    );
}

/// Logging out with windows still open saves them. Mutter gets this from unmanaging every window
/// before its final save (`display.c:1052`); we have no unmanage-all, so the shutdown sweep is
/// what stands in for it — and it is the flagship case for the whole protocol.
#[test]
fn shutting_down_saves_windows_that_are_still_mapped() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    let _surface = map_session_window(&mut f, id, &session, "main");

    assert!(
        f.synoik()
            .session_manager_state
            .store
            .get(&session_id)
            .unwrap()
            .toplevels
            .is_empty(),
        "nothing is saved while it is still mapped"
    );

    f.synoik_state().save_session_toplevels_still_mapped();

    let record = f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .unwrap()
        .toplevels
        .get("main")
        .cloned()
        .expect("a still-mapped window must be swept into the store at shutdown");
    assert_eq!(record.restorable_state(), Some(WindowState::Floating));
    assert_eq!(record.workspace, Some(0));
}

/// Renaming carries the saved state to the new name, rather than orphaning it under the old one.
#[test]
fn renaming_a_toplevel_moves_its_saved_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(id).qh.clone();
    let handle = session.add_toplevel(&toplevel, String::from("old"), &qh, String::from("old"));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(300, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Unmap first, so there is a saved record to move, then rename.
    let window = f.client(id).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(id);
    handle.rename(String::from("new"));
    f.roundtrip(id);

    let toplevels = &f
        .synoik()
        .session_manager_state
        .store
        .get(&session_id)
        .unwrap()
        .toplevels;
    assert!(
        !toplevels.contains_key("old"),
        "the old name must be released"
    );
    assert!(
        toplevels.contains_key("new"),
        "the saved state must follow the rename: {toplevels:?}"
    );
}

/// Seeds the store as a previous run would have left it.
fn remember(f: &mut Fixture, session_id: &str, name: &str, record: ToplevelRecord) {
    f.synoik()
        .session_manager_state
        .store
        .save_toplevel(session_id, name, record);
}

/// Asks to restore `name` and returns the surface, configured but not yet mapped.
#[track_caller]
fn restore_window(
    f: &mut Fixture,
    id: ClientId,
    session: &XdgSessionV1,
    name: &str,
) -> (WlSurface, XdgToplevelSessionV1) {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(id).qh.clone();
    let handle = session.restore_toplevel(&toplevel, String::from(name), &qh, String::from(name));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    (surface, handle)
}

/// Acks the pending configure at the size the compositor asked for, mapping the window.
#[track_caller]
fn map_at_configured_size(f: &mut Fixture, id: ClientId, surface: &WlSurface) -> (i32, i32) {
    let size = f
        .client(id)
        .window(surface)
        .recent_configures()
        .last()
        .expect("the window must have been configured")
        .size;
    let window = f.client(id).window(surface);
    window.attach_new_buffer();
    window.set_size(size.0 as u16, size.1 as u16);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    size
}

/// The heart of the protocol: close a window somewhere, and it comes back there.
#[test]
fn a_windows_geometry_survives_a_restart() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    // First run: map a window, move it somewhere unmistakable, close it.
    let first = f.add_client();
    f.roundtrip(first);
    let (session, session_id) = new_session(&mut f, first);
    let surface = map_session_window(&mut f, first, &session, "main");
    let win = f.synoik().layout.focus().unwrap().window.clone();
    f.synoik_state()
        .do_action(Action::MoveWindowToWorkspaceDown(true), false);
    f.synoik_complete_animations();
    f.double_roundtrip(first);
    let saved = f.synoik().layout.session_snapshot(&win).unwrap();
    let saved_rect = saved.floating_rect.expect("a floated window has a rect");

    let window = f.client(first).window(&surface);
    window.attach_null_buffer();
    window.commit();
    f.double_roundtrip(first);
    drop(session);

    // Second run: a fresh client asks for the same session and restores the same name.
    let second = f.add_client();
    f.roundtrip(second);
    let session = f
        .client(second)
        .get_session(Reason::SessionRestore, Some(&session_id));
    f.roundtrip(second);
    let (surface, _handle) = restore_window(&mut f, second, &session, "main");
    let size = map_at_configured_size(&mut f, second, &surface);
    f.synoik_complete_animations();

    assert_eq!(
        size,
        (
            saved_rect.size.w.round() as i32,
            saved_rect.size.h.round() as i32
        ),
        "the restored window must be configured at its saved size"
    );

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let restored = f
        .synoik()
        .layout
        .session_snapshot(&win)
        .unwrap()
        .floating_rect
        .expect("the restored window has a rect");
    assert_eq!(
        (restored.loc.x.round(), restored.loc.y.round()),
        (saved_rect.loc.x.round(), saved_rect.loc.y.round()),
        "the restored window must come back where it was closed"
    );
}

/// A restored window lands at the exact position it was saved at, on the output the saved rect
/// names — including the second one, which pins that the global origin is folded back out, and
/// clearing the panel strut, which pins that the per-workspace working area is too.
#[test]
fn a_restored_window_lands_at_its_saved_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    f.add_output(2, (1280, 720));

    let origin = {
        let synoik = f.synoik();
        let output = synoik
            .global_space
            .outputs()
            .find(|output| output.name() == "headless-2")
            .cloned()
            .expect("the second output must exist");
        synoik
            .global_space
            .output_geometry(&output)
            .expect("in global space")
            .loc
    };
    assert_ne!(
        origin.x, 0,
        "the outputs must not overlap, or this proves nothing"
    );

    // Somewhere unmistakable on the second output, well clear of any default placement.
    let saved = [origin.x + 500, origin.y + 400, 300, 200];

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some(saved),
            workspace: Some(0),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let global = {
        let synoik = f.synoik();
        let snapshot = synoik.layout.session_snapshot(&win).unwrap();
        let rect = snapshot.floating_rect.expect("it has a rect");
        let origin = snapshot
            .output
            .and_then(|output| synoik.global_space.output_geometry(output))
            .expect("mapped on an output")
            .loc;
        rect.loc + origin.to_f64()
    };

    assert_eq!(
        (global.x.round() as i32, global.y.round() as i32),
        (saved[0], saved[1]),
        "the restored window must land exactly where it was saved"
    );
}

/// The spec pins `restored` to "prior to the first `xdg_toplevel.configure`".
#[test]
fn restored_arrives_before_the_first_configure() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 200, 300, 400]),
            workspace: Some(0),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");

    assert_eq!(
        f.client(id).session_events().last(),
        Some(&SessionEvent::ToplevelRestored(String::from("main"))),
        "restore must report the name as restored"
    );
    assert!(
        f.client(id)
            .window(&surface)
            .recent_configures()
            .next()
            .is_some(),
        "and the configure must have followed it"
    );
}

/// A saved state comes back as pending toplevel state on the very first configure, so the client
/// never sees an unmaximized frame first.
#[test]
fn a_saved_window_state_is_restored_on_the_first_configure() {
    for (saved, expected) in [
        (WindowState::Maximized, xdg_toplevel::State::Maximized),
        (WindowState::Fullscreen, xdg_toplevel::State::Fullscreen),
    ] {
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));

        let id = f.add_client();
        f.roundtrip(id);
        let (session, session_id) = new_session(&mut f, id);
        remember(
            &mut f,
            &session_id,
            "main",
            ToplevelRecord {
                state: Some(saved.as_raw()),
                floating_rect: Some([100, 200, 300, 400]),
                workspace: Some(0),
                ..Default::default()
            },
        );

        let (surface, _handle) = restore_window(&mut f, id, &session, "main");
        let configure = f
            .client(id)
            .window(&surface)
            .recent_configures()
            .last()
            .expect("configured")
            .clone();
        assert!(
            configure.states.contains(&expected),
            "restoring {saved:?} must configure {expected:?}, got {configure}"
        );
    }
}

/// A restored window goes back to its workspace under **all three** reasons. This is the
/// deliberate divergence from the spec's hint that only `session_restore` restores placement.
#[test]
fn a_restored_window_returns_to_its_workspace_under_every_reason() {
    for reason in [Reason::Launch, Reason::Recover, Reason::SessionRestore] {
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));

        let id = f.add_client();
        f.roundtrip(id);
        let session = f.client(id).get_session(reason, None);
        f.roundtrip(id);
        let events = f.client(id).session_events();
        let [SessionEvent::Created(session_id)] = events else {
            panic!("expected created, got {events:?}");
        };
        let session_id = session_id.clone();

        remember(
            &mut f,
            &session_id,
            "main",
            ToplevelRecord {
                state: Some(WindowState::Floating.as_raw()),
                floating_rect: Some([100, 200, 300, 400]),
                workspace: Some(1),
                ..Default::default()
            },
        );

        let (surface, _handle) = restore_window(&mut f, id, &session, "main");
        map_at_configured_size(&mut f, id, &surface);
        f.synoik_complete_animations();

        // Not via `focus()`: two of the three reasons deliberately do not take focus.
        let win = f
            .synoik()
            .layout
            .windows()
            .map(|(_, mapped)| mapped.window.clone())
            .next()
            .expect("the restored window must be mapped");
        assert_eq!(
            f.synoik()
                .layout
                .session_snapshot(&win)
                .unwrap()
                .workspace_idx,
            1,
            "reason {reason:?} must still restore the workspace"
        );
    }
}

/// The saved workspace decides the *configure*, not just where the window lands: a window is sized
/// against its own workspace's working area, and workspaces can have different struts.
#[test]
fn a_restored_window_is_configured_against_its_saved_workspace() {
    // Two workspaces on one output with different struts, so the toplevel bounds in the configure
    // say which of the two the window was configured against — as in
    // `window_opening::maximize_after_the_initial_configure_keeps_the_windows_workspace`.
    let struts = |side| {
        Some(synoik_config::WorkspaceLayoutPart(
            synoik_config::LayoutPart {
                struts: Some(synoik_config::Struts {
                    left: synoik_config::FloatOrInt(side),
                    right: synoik_config::FloatOrInt(side),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
    };
    let config = Config {
        workspaces: vec![
            synoik_config::workspace::Workspace {
                name: synoik_config::workspace::WorkspaceName(String::from("ws-a")),
                open_on_output: Some(String::from("headless-1")),
                layout: struts(0.),
            },
            synoik_config::workspace::Workspace {
                name: synoik_config::workspace::WorkspaceName(String::from("ws-b")),
                open_on_output: Some(String::from("headless-1")),
                layout: struts(100.),
            },
        ],
        ..Default::default()
    };

    // Scrolling mode: GNOME mode derives the working area from the layer zone and the panel
    // alone, so per-workspace struts — the only lever that makes two workspaces on one monitor
    // configure differently — do not apply there. The configure path itself is shared.
    let mut f = Fixture::with_config(scrolling(config));
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    // Two windows, one saved to each workspace. Comparing the two configures is what makes this
    // discriminating: `Workspace::working_area` deliberately excludes the config struts (see its
    // comment there), so there is no accessor to read the expected width off, and one window's
    // bounds on its own is a bare number that either workspace could plausibly have produced.
    for (name, workspace) in [("on-a", 0u32), ("on-b", 1)] {
        remember(
            &mut f,
            &session_id,
            name,
            ToplevelRecord {
                state: Some(WindowState::Floating.as_raw()),
                floating_rect: Some([100, 200, 300, 200]),
                workspace: Some(workspace),
                ..Default::default()
            },
        );
    }

    let bounds_for = |f: &mut Fixture, name: &str| {
        let (surface, handle) = restore_window(f, id, &session, name);
        let bounds = f
            .client(id)
            .window(&surface)
            .recent_configures()
            .last()
            .expect("configured")
            .bounds
            .expect("bounded");
        (bounds, handle)
    };

    let (on_a, _a) = bounds_for(&mut f, "on-a");
    let (on_b, _b) = bounds_for(&mut f, "on-b");

    assert_eq!(
        on_a.0 - on_b.0,
        200,
        "ws-b's 100px-per-side struts must narrow its bounds ({}) against ws-a's ({})",
        on_b.0,
        on_a.0
    );
    assert_eq!(on_a.1, on_b.1, "neither workspace struts the vertical axis");
}

/// GNOME auto-maximize must not touch a restored window. Its size is remembered, not guessed, so
/// a saved rect that happens to cover most of the work area is a deliberate floating size.
#[test]
fn a_restored_window_is_not_auto_maximized() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            // Well over the 80% of the work area that triggers auto-maximize.
            floating_rect: Some([0, 40, 1270, 660]),
            workspace: Some(0),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let snapshot = f.synoik().layout.session_snapshot(&win).unwrap();
    // The *geometry* is the observable, not the sizing mode: auto-maximize's visible effect on a
    // window this size is that it scales the floating rect down by `sqrt(0.8)` and keeps that as
    // the restore size. The mode reads `Normal` either way, so asserting on it pins nothing.
    assert_eq!(
        snapshot.floating_rect.map(|r| (r.size.w, r.size.h)),
        Some((1270., 660.)),
        "a restored window keeps its saved size; auto-maximize would have scaled it down"
    );
    assert_eq!(
        snapshot.sizing_mode,
        SizingMode::Normal,
        "and it stays floating"
    );
}

/// A saved index past the end of the monitor's workspaces **grows** the strip to reach it. A fresh
/// monitor has two desktops, so anything saved past the second would otherwise have nowhere to go.
#[test]
fn a_saved_workspace_index_past_the_end_grows_the_strip() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    assert_eq!(
        f.synoik().layout.workspaces().count(),
        2,
        "a fresh monitor shows two desktops"
    );

    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 200, 300, 400]),
            workspace: Some(4),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    let win = session_window(&mut f, "main");
    assert_eq!(
        f.synoik()
            .layout
            .session_snapshot(&win)
            .unwrap()
            .workspace_idx,
        4,
        "the window must land on the desktop it was saved on"
    );
    assert_eq!(
        f.synoik().layout.workspaces().count(),
        6,
        "0..=4 plus the trailing empty that landing on the last one appends"
    );
}

/// Restoring a set of windows must not depend on the order the client asks in. This is the
/// regression test for the bug that clamping caused: three windows saved on desktops 0/1/2
/// restored correctly in ascending order — each landing on the trailing empty grew the strip just
/// in time for the next — and collapsed two onto one desktop in any other order.
#[test]
fn restoring_windows_out_of_order_still_lands_each_on_its_own_workspace() {
    for order in [["w0", "w1", "w2"], ["w2", "w1", "w0"], ["w1", "w2", "w0"]] {
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));

        let id = f.add_client();
        f.roundtrip(id);
        let (session, session_id) = new_session(&mut f, id);

        for (name, idx) in [("w0", 0u32), ("w1", 1), ("w2", 2)] {
            remember(
                &mut f,
                &session_id,
                name,
                ToplevelRecord {
                    state: Some(WindowState::Floating.as_raw()),
                    floating_rect: Some([100, 100, 300, 200]),
                    workspace: Some(idx),
                    ..Default::default()
                },
            );
        }

        let mut landed = Vec::new();
        for name in order {
            let (surface, _handle) = restore_window(&mut f, id, &session, name);
            map_at_configured_size(&mut f, id, &surface);
            f.synoik_complete_animations();
            let win = session_window(&mut f, name);
            let idx = f
                .synoik()
                .layout
                .session_snapshot(&win)
                .unwrap()
                .workspace_idx;
            landed.push((name, idx));
        }

        let expected: Vec<_> = order
            .iter()
            .map(|name| (*name, name[1..].parse::<usize>().unwrap()))
            .collect();
        assert_eq!(
            landed, expected,
            "restored in the order {order:?}, every window must land on its saved desktop"
        );
    }
}

/// The everyday case, end to end: two windows, one left on the first desktop and one moved to the
/// third, saved by quitting the app and restored into a *fresh* session that starts with two
/// desktops. The gap matters — desktop 1 stays empty — and so does the order, since neither window
/// can rely on the other having grown the strip first.
#[test]
fn a_window_moved_to_the_third_desktop_comes_back_to_the_third_desktop() {
    for order in [["a", "b"], ["b", "a"]] {
        // First run: `a` stays put, `b` moves two desktops down. Quitting unmaps both, which is
        // what writes them to the store.
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));
        let id = f.add_client();
        f.roundtrip(id);
        let (session, session_id) = new_session(&mut f, id);

        let a = map_session_window(&mut f, id, &session, "a");
        let b = map_session_window(&mut f, id, &session, "b");
        for _ in 0..2 {
            f.synoik_state()
                .do_action(Action::MoveWindowToWorkspaceDown(true), false);
            f.synoik_complete_animations();
        }

        for surface in [&a, &b] {
            f.client(id).window(surface).attach_null_buffer();
            f.client(id).window(surface).commit();
            f.double_roundtrip(id);
        }

        let saved = |f: &mut Fixture, name: &str| {
            f.synoik()
                .session_manager_state
                .store
                .get(&session_id)
                .and_then(|record| record.toplevels.get(name))
                .and_then(|toplevel| toplevel.workspace)
        };
        assert_eq!(
            saved(&mut f, "a"),
            Some(0),
            "`a` never left the first desktop"
        );
        assert_eq!(saved(&mut f, "b"), Some(2), "`b` was moved to the third");

        // Second run: a fresh session, which starts with two desktops — so the third has to be
        // grown before `b` can go back to it.
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));
        let id = f.add_client();
        f.roundtrip(id);
        // Seed the store before asking for the session: an id the store does not know is treated
        // as a brand new one, which would hand back a different id entirely.
        for (name, idx) in [("a", 0u32), ("b", 2)] {
            remember(
                &mut f,
                &session_id,
                name,
                ToplevelRecord {
                    state: Some(WindowState::Floating.as_raw()),
                    floating_rect: Some([100, 100, 300, 200]),
                    workspace: Some(idx),
                    ..Default::default()
                },
            );
        }
        let session = f.client(id).get_session(Reason::Launch, Some(&session_id));
        f.roundtrip(id);

        let mut landed = Vec::new();
        for name in order {
            let (surface, _handle) = restore_window(&mut f, id, &session, name);
            map_at_configured_size(&mut f, id, &surface);
            f.synoik_complete_animations();
            let win = session_window(&mut f, name);
            let synoik = f.synoik();
            landed.push((
                name,
                synoik.layout.session_snapshot(&win).unwrap().workspace_idx,
            ));
        }
        landed.sort();

        assert_eq!(
            landed,
            vec![("a", 0), ("b", 2)],
            "restored in the order {order:?}, `b` must come back to the third desktop"
        );
    }
}

/// An app with a window demanding attention is flagged in the dash, so the dock can poke its icon
/// above the bottom edge — our affordance for urgency, which GNOME has no equivalent of
/// (`windowAttentionHandler.js` shows a notification and touches nothing in the dash).
#[test]
fn a_window_demanding_attention_marks_its_app_urgent_in_the_dash() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    // The catalog is keyed by desktop id; a toplevel `app_id` of "a" resolves through
    // `lookup_desktop_wmclass` to "a.desktop".
    seed_favorites(&mut f, &["a.desktop"]);

    let id = f.add_client();
    f.roundtrip(id);

    // Two windows: one to hold focus on the first desktop, and one for "a" that maps on the
    // second — mapping where the user is not looking is what marks it urgent.
    let holder = f.client(id).create_window();
    let holder_surface = holder.surface.clone();
    holder.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&holder_surface);
    window.attach_new_buffer();
    window.set_size(300, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 100, 300, 200]),
            workspace: Some(1),
            ..Default::default()
        },
    );

    let (surface, toplevel, qh) = {
        let win = f.client(id).create_window();
        win.set_app_id("a");
        (
            win.surface.clone(),
            win.xdg_toplevel.clone(),
            f.client(id).qh.clone(),
        )
    };
    let _handle =
        session.restore_toplevel(&toplevel, String::from("main"), &qh, String::from("main"));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    assert!(
        f.synoik()
            .layout
            .windows()
            .any(|(_, mapped)| mapped.is_urgent()),
        "a window mapping on another desktop demands attention"
    );

    f.synoik().sync_running_apps();
    f.synoik().sync_dash_favorites();
    let urgent: Vec<&str> = f
        .synoik()
        .dash
        .items()
        .iter()
        .filter(|item| item.urgent)
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(
        urgent,
        ["a.desktop"],
        "the dash must know which app is urgent, or there is nothing to poke"
    );
}

/// A window that already knows its workspace **still completes its app's startup sequence.**
///
/// mutter completes the sequence as soon as a window matches it
/// (`meta_startup_sequence_complete`, `display.c:2712`) and only afterwards asks whether to apply
/// its properties, guarded by `if (!window->initial_workspace_set)`. We had the completion nested
/// inside the workspace lookup, so every window that arrived with a workspace already decided —
/// every restored window, and anything an `open-on-workspace` rule pinned — left its app STARTING
/// until the sequence timed out. Seat symptom: ghost showed a loading state for twenty seconds
/// after it was up, and its dash icon did nothing when clicked, because a STARTING app is not
/// activatable.
#[test]
fn a_window_with_a_seeded_workspace_still_finishes_its_apps_startup() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")])),
        Box::new(RecordingLauncher::default()),
    );

    let id = f.add_client();
    f.roundtrip(id);

    // The app is launching: a sequence is open, so the dash shows it as STARTING.
    f.synoik()
        .app_system
        .begin_startup("a.desktop", None, None, get_monotonic_time());
    assert_eq!(
        f.synoik().app_system.starting_apps().collect::<Vec<_>>(),
        ["a.desktop"],
        "the launch must open a sequence, or this test proves nothing"
    );

    // Its window arrives with a workspace already chosen — here by a session restore, which is
    // how ghost hits this on every launch that has something to restore.
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 100, 300, 200]),
            workspace: Some(1),
            ..Default::default()
        },
    );
    let (surface, toplevel, qh) = {
        let win = f.client(id).create_window();
        win.set_app_id("a");
        (
            win.surface.clone(),
            win.xdg_toplevel.clone(),
            f.client(id).qh.clone(),
        )
    };
    let _handle =
        session.restore_toplevel(&toplevel, String::from("main"), &qh, String::from("main"));
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    assert!(
        f.synoik()
            .app_system
            .starting_apps()
            .collect::<Vec<_>>()
            .is_empty(),
        "the window is up, so its app is running — not starting until the sequence expires"
    );
}

/// A **`recover`** puts its windows back and marks the ones that landed elsewhere; a
/// **`session_restore`** marks nothing.
///
/// One app coming back from a crash can say where it went — "your app is back, over there" — and
/// that is a single window's worth of noise. A login restoring everything you had open cannot:
/// every app would shout at once, which is no signal at all. Neither takes focus; only the mark
/// differs (decided 2026-08-08).
#[test]
fn a_recover_demands_attention_from_another_desktop_but_a_restore_does_not() {
    for (reason, expected) in [(Reason::Recover, true), (Reason::SessionRestore, false)] {
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));

        let id = f.add_client();
        f.roundtrip(id);

        // A window on the active desktop, so the restored one lands somewhere else.
        let holder = f.client(id).create_window();
        let holder_surface = holder.surface.clone();
        holder.commit();
        f.roundtrip(id);
        let window = f.client(id).window(&holder_surface);
        window.attach_new_buffer();
        window.set_size(300, 200);
        window.ack_last_and_commit();
        f.double_roundtrip(id);

        let session_id = format!("{reason:?}-session");
        let session = f.client(id).get_session(reason, Some(&session_id));
        remember(
            &mut f,
            &session_id,
            "main",
            ToplevelRecord {
                state: Some(WindowState::Floating.as_raw()),
                floating_rect: Some([100, 100, 300, 200]),
                workspace: Some(1),
                ..Default::default()
            },
        );

        let (surface, toplevel, qh) = {
            let win = f.client(id).create_window();
            (
                win.surface.clone(),
                win.xdg_toplevel.clone(),
                f.client(id).qh.clone(),
            )
        };
        let _handle =
            session.restore_toplevel(&toplevel, String::from("main"), &qh, String::from("main"));
        f.client(id).window(&surface).commit();
        f.roundtrip(id);
        map_at_configured_size(&mut f, id, &surface);
        f.synoik_complete_animations();

        let urgent = f
            .synoik()
            .layout
            .windows()
            .any(|(_, mapped)| mapped.is_urgent());
        assert_eq!(
            urgent, expected,
            "{reason:?} landing a window on another desktop: urgency should be {expected}"
        );
        let restored = session_window(&mut f, "main");
        assert!(
            f.synoik()
                .layout
                .focus()
                .is_some_and(|focused| focused.window != restored),
            "{reason:?} must not take focus either way"
        );
    }
}

/// **An app with a focused window is not urgent**, even when another of its windows demanded
/// attention from a desktop you are not on.
///
/// Urgency stays per window, as mutter keeps it (`window.c:5090-5091` unsets it for the window
/// that took focus and no other). The dash and dock, though, aggregate per app — so this is the
/// app-level half of that rule, and without it an app that launches a focused window here and a
/// second one on another desktop pokes the dock at you while you are already using it. Reported
/// from the seat on 2026-08-08, and the reason it is checked at snapshot time rather than on a
/// focus change: the second window arrives by *mapping*, long after focus last moved.
#[test]
fn an_app_you_are_looking_at_does_not_demand_attention() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    seed_favorites(&mut f, &["a.desktop"]);

    let id = f.add_client();
    f.roundtrip(id);

    // Window one of app "a": maps where the user is, takes focus.
    let here = {
        let win = f.client(id).create_window();
        win.set_app_id("a");
        win.commit();
        win.surface.clone()
    };
    f.roundtrip(id);
    let window = f.client(id).window(&here);
    window.attach_new_buffer();
    window.set_size(300, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    // Window two of the same app, restored onto the second desktop — the arm that marks urgent.
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "elsewhere",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 100, 300, 200]),
            workspace: Some(1),
            ..Default::default()
        },
    );
    let (surface, toplevel, qh) = {
        let win = f.client(id).create_window();
        win.set_app_id("a");
        (
            win.surface.clone(),
            win.xdg_toplevel.clone(),
            f.client(id).qh.clone(),
        )
    };
    let _handle = session.restore_toplevel(
        &toplevel,
        String::from("elsewhere"),
        &qh,
        String::from("elsewhere"),
    );
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    // The snapshot is where the invariant runs, and it must reach the dash cleared.
    f.synoik().sync_running_apps();
    f.synoik().sync_dash_favorites();
    f.synoik().sync_dock_urgency();

    assert!(
        !f.synoik()
            .layout
            .windows()
            .any(|(_, mapped)| mapped.is_urgent()),
        "no window of a focused app may stay urgent"
    );
    assert!(
        !f.synoik().dash.items().iter().any(|item| item.urgent),
        "so the dash has nothing to poke"
    );
    assert!(
        !f.synoik().dock.is_poking(),
        "and the dock must not poke at an app the user is already in"
    );
}

/// The mapped window registered in the session under `name`.
///
/// Session tests cannot ask the layout for the *focused* window any more: a restored window that
/// lands on a workspace the user is not looking at deliberately does not take focus, so `focus()`
/// answers about some other window entirely. Going through the registration asks the question the
/// test actually means.
#[track_caller]
fn session_window(f: &mut Fixture, name: &str) -> smithay::desktop::Window {
    let synoik = f.synoik();
    let toplevel = synoik
        .session_manager_state
        .live_registrations()
        .into_iter()
        .find(|(_, registered, _)| registered == name)
        .map(|(_, _, toplevel)| toplevel)
        .expect("a live registration under that name");
    synoik
        .layout
        .windows()
        .find(|(_, mapped)| mapped.toplevel().xdg_toplevel() == &toplevel)
        .map(|(_, mapped)| mapped.window.clone())
        .expect("a mapped window for that toplevel")
}

/// The active workspace index of the only monitor.
fn active_idx(f: &mut Fixture) -> usize {
    let synoik = f.synoik();
    synoik
        .layout
        .workspaces()
        .find(|(mon, idx, _)| mon.is_some_and(|mon| mon.active_workspace_idx() == *idx))
        .map(|(_, idx, _)| idx)
        .expect("an active workspace")
}

/// Mapping a window never moves you to it.
///
/// GNOME's rule, and it is not about session restore: `meta_window_show` focuses but never calls
/// `meta_workspace_activate`, and the one activation site in `window.c` (`:3921`) is reached only
/// from an explicit activation. A window that appears elsewhere gets the pulsing indicator instead
/// — "we just set up a pulsing indicator, rather than move windows or workspaces"
/// (`window.c:3891-3899`).
#[test]
fn a_window_mapping_on_another_workspace_does_not_take_you_there() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 100, 300, 200]),
            workspace: Some(1),
            ..Default::default()
        },
    );

    assert_eq!(active_idx(&mut f), 0, "starting on the first desktop");

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    assert_eq!(
        active_idx(&mut f),
        0,
        "the window landed on the second desktop; we must not have followed it"
    );
    let win = f.synoik().layout.focus().map(|f| f.window.clone());
    assert!(
        win.is_none() || {
            let synoik = f.synoik();
            synoik
                .layout
                .session_snapshot(&win.clone().unwrap())
                .unwrap()
                .workspace_idx
                == 0
        },
        "and nothing on another desktop may hold focus"
    );
}

/// Restoring a set of windows must not walk the active workspace across all of them — the jarring
/// case that made this rule worth writing down.
#[test]
fn restoring_several_windows_leaves_the_active_workspace_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    for (name, idx) in [("w0", 0u32), ("w1", 1), ("w2", 2)] {
        remember(
            &mut f,
            &session_id,
            name,
            ToplevelRecord {
                state: Some(WindowState::Floating.as_raw()),
                floating_rect: Some([100, 100, 300, 200]),
                workspace: Some(idx),
                ..Default::default()
            },
        );
    }

    for name in ["w0", "w1", "w2"] {
        let (surface, _handle) = restore_window(&mut f, id, &session, name);
        map_at_configured_size(&mut f, id, &surface);
        f.synoik_complete_animations();
    }

    assert_eq!(
        active_idx(&mut f),
        0,
        "restoring onto three desktops must leave us on the one we started on"
    );
}

/// At login, restoring must not steal focus even for windows landing on the desktop you are on.
///
/// The workspace rule above already covers anything landing elsewhere; this is the remaining case,
/// and the reason the policy is keyed on `reason` at all — five windows coming back on your current
/// desktop must not each grab focus in turn.
#[test]
fn a_login_restore_does_not_steal_focus_from_what_you_are_using() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);

    // Something already focused, on the desktop the restored windows will land on.
    let _mine = map_session_window(&mut f, id, &session, "mine");
    let mine = session_window(&mut f, "mine");

    for name in ["a", "b"] {
        remember(
            &mut f,
            &session_id,
            name,
            ToplevelRecord {
                state: Some(WindowState::Floating.as_raw()),
                floating_rect: Some([100, 100, 300, 200]),
                workspace: Some(0),
                ..Default::default()
            },
        );
    }

    let restoring = f.add_client();
    f.roundtrip(restoring);
    let session2 = f
        .client(restoring)
        .get_session(Reason::SessionRestore, Some(&session_id));
    f.roundtrip(restoring);
    for name in ["a", "b"] {
        let (surface, _handle) = restore_window(&mut f, restoring, &session2, name);
        map_at_configured_size(&mut f, restoring, &surface);
        f.synoik_complete_animations();
    }

    let focused = f.synoik().layout.focus().map(|focus| focus.window.clone());
    assert_eq!(
        focused,
        Some(mine),
        "a session_restore must leave focus where the user left it"
    );
}

/// The one exception, and the client's lever: a window carrying a fresh activation token *does*
/// take you to its workspace. mutter gates exactly this on `allow_workspace_switch = (timestamp
/// != 0)` (`window.c:3866`), so an app that wants you looking at a particular window says so by
/// activating it.
#[test]
fn an_activation_token_takes_you_to_the_windows_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 100, 300, 200]),
            workspace: Some(1),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    {
        let synoik = f.synoik();
        let unmapped = synoik.unmapped_windows.values_mut().next().unwrap();
        unmapped.activation_token_data = Some(XdgActivationTokenData {
            client_id: None,
            serial: None,
            app_id: None,
            surface: None,
            timestamp: Instant::now(),
            user_data: Arc::new(UserDataMap::new()),
        });
    }
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    assert_eq!(
        active_idx(&mut f),
        1,
        "an activated window is allowed to take us to its desktop"
    );
}

/// Growth is driven by a number read off disk, so it is capped. Past the cap the index clamps —
/// the window still maps, it just doesn't get to inflate the strip.
#[test]
fn a_nonsense_saved_workspace_index_does_not_inflate_the_strip() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);

    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            floating_rect: Some([100, 200, 300, 400]),
            workspace: Some(u32::MAX),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    // A literal, not `MAX_NUM_WORKSPACES`: the property is "the strip stays small whatever the
    // file says", and an assertion phrased in terms of the constant it is guarding moves with it
    // and can never fail.
    let count = f.synoik().layout.workspaces().count();
    assert!(
        count <= 64,
        "a corrupt index must not grow the strip without bound, got {count}"
    );
    assert_eq!(
        f.synoik().layout.windows().count(),
        1,
        "and the window must still map"
    );
}

/// The saved rect names the output. When that monitor is gone the window must still map, by the
/// normal placement chain rather than at the saved off-screen coordinates.
#[test]
fn restoring_onto_a_monitor_that_is_gone_still_maps() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Floating.as_raw()),
            // Far off to the right of every connected output.
            floating_rect: Some([9000, 9000, 300, 400]),
            workspace: Some(0),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    let win = f
        .synoik()
        .layout
        .focus()
        .expect("the window must still map somewhere")
        .window
        .clone();
    let rect = f
        .synoik()
        .layout
        .session_snapshot(&win)
        .unwrap()
        .floating_rect
        .expect("it has a rect");
    assert!(
        rect.loc.x < 1280. && rect.loc.y < 720.,
        "it must land on a connected output, got {rect:?}"
    );
}

/// Unmaximizing a restored window returns it to the rect it was saved with — the `saved_rect`
/// round trip the whole format exists for. The window never floated this run, so nothing but the
/// restore can have seeded it.
#[test]
fn unmaximizing_a_restored_window_returns_it_to_the_saved_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);
    let (session, session_id) = new_session(&mut f, id);
    remember(
        &mut f,
        &session_id,
        "main",
        ToplevelRecord {
            state: Some(WindowState::Maximized.as_raw()),
            floating_rect: Some([100, 200, 300, 400]),
            workspace: Some(0),
            ..Default::default()
        },
    );

    let (surface, _handle) = restore_window(&mut f, id, &session, "main");
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);
    map_at_configured_size(&mut f, id, &surface);
    f.synoik_complete_animations();

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let snapshot = f.synoik().layout.session_snapshot(&win).unwrap();
    assert_eq!(
        snapshot.sizing_mode,
        SizingMode::Normal,
        "the window must have unmaximized"
    );
    let rect = snapshot.floating_rect.expect("it has a rect");
    assert_eq!(
        (rect.size.w.round() as i32, rect.size.h.round() as i32),
        (300, 400),
        "unmaximizing must return it to the saved size, not a default"
    );
}

/// Creating a session dirties the store and arms the debounced write, rather than writing inline.
#[test]
fn creating_a_session_schedules_a_write() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    assert!(!f.synoik().session_manager_state.store.is_dirty());
    assert!(f.synoik().session_save_timer.is_none());

    let id = f.add_client();
    f.roundtrip(id);
    let _session = f.client(id).get_session(Reason::Launch, None);
    f.roundtrip(id);

    assert!(
        f.synoik().session_manager_state.store.is_dirty(),
        "the new session must be pending a write"
    );
    assert!(
        f.synoik().session_save_timer.is_some(),
        "the debounced write must be armed"
    );

    // The shutdown path cancels the timer and writes whatever is outstanding.
    f.synoik().flush_session_store();
    assert!(!f.synoik().session_manager_state.store.is_dirty());
    assert!(f.synoik().session_save_timer.is_none());
}

/// Dropping a preview must not re-flow the picker while the dropped window
/// eases back into place. The layout strategy reads the windows' settled
/// frame rects (gnome-shell's `computeLayout` takes `metaWindow`
/// geometry, `workspace.js`), so a tile's move animation must not reach
/// `compute_slots` — its row assignment sorts by `center().y` and its column
/// assignment by `center().x`, so an animating rect re-sorts the whole grid
/// for the length of the animation and then snaps back.
///
/// The endpoints are blind to this: before the pickup and after the settle
/// the layout is identical either way. Sample across the settle.
///
/// The *dropped* window is exempt: since 2026-08-11 it eases from the box it was released
/// at into its slot, which is `dropped_preview_flies_back_into_its_slot`. What must not
/// move is everything else — a preview the drop did not displace.
#[test]
fn overview_drop_does_not_reflow_the_picker_while_the_window_settles() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A big window and two smaller ones: two rows, the small pair sharing the
    // bottom one, which is what makes a mis-sorted grid visible as a swap.
    let _a = map_window_sized(&mut f, id, (1600, 1000), None);
    let win_a = f.synoik().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(&mut f, id, (760, 600), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();
    let _c = map_window_sized(&mut f, id, (740, 480), None);
    let win_c = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    use smithay::utils::{Logical, Rectangle};

    let wins = [win_a, win_b, win_c];
    let slots = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        wins.iter()
            .map(|w| f.synoik().layout.expose_target_rect(w).unwrap())
            .collect()
    };
    let before = slots(&mut f);

    // Pick C's preview up, carry it a short way, and drop it back on its own
    // workspace — `keep_position`, so the settled layout is the one we started
    // with and every sample in between must match it too.
    let rect = before[2];
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(-60., -40.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let samples = f.sample_animation(Duration::from_millis(600), 12, |f| slots(f));
    for (i, sample) in samples.iter().enumerate() {
        assert_eq!(
            &sample[..2],
            &before[..2],
            "the picker must hold its layout across the drop settle, sample {i}"
        );
    }
    // ...and the dropped one lands back exactly where it started, so the grid the samples
    // above were checked against is the one the drop actually settles into.
    assert_eq!(samples.last().unwrap(), &before);
}

/// The previews already on the workspace you drop onto have to make room for the arrival,
/// and they ease into their new slots rather than re-packing on one frame — gnome-shell
/// eases each child from its current allocation whenever the layout changes
/// (`_syncWindowPositions` / `animateAllocation`, `workspace.js:759-766`, `:389-399`).
#[test]
fn a_drop_eases_the_previews_it_displaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A on desktop 1, B on desktop 2.
    let (win_a, win_b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();
    // Workspace-local: the workspace's own placement animates too, and a rect that folded
    // it in would be two motions at once.
    let b_before = f.synoik().layout.expose_slot_local(&win_b).unwrap();

    // Drop A on B's thumbnail: B now shares its desktop and must move over.
    let rect = f.synoik().layout.expose_target_rect(&win_a).unwrap();
    let (tx, ty) = thumbnail_center(&mut f, 1);
    pointer_motion_to(
        &mut f,
        rect.loc.x + rect.size.w / 2.,
        rect.loc.y + rect.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    pointer_motion_to(&mut f, tx, ty);
    // No roundtrip before sampling: it advances the clock, and this ease is 200ms long.
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let samples = f.sample_animation(Duration::from_millis(200), 4, |f| {
        f.synoik().layout.expose_slot_local(&win_b).unwrap()
    });
    f.settle_animations();
    let b_after = f.synoik().layout.expose_slot_local(&win_b).unwrap();
    assert_ne!(
        b_after, b_before,
        "the arrival must have displaced B at all, or there is nothing to ease",
    );

    // It starts where it was and arrives where it is going, having been in between.
    assert_eq!(samples[0], b_before, "B must not jump on the drop frame");
    assert_eq!(samples[4], b_after);
    for (i, sample) in samples[1..4].iter().enumerate() {
        assert!(
            *sample != b_before && *sample != b_after,
            "sample {} sits on an endpoint — B snapped rather than eased: {samples:?}",
            i + 1,
        );
    }
}

/// The dropped preview eases from the box it was released at into its picker slot, rather
/// than appearing there.
///
/// **Divergence (approved 2026-08-11).** gnome-shell pops the added clone up from scale 0
/// (`workspace.js:1235-1243`); it only eases previews that were *already* in the layout
/// (`_syncWindowPositions`, `workspace.js:759-766`). We fly the dropped one too, which is
/// the motion that makes a drop read as a move rather than a teleport.
#[test]
fn dropped_preview_flies_back_into_its_slot() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (1600, 1000), None);
    let _b = map_window_sized(&mut f, id, (760, 600), None);
    let win_b = f.synoik().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let slot = f.synoik().layout.expose_target_rect(&win_b).unwrap();
    pointer_motion_to(
        &mut f,
        slot.loc.x + slot.size.w / 2.,
        slot.loc.y + slot.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    // A short carry, well inside the workspace: a longer one lands on a neighbour, and
    // then the slot it settles into is not the one it started from.
    f.pointer_motion(-60., -40.);
    // The tile is out of the workspace while the drag is in flight, so there is no slot
    // to read here — the drop below is what puts it back.
    assert_eq!(f.synoik().layout.expose_target_rect(&win_b), None);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // It travels: neither stuck at the release box nor already home.
    let samples = f.sample_animation(Duration::from_millis(200), 6, |f| {
        f.synoik().layout.expose_target_rect(&win_b).unwrap()
    });
    let progress = |r: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        (r.loc.x - samples[0].loc.x) / (slot.loc.x - samples[0].loc.x)
    };
    for pair in samples.windows(2) {
        assert!(
            progress(pair[1]) >= progress(pair[0]) - 0.001,
            "the preview must close on its slot monotonically, got {samples:?}",
        );
    }
    assert!(
        progress(samples[1]) > 0.,
        "it must leave the release box, got {samples:?}",
    );
    assert!(
        progress(samples[2]) < 1.,
        "it must not be home a third of the way in, got {samples:?}",
    );

    f.settle_animations();
    assert_eq!(
        f.synoik().layout.expose_target_rect(&win_b),
        Some(slot),
        "and it must land on the slot the picker gives it",
    );
}

/// Restore must not drift: a window that is saved, restored, saved again and
/// restored again comes back at the same rect every cycle, not a slightly
/// smaller one. Every existing restore test is a *single* cycle, and a
/// per-cycle loss of a pixel or a border is invisible in one — it only reads as
/// "my windows shrink every time I log in".
///
/// Two entry sizes, because they reach the first save differently: the modest
/// one is saved at the size the client asked for, the oversized one trips
/// GNOME's map-time auto-maximize on its first (un-restored) run and is saved
/// at the sqrt(0.8)-clamped rect instead. Both then have to hold still.
///
/// This does *not* pin the auto-maximize skip — by cycle 2 the clamped rect is
/// already under the 80% threshold, so the guard has nothing to do here.
/// `a_restored_window_is_not_auto_maximized` is what pins that.
#[test]
fn restoring_the_same_window_repeatedly_does_not_drift() {
    for (label, first_size) in [("modest", (900u16, 600u16)), ("oversized", (1200, 680))] {
        let mut f = Fixture::new();
        f.add_output(1, (1280, 720));

        // Run 1: a plain add (no restore), mapped at the client's own size.
        let first = f.add_client();
        f.roundtrip(first);
        let (session, session_id) = new_session(&mut f, first);

        let window = f.client(first).create_window();
        let surface = window.surface.clone();
        let toplevel = window.xdg_toplevel.clone();
        let qh = f.client(first).qh.clone();
        session.add_toplevel(&toplevel, String::from("main"), &qh, String::from("main"));
        f.client(first).window(&surface).commit();
        f.roundtrip(first);
        let w = f.client(first).window(&surface);
        w.attach_new_buffer();
        w.set_size(first_size.0, first_size.1);
        w.ack_last_and_commit();
        f.double_roundtrip(first);
        f.synoik_complete_animations();

        let win = f.synoik().layout.focus().unwrap().window.clone();
        let first_rect = f
            .synoik()
            .layout
            .session_snapshot(&win)
            .unwrap()
            .floating_rect
            .expect("a floated window has a rect");

        let w = f.client(first).window(&surface);
        w.attach_null_buffer();
        w.commit();
        f.double_roundtrip(first);
        drop(session);

        for cycle in 2..=5 {
            let next = f.add_client();
            f.roundtrip(next);
            let session = f
                .client(next)
                .get_session(Reason::SessionRestore, Some(&session_id));
            f.roundtrip(next);
            let (surface, _handle) = restore_window(&mut f, next, &session, "main");
            let size = map_at_configured_size(&mut f, next, &surface);
            f.synoik_complete_animations();

            assert_eq!(
                size,
                (
                    first_rect.size.w.round() as i32,
                    first_rect.size.h.round() as i32
                ),
                "[{label}] cycle {cycle} must be configured at the saved size"
            );

            let win = f.synoik().layout.focus().unwrap().window.clone();
            let rect = f
                .synoik()
                .layout
                .session_snapshot(&win)
                .unwrap()
                .floating_rect
                .expect("the restored window has a rect");
            assert_eq!(
                (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h),
                (
                    first_rect.loc.x,
                    first_rect.loc.y,
                    first_rect.size.w,
                    first_rect.size.h
                ),
                "[{label}] cycle {cycle} drifted from the first run's rect"
            );

            let w = f.client(next).window(&surface);
            w.attach_null_buffer();
            w.commit();
            f.double_roundtrip(next);
            drop(session);
        }
    }
}

/// A window that changes its title (or app id) between the initial configure
/// and the map still comes back where it was saved.
///
/// Restore seeds itself into the *rules* so that placement, sizing and state
/// stay one path — but `title_changed` recomputes those rules from the config's
/// window rules alone and assigns over the whole struct, which drops every seed
/// the restore wrote. Clients set their title on the first commit after the
/// configure all the time (a terminal names itself after its shell), so this is
/// the common case, not a corner.
///
/// The tell on the live seat was that the *workspace* restored and the position
/// did not: the workspace rides `RestoreOnMap`, which survives the recompute.
/// Size survives too, because its configure has already gone out — so only the
/// position, which is read from the rules at map time, is actually lost.
#[test]
fn a_title_change_between_configure_and_map_keeps_the_restored_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));

    let first = f.add_client();
    f.roundtrip(first);
    let (session, session_id) = new_session(&mut f, first);

    let window = f.client(first).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    let qh = f.client(first).qh.clone();
    session.add_toplevel(&toplevel, String::from("main"), &qh, String::from("main"));
    f.client(first).window(&surface).commit();
    f.roundtrip(first);
    let w = f.client(first).window(&surface);
    w.attach_new_buffer();
    w.set_size(400, 300);
    w.ack_last_and_commit();
    f.double_roundtrip(first);
    f.synoik_complete_animations();

    // Somewhere the placement cascade would never put it, so that coming back
    // centred is distinguishable from coming back restored.
    f.synoik_state().do_action(
        Action::MoveFloatingWindowById {
            id: None,
            x: synoik_ipc::PositionChange::SetFixed(150.),
            y: synoik_ipc::PositionChange::SetFixed(90.),
        },
        false,
    );
    f.synoik_complete_animations();
    f.double_roundtrip(first);

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let saved = f
        .synoik()
        .layout
        .session_snapshot(&win)
        .unwrap()
        .floating_rect
        .expect("a floated window has a rect");

    let w = f.client(first).window(&surface);
    w.attach_null_buffer();
    w.commit();
    f.double_roundtrip(first);
    drop(session);

    let second = f.add_client();
    f.roundtrip(second);
    let session = f
        .client(second)
        .get_session(Reason::SessionRestore, Some(&session_id));
    f.roundtrip(second);
    let (surface, _handle) = restore_window(&mut f, second, &session, "main");

    // The initial configure has landed. Now name ourselves, as a terminal does
    // once its shell is up, and only then map.
    f.client(second)
        .window(&surface)
        .set_title("gustavo@host: ~");
    f.roundtrip(second);
    map_at_configured_size(&mut f, second, &surface);
    f.synoik_complete_animations();

    let win = f.synoik().layout.focus().unwrap().window.clone();
    let got = f
        .synoik()
        .layout
        .session_snapshot(&win)
        .unwrap()
        .floating_rect
        .expect("the restored window has a rect");
    assert_eq!(
        (got.loc.x, got.loc.y),
        (saved.loc.x, saved.loc.y),
        "a title change after the configure must not drop the restored position"
    );
}

/// Mapping a window must not configure it smaller than the size it chose.
///
/// A client that draws its own decorations declares a window geometry covering
/// them — `(0, -35, w, content + 35)`, negative because the header subsurface
/// sits above the content origin. A subsurface with no buffer yet is not in the
/// surface tree's bounding box (smithay's `bbox_from_surface_tree` skips it, and
/// mutter's `meta_wayland_subsurface_union_geometry` skips it too), so until the
/// header draws, the declared geometry is *correctly* clamped down to the bare
/// content. That truncation is a transient and heals on the next commit.
///
/// It only does damage if the compositor reads it and hands it back as a
/// configure size, because the client must honour a configure as a
/// compositor-driven resize. Then the transient is frozen into the client's real
/// size, and it loses its header again every time it starts — 35px a launch.
///
/// GNOME does not do this: a client-driven geometry change updates min/max size
/// and recalculates features, and nothing else (mutter
/// `meta-wayland-xdg-shell.c:1081-1103`). The window's size comes from the
/// compositor's own model, never read back off the client.
#[test]
fn a_window_is_not_configured_smaller_than_it_asked_for() {
    const HEADER: i32 = 35;
    const CONTENT: i32 = 300;

    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // Map with the decorations declared but not yet drawn, which is the state a
    // real toolkit is in on its first frame.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(400, CONTENT as u16);
    w.set_window_geometry(0, -HEADER, 400, CONTENT + HEADER);
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    let configured = f
        .client(id)
        .window(&surface)
        .recent_configures()
        .last()
        .expect("the window must have been configured")
        .size;

    // Zero means "you choose", which is the correct answer here. Anything else
    // must at least not be shorter than what the client asked for.
    assert!(
        configured.1 == 0 || configured.1 >= CONTENT + HEADER,
        "mapping configured the window to {}px tall, shorter than the {}px of \
         window geometry it declared — it will shrink to fit and lose its \
         decorations, once per launch",
        configured.1,
        CONTENT + HEADER,
    );
}

/// `org.gnome.desktop.interface enable-animations` turns our animations off, the way it turns
/// mutter's off.
///
/// We read this key from the start — the introspect interface publishes it, and the portal animates
/// its dialogs to match — but for a long time the shell itself kept animating regardless. A session
/// that asked for no animations (an a11y profile, a VM image, a user who dislikes them) got them
/// anyway, and disagreed with every GTK app in it.
///
/// It is also the deterministic mode a test rig needs: no transition to race, reached through the
/// same key a user has rather than a test-only switch.
#[test]
fn enable_animations_off_stops_the_shell_animating() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    assert!(
        !f.synoik().clock.should_complete_instantly(),
        "precondition: animations are on by default",
    );

    // Drive the real entry point — the one the gsettings watcher calls on a change.
    f.synoik_state().synoik.gnome_settings.enable_animations = false;
    f.synoik_state().refresh_animation_clock();
    assert!(
        f.synoik().clock.should_complete_instantly(),
        "enable-animations=false must reach the animation clock",
    );

    // Observable, not just the flag: opening the overview leaves nothing running.
    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik().advance_animations();
    assert!(
        f.synoik().layout.is_overview_open(),
        "the overview must still open — off means instant, not inert",
    );
    assert!(
        !f.synoik().layout.are_animations_ongoing(Some(&output)),
        "with animations off the transition must already be over",
    );

    // And back: turning the key on animates again, so this is a setting and not a one-way door.
    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.settle_animations();
    f.synoik_state().synoik.gnome_settings.enable_animations = true;
    f.synoik_state().refresh_animation_clock();
    assert!(!f.synoik().clock.should_complete_instantly());

    f.synoik_state().do_action(Action::ToggleOverview, false);
    f.synoik().advance_animations();
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "with animations on the overview transition must actually run",
    );
}

/// `synoik msg windows` reports the window states the client was actually told.
///
/// Everything a consumer could ask about a window's shape used to have to be inferred: the listing
/// carried focus and floating, and nothing about maximized, fullscreen, tiled or activated. That
/// leaves an outside test (a toolkit's own suite, say) checking its beliefs against itself, which
/// is exactly where "I dropped my shadow margins on maximize" hides — the compositor's view is the
/// only independent witness.
///
/// `is_activated` is deliberately separate from `is_focused`: focus is ours, activated is what the
/// client last acked, and they disagree while a configure is in flight.
#[test]
fn ipc_windows_report_maximized_and_tiled_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_window_for_app(&mut f, id, "org.example.App");
    f.synoik_state().ipc_refresh_layout();

    let win = |f: &mut Fixture| -> synoik_ipc::Window {
        f.synoik()
            .ipc_server
            .as_ref()
            .unwrap()
            .event_stream_state()
            .windows
            .windows
            .values()
            .next()
            .expect("the mapped window must be in the IPC listing")
            .clone()
    };

    let w = win(&mut f);
    assert!(!w.is_maximized, "a freshly mapped window is not maximized");
    assert!(!w.is_fullscreen);
    assert!(!w.tiled_edges.any(), "nor tiled against anything");

    // Maximize through the real action, then let the client ack: the state is reported from the
    // acked configure, so a test that never acks is asserting on a window the client has not
    // agreed to be yet.
    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_state().ipc_refresh_layout();

    let w = win(&mut f);
    assert!(w.is_maximized, "Maximize must show up in the listing");
    assert!(
        !w.tiled_edges.any(),
        "a maximized window is maximized, not tiled — they are different xdg states",
    );

    // Tiling to an edge is the other half: GNOME's half-tile sets one vertical edge plus top and
    // bottom, and drops the maximized state.
    f.synoik_state().do_action(Action::ToggleTiledLeft, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_state().ipc_refresh_layout();

    let w = win(&mut f);
    assert!(!w.is_maximized, "tiling replaces the maximized state");
    assert!(
        w.tiled_edges.any(),
        "a tiled window must report the edges it is tiled against",
    );
}

/// The un-maximize resize rides gnome-shell's size-change curve: `WINDOW_ANIMATION_TIME`
/// (250 ms) on `EASE_OUT_QUAD` (`js/ui/windowManager.js` `_sizeChangedWindow`).
///
/// This is pinned on the case that exposed it — a window that auto-maximized at map, whose
/// restore rect is mutter's sqrt(0.8) clamp of the work area and whose position does not move
/// at all. There the resize is the *only* thing on screen, so a front-loaded curve (niri's
/// critically-damped spring spent ~77% of the travel in the first 100 ms) reads as a snap.
#[test]
fn unmaximize_resizes_on_gnomes_size_change_curve() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Big enough to trip the map-time auto-maximize (mutter's place.c 80% rule).
    let surface = map_window_sized(&mut f, id, (1900, 1040), None);
    assert_eq!(
        focused_window_pos(&mut f),
        (0., 32.),
        "a work-area-sized window is placed at the work-area origin",
    );

    // Take the maximized size the auto-maximize configured.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(1920, 1048);
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(1717, 937);
    w.ack_last_and_commit();
    f.double_roundtrip(id);

    let from = 1048.;
    let to = 937.;
    let heights = f.sample_animation(Duration::from_millis(250), 4, |f| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap()
            .active_workspace_ref()
            .tiles()
            .next()
            .unwrap()
            .animated_window_size()
            .h
    });

    // EASE_OUT_QUAD is 1 - (1 - t)², sampled at t = 0, ¼, ½, ¾, 1.
    let expected: Vec<f64> = [0., 0.25, 0.5, 0.75, 1.]
        .iter()
        .map(|t: &f64| {
            let p = 1. - (1. - t) * (1. - t);
            (from + (to - from) * p).round()
        })
        .collect();
    assert_eq!(
        heights.iter().map(|h| h.round()).collect::<Vec<_>>(),
        expected,
        "the un-maximize must follow GNOME's 250 ms ease-out-quad, not niri's spring",
    );
}

/// A sizing-mode transition always animates its move, however short.
///
/// Free moves have a 10 px dead zone so that rounding jitter does not animate, but a maximize is
/// something the user asked for: a window that happens to sit a few pixels off the work-area
/// origin must not lose half its transition to that threshold.
#[test]
fn a_short_maximize_move_still_animates() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = map_window_sized(&mut f, id, (800, 600), None);

    f.synoik().layout.move_floating_window(
        None,
        synoik_ipc::PositionChange::SetFixed(4.),
        synoik_ipc::PositionChange::SetFixed(3.),
        false,
    );
    f.synoik_complete_animations();
    assert_eq!(focused_window_pos(&mut f), (4., 35.));

    // (4, 35) -> (0, 32) is 5 px, inside the free-move dead zone.
    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);

    let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
    let offset = mon
        .unwrap()
        .active_workspace_ref()
        .tiles()
        .next()
        .unwrap()
        .render_offset();
    assert!(
        offset.x != 0. || offset.y != 0.,
        "a maximize must animate its move even when it is shorter than the free-move threshold, \
         got offset {offset:?}",
    );
}

/// A client may take a sizing-mode change on one commit and redraw at the new size on the next,
/// and the animation belongs to the second one.
///
/// This is what made un-maximize snap while maximize animated: GTK4 answers a maximize in one
/// commit, but on the way out it has to put its CSD shadow margins back, so the commit that acks
/// the un-maximize still carries the maximized size. Spending the arm there left the resize that
/// followed unarmed — measured on the seat as 8 snaps out of 8, against 8 clean animations in the
/// other direction.
#[test]
fn a_resize_the_client_defers_to_its_next_commit_still_animates() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);

    let resize_animation = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap()
            .active_workspace_ref()
            .tiles()
            .next()
            .unwrap()
            .resize_animation()
            .is_some()
    };

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);

    // Ack the configure without redrawing: the window takes the maximized state at its old size.
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(
        !resize_animation(&mut f),
        "nothing has resized yet, so there is nothing to animate on the ack",
    );

    // The redraw at the configured size, one commit later.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(1920, 1048);
    w.commit();
    f.double_roundtrip(id);
    assert!(
        resize_animation(&mut f),
        "the commit that actually resizes must still be animated",
    );
}

/// The held arm lasts exactly one commit, and only across a sizing-mode change.
///
/// Without both bounds it becomes a licence to animate whatever the client does next: a window
/// that resizes itself — an EGL surface changing size, a client honouring nothing in particular —
/// must not pick up an animation left lying around by an earlier configure.
#[test]
fn a_held_arm_does_not_animate_a_later_client_resize() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);

    let resize_animation = |f: &mut Fixture| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        mon.unwrap()
            .active_workspace_ref()
            .tiles()
            .next()
            .unwrap()
            .resize_animation()
            .is_some()
    };

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);

    // One commit that does not resize spends the held arm...
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    // ...so this resize, which the client chose on its own, is not animated.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(1920, 1048);
    w.commit();
    f.double_roundtrip(id);
    assert!(
        !resize_animation(&mut f),
        "a client-side resize must not inherit an animation from an older configure",
    );
}

/// A maximize moves and grows as one transition, not a slide followed by a grow.
///
/// The move can start the moment the user asks for it; the resize cannot, because it waits on the
/// client committing the new size. Left to run on its own the move finished first, so a centered
/// window slid into the corner at its old size and only then grew — measured on a real
/// gnome-system-monitor as six frames of constant width before the first pixel of growth.
/// An untile is a state change too, even though xdg-shell keeps the tiled edges out of the
/// maximized/fullscreen state: it must hold its animate arm across the acking commit and park its
/// move for the resize, exactly like an unmaximize.
#[test]
fn an_untile_moves_and_shrinks_together() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik().layout.move_floating_window(
        None,
        synoik_ipc::PositionChange::SetFixed(400.),
        synoik_ipc::PositionChange::SetFixed(200.),
        false,
    );
    f.synoik_complete_animations();
    let restored_pos = focused_window_pos(&mut f).0;

    // Tile left, and let the client answer it.
    f.synoik_state().do_action(Action::ToggleTiledLeft, false);
    f.double_roundtrip(id);
    let tiled_size = f
        .client(id)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(tiled_size.0 as u16, tiled_size.1 as u16);
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    let tiled_pos = focused_window_pos(&mut f).0;
    assert!(
        tiled_pos < restored_pos,
        "tiling left must move the window left"
    );

    // Super+Down on a tiled window untiles it.
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);

    // The client acks without redrawing yet — GTK4's shape on the way out of a sized state. The
    // window must not slide back at the tiled width in the meantime.
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(
        focused_window_pos(&mut f).0,
        tiled_pos,
        "the move must wait for the resize rather than run ahead of it",
    );

    // The commit that resizes starts both halves.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(800, 600);
    w.commit();
    f.double_roundtrip(id);

    let samples = f.sample_animation(Duration::from_millis(250), 4, |f| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let ws = mon.unwrap().active_workspace_ref();
        let (tile, pos, _) = ws.tiles_with_render_positions().next().unwrap();
        (pos.x, tile.animated_window_size().w)
    });

    let tiled_w = tiled_size.0 as f64;
    for (i, (x, w)) in samples.iter().enumerate() {
        let moved = (x - tiled_pos) / (restored_pos - tiled_pos);
        let shrunk = (tiled_w - w) / (tiled_w - 800.);
        assert!(
            (moved - shrunk).abs() < 0.02,
            "sample {i}: moved {moved:.3} of the way but shrunk {shrunk:.3} — the halves are not \
             running as one transition",
        );
    }
    assert!(
        samples[0].1 > tiled_w - 1. && samples[4].1 < 801.,
        "the resize must actually run over the sampled window, got {:?} -> {:?}",
        samples[0],
        samples[4],
    );
}

#[test]
fn a_maximize_moves_and_grows_together() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik().layout.move_floating_window(
        None,
        synoik_ipc::PositionChange::SetFixed(400.),
        synoik_ipc::PositionChange::SetFixed(200.),
        false,
    );
    f.synoik_complete_animations();
    assert_eq!(focused_window_pos(&mut f), (400., 232.));

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);

    // The window is not going to redraw for another commit. Its rendered position must not creep
    // toward the corner in the meantime.
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(
        focused_window_pos(&mut f),
        (400., 232.),
        "the move must wait for the resize rather than run ahead of it",
    );

    // The commit that resizes starts both halves.
    let w = f.client(id).window(&surface);
    w.attach_new_buffer();
    w.set_size(1920, 1048);
    w.commit();
    f.double_roundtrip(id);

    let samples = f.sample_animation(Duration::from_millis(250), 4, |f| {
        let (mon, _, _) = f.synoik().layout.workspaces().next().unwrap();
        let ws = mon.unwrap().active_workspace_ref();
        let (tile, pos, _) = ws.tiles_with_render_positions().next().unwrap();
        (pos.x, tile.animated_window_size().w)
    });

    // Both halves cover the same fraction of their travel at every sample: one transition.
    for (i, (x, w)) in samples.iter().enumerate() {
        let moved = (400. - x) / (400. - 0.);
        let grown = (w - 800.) / (1920. - 800.);
        assert!(
            (moved - grown).abs() < 0.02,
            "sample {i}: moved {moved:.3} of the way but grew {grown:.3} — the halves are not \
             running as one transition",
        );
    }
    assert!(
        samples[0].0 > 399. && samples[4].0 < 1.,
        "the move must actually run over the sampled window, got {:?} -> {:?}",
        samples[0],
        samples[4],
    );
}

/// Redraws the client at `size` and lets the compositor see it.
fn redraw_window_at(f: &mut Fixture, id: ClientId, surface: &WlSurface, size: (u16, u16)) {
    let w = f.client(id).window(surface);
    w.attach_new_buffer();
    w.set_size(size.0, size.1);
    w.commit();
    f.double_roundtrip(id);
}

/// The last configure the client got, as `(size, maximized)`.
fn last_configure(f: &mut Fixture, id: ClientId, surface: &WlSurface) -> ((i32, i32), bool) {
    let (_, configure) = f
        .client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap();
    (
        configure.size,
        configure.states.contains(&xdg_toplevel::State::Maximized),
    )
}

#[test]
fn a_lazy_ack_does_not_corrupt_the_restore_rect() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // Maximize for real: the client answers and redraws at the work-area size.
    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    redraw_window_at(&mut f, id, &surface, (1920, 1048));

    // Unmaximize, and let the client ack it *at the old size* — GTK4 does exactly this, putting
    // its CSD margins back on the commit after (see `448e2dc5`). For that one commit the window
    // legitimately *is* work-area sized while its restore is still in flight.
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);

    // Maximize again inside that window, then unmaximize once more. The rect to come back to is
    // still the 800x600 the window had before any of this — not the size it was caught wearing.
    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);

    assert_eq!(
        last_configure(&mut f, id, &surface),
        ((800, 600), false),
        "the second unmaximize must restore the pre-maximize size, not the work area",
    );
}

#[test]
fn a_restore_the_client_answered_is_the_clients_size_again() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    redraw_window_at(&mut f, id, &surface, (1920, 1048));

    // This time the client answers the restore honestly, and then resizes itself smaller.
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    redraw_window_at(&mut f, id, &surface, (800, 600));
    redraw_window_at(&mut f, id, &surface, (640, 480));

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);

    assert_eq!(
        last_configure(&mut f, id, &surface),
        ((640, 480), false),
        "once the client has answered a restore, its own size is the rect to come back to",
    );
}

#[test]
fn a_window_resized_back_to_the_work_area_still_saves_its_own_rect() {
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_sized(&mut f, id, (800, 600), None);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    redraw_window_at(&mut f, id, &surface, (1920, 1048));

    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    redraw_window_at(&mut f, id, &surface, (800, 600));

    // The user then drags the floating window out to exactly the work-area size. That is the same
    // size the restore was in flight *from*, so a stale in-flight marker would read as "the client
    // still has not answered" and hand back the old 800x600.
    redraw_window_at(&mut f, id, &surface, (1920, 1048));

    f.synoik_state().do_action(Action::Maximize, false);
    f.double_roundtrip(id);
    f.synoik_state().do_action(Action::Unmaximize, false);
    f.double_roundtrip(id);

    assert_eq!(
        last_configure(&mut f, id, &surface),
        ((1920, 1048), false),
        "a floating window that happens to be work-area sized comes back to that size",
    );
}
