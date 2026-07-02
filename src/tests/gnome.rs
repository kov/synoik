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
use smithay::backend::input::ButtonState;
use smithay::input::keyboard::Keysym;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;

/// Linux evdev codes (`input-event-codes.h`) for the inputs these tests inject.
const KEY_A: u32 = 30;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;
const BTN_LEFT: u32 = 0x110;

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
        !f.niri().layout.is_overview_open(),
        "overview must start closed"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "a lone Super tap opens the overview"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "a second Super tap closes it"
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
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
        "a lone right Super tap opens the overview by default"
    );
}

/// Using Super as a modifier (Super+key) must *not* trigger the overlay key:
/// once another key participates, the press is no longer a lone tap.
#[test]
fn super_plus_key_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_A);
    f.key_release(KEY_A);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
        "Super+key must not trigger the overlay key"
    );
}

/// Pointer activity between the Super press and release cancels the tap, so the
/// overview must not open (mutter clears `overlay_key_only_pressed` on a click).
#[test]
fn super_then_click_does_not_toggle_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.key_press(KEY_LEFTMETA);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
        "a click between Super press and release must cancel the tap"
    );
}

/// The overlay key honors `org.gnome.mutter overlay-key`: setting it to `None`
/// (mutter's empty-string "disabled") means a Super tap does nothing.
#[test]
fn overlay_key_setting_can_disable() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri().gnome_settings.overlay_keys.clear();

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
        "a disabled overlay key must not open the overview"
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
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "the overlay key must be inert while the focused window inhibits shortcuts"
    );

    f.client(id).release_shortcuts_inhibitor(&surface);
    f.roundtrip(id);

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "releasing the inhibitor must restore the overlay key"
    );
}

/// The overlay key is rebindable: pointing the setting at `Super_R` makes the
/// right Super the trigger, and the (now non-overlay) left Super inert.
#[test]
fn overlay_key_setting_rebinds() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri().gnome_settings.overlay_keys = vec![Keysym::Super_R];

    // Left Super is no longer the overlay key.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "left Super must be inert once the overlay key is Super_R"
    );

    // Right Super now toggles the overview.
    f.key_press(KEY_RIGHTMETA);
    f.key_release(KEY_RIGHTMETA);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "a right Super tap must open the overview when it is the overlay key"
    );
}
