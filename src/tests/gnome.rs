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
use niri_config::{Action, Config};
use smithay::backend::input::ButtonState;
use smithay::input::keyboard::Keysym;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel;
use smithay::utils::user_data::UserDataMap;
use smithay::wayland::xdg_activation::XdgActivationTokenData;
use wayland_client::protocol::wl_keyboard::KeyState as WlKeyState;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::gnome::{Accel, AccelMods, AccelTrigger, FocusNewWindows, GnomeKeyAction};
use crate::ui::osd::OsdLevel;
use crate::utils::get_monotonic_time;

/// Linux evdev codes (`input-event-codes.h`) for the inputs these tests inject.
const KEY_ESC: u32 = 1;
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
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;
const BTN_LEFT: u32 = 0x110;
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
        f.niri().layout.is_overview_open(),
        "the first tap opens the overview"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "a second tap during the open animation must not close the overview"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
        "it must shift a state up, into the app grid"
    );

    // A third tap, now that nothing is transitioning, toggles as always — the
    // shift is clamped at APP_GRID, so it never toggles the grid back down.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
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
        !f.niri().layout.is_overview_open(),
        "a second tap after the open animation closes the overview"
    );
    assert!(
        !f.niri().layout.is_app_grid_open(),
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
        f.niri().layout.is_app_grid_open(),
        "two taps within 250 ms must reach the app grid with animations off"
    );

    // Past the window, the same tap is a plain toggle again.
    f.advance_input_time(300);
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.settle_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
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

    // Click mid-screen, clear of the panel's Activities button (which itself
    // toggles the overview) so this exercises only the tap-cancel.
    f.pointer_motion(960., 540.);

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

    // A canceled tap (Super+A): the client sees all four key events.
    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_A);
    f.key_release(KEY_A);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).take_key_events(),
        vec![
            (KEY_LEFTMETA, WlKeyState::Pressed),
            (KEY_A, WlKeyState::Pressed),
            (KEY_A, WlKeyState::Released),
            (KEY_LEFTMETA, WlKeyState::Released),
        ],
        "a canceled tap must deliver both Super key events to the client"
    );

    // A firing tap: the press is delivered, the release is swallowed.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert!(
        f.niri().layout.is_overview_open(),
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
    f.niri().gnome_settings.overlay_keys.clear();

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
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

/// The numbered workspace switches (`switch-to-workspace-N`, default
/// `<Super>N` in the GNOME session) focus that workspace.
#[test]
fn super_number_switches_workspaces() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Mapping a window gives the monitor an occupied workspace plus the
    // trailing empty one, so there is a workspace 2 to go to.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    assert_eq!(
        f.niri()
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
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        1,
        "<Super>2 must focus the second workspace"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
        "using Super as a modifier must not have fired the overlay key"
    );

    f.key_press(KEY_LEFTMETA);
    f.key_press(KEY_1);
    f.key_release(KEY_1);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0,
        "<Super>1 must focus the first workspace again"
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
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
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
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
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
    f.niri_complete_animations();

    let monitor = f.niri().layout.active_monitor_ref().unwrap();
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
        .niri()
        .gnome_settings
        .keybindings
        .iter_mut()
        .find(|kb| kb.action == GnomeKeyAction::Close)
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

/// GNOME keybindings take precedence over binds from the niri config file:
/// the GSettings store is the keybinding config of a GNOME session, so a
/// conflicting config bind must lose.
#[test]
fn gnome_keybindings_beat_niri_config_binds() {
    let mut config = Config::default();
    config.binds.0.push(niri_config::Bind {
        key: niri_config::Key {
            trigger: niri_config::Trigger::Keysym(Keysym::F4),
            modifiers: niri_config::Modifiers::ALT,
        },
        action: Action::ToggleOverview,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    });
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let surface = map_focused_window(&mut f, id);

    f.key_press(KEY_LEFTALT);
    f.key_press(KEY_F4);
    f.key_release(KEY_F4);
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.client(id).window(&surface).close_requested,
        "the GNOME close binding must win the conflict"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
        "the conflicting niri config bind must not have fired"
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
    let second_focused = f.niri().layout.focus().unwrap().id();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(
        f.niri().switcher.is_open(),
        "Alt+Tab must open the window switcher"
    );
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    // Let the focus change go through a refresh cycle, like in a real event
    // loop iteration, so the MRU bookkeeping sees it.
    f.double_roundtrip(id);

    assert!(
        !f.niri().switcher.is_open(),
        "releasing Alt must commit and close the switcher"
    );
    let now_focused = f.niri().layout.focus().unwrap().id();
    assert_ne!(
        now_focused, second_focused,
        "Alt+Tab must move focus to the previous window"
    );

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
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
        f.niri().run_dialog.is_open(),
        "<Alt>F2 must open the run dialog"
    );

    tap(&mut f, KEY_Z);
    assert_eq!(
        f.niri().run_dialog.entry(),
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

    f.niri_state().do_action(Action::ShowRunDialog, false);
    assert!(f.niri().run_dialog.is_open());

    tap(&mut f, KEY_ESC);
    assert!(
        !f.niri().run_dialog.is_open(),
        "an Escape tap must close the run dialog"
    );
}

/// An unknown command shows "Command not found" in-dialog and keeps the
/// dialog open with the entry intact — and still enters the history
/// (gnome-shell's `_run` records the attempt before trying it).
#[test]
fn run_dialog_unknown_command_shows_error_and_stays_open() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state().do_action(Action::ShowRunDialog, false);
    tap(&mut f, KEY_Z);
    tap(&mut f, KEY_Z);
    tap(&mut f, KEY_ENTER);

    assert!(
        f.niri().run_dialog.is_open(),
        "a failed command must keep the dialog open"
    );
    assert_eq!(
        f.niri().run_dialog.entry(),
        "zz",
        "the entry must be intact"
    );
    assert_eq!(
        f.niri().run_dialog.error(),
        Some("Command not found"),
        "the error must show in-dialog"
    );
    assert_eq!(
        f.niri().gnome_settings.command_history,
        vec!["zz".to_owned()],
        "even a failed command enters the history"
    );

    // Enter on an empty entry is also an error (the tokenizer rejects it),
    // not a close; and empty input never enters the history.
    f.niri_state().do_action(Action::ShowRunDialog, false);
    tap(&mut f, KEY_ENTER);
    assert!(f.niri().run_dialog.is_open());
    assert_eq!(
        f.niri().gnome_settings.command_history,
        vec!["zz".to_owned()]
    );
}

/// A valid command spawns and closes the dialog, entering the history; Up
/// then recalls it (gnome-shell's HistoryManager).
#[test]
fn run_dialog_runs_command_and_records_history() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state().do_action(Action::ShowRunDialog, false);
    for key in [KEY_T, KEY_R, KEY_U, KEY_E] {
        tap(&mut f, key);
    }
    tap(&mut f, KEY_ENTER);

    assert!(
        !f.niri().run_dialog.is_open(),
        "a successful run must close the dialog"
    );
    assert_eq!(
        f.niri().gnome_settings.command_history,
        vec!["true".to_owned()]
    );

    f.niri_state().do_action(Action::ShowRunDialog, false);
    assert_eq!(
        f.niri().run_dialog.entry(),
        "",
        "the entry must open cleared"
    );
    tap(&mut f, KEY_UP);
    assert_eq!(
        f.niri().run_dialog.entry(),
        "true",
        "Up must recall the last history entry"
    );
    tap(&mut f, KEY_DOWN);
    assert_eq!(
        f.niri().run_dialog.entry(),
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
    f.niri().gnome_settings.disable_command_line = true;

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F2);
    f.key_release(KEY_LEFTALT);
    assert!(
        !f.niri().run_dialog.is_open(),
        "the lockdown key must disable the run dialog"
    );

    f.niri_state().do_action(Action::ShowRunDialog, false);
    assert!(
        !f.niri().run_dialog.is_open(),
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
        .niri_state()
        .grab_accelerator("<Super>z", 1, 0, ":1.10".to_owned());
    assert_ne!(action, 0, "a free combo must be grabbable");

    assert_eq!(
        f.niri_state()
            .grab_accelerator("<Super>z", 1, 0, ":1.11".to_owned()),
        0,
        "a combo held by another grab must be refused"
    );
    assert_eq!(
        f.niri_state()
            .grab_accelerator("<Alt>F4", 1, 0, ":1.11".to_owned()),
        0,
        "a combo held by a GNOME keybinding must be refused"
    );
    assert_eq!(
        f.niri_state()
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
        f.niri().accel_grab_release_pending.is_empty(),
        "the release must have cleared the pending deactivation"
    );

    assert!(
        !f.niri_state().ungrab_accelerator(action, ":1.11"),
        "only the owner may ungrab"
    );
    assert!(f.niri_state().ungrab_accelerator(action, ":1.10"));

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
    f.niri_complete_animations();

    surface
}

/// The focused window's position within the workspace view.
fn focused_window_pos(f: &mut Fixture) -> (f64, f64) {
    let niri = f.niri();
    let focused = niri.layout.focus().unwrap().id();
    let ws = niri.layout.active_workspace().unwrap();
    let (_, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(tile, _, _)| tile.window().id() == focused)
        .unwrap();
    (pos.x, pos.y)
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

    let niri = f.niri();
    let focused = niri.layout.focus().unwrap().window.clone();
    let ws = niri.layout.active_workspace().unwrap();
    assert!(
        ws.is_floating(&focused),
        "a new window must open floating by default"
    );

    let mut f = Fixture::with_config(scrolling(Config::default()));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window_sized(&mut f, id, (100, 100), None);

    let niri = f.niri();
    let focused = niri.layout.focus().unwrap().window.clone();
    let ws = niri.layout.active_workspace().unwrap();
    assert!(
        !ws.is_floating(&focused),
        "windowing-mode scrolling must keep niri's tiled-by-default behavior"
    );
}

/// New windows follow mutter's placement (`place.c`): the first window goes
/// to the "centered tile" slot, and subsequent same-size windows first-fit
/// *below* existing ones before going anywhere else.
#[test]
fn placement_first_fit_prefers_below() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
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

/// When nothing fits, placement cascades from the work-area origin in 50px
/// diagonal steps (place.c find_next_cascade).
#[test]
fn placement_cascades_when_nothing_fits() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // 1000×600 windows: after the first takes the centered-tile slot,
    // below/right candidates all overflow the 1920×1048 work area (the top
    // panel insets it), so first-fit fails and every subsequent window cascades.
    let _w1 = map_window_sized(&mut f, id, (1000, 600), None);

    let _w2 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 32.),
        "the first cascaded window must sit at the work-area origin",
    );

    let _w3 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (50., 82.),
        "the next cascade slot is one 50px diagonal step down",
    );

    let _w4 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (100., 132.),
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
    let a_id = f.niri().layout.focus().unwrap().id();

    // Interacting with the focused window must not block a token-less window:
    // with no launch time there is nothing to compare against.
    tap(&mut f, KEY_A);

    let _b = map_window_sized(&mut f, id, (100, 100), None);
    let b_id = f.niri().layout.focus().unwrap().id();
    assert_ne!(
        a_id, b_id,
        "a new window with no launch information must take focus"
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
    let a_id = f.niri().layout.focus().unwrap().id();

    // Start mapping B; its launch token is minted now.
    let window = f.client(id).create_window();
    let b_surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    {
        let niri = f.niri();
        assert_eq!(niri.unmapped_windows.len(), 1);
        let unmapped = niri.unmapped_windows.values_mut().next().unwrap();
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
    f.niri_complete_animations();

    let niri = f.niri();
    assert_eq!(
        niri.layout.focus().unwrap().id(),
        a_id,
        "focus must stay on the interacted-with window"
    );

    let ws = niri.layout.active_workspace().unwrap();
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
    f.niri().gnome_settings.focus_new_windows = FocusNewWindows::Strict;
    let id = f.add_client();

    // The first window still becomes active: niri ties workspace state to
    // focus with nothing else focused. (mutter would literally leave it
    // unfocused; accepted divergence.)
    let a = map_window_sized(&mut f, id, (600, 400), None);
    let a_id = f.niri().layout.focus().unwrap().id();

    let _b = map_window_sized(&mut f, id, (100, 100), None);
    {
        let niri = f.niri();
        assert_eq!(
            niri.layout.focus().unwrap().id(),
            a_id,
            "strict mode must deny focus to a new non-transient window"
        );
        let ws = niri.layout.active_workspace().unwrap();
        let b = ws.windows().find(|w| w.id() != a_id).unwrap();
        assert!(b.is_urgent(), "the denied window must be marked urgent");
    }

    let _c = map_window_sized(&mut f, id, (200, 100), Some(&a));
    assert_ne!(
        f.niri().layout.focus().unwrap().id(),
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        original_pos,
        "unmaximizing a window maximized from a tile must restore the pre-tile position",
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
/// stays visually topmost even though maximizing moves it into the scrolling
/// layer, which niri renders below the floating one. Switching back to a
/// floating window puts the floating layer back on top.
#[test]
fn active_maximized_window_covers_floating() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let first = map_window_sized(&mut f, id, (800, 600), None);
    let first_id = f.niri().layout.focus().unwrap().id();
    let _second = map_window_sized(&mut f, id, (800, 600), None);
    let second_id = f.niri().layout.focus().unwrap().id();
    f.double_roundtrip(id);

    let window_pos = |f: &mut Fixture, wanted| {
        let ws = f.niri().layout.active_workspace().unwrap();
        let (_, pos, _) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == wanted)
            .unwrap();
        (pos.x, pos.y)
    };
    let window_under = |f: &mut Fixture, pos: (f64, f64)| {
        let ws = f.niri().layout.active_workspace().unwrap();
        ws.window_under(pos.into()).map(|(w, _)| w.id())
    };

    // Click the first window: GNOME activates and raises it.
    let (x, y) = window_pos(&mut f, first_id);
    f.pointer_motion(x + 20., y + 20.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.double_roundtrip(id);
    assert_eq!(f.niri().layout.focus().unwrap().id(), first_id);

    // Maximize it and ack the full-size configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&first);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();

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
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(f.niri().layout.focus().unwrap().id(), second_id);
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
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

    f.niri().layout.set_gnome_edge_tiling(false);
    super_drag_to(&mut f, id, (100., 100.), (20., 500.));

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !configures.contains("TiledLeft"),
        "with edge-tiling off an edge drop must not tile, got: {configures}"
    );
    let niri = f.niri();
    let focused = niri.layout.focus().unwrap().window.clone();
    let ws = niri.layout.active_workspace().unwrap();
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
    f.niri_complete_animations();
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
    f.niri_complete_animations();
    f.double_roundtrip(id);
    let niri = f.niri();
    let focused = niri.layout.focus().unwrap().window.clone();
    let ws = niri.layout.active_workspace().unwrap();
    assert!(
        ws.is_floating(&focused),
        "the shaken-loose window must land floating"
    );
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
    let first_id = f.niri().layout.focus().unwrap().id();
    let first_win = f.niri().layout.focus().unwrap().window.clone();
    let _second = map_window_sized(&mut f, id, (800, 600), None);
    let second_win = f.niri().layout.focus().unwrap().window.clone();

    // A lone Super tap opens the picker.
    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(f.niri().layout.is_overview_open());

    // Every window has a picker slot, and slots don't overlap.
    let first_rect = f
        .niri()
        .layout
        .expose_target_rect(&first_win)
        .expect("windows must have picker slots in the overview");
    let second_rect = f.niri().layout.expose_target_rect(&second_win).unwrap();
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
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert!(
        !f.niri().layout.is_overview_open(),
        "clicking a preview must leave the overview"
    );
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
        first_id,
        "clicking a preview must activate that window"
    );
}

/// Every workspace's render rect on output 1 — the geometry the overview's row
/// tests measure, settled.
fn workspace_geo(f: &mut Fixture) -> Vec<smithay::utils::Rectangle<f64, smithay::utils::Logical>> {
    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
    f.niri().layout.toggle_app_grid();
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
    f.niri().layout.toggle_app_grid();
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
    f.niri().layout.toggle_app_grid();
    f.settle_animations();

    f.niri_state().do_action(Action::CloseOverview, false);
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
    let first_win = f.niri().layout.focus().unwrap().window.clone();
    let _second = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let rest = f
        .niri()
        .layout
        .expose_drawn_rect(&first_win)
        .expect("a preview draws in the overview");
    assert_eq!(
        rest,
        f.niri().layout.expose_target_rect(&first_win).unwrap(),
        "un-hovered, a preview draws exactly in its slot"
    );

    let center = rest.loc + rest.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    f.settle_animations();

    let grown = f.niri().layout.expose_drawn_rect(&first_win).unwrap();
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
        f.niri().layout.expose_drawn_rect(&first_win).unwrap(),
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let output = f.niri_output(1);
    let slot = f.niri().layout.expose_target_rect(&win).unwrap();
    let bg = f
        .niri()
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let output = f.niri_output(1);
    let slot = f.niri().layout.expose_target_rect(&win).unwrap();
    let bg = f
        .niri()
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
        let output = f.niri_output(1);
        f.niri()
            .layout
            .monitor_for_output(&output)
            .unwrap()
            .preview_icon_scale()
    };
    let previews = |f: &mut Fixture| -> usize {
        let output = f.niri_output(1);
        f.niri()
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

    f.niri().layout.toggle_app_grid();
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
    let output = f.niri_output(1);
    assert!(
        f.niri()
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let overlay_alpha = |f: &mut Fixture| -> f64 {
        let output = f.niri_output(1);
        let mon = f.niri().layout.monitor_for_output(&output).unwrap();
        mon.preview_overlays()
            .into_iter()
            .find(|(w, _, _)| *w == win)
            .map_or(0., |(_, _, alpha)| alpha)
    };

    // Hover the preview and let the overlay fade all the way in.
    let slot = f.niri().layout.expose_target_rect(&win).unwrap();
    let inside = slot.loc + slot.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, inside.x, inside.y);
    f.settle_animations();
    assert_eq!(overlay_alpha(&mut f), 1., "hovering must show the overlay");

    // Now move onto the overhanging half of the button — outside the slot on both axes —
    // and let everything settle, which is what a human aiming at it does.
    let drawn = f.niri().layout.expose_drawn_rect(&win).unwrap();
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // Un-hovered there is no button: clicking where it would be — the half of its
    // box that overhangs the preview's corner, clear of the preview itself —
    // closes nothing.
    let slot = f.niri().layout.expose_target_rect(&win).unwrap();
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
    if !f.niri().layout.is_overview_open() {
        tap(&mut f, KEY_LEFTMETA);
    }
    f.settle_animations();

    // Hover the preview, then click the button on its top-right corner. The
    // preview has grown by then, so take the button from the drawn rect.
    let inside = slot.loc + slot.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, inside.x, inside.y);
    f.settle_animations();

    let drawn = f.niri().layout.expose_drawn_rect(&win).unwrap();
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
        f.niri().layout.is_overview_open(),
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // Two workspaces (active + trailing empty) is at the strip threshold, so no
    // thumbnails band is reserved. Work area 1048 tall ⇒ spacing round(20.96) = 21,
    // round(21·0.6) = 13 above the (zero-height) band. The search entry floats
    // (approved divergence), so it costs the picker nothing:
    //   y = 32 + 13                            = 45
    //   h = 1048 − 112(dash) − 21 − 13        = 902
    let controls = overview_controls(&mut f);
    assert_eq!(controls.workspaces.loc.y, 45.);
    assert_eq!(controls.workspaces.size.h, 902.);

    // The row is fit by height into that box, and centered on what width is left.
    let zoom: f64 = 902. / 1080.;
    let ws_w = (1920. * zoom).ceil(); // 1599
    let offset_x = ((1920. - ws_w) / 2.).round(); // 161

    // Workspace-local slot (see expose::tests): 760 × 570 centered in the picker's area,
    // which is the work area symmetrized about the view — the 32px panel strut is applied
    // at both edges, giving 1920×1016 at y = 32, so the slot sits at
    // (580, 32 + (1016−570)/2) = (580, 255), scaled into the picker box at y = 45.
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    assert_pos_eq(
        (rect.loc.x, rect.loc.y),
        (offset_x + 580. * zoom, 45. + 255. * zoom),
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
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
    config.layout.border = niri_config::Border {
        off: false,
        width: 8.,
        ..Default::default()
    };
    config.layout.focus_ring = niri_config::FocusRing {
        off: false,
        width: 8.,
        ..Default::default()
    };
    // A rule that explicitly turns the border back on: the case that must still
    // lose to GNOME mode.
    config.window_rules.push(niri_config::WindowRule {
        border: niri_config::BorderRule {
            on: true,
            width: Some(niri_config::FloatOrInt(6.)),
            ..Default::default()
        },
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = map_window_sized(&mut f, id, (800, 600), None);

    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
        let niri = f.niri();
        let now = niri.clock.now_unadjusted();
        niri.clock.set_unadjusted(now + Duration::from_millis(60));
        niri.advance_animations();
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

/// **Divergence (approved 2026-07-28/29).** The search entry floats at the top right
/// instead of taking a full-width row, and a thumbnail is the app-grid row's workspace
/// rather than gnome-shell's 5% speck. Judged on both the reference canvas and the
/// 1024×665 one the adaptive chrome ramp was written for
/// (`docs/fork/adaptive-overview-chrome.md`), because that is the canvas the sizes
/// actually have to work on.
#[test]
fn overview_entry_floats_right_of_an_app_grid_sized_thumbnail_strip() {
    for size in [(1920u16, 1080u16), (1024, 665)] {
        let mut f = Fixture::new();
        f.add_output(1, size);
        let id = f.add_client();
        let (_a, _b) = setup_two_desktops_in_overview_on(&mut f, id, size);
        f.settle_animations();

        let controls = overview_controls(&mut f);
        let pill = f.niri().overview_search.entry_pill(controls.into());
        let band = controls.thumbnails;
        let view_w = f64::from(size.0);
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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

        // It costs the strip no vertical space: the band starts within one spacing of
        // the panel, where GNOME would have had the entry's whole row above it.
        assert!(
            band.loc.y - crate::ui::panel::panel_height()
                < crate::ui::overview_search::PREFERRED_ENTRY_HEIGHT,
            "{size:?}: the entry still displaces the strip (band at {})",
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

        // And the two never collide. The *band* is what has to clear the pill's column,
        // not the row: at the app-grid size the row overflows and scrolls, and what runs
        // past the band is clipped away rather than drawn under the entry.
        let band_right = band.loc.x + band.size.w;
        assert!(
            band_right <= pill.loc.x,
            "{size:?}: the strip's band runs under the floating entry ({band_right} vs {})",
            pill.loc.x
        );
        assert!(
            strip.thumbs[0].loc.x >= band.loc.x,
            "{size:?}: the row must start inside its band"
        );
    }
}

/// **Divergence (approved 2026-07-29).** The strip is the app-grid row's twin: a
/// thumbnail is exactly the workspace that row draws, at the same size, so the two rows
/// cannot drift apart. gnome-shell has no such relationship — its thumbnails are
/// `MAX_THUMBNAIL_SCALE` (5%) and its app-grid workspaces `SMALL_WORKSPACE_RATIO` (15%).
///
/// Asserted against the *rendered* app-grid row rather than the constant, so a change to
/// either side has to move both.
#[test]
fn overview_thumbnail_is_the_app_grid_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    f.settle_animations();

    let thumb = {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .thumbnail_strip()
            .expect("three workspaces must show the strip")
            .thumbs[0]
            .size
    };

    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
    let ws = mon
        .expect("workspaces must be on a monitor")
        .workspaces_render_geo()
        .next()
        .expect("the app grid lays the workspaces out too");

    assert!(
        (thumb.h - ws.size.h).abs() <= 1. && (thumb.w - ws.size.w).abs() <= 1.,
        "a thumbnail must be the app-grid row's workspace: {thumb:?} vs {:?}",
        ws.size
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

    f.niri_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    let controls = overview_controls(&mut f);
    let pill = f.niri().overview_search.entry_pill(controls.into());
    let band = controls.thumbnails;

    let strip_now = |f: &mut Fixture| {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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

    // Walk down the strip: the active workspace's thumbnail is inside the band
    // every step of the way, and the band is clear of the floating entry.
    assert!(
        band.loc.x + band.size.w <= pill.loc.x,
        "the band must stay clear of the entry pill"
    );
    for _ in 0..n {
        let active = f
            .niri()
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

        f.niri_state().do_action(Action::FocusWorkspaceDown, false);
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

    let band = overview_controls(&mut f).thumbnails;
    // 32 + round(21 × 0.6) = 45 (the entry floats and takes no row), and the app-grid
    // row's workspace height, round((1080 - 32) × SMALL_WORKSPACE_RATIO) = 157.
    assert_eq!((band.loc.y, band.size.h), (45., 157.));

    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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

/// The collapse direction eases too, and the strip stays drawn until the band is
/// actually gone: emptying the second desktop takes the workspace count back
/// under the threshold, and the picker must grow into the band continuously
/// rather than snapping the zoom out from under the previews.
#[test]
fn overview_picker_grows_smoothly_when_the_strip_collapses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let _w2 = map_window_sized(&mut f, id, (640, 480), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // Populate a second desktop and let the band settle in. The ease is armed on
    // the frame after the count changes, so advance once before settling.
    f.niri().layout.move_to_workspace_down(true);
    f.niri().advance_animations();
    f.settle_animations();
    let expanded = overview_controls(&mut f).workspaces;
    assert_eq!((expanded.loc.y, expanded.size.h), (227., 720.));

    // Back to one populated desktop: the emptied workspace is only reaped once the
    // switch settles, and the collapse ease arms on that frame.
    f.niri().layout.move_to_workspace_up(true);
    f.settle_animations();
    {
        let niri = f.niri();
        let now = niri.clock.now_unadjusted();
        niri.clock.set_unadjusted(now + Duration::from_millis(60));
        niri.advance_animations();
    }

    let mid = overview_controls(&mut f).workspaces;
    assert!(
        mid.size.h > expanded.size.h && mid.size.h < 899.,
        "mid-collapse picker height must be between the two rest states, got {}",
        mid.size.h
    );
    // The strip is still drawn while the band is shrinking — dropping it the
    // instant the count changes would leave a hole over the still-reserved band.
    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
    assert!(
        mon.expect("workspaces must be on a monitor")
            .thumbnails_visible(),
        "the strip must stay visible until the expand fraction reaches zero"
    );

    f.settle_animations();
    let collapsed = overview_controls(&mut f).workspaces;
    assert_eq!((collapsed.loc.y, collapsed.size.h), (45., 902.));
    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
    assert!(!mon.unwrap().thumbnails_visible());
}

/// The picker box contains the thumbnails band, so the workspace zoom depends on
/// whether the strip is showing. Crossing the strip threshold happens *inside*
/// the overview (drag a window onto the trailing empty desktop), so gnome-shell
/// eases `ThumbnailsBox.expandFraction` (`overviewControls.js:358-366`) and the
/// picker follows continuously instead of popping.
#[test]
fn overview_picker_shrinks_smoothly_when_the_strip_expands() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    // Two workspaces: no band, so the picker has the whole space.
    let collapsed = overview_controls(&mut f).workspaces;
    assert_eq!(collapsed.size.h, 902.);

    // Populate a second desktop, which brings the strip in. The ease starts on
    // the next frame, so advance once to arm it and once more to sample it.
    f.niri().layout.move_to_workspace_down(true);
    f.niri().advance_animations();

    // Mid-expand the picker must be strictly between the two resting boxes —
    // never at either end, which is what a popped (un-eased) flip would give.
    {
        let niri = f.niri();
        let now = niri.clock.now_unadjusted();
        niri.clock.set_unadjusted(now + Duration::from_millis(60));
        niri.advance_animations();
    }
    let mid = overview_controls(&mut f).workspaces;
    assert!(
        mid.size.h < collapsed.size.h && mid.size.h > 779.,
        "mid-expand picker height must be between the two rest states, got {}",
        mid.size.h
    );
    assert!(mid.loc.y > collapsed.loc.y && mid.loc.y < 168.);

    // Settled: the band is fully reserved (54 tall, plus round(21 × 0.4) = 8 below).
    f.settle_animations();
    let expanded = overview_controls(&mut f).workspaces;
    assert_eq!((expanded.loc.y, expanded.size.h), (227., 720.));
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
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // The trailing empty workspace peeks at the right edge of the row:
    // the active workspace spans 161..1760 and the neighbor, drawn a touch
    // smaller, is visible from 1832 on (gnome-shell keeps the spacing at
    // its minimum exactly so neighbors peek in).
    f.pointer_motion(1850., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
        "clicking a neighbor workspace must not leave the overview"
    );
    let active = f.niri().layout.active_workspace().unwrap().id();
    assert_ne!(
        active, ws1_id,
        "clicking a neighbor workspace must switch to it"
    );

    // Clicking the empty area of the (now centered) active workspace leaves
    // the overview.
    f.pointer_motion(-940., 0.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        !f.niri().layout.is_overview_open(),
        "clicking the active workspace's empty area must leave the overview"
    );
    assert_eq!(
        f.niri().layout.active_workspace().unwrap().id(),
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
    let win = f.niri().layout.focus().unwrap().window.clone();
    let original_pos = focused_window_pos(&mut f);

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // Drag the preview towards the workspace's top-left corner and drop it.
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    f.pointer_motion(rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(-400., -300.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.niri().layout.is_overview_open(),
        "dropping a preview must not leave the overview"
    );

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();

    let slot = f.niri().layout.expose_target_rect(&win).unwrap();
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
        .niri()
        .layout
        .interactive_move_drawn_size()
        .expect("the drag must be in flight");
    assert!(
        (picked.w - slot.size.w).abs() <= 2.,
        "the drag starts at the preview's own footprint, got {picked:?} vs {:?}",
        slot.size
    );

    f.settle_animations();
    let shrunk = f.niri().layout.interactive_move_drawn_size().unwrap();
    assert!(
        (f64::max(shrunk.w, shrunk.h) - 256.).abs() <= 1.,
        "the dragged preview must shrink to 256px on its longest side, got {shrunk:?}"
    );
    assert!(
        (shrunk.w / shrunk.h - picked.w / picked.h).abs() <= 0.01,
        "and keep its aspect, got {shrunk:?} from {picked:?}"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
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
    let win = f.niri().layout.focus().unwrap().window.clone();
    let original_pos = focused_window_pos(&mut f);
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // Drag the preview onto the trailing workspace peeking at the right
    // screen edge (visible from 1752 on; see the neighbor click test).
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(1800. - grab.0, 540. - grab.1 - 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.niri().layout.is_overview_open(),
        "dropping a preview on a neighbor must not leave the overview"
    );

    let niri = f.niri();
    let (_, _, ws) = niri
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

/// Opening the app grid switches the workspace row from gnome-shell's
/// `FitMode.SINGLE` to `FitMode.ALL`: instead of sliding the row so the *active*
/// workspace lands on the centered slot (`_getFirstFitSingleWorkspaceBox`,
/// `workspacesView.js:172-204`), every workspace is laid out inside the
/// allocation and the run is centered as a whole
/// (`_getFirstFitAllWorkspaceBox`, `:128-170`) — so which workspace is active no
/// longer shifts anything. The fitted row also packs at `WORKSPACE_MIN_SPACING`
/// rather than the picker's roomy peek-at-the-edges gap (`_getSpacing`'s
/// `(1 - fitMode)` factor, `:207-226`).
///
/// Driven from the *first* of three workspaces, where the two arrangements are
/// furthest apart: fit-single puts a third of the row off the left edge.
#[test]
fn app_grid_fits_the_whole_workspace_row() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _ = setup_two_desktops_in_overview(&mut f, id);

    // Back to the first workspace, so the active one is off-center in the row.
    while f
        .niri()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.niri_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    use smithay::utils::{Logical, Rectangle};

    let row = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .workspaces_render_geo()
            .take(3)
            .collect()
    };
    // Each workspace is centered in its slot, so slot geometry is read off the
    // rect *centers* — the inactive-workspace shrink leaves those untouched.
    let center_of = |r: &Rectangle<f64, Logical>| r.loc.x + r.size.w / 2.;
    let run_center = |row: &[Rectangle<f64, Logical>]| {
        (center_of(&row[0]) + center_of(&row[row.len() - 1])) / 2.
    };
    let view_center = 1920. / 2.;

    // The picker: fit-single, so the *active* workspace is the centered one and
    // the run as a whole hangs off to the right.
    let picker = row(&mut f);
    assert_eq!(picker.len(), 3, "three workspaces must be laid out");
    assert!(
        (center_of(&picker[0]) - view_center).abs() <= 1.,
        "in the picker the active workspace must be centered, got {picker:?}"
    );
    assert!(
        run_center(&picker) > view_center + 100.,
        "in the picker the run must hang off to one side, got {picker:?}"
    );

    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.niri().layout.is_app_grid_open(), "app grid must open");

    // The app grid: fit-all, so the run is centered and the active workspace is
    // wherever its index puts it.
    let grid = row(&mut f);
    assert!(
        (run_center(&grid) - view_center).abs() <= 1.,
        "in the app grid the whole run must be centered, got {grid:?}"
    );
    assert!(
        center_of(&grid[1]) > center_of(&grid[0]) && center_of(&grid[2]) > center_of(&grid[1]),
        "the row must stay in workspace order, got {grid:?}"
    );

    // Packed at WORKSPACE_MIN_SPACING. `_getSpacing`'s `(1 - fitMode)` factor is
    // what does this: at the app grid's small zoom the workspace takes little of
    // the width, so the fit-*single* formula would run all the way up to
    // WORKSPACE_MAX_SPACING (80) instead — which is exactly what we drew before
    // the row learned about fit modes.
    // Slot pitch minus the slot width; workspace 0 is the active one, so its rect
    // is the unshrunk slot.
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
        f.niri_state().do_action(Action::FocusWorkspaceDown, false);
    }
    f.niri_complete_animations();
}

/// With enough workspaces the app grid's fitted row no longer fits, and every
/// workspace past the edge used to be unreachable: gnome-shell keeps them on screen
/// by narrowing each box to `availableWidth / n`
/// (`_getFirstFitAllWorkspaceBox`, `workspacesView.js:127-169`), which we can't do
/// with one aspect-locked zoom per monitor. The overflowing row scrolls to follow
/// the active workspace instead (**divergence**, approved 2026-07-29).
#[test]
fn app_grid_scrolls_an_overflowing_workspace_row_into_view() {
    use smithay::utils::{Logical, Rectangle};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    // Eight populated workspaces at the app grid's zoom overflow 1920 (~279px wide,
    // packed at the 24px minimum spacing) with room to spare.
    setup_n_desktops(&mut f, id, 8);

    f.niri_state().do_action(Action::OpenOverview, false);
    f.settle_animations();
    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    assert!(f.niri().layout.is_app_grid_open(), "app grid must open");

    let row = |f: &mut Fixture| -> Vec<Rectangle<f64, Logical>> {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        mon.expect("workspaces must be on a monitor")
            .workspaces_render_geo()
            .collect()
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
        .niri()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.niri_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    let mut visited = 0;
    loop {
        let active = f
            .niri()
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
        f.niri_state().do_action(Action::FocusWorkspaceDown, false);
        f.settle_animations();
        let moved = f
            .niri()
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
        .niri()
        .layout
        .active_monitor_ref()
        .unwrap()
        .active_workspace_idx()
        != 0
    {
        f.niri_state().do_action(Action::FocusWorkspaceUp, false);
    }
    f.settle_animations();

    use crate::layout::monitor::WORKSPACE_INACTIVE_SCALE;

    let scales = |f: &mut Fixture| -> Vec<f64> {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
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
    f.niri_state().do_action(Action::CloseOverview, false);
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
    let win_a = f.niri().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(f, id, (640, 480), None);
    let win_b = f.niri().layout.focus().unwrap().window.clone();

    tap(f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // Drag B's preview onto the trailing workspace peeking at the right.
    let rect = f.niri().layout.expose_target_rect(&win_b).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.pointer_motion(drop_x - grab.0, drop_y - grab.1 - 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    (win_a, win_b)
}

/// Absolute pointer motion: `Fixture::pointer_motion` takes deltas.
fn pointer_motion_to(f: &mut Fixture, x: f64, y: f64) {
    let cur = f.niri().seat.get_pointer().unwrap().current_location();
    f.pointer_motion(x - cur.x, y - cur.y);
}

/// The center of the given strip thumbnail, for pointer input.
fn thumbnail_center(f: &mut Fixture, idx: usize) -> (f64, f64) {
    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
    let strip = mon
        .expect("workspaces must be on a monitor")
        .thumbnail_strip()
        .expect("the thumbnails strip must be visible");
    let rect = strip.thumbs[idx];
    (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.)
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
        f.niri()
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
    // passed has closed up into the slot it left.
    {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        let strip = mon.unwrap().thumbnail_strip().unwrap();
        assert_eq!(
            strip.thumbs[0].loc.x + strip.thumbs[0].size.w / 2.,
            t1x + 1.,
            "the dragged thumbnail must hang off the pointer"
        );
        assert_eq!(
            strip.thumbs[1].loc.x,
            t0x - strip.thumbs[1].size.w / 2.,
            "the passed thumbnail must close up into the slot the drag left"
        );
    }

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert_eq!(
        (ws_idx_of(&mut f, &win_a), ws_idx_of(&mut f, &win_b)),
        (1, 0),
        "dropping a thumbnail past its neighbour must swap the workspaces"
    );
    assert_eq!(
        f.niri().layout.workspaces().count(),
        3,
        "reordering must not add or drop a workspace"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "reordering must not leave the overview"
    );
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

    let active = |f: &mut Fixture| f.niri().layout.active_workspace().unwrap().id();
    let ws_of = |f: &mut Fixture, win: &smithay::desktop::Window| {
        f.niri()
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
    f.niri_complete_animations();

    assert_eq!(
        active(&mut f),
        id_b,
        "a click on a thumbnail must switch to its workspace"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "…and stay in the overview, as clicking a non-active workspace does"
    );
    assert_eq!(
        (ws_of(&mut f, &win_a).0, ws_of(&mut f, &win_b).0),
        (idx_a, idx_b),
        "a click must not reorder anything"
    );
}

/// gnome-shell's ThumbnailsBox visibility rule with dynamic workspaces: the
/// strip appears only once there are more than two workspaces, i.e. once a
/// second desktop is populated.
#[test]
fn thumbnail_strip_appears_once_second_desktop_is_populated() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    let visible = |f: &mut Fixture| {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        mon.unwrap().thumbnails_visible()
    };
    assert!(
        !visible(&mut f),
        "one populated desktop must not show the thumbnails strip"
    );
    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    let (_a, _b) = setup_two_desktops_in_overview(&mut f, id);
    assert!(
        visible(&mut f),
        "a second populated desktop must bring up the thumbnails strip"
    );
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
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();

    // Click the second desktop's thumbnail: switch, stay in the overview.
    let (x, y) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, x, y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
        "clicking a non-active thumbnail must stay in the overview"
    );
    let active = f.niri().layout.active_workspace().unwrap().id();
    assert_ne!(active, ws1_id, "clicking a thumbnail must switch to it");

    // Click it again (now active): leave the overview. The pointer has to be re-aimed —
    // the row keeps the active workspace on the band's center, so switching to a
    // thumbnail slides it out from under wherever it was clicked.
    let (x, y) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, x, y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "clicking the active thumbnail must leave the overview"
    );
    assert_eq!(f.niri().layout.active_workspace().unwrap().id(), active);
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
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();
    let original_pos = focused_window_pos(&mut f);

    // Drag A's preview onto the second desktop's thumbnail.
    let rect = f.niri().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    let (tx, ty) = thumbnail_center(&mut f, 1);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    pointer_motion_to(&mut f, tx, ty);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert!(
        f.niri().layout.is_overview_open(),
        "dropping on a thumbnail must not leave the overview"
    );

    let niri = f.niri();
    let (_, _, ws) = niri
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
    let workspace_count = |f: &mut Fixture| f.niri().layout.workspaces().count();
    assert_eq!(workspace_count(&mut f), 3);

    // Drag A's preview into the gap between the first two thumbnails.
    let rect = f.niri().layout.expose_target_rect(&win_a).unwrap();
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
    f.niri().layout.update_render_elements(None);
    {
        let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
        let strip = mon.unwrap().thumbnail_strip().unwrap();
        assert!(
            strip.placeholder.is_some(),
            "hovering a thumbnail gap must show the drop placeholder"
        );
    }

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    assert_eq!(
        workspace_count(&mut f),
        4,
        "dropping into a thumbnail gap must insert a workspace"
    );
    let niri = f.niri();
    let (_, ws_idx, _) = niri
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
    let win = f.niri().layout.focus().unwrap().window.clone();
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();

    // Tile left and ack the half-width configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(960, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();
    let _ = f.client(id).window(&surface).recent_configures();

    // Drag the preview onto the trailing workspace's peeking edge. The real
    // window is never touched (gnome-shell drags the preview), so the client
    // must not see any untile/resize along the way.
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    f.double_roundtrip(id);
    pointer_motion_to(&mut f, 1800., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    for configure in f.client(id).window(&surface).recent_configures() {
        assert_eq!(
            configure.size,
            (960, 1080),
            "an overview drag must never resize the tiled window, got: {configure}"
        );
    }

    let niri = f.niri();
    let (_, _, ws) = niri
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
    let win = f.niri().layout.focus().unwrap().window.clone();
    let ws1_id = f.niri().layout.active_workspace().unwrap().id();

    // Maximize and ack the full-size configure.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_UP);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(1920, 1048);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();
    let _ = f.client(id).window(&surface).recent_configures();

    // A 20px drag is well under the 48px shake threshold, yet the preview
    // must already be moving (picked out of its workspace).
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    f.pointer_motion(grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 20.);
    assert!(
        f.niri()
            .layout
            .workspaces()
            .all(|(_, _, ws)| !ws.has_window(&win)),
        "a preview pick-up must not need mutter's shake-loose threshold"
    );

    // Drop it on the neighbor workspace peeking at the right edge.
    f.pointer_motion(1800. - grab.0, 540. - grab.1 - 20.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
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

    let niri = f.niri();
    let (_, _, ws) = niri
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

/// While a preview drag is in flight, the source desktop's picker layout is
/// frozen (gnome-shell's layout_frozen): the remaining previews hold their
/// slots — the dragged window leaves a gap — until the drop.
#[test]
fn overview_drag_freezes_the_other_previews() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _a = map_window_sized(&mut f, id, (800, 600), None);
    let win_a = f.niri().layout.focus().unwrap().window.clone();
    let _b = map_window_sized(&mut f, id, (640, 480), None);
    let win_b = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    let slot_a = f.niri().layout.expose_target_rect(&win_a).unwrap();

    // Pick up B's preview and move it away from its slot.
    let rect = f.niri().layout.expose_target_rect(&win_b).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 100.);

    assert_eq!(
        f.niri().layout.expose_target_rect(&win_a),
        Some(slot_a),
        "the other previews must hold their slots while the drag is in flight"
    );

    // Drop B on the trailing workspace: A, now alone, re-layouts.
    pointer_motion_to(&mut f, 1800., 540.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_ne!(
        f.niri().layout.expose_target_rect(&win_a),
        Some(slot_a),
        "the drop must let the source desktop's picker layout recompute"
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
        f.niri_complete_animations();
        let _c = map_window_sized(&mut f, id, (500, 400), None);
        let win_c = f.niri().layout.focus().unwrap().window.clone();
        tap(&mut f, KEY_LEFTMETA);
        f.niri_complete_animations();
        let rect = f.niri().layout.expose_target_rect(&win_c).unwrap();
        let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
        pointer_motion_to(&mut f, grab.0, grab.1);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_motion(0., 10.);
        let (tx, ty) = thumbnail_center(&mut f, 2);
        pointer_motion_to(&mut f, tx, ty);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.niri_complete_animations();
        f.double_roundtrip(id);
    }
    assert!(f.niri().layout.is_overview_open());

    let active_idx = |f: &mut Fixture| {
        let active = f.niri().layout.active_workspace().unwrap().id();
        f.niri()
            .layout
            .workspaces()
            .position(|(_, _, ws)| ws.id() == active)
            .unwrap()
    };
    assert_eq!(active_idx(&mut f), 0, "the drags must not have switched");

    // Pick up A's preview and hold it against the right screen edge,
    // driving the DnD scroll by hand with a pinned clock.
    let rect = f.niri().layout.expose_target_rect(&win_a).unwrap();
    let grab = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);
    pointer_motion_to(&mut f, grab.0, grab.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_motion(0., 10.);
    assert!(
        f.niri()
            .layout
            .workspaces()
            .all(|(_, _, ws)| !ws.has_window(&win_a)),
        "the preview must be picked up before it reaches the edge"
    );
    pointer_motion_to(&mut f, 1919., 540.);

    let base = f.niri().clock.now_unadjusted() + Duration::from_millis(200);
    let at = |f: &mut Fixture, offset_ms: u64| {
        let mut clock = f.niri().clock.clone();
        clock.set_unadjusted(base + Duration::from_millis(offset_ms));
        f.niri().layout.advance_animations();
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
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();
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
        std::rc::Rc::new(std::cell::RefCell::new(niri_config::Config::default())),
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
        std::rc::Rc::new(std::cell::RefCell::new(niri_config::Config::default())),
    );
    panel.update_clock_at(0);

    let rect = panel.date_menu_rect(2560.);
    let text_w = niri_vk::text::measure_line_width_weighted(
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

    assert!(!f.niri().layout.is_overview_open());
    f.niri().update_render_elements(None);
    assert!(
        !f.niri().panel.activities_checked(),
        "Activities starts unchecked"
    );

    // Click within the Activities button at the top-left of the panel.
    f.pointer_motion(10., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
        "clicking Activities must open the overview"
    );
    f.niri().update_render_elements(None);
    assert!(
        f.niri().panel.activities_checked(),
        "Activities must be checked while the overview is open"
    );

    // A second click toggles it back.
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "clicking Activities again must close the overview"
    );
    f.niri().update_render_elements(None);
    assert!(
        !f.niri().panel.activities_checked(),
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
        f.niri()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx(),
        0
    );

    // Park the pointer over the indicator (top-left of the panel) and scroll down.
    pointer_motion_to(&mut f, 10., 10.);
    f.scroll_wheel();
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
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
    f.niri_complete_animations();
    assert_eq!(
        f.niri()
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
    assert!(!f.niri().panel_popover.is_open());

    // The clock is centered; click it.
    let open = |f: &mut Fixture| {
        pointer_motion_to(f, 960., 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    open(&mut f);
    assert!(
        f.niri().panel_popover.is_open(),
        "clicking the clock must open the calendar popover"
    );

    // Escape closes it (the modal keyboard grab). The close is animated (fade-out),
    // so settle the animation before asserting it's gone.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "Escape must close the popover"
    );

    // Reopen, then a click well outside the popover dismisses it.
    open(&mut f);
    assert!(f.niri().panel_popover.is_open());
    pointer_motion_to(&mut f, 10., 700.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "a click outside the popover must dismiss it"
    );
}

/// Panel menus work inside the overview: a popover opened while the overview is
/// up pushes its own grab on top of the overview's modal and stays open
/// (`js/ui/popupMenu.js:1520`) — it must not be dismissed on the next frame.
#[test]
fn panel_popover_stays_open_in_overview() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri().update_render_elements(None);
    assert!(f.niri().layout.is_overview_open());

    // Click the clock: the calendar popover opens.
    pointer_motion_to(&mut f, 960., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.niri().panel_popover.is_open(),
        "clicking the clock in the overview must open the calendar popover"
    );

    // Subsequent frames must not dismiss it (a level-triggered overview check
    // once closed the popover on the render update right after it opened).
    f.niri().update_render_elements(None);
    f.niri().update_render_elements(None);
    f.settle_animations();
    assert!(
        f.niri().panel_popover.is_open(),
        "a popover opened in the overview must stay open across frames"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "the overview stays open under the popover"
    );
}

/// A popover that is open when the overview *opens* is dismissed: GNOME's
/// overview modal does not coexist with a held menu grab
/// (`js/ui/overview.js:461` hides rather than fight an existing grab).
#[test]
fn overview_open_dismisses_open_panel_popover() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri().update_render_elements(None);

    pointer_motion_to(&mut f, 960., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.niri().panel_popover.is_open());

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri().update_render_elements(None);
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "opening the overview must dismiss an already-open popover"
    );
    assert!(f.niri().layout.is_overview_open());
}

/// Clicking the right-box quick-settings indicator opens its popover; Escape and
/// an outside click both dismiss it (the same popup-menu grab as the calendar).
/// Clicking a tile inside flips its gsettings-backed state.
#[test]
fn panel_quick_settings_click_opens_toggles_and_dismisses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert!(!f.niri().panel_popover.is_open());

    // The indicator is right-anchored; with the default toggles it's a single
    // anchor icon in the top-right corner. Click it.
    let open = |f: &mut Fixture| {
        pointer_motion_to(f, 1906., 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    open(&mut f);
    assert!(
        f.niri().panel_popover.is_open(),
        "clicking the quick-settings indicator must open its popover"
    );

    // A click on the Do Not Disturb tile flips the local state and keeps the menu
    // open. The menu is centered under the indicator, clamped into the output. The
    // grid is [Network, Dark Style, Do Not Disturb, Night Light] row-major over two
    // columns, so DND is the bottom-left tile (row 1, col 0).
    let output_w = 1920.0_f64;
    let anchor = f.niri().panel.quick_settings_rect(output_w);
    // Recompute the popover origin the way the popover does (centered, clamped with a
    // POPOVER_MARGIN inset from the screen edges).
    let menu_w = 332.0_f64; // PAD*2 + 2*TILE_W + TILE_GAP
    let margin = 6.0_f64; // POPOVER_MARGIN
    let center_x = anchor.loc.x + anchor.size.w / 2.;
    let origin_x = (center_x - menu_w / 2.).clamp(margin, (output_w - menu_w - margin).max(margin));
    // DND tile center (row 1, col 0), menu-local: x = PAD + TILE_W/2; y = PAD + SYS_H
    // + TILE_GAP + (TILE_H + TILE_GAP) [second row] + TILE_H/2. Plus the popover
    // origin (menu y = panel_height() + margin).
    let tile_x = origin_x + 12. + 75.;
    let tile_y = (32. + margin) + (12. + 44. + 8.) + (56. + 8.) + 28.;
    pointer_motion_to(&mut f, tile_x, tile_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.niri().panel_popover.is_open(),
        "a tile click must not close the quick-settings menu"
    );
    assert!(
        f.niri().gnome_settings.quick_toggles.do_not_disturb,
        "clicking the Do Not Disturb tile must flip its state on"
    );

    // Escape closes it (animated fade-out; settle before asserting it's gone).
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "Escape must close the quick-settings popover"
    );

    // Reopen, then a click well outside dismisses it.
    open(&mut f);
    assert!(f.niri().panel_popover.is_open());
    pointer_motion_to(&mut f, 960., 700.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
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
    assert!(f.niri().panel.messages_indicator_visible());

    // The DND tile center, computed the way `panel_quick_settings_*` does.
    let output_w = 1920.0_f64;
    let anchor = f.niri().panel.quick_settings_rect(output_w);
    let menu_w = 332.0_f64;
    let margin = 6.0_f64;
    let center_x = anchor.loc.x + anchor.size.w / 2.;
    let origin_x = (center_x - menu_w / 2.).clamp(margin, (output_w - menu_w - margin).max(margin));
    let tile_x = origin_x + 12. + 75.;
    let tile_y = (32. + margin) + (12. + 44. + 8.) + (56. + 8.) + 28.;
    let open_qs = |f: &mut Fixture| {
        pointer_motion_to(f, 1906., 10.);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let click_dnd = |f: &mut Fixture| {
        pointer_motion_to(f, tile_x, tile_y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };

    // Enable DND → the dot clears even though the notification is still unseen.
    open_qs(&mut f);
    click_dnd(&mut f);
    assert!(f.niri().gnome_settings.quick_toggles.do_not_disturb);
    assert!(
        !f.niri().panel.messages_indicator_visible(),
        "DND hides the dot with no new notification"
    );

    // Disable DND again → the dot re-lights (the notification is still unseen).
    click_dnd(&mut f);
    assert!(!f.niri().gnome_settings.quick_toggles.do_not_disturb);
    assert!(
        f.niri().panel.messages_indicator_visible(),
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
    let _ = f.niri().brightness.monitors_changed(&snapshot);
    f.niri().backlight = snapshot;

    // The first sync adopts the hardware, so the global slider sits at the maximum.
    assert_eq!(f.niri().brightness.global_scale().unwrap().value(), 1.0);
    assert_eq!(f.niri().brightness.scales()[1].value(), 0.5);

    // A card row: pushing the external monitor to full makes IT the maximum, so the global scale
    // follows it and the two are now in step.
    f.niri_state()
        .apply_popover_action(PopoverAction::SetMonitorBrightness("DP-2".into(), 1.0));
    assert_eq!(f.niri().brightness.global_scale().unwrap().value(), 1.0);
    assert_eq!(f.niri().brightness.scales()[0].value(), 1.0);
    assert_eq!(f.niri().brightness.scales()[1].value(), 1.0);

    // The top-level slider now moves both together, through the re-derived factors.
    f.niri_state()
        .apply_popover_action(PopoverAction::SetBrightness(0.4));
    assert_eq!(f.niri().brightness.scales()[0].value(), 0.4);
    assert_eq!(f.niri().brightness.scales()[1].value(), 0.4);

    // An unknown connector is a no-op, not a panic.
    f.niri_state()
        .apply_popover_action(PopoverAction::SetMonitorBrightness("HDMI-A-1".into(), 0.9));
    assert_eq!(f.niri().brightness.scales()[0].value(), 0.4);
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
    let _ = f.niri().brightness.monitors_changed(&snapshot);
    f.niri().backlight = snapshot;

    // A plain brightness-down key: one step of 1/20 off the global scale, fanned out to both.
    f.niri_state().step_brightness(Step::Down, false);
    assert!(close(
        f.niri().brightness.global_scale().unwrap().value(),
        0.95
    ));
    assert!(close(f.niri().brightness.scales()[0].value(), 0.95));
    assert!(close(f.niri().brightness.scales()[1].value(), 0.95));

    // The `-monitor` variant follows the pointer. Park it on the second output.
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    f.niri_state().step_brightness(Step::Down, true);
    assert!(
        close(f.niri().brightness.scales()[0].value(), 0.95),
        "the other monitor must not move"
    );
    assert!(close(f.niri().brightness.scales()[1].value(), 0.9));

    // Cycle wraps at the top rather than stopping there -- the single-key control.
    f.niri_state().step_brightness(Step::Up, false);
    assert!(close(
        f.niri().brightness.global_scale().unwrap().value(),
        1.0
    ));
    f.niri_state().step_brightness(Step::Cycle, false);
    assert!(close(
        f.niri().brightness.global_scale().unwrap().value(),
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
    let one = f.niri_output(1);
    let two = f.niri_output(2);

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
    let update = f.niri().brightness.monitors_changed(&snapshot);
    assert!(
        update.osd.is_empty(),
        "a monitors-changed pass must not put an OSD on screen"
    );
    f.niri().backlight = snapshot;
    assert!(!f.niri().osd.is_visible());

    // A plain brightness key moves the global scale, which fans out to every monitor -- so every
    // monitor shows the bar, at its own (here identical) level.
    f.niri_state().step_brightness(Step::Down, false);
    let content = f.niri().osd.content(&one).expect("output 1 shows the OSD");
    assert_eq!(content.icon, vec!["display-brightness-symbolic"]);
    assert_eq!(content.label, None, "the brightness OSD carries no label");
    assert_eq!(content.max_level, 1.0, "brightness tops out at 1.0");
    assert!((content.level.unwrap() - 0.95).abs() < 1e-9);
    assert!(f.niri().osd.content(&two).is_some());

    // The `-monitor` variant moves one scale, so only that monitor shows one and the other's is
    // cancelled -- the behavior `osdWindowManager.show`'s level map exists for.
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    f.niri_state().step_brightness(Step::Down, true);
    // A cancel is a fade-out, not an instant hide, so let it finish before looking.
    tick(&mut f, 200);
    assert!(
        f.niri().osd.content(&one).is_none(),
        "the monitor that did not move must have its OSD cancelled"
    );
    let content = f.niri().osd.content(&two).unwrap();
    assert!((content.level.unwrap() - 0.9).abs() < 1e-9);

    // The quick-settings slider is the global scale too, so it is back to both.
    f.niri_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetBrightness(0.5));
    assert!(f.niri().osd.content(&one).is_some());
    assert!(f.niri().osd.content(&two).is_some());

    // Idle dimming clamps the hardware without moving a scale, so neither branch runs: whatever is
    // on screen is left to expire on its own deadline rather than being replaced or cancelled.
    let before = f.niri().osd.content(&two);
    let snapshot = f.niri().backlight.clone();
    let update = f.niri().brightness.set_dimming(true, &snapshot);
    assert!(update.osd.is_empty(), "dimming moves no scale");
    assert_eq!(f.niri().osd.content(&two), before);
}

/// `org.gnome.Shell.Brightness` is gsd-power's way in (`js/ui/shellDBus.js:595-637`): idle dimming
/// clamps the backlight without moving the scales, and the auto-brightness target biases them.
/// `BrightnessChanged` marks changes *the user* made, so the ambient-light loop can tell its own
/// adjustments apart from ours (`brightnessManager.js:151-158,172-179` emit `user-update` only
/// from the slider handlers).
#[cfg(feature = "dbus")]
#[test]
fn brightness_dbus_dims_without_moving_the_scales() {
    use crate::dbus::gnome_shell_brightness::{BrightnessToNiri, NiriToBrightness};

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
    let _ = f.niri().brightness.monitors_changed(&snapshot);
    f.niri().backlight = snapshot;

    // Stand in for the D-Bus service's outbound half so the emissions are observable.
    let (tx, rx) = async_channel::unbounded();
    f.niri().brightness_emit = Some(tx);

    // gsd-power dims: the scale stays where the user put it; only the written brightness drops.
    f.niri_state()
        .on_brightness_msg(BrightnessToNiri::SetDimming(true));
    assert!(f.niri().brightness.dimming());
    assert_eq!(f.niri().brightness.global_scale().unwrap().value(), 1.0);

    // ... and none of that is a user change.
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, NiriToBrightness::UserChanged)),
        "gsd-power's own request must not come back as BrightnessChanged"
    );
    // The property is pushed (the service dedups it), and it is true: we have a backlight.
    assert!(emitted
        .iter()
        .any(|m| matches!(m, NiriToBrightness::HasControl(true))));

    // An auto-brightness target biases around the scale's midpoint, still not a user change.
    f.niri_state()
        .on_brightness_msg(BrightnessToNiri::SetAutoBrightnessTarget(0.6));
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(!emitted
        .iter()
        .any(|m| matches!(m, NiriToBrightness::UserChanged)));

    // A brightness KEY is a user change, so it does emit.
    f.niri_state()
        .step_brightness(crate::brightness::Step::Down, false);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        emitted
            .iter()
            .any(|m| matches!(m, NiriToBrightness::UserChanged)),
        "a brightness key is a user change"
    );

    // Losing the backlight clears HasBrightnessControl.
    f.niri().backlight = crate::backlight::BacklightSnapshot::default();
    let snapshot = f.niri().backlight.clone();
    let _ = f.niri().brightness.monitors_changed(&snapshot);
    f.niri_state()
        .on_brightness_msg(BrightnessToNiri::SetDimming(false));
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(emitted
        .iter()
        .any(|m| matches!(m, NiriToBrightness::HasControl(false))));
}

/// gnome-shell registers the brightness keys with `Shell.ActionMode.ALL`
/// (`js/misc/brightnessManager.js:35-76`), so they keep working on the lock screen -- which is
/// when you need them most, gsd-power having dimmed the panel -- and while the screenshot UI is up.
#[test]
fn brightness_keys_work_when_locked() {
    use niri_config::Action;

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
#[cfg(feature = "dbus")]
#[test]
fn a_brightness_key_with_no_backlight_is_silent() {
    use crate::dbus::gnome_shell_brightness::NiriToBrightness;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (tx, rx) = async_channel::unbounded();
    f.niri().brightness_emit = Some(tx);
    assert!(f.niri().brightness.global_scale().is_none(), "no backlight");

    f.niri_state()
        .step_brightness(crate::brightness::Step::Up, false);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, NiriToBrightness::UserChanged)),
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
    let _ = f.niri().brightness.monitors_changed(&snapshot);
    f.niri().backlight = snapshot;
    f.add_output(2, (1920, 1080));
    pointer_motion_to(&mut f, 1920. + 100., 100.);
    let _: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    f.niri_state()
        .step_brightness(crate::brightness::Step::Up, true);
    let emitted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !emitted
            .iter()
            .any(|m| matches!(m, NiriToBrightness::UserChanged)),
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

    // Open the calendar (click the centered clock).
    pointer_motion_to(&mut f, 960., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.niri().panel_popover.is_open());
    assert!(
        f.niri().panel_popover.are_animations_ongoing(),
        "opening must start a fade animation"
    );

    // Once settled, it's still open but no longer animating.
    f.settle_animations();
    assert!(f.niri().panel_popover.is_open());
    assert!(!f.niri().panel_popover.are_animations_ongoing());

    // Dismiss with Escape: the popover must NOT vanish instantly — it stays visible,
    // fading out, with an ongoing animation.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    assert!(
        f.niri().panel_popover.is_open(),
        "the close must be animated, not instant — the popover stays visible while fading"
    );
    assert!(
        f.niri().panel_popover.are_animations_ongoing(),
        "closing must run a fade-out animation"
    );

    // After the fade-out settles, it's gone.
    f.settle_animations();
    assert!(!f.niri().panel_popover.is_open());
    assert!(!f.niri().panel_popover.are_animations_ongoing());
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
        f.niri().layout.windows().count(),
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
    let base = f.niri().clock.now_unadjusted();
    assert!(
        f.niri()
            .idle_monitor
            .idletime_ms(base + Duration::from_secs(600))
            >= 600_000,
        "idle time must grow while the user is inactive",
    );

    // Any input resets it (input `should_notify_activity` -> `Niri::notify_activity`).
    f.key_press(KEY_A);
    f.key_release(KEY_A);

    let now = f.niri().clock.now_unadjusted();
    assert!(
        f.niri().idle_monitor.idletime_ms(now) < 1000,
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
#[cfg(feature = "dbus")]
#[test]
fn idle_monitor_dbus_idle_watch_fires_and_rearms() {
    use crate::dbus::mutter_idle_monitor::IdleMonitorToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Pin the idle clock to a known instant via ResetIdletime, then register a 5s watch.
    let t0 = Duration::from_secs(10_000);
    f.niri().clock.set_unadjusted(t0);
    f.niri_state()
        .on_idle_monitor_msg(IdleMonitorToNiri::ResetIdletime);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_idle_monitor_msg(IdleMonitorToNiri::AddIdleWatch {
            interval: 5000,
            owner: ":1.gsd".to_owned(),
            reply,
        });
    let id = rx.try_recv().expect("AddIdleWatch must reply with an id");
    assert!(id > 0, "watch ids are greater than zero");

    assert!(
        f.niri()
            .idle_monitor
            .refresh(t0 + Duration::from_millis(4999))
            .is_empty(),
        "must not fire before the interval elapses",
    );
    let fired = f
        .niri()
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
    f.niri().clock.set_unadjusted(t1);
    f.niri_state()
        .on_idle_monitor_msg(IdleMonitorToNiri::ResetIdletime);
    assert!(
        f.niri()
            .idle_monitor
            .refresh(t1 + Duration::from_millis(4999))
            .is_empty(),
        "the just-reset watch must not fire early",
    );
    assert_eq!(
        f.niri()
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
#[cfg(feature = "dbus")]
#[test]
fn end_session_dialog_open_confirm_and_cancel() {
    use crate::dbus::gnome_session::EndSessionDialogToNiri;
    use crate::end_session::EndSessionType;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // gnome-session raises the shutdown dialog with a 60s countdown.
    f.niri_state()
        .on_end_session_msg(EndSessionDialogToNiri::Open {
            kind: 1,
            seconds: 60,
        });
    assert!(
        f.niri().end_session.is_open(),
        "Open must raise the end-session lifecycle",
    );
    assert!(
        f.niri().end_session_dialog.is_open(),
        "Open must raise the visible dialog too",
    );
    assert_eq!(f.niri().end_session.kind(), Some(EndSessionType::Shutdown));
    assert_eq!(
        f.niri().end_session.kind().unwrap().confirmed_signal(),
        "ConfirmedShutdown",
        "confirming a shutdown dialog must emit ConfirmedShutdown",
    );

    // Confirming (Enter / clicking Power Off) closes the dialog; gnome-session then powers off.
    f.niri_state().niri.confirm_end_session();
    assert!(!f.niri().end_session.is_open(), "confirm must close it");
    assert!(!f.niri().end_session_dialog.is_open());

    // A fresh dialog can be cancelled (Esc / Cancel), which aborts the request.
    f.niri_state()
        .on_end_session_msg(EndSessionDialogToNiri::Open {
            kind: 0,
            seconds: 60,
        });
    assert_eq!(f.niri().end_session.kind(), Some(EndSessionType::Logout));
    f.niri_state().niri.cancel_end_session();
    assert!(!f.niri().end_session.is_open(), "cancel must close it");

    // gnome-session withdrawing the request (Close) also dismisses the dialog.
    f.niri_state()
        .on_end_session_msg(EndSessionDialogToNiri::Open {
            kind: 2,
            seconds: 60,
        });
    f.niri_state()
        .on_end_session_msg(EndSessionDialogToNiri::Close);
    assert!(
        !f.niri().end_session.is_open(),
        "gnome-session's Close must dismiss the dialog",
    );
}

// RecordArea screencast: an area is recorded from a single output (the one it overlaps most),
// cropped to the recorded rectangle. See docs/fork/panel-status-port.md (slice 1, Half A).

#[test]
fn screencast_area_resolves_to_the_containing_output() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::niri::CastTarget;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.niri_output(1);

    // A rect fully inside the output resolves to it, at 1:1 physical size (headless scale 1).
    let rect = Rectangle::new(Point::from((100, 100)), Size::from((300, 200)));
    let (target, size, _refresh) = f
        .niri()
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
    assert!(f.niri().cast_params_for_area(off).is_none());
}

#[test]
fn screencast_area_picks_the_largest_intersection_output() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::niri::CastTarget;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    // Order the two outputs left→right by their global position.
    let mut outs = [f.niri_output(1), f.niri_output(2)];
    outs.sort_by_key(|o| f.niri().global_space.output_geometry(o).unwrap().loc.x);
    let right_geo = f.niri().global_space.output_geometry(&outs[1]).unwrap();
    let seam = right_geo.loc.x;

    // Straddle the seam: 40px on the left output, 200px on the right → the right output wins.
    let rect = Rectangle::new(
        Point::from((seam - 40, right_geo.loc.y + 100)),
        Size::from((240, 100)),
    );
    let (target, _, _) = f
        .niri()
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
    let mut clock = f.niri().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let ow = 1920.;
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };

    // Nothing recording → no indicator.
    assert!(f
        .niri()
        .panel
        .items(ow, ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));

    let id = CastSessionId::next();
    f.niri().screen_recording_started(id);

    // Shows at 0:00, as a right-box item.
    assert_eq!(f.niri().panel.recording_label(), Some("0:00"));
    assert!(f
        .niri()
        .panel
        .items(ow, ws)
        .iter()
        .any(|i| i.role == ROLE_SCREEN_RECORDING && i.r#box == PanelBox::Right));

    // Re-ticking the label (the seam the 1 s recording timer calls) tracks elapsed time.
    clock.set_unadjusted(t0 + Duration::from_secs(65));
    assert!(f.niri().panel.update_recording_label());
    assert_eq!(f.niri().panel.recording_label(), Some("1:05"));

    clock.set_unadjusted(t0 + Duration::from_secs(600));
    assert!(f.niri().panel.update_recording_label());
    assert_eq!(f.niri().panel.recording_label(), Some("10:00"));
}

#[test]
fn screen_recording_indicator_click_stops_the_recording() {
    use crate::ui::panel::{WorkspaceState, ROLE_SCREEN_RECORDING};
    use crate::utils::CastSessionId;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut clock = f.niri().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let id = CastSessionId::next();
    f.niri().screen_recording_started(id);
    assert!(!f.niri().casting.recordings.is_empty());

    // Click the indicator's center (top panel band).
    let r1 = f.niri().panel.screen_recording_rect(1920.);
    let cx = r1.loc.x + r1.size.w / 2.;
    f.pointer_motion(cx, 16.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    // Clicking it stops the recording through the real hit-test → stop_cast path, and the
    // indicator disappears.
    assert!(
        f.niri().casting.recordings.is_empty(),
        "clicking the indicator stops the recording",
    );
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };
    assert!(f
        .niri()
        .panel
        .items(1920., ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));
}

#[test]
fn native_screen_recording_registers_and_stops() {
    use crate::screencasting::RecordingKind;
    use crate::ui::panel::{WorkspaceState, ROLE_SCREEN_RECORDING};

    // A native recording spawns an ffmpeg encoder; skip cleanly where ffmpeg is unavailable.
    let ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ffmpeg {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let mut clock = f.niri().clock.clone();
    let t0 = clock.now_unadjusted();
    clock.set_unadjusted(t0);

    let output = f.niri().global_space.outputs().next().cloned().unwrap();
    let path = std::env::temp_dir().join(format!("niri-native-rec-{}.webm", std::process::id()));

    // Starting registers a Native recording and shows the R1 pill.
    f.niri()
        .start_native_recording(&output, path.clone(), 30, true, None)
        .unwrap();
    assert!(f
        .niri()
        .casting
        .recordings
        .iter()
        .any(|r| matches!(r.kind, RecordingKind::Native(_))));
    let ws = WorkspaceState {
        count: 1,
        active: 0,
    };
    assert!(f
        .niri()
        .panel
        .items(1920., ws)
        .iter()
        .any(|i| i.role == ROLE_SCREEN_RECORDING));

    // Clicking the pill runs the real hit-test → stop_screen_recordings → finalize-encoder path;
    // the ledger clears and the indicator disappears (regardless of the zero-frame file).
    let r1 = f.niri().panel.screen_recording_rect(1920.);
    let cx = r1.loc.x + r1.size.w / 2.;
    f.pointer_motion(cx, 16.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.niri().casting.recordings.is_empty(),
        "clicking the indicator stops the native recording",
    );
    assert!(f
        .niri()
        .panel
        .items(1920., ws)
        .iter()
        .all(|i| i.role != ROLE_SCREEN_RECORDING));

    std::fs::remove_file(&path).ok();
}

#[cfg(feature = "xdp-gnome-screencast")]
#[test]
fn shell_screencast_dbus_start_and_stop() {
    use crate::dbus::gnome_shell_screencast::ScreencastToNiri;
    use crate::screencasting::RecordingKind;

    let ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ffmpeg {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Land the recording under a temp dir via an absolute template (no XDG env dependency).
    let dir = std::env::temp_dir().join(format!("niri-shell-sc-{}", std::process::id()));
    let template = dir.join("clip %%").to_string_lossy().into_owned();

    let start = |f: &mut Fixture, template: String| {
        let (reply, rx) = async_channel::bounded(1);
        f.niri().on_shell_screencast_msg(ScreencastToNiri::Start {
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
        .niri()
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
    f.niri()
        .on_shell_screencast_msg(ScreencastToNiri::Stop { reply });
    assert!(rx.recv_blocking().unwrap(), "stop found a live recording");
    assert!(f.niri().casting.recordings.is_empty());

    // A ScreencastArea request records a region of the output (a later slice used to decline it).
    let (reply, rx) = async_channel::bounded(1);
    f.niri().on_shell_screencast_msg(ScreencastToNiri::Start {
        area: Some((100, 100, 640, 480)),
        template: dir.join("area %%").to_string_lossy().into_owned(),
        draw_cursor: false,
        framerate: 30,
        reply,
    });
    let area_path = rx.recv_blocking().unwrap().expect("area recording starts");
    assert_eq!(area_path, dir.join("area %.webm").to_string_lossy());
    assert!(f
        .niri()
        .casting
        .recordings
        .iter()
        .any(|r| matches!(r.kind, RecordingKind::Native(_))));

    let (reply, rx) = async_channel::bounded(1);
    f.niri()
        .on_shell_screencast_msg(ScreencastToNiri::Stop { reply });
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
    use crate::notifications::{NotificationsToNiri, NotifyRequest, Urgency};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let mut entry = AppEntry::fake("firefox.desktop", "Firefox");
    entry.icon = AppIconRef::Themed(vec!["firefox".to_owned()]);
    f.niri().app_system = AppSystem::with_parts(
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
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify {
            req: web_notification("Firefox", Some("firefox")),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));

    let source = &f.niri().notifications.sources[0];
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
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify {
            req: web_notification("Some Unknown App", Some("not-installed")),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(2));
    let unresolved = f
        .niri()
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
    use crate::notifications::{NotificationsToNiri, NotifyRequest, Urgency};

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
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify {
            req: req("app", ":1.7", 0),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));
    assert_eq!(f.niri().notifications.sources.len(), 1);
    assert_eq!(f.niri().notifications.find(1).unwrap().title, "title");

    // Replace (same sender) mutates in place, same id, no new notification.
    let mut update = req("app", ":1.7", 1);
    update.title = "updated".to_owned();
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify { req: update, reply });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(1));
    assert_eq!(f.niri().notifications.sources[0].notifications.len(), 1);
    assert_eq!(f.niri().notifications.find(1).unwrap().title, "updated");

    // Replace from a different sender is rejected (the fdo proxy's
    // "Invalid notification ID").
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify {
            req: req("evil", ":1.66", 1),
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());

    // CloseNotification: foreign sender rejected, own sender destroys and the
    // owed NotificationClosed emission (reason 3 = the app asked) reaches the
    // server's emit channel.
    let (to_notifications, emitted) = async_channel::unbounded();
    f.niri_state().niri.notifications_emit = Some(to_notifications);

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Close {
            id: 1,
            sender: ":1.66".to_owned(),
            reply,
        });
    assert!(rx.recv_blocking().unwrap().is_err());
    assert!(f.niri().notifications.find(1).is_some());

    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Close {
            id: 1,
            sender: ":1.7".to_owned(),
            reply,
        });
    assert_eq!(rx.recv_blocking().unwrap(), Ok(()));
    assert!(f.niri().notifications.find(1).is_none());
    assert!(
        f.niri().notifications.sources.is_empty(),
        "a source with zero notifications removes itself",
    );
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::Closed { id, reason, sender } => {
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
    use crate::notifications::{NotificationsToNiri, NotifyRequest, SourceKey, Urgency};

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
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify {
            req: app.clone(),
            reply,
        });
    rx.recv_blocking().unwrap().unwrap();

    app.desktop_entry = None;
    app.app_name = "notify-send".to_owned();
    let (reply, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::Notify { req: app, reply });
    rx.recv_blocking().unwrap().unwrap();

    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::SenderVanished(":1.9".to_owned()));
    let sources = &f.niri().notifications.sources;
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
        GtkNotifyRequest, GtkToNotifications, NotificationsToNiri, SourceKey, Urgency,
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
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::AddGtk {
            req: gtk_req("msg-1"),
        });
    assert_eq!(f.niri().notifications.sources.len(), 1);
    let source = &f.niri().notifications.sources[0];
    assert!(matches!(&source.key, SourceKey::GtkApp(a) if a == "org.example.Chat"));
    assert_eq!(source.title, "Chat");
    let id = source.notifications[0].id;

    // Add with the same (app_id, gtk_id) replaces in place — no second card.
    let mut update = gtk_req("msg-1");
    update.title = "updated".to_owned();
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::AddGtk { req: update });
    assert_eq!(f.niri().notifications.sources[0].notifications.len(), 1);
    assert_eq!(f.niri().notifications.find(id).unwrap().title, "updated");

    // A non-`app.` action routes to the Gtk emit channel (NOT the fdo one).
    let (to_gtk, gtk_emitted) = async_channel::unbounded();
    f.niri_state().niri.gtk_notifications_emit = Some(to_gtk);
    let (to_fdo, fdo_emitted) = async_channel::unbounded();
    f.niri_state().niri.notifications_emit = Some(to_fdo);

    assert!(
        f.niri_state()
            .niri
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
        .niri_state()
        .niri
        .emit_notification_action(id, "default".to_owned()));
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::ActionInvoked { action, .. } => assert_eq!(action, "app.open"),
        _ => panic!("expected ActionInvoked"),
    }

    // Remove destroys it and emits no fdo NotificationClosed (no sender).
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::RemoveGtk {
            app_id: "org.example.Chat".to_owned(),
            gtk_id: "msg-1".to_owned(),
        });
    assert!(f.niri().notifications.find(id).is_none());
    assert!(f.niri().notifications.sources.is_empty());
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
        GtkNotifyRequest, GtkToNotifications, NotificationsToNiri, Urgency,
    };

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri_state()
        .on_notifications_msg(NotificationsToNiri::AddGtk {
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
    let id = f.niri().notifications.sources[0].notifications[0].id;

    let (to_gtk, gtk_emitted) = async_channel::unbounded();
    f.niri_state().niri.gtk_notifications_emit = Some(to_gtk);
    assert!(f.niri_state().niri.open_notification_app(id));
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::Activate { app_id, .. } => assert_eq!(app_id, "org.example.App"),
        _ => panic!("expected Activate"),
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
        f.niri_state().do_action(Action::OpenOverview, false);
        f.niri_complete_animations();
        assert!(f.niri().layout.is_overview_open());
    };

    // An *empty* command: `spawn` returns early on it (`utils::spawning`), so this
    // exercises the choke point without a test really launching gnome-control-center.
    open_overview(&mut f);
    f.niri_state()
        .apply_popover_action(PopoverAction::Spawn(Vec::new()));
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "a panel/quick-settings button that starts an app must leave the overview"
    );

    // A toggle that changes a setting stays put — GNOME hides only for the rows that
    // raise a window.
    open_overview(&mut f);
    f.niri_state()
        .apply_popover_action(PopoverAction::SetDarkStyle(true));
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "a quick-settings toggle must not close the overview"
    );

    // An fdo notification action is a signal to the app, not an activation by us.
    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.niri_state()
        .apply_popover_action(PopoverAction::InvokeNotificationAction {
            id,
            key: "reply".to_owned(),
        });
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
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
    f.niri_state()
        .on_notifications_msg(crate::notifications::NotificationsToNiri::Notify { req, reply });
    rx.recv_blocking().unwrap().unwrap()
}

/// Pin the clock forward and advance — the banner's deadline authority is the
/// pinned clock, not the wake-up timer (see the headless-animation-clock trap).
fn tick(f: &mut Fixture, ms: u64) {
    let niri = f.niri();
    let now = niri.clock.now_unadjusted();
    niri.clock.set_unadjusted(now + Duration::from_millis(ms));
    niri.advance_animations();
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
    assert!(f.niri().notification_banner.is_visible());
    f.settle_animations();
    assert_eq!(f.niri().notification_banner.content_id(), Some(id));
    // Showing acknowledged it (`js/ui/messageTray.js:1167`).
    assert!(f.niri().notifications.find(id).unwrap().acknowledged);
    // The deadline is armed at the Showing->Shown transition inside
    // advance_animations — the wake-up timer must be re-armed there too, or a
    // banner over a damage-free desktop would never wake the loop to expire.
    assert!(f.niri().notification_banner_timer.is_some());

    // The 4 s timeout elapses -> hide -> the notification SURVIVES in the store.
    tick(&mut f, 4100);
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_visible());
    assert!(f.niri().notifications.find(id).is_some());

    // A transient notification is destroyed by its banner hiding (EXPIRED).
    // Fresh activity first: the pinned clock has drifted past the idle
    // threshold, which would otherwise idle-gate the expiry (a real behavior,
    // pinned by `notification_banner_idle_gates_expiry`). `notify_activity`
    // runs once per event-loop iteration; clear the guard by hand since no
    // real iteration boundary passes in this test.
    f.niri().notified_activity_this_iteration = false;
    f.pointer_motion(1., 1.);
    let mut transient = banner_req("app", ":1.1");
    transient.transient = true;
    let tid = banner_notify(&mut f, transient);
    f.settle_animations();
    tick(&mut f, 4100);
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_visible());
    assert!(f.niri().notifications.find(tid).is_none());
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
    assert!(!f.niri().notification_banner.is_visible());

    f.niri().gnome_settings.quick_toggles.do_not_disturb = true;
    banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(!f.niri().notification_banner.is_visible());

    let mut critical = banner_req("app", ":1.1");
    critical.urgency = crate::notifications::Urgency::Critical;
    let cid = banner_notify(&mut f, critical);
    assert!(f.niri().notification_banner.is_visible());
    f.settle_animations();
    // No deadline: still up long past the normal timeout.
    tick(&mut f, 60_000);
    f.settle_animations();
    assert_eq!(f.niri().notification_banner.content_id(), Some(cid));
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
    assert_eq!(f.niri().notification_banner.content_id(), Some(first));
    tick(&mut f, 4100);
    f.settle_animations();
    // The critical one jumped the queue.
    assert_eq!(f.niri().notification_banner.content_id(), Some(crit));
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
    assert!(!f.niri().notification_banner.is_visible());

    // Replace the now-hidden, acked notification: it banners again.
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    assert_eq!(banner_notify(&mut f, update), id);
    assert!(f.niri().notification_banner.is_visible());
    f.settle_animations();
    assert_eq!(f.niri().notification_banner.content_id(), Some(id));

    // Replace while showing: stays visible, re-acked (never counts unseen).
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    update.title = "updated".to_owned();
    assert_eq!(banner_notify(&mut f, update), id);
    assert!(f.niri().notification_banner.is_visible());
    assert!(f.niri().notifications.find(id).unwrap().acknowledged);
    assert_eq!(f.niri().notifications.unseen_count(), 0);

    // Replace while the banner is mid-hide: "we stop hiding it and show it
    // again" (`js/ui/messageTray.js:938-943`). Fresh activity first so the
    // deadline is armed, then let it lapse to start the hide animation.
    f.niri().notified_activity_this_iteration = false;
    f.pointer_motion(1., 1.);
    tick(&mut f, 2100);
    assert!(f.niri().notification_banner.is_visible()); // Hiding, not yet gone
    let mut update = banner_req("app", ":1.1");
    update.replaces_id = id;
    update.title = "updated again".to_owned();
    assert_eq!(banner_notify(&mut f, update), id);
    f.settle_animations();
    assert!(f.niri().notification_banner.is_visible());
    assert_eq!(f.niri().notification_banner.content_id(), Some(id));
}

/// Clicking the close button destroys DISMISSED and the owed NotificationClosed
/// emission reaches the server channel (`js/ui/messageList.js:725-728`).
#[test]
fn notification_banner_close_click_dismisses() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, emitted) = async_channel::unbounded();
    f.niri().notifications_emit = Some(tx);

    let id = banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();

    // Banner geometry: 34em wide centered, y = panel(32) + margin(4); the close
    // circle (28px) sits PAD + its 3px margin from the right edge, centered in
    // the header row (`_message-list.scss:152-155`).
    let em = crate::ui::pt_to_px(11.);
    let w = 34. * em;
    let x0 = (1920. - w) / 2.;
    let close_x = x0 + w - 6. - 3. - 14.;
    let close_y = 36. + 6. + 12.;
    pointer_motion_to(&mut f, close_x, close_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(!f.niri().notification_banner.is_visible());
    assert!(f.niri().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::Closed {
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
    f.niri().notifications_emit = Some(tx);

    let mut req = banner_req("app", ":1.1");
    req.actions = vec![("ok".to_owned(), "OK".to_owned())];
    let id = banner_notify(&mut f, req);
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_expanded());

    // Hovering the shown banner expands it, revealing the action row.
    pointer_motion_to(&mut f, 960., 80.);
    assert!(f.niri().notification_banner.is_expanded());

    // Single action button: centered in the action row below the body block.
    let action_y = 36. + 6. + 24. + 6. + 48. + 6. + 14.;
    pointer_motion_to(&mut f, 960., action_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(f.niri().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::ActionInvoked {
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
        crate::notifications::NiriToNotifications::Closed { reason, .. } => {
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
    assert!(f.niri().notification_banner.is_visible());

    // Open the calendar via a clock click (panel y < banner y: no overlap).
    pointer_motion_to(&mut f, 960., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.niri().panel_popover.is_open());
    f.settle_animations();
    f.settle_animations();
    assert!(
        !f.niri().notification_banner.is_visible(),
        "banners are blocked while a popover is open"
    );

    // A notification arriving while blocked stays queued.
    let queued = banner_notify(&mut f, banner_req("other", ":1.2"));
    assert!(!f.niri().notification_banner.is_visible());

    // Closing the popover drains the queue.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    f.settle_animations();
    assert_eq!(f.niri().notification_banner.content_id(), Some(queued));
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
    assert_eq!(f.niri().notification_banner.content_id(), Some(id));

    // Long past the normal timeout, still up: waiting for the user.
    tick(&mut f, 30_000);
    f.settle_animations();
    assert!(f.niri().notification_banner.is_visible());

    // First activity arms the 2 s deadline.
    f.pointer_motion(1., 1.);
    tick(&mut f, 2500);
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_visible());
    assert!(f.niri().notifications.find(id).is_some());
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
    assert!(f.niri().notification_banner.is_visible());
    f.pointer_motion(1., 1.);
    f.settle_animations();
    assert_eq!(f.niri().notification_banner.content_id(), Some(id));

    // The short timeout applies — without the fix this waited forever.
    tick(&mut f, 2500);
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_visible());
}

/// Open the calendar popover with a clock click.
fn open_calendar(f: &mut Fixture) {
    pointer_motion_to(f, 960., 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.niri().panel_popover.is_open());
}

/// Calendar events flow into the store through `on_calendar_events_msg`, the way
/// the `org.gnome.Shell.CalendarServer` watcher would deliver them, and
/// `has_calendars` gates section visibility (DBusEventSource / `_sync`,
/// `js/ui/calendar.js`, `js/ui/dateMenu.js`).
#[test]
fn calendar_events_flow_into_the_store() {
    use crate::calendar_events::{CalendarEvent, CalendarToNiri};
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let ev = |id: &str, start: i64, end: i64| CalendarEvent {
        id: id.into(),
        summary: "Meeting".into(),
        start,
        end,
    };

    // No calendars yet → section hidden.
    assert!(!f.niri().calendar_events.has_calendars());
    f.niri_state()
        .on_calendar_events_msg(CalendarToNiri::HasCalendars(true));
    assert!(f.niri().calendar_events.has_calendars());

    // A batch lands in the store.
    f.niri_state()
        .on_calendar_events_msg(CalendarToNiri::EventsAddedOrUpdated(vec![
            ev("uid\n1", 100, 200),
            ev("uid\n2", 300, 400),
        ]));
    assert_eq!(f.niri().calendar_events.events_for(0, 1000).len(), 2);

    // A removal is a prefix delete.
    f.niri_state()
        .on_calendar_events_msg(CalendarToNiri::EventsRemoved(vec!["uid\n1".into()]));
    assert_eq!(f.niri().calendar_events.events_for(0, 1000).len(), 1);

    // A range change wipes the cache (the watcher sends this before the new
    // range loads) but keeps `has_calendars`.
    f.niri_state()
        .on_calendar_events_msg(CalendarToNiri::CacheReset);
    assert!(f.niri().calendar_events.events_for(0, 1000).is_empty());
    assert!(f.niri().calendar_events.has_calendars());

    // The server vanishing clears the store and hides the section.
    f.niri_state()
        .on_calendar_events_msg(CalendarToNiri::OwnerVanished);
    assert!(!f.niri().calendar_events.has_calendars());
    assert!(f.niri().calendar_events.events_for(0, 1000).is_empty());
}

/// Opening the calendar asks the CalendarServer watcher to load exactly the
/// shown month's 42-cell grid range (`js/ui/calendar.js:748` — the per-rebuild
/// `requestRange`; paging re-runs the same `sync_calendar_range`).
#[test]
fn opening_the_calendar_requests_its_grid_range() {
    use crate::calendar_events::NiriToCalendar;
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);

    let (tx, rx) = async_channel::unbounded();
    f.niri_state().niri.calendar_range_emit = Some(tx);

    open_calendar(&mut f);

    let expected = f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .calendar
        .grid_range();
    // The open path issued a range request for the shown grid.
    let mut last = None;
    while let Ok(NiriToCalendar::SetRange { since, until }) = rx.try_recv() {
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
    assert_eq!(f.niri().notifications.unseen_count(), 1);
    assert!(!f.niri().notifications.banner_queue.is_empty());

    open_calendar(&mut f);
    assert_eq!(
        f.niri().notifications.unseen_count(),
        0,
        "opening the list acknowledges everything"
    );
    assert!(
        f.niri().notifications.banner_queue.is_empty(),
        "acked notifications drop out of the banner queue"
    );
    assert_eq!(
        f.niri().panel_popover.date_menu().unwrap().list().len(),
        2,
        "the list snapshots the whole store"
    );

    // A notification arriving while open lands in the list WITHOUT an ack.
    let id3 = banner_notify(&mut f, banner_req("app-c", ":1.3"));
    assert_eq!(f.niri().panel_popover.date_menu().unwrap().list().len(), 3);
    assert_eq!(
        f.niri().notifications.unseen_count(),
        1,
        "arrivals while open stay unseen"
    );

    // Closing does not acknowledge: the still-unseen notification banners as
    // soon as the popover unblocks the tray.
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();
    f.settle_animations();
    assert!(!f.niri().panel_popover.is_open());
    assert_eq!(
        f.niri().notification_banner.content_id(),
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
    let output = f.niri_output(1);
    let id = f.add_client();
    // A large floating window covering the top-center where the popover opens.
    let _w = map_window_sized(&mut f, id, (1800, 1000), None);

    // A point inside the window, before the popover opens, focuses the window.
    let over_window = Point::<f64, Logical>::from((900., 120.));
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.niri().contents_under(over_window).surface.is_some(),
        "the window under the pointer normally receives pointer focus"
    );
    assert!(
        f.niri()
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
    let origin = f.niri().panel_popover.content_location(&output);
    let over_popover = origin + Point::from((50., 50.));
    assert!(
        f.niri().panel_popover.contains(&output, over_popover),
        "the sampled point is inside the popover content"
    );
    pointer_motion_to(&mut f, over_popover.x, over_popover.y);
    assert!(
        f.niri().contents_under(over_popover).surface.is_none(),
        "no window under the open popover receives pointer focus"
    );
    assert!(
        f.niri()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "the seat pointer focus is cleared while the popover is open"
    );
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
    assert!(!f.niri().notification_banner.is_visible());
    assert!(
        f.niri().panel.messages_indicator_visible(),
        "an unseen low notification lights the dot"
    );

    // Opening the calendar acknowledges everything → the dot clears.
    open_calendar(&mut f);
    assert!(
        !f.niri().panel.messages_indicator_visible(),
        "opening the list clears the dot"
    );
    f.key_press(KEY_ESC);
    f.key_release(KEY_ESC);
    f.settle_animations();

    // Under DND, an unseen notification does NOT light the dot — GNOME gates the
    // indicator on `show-banners` (`js/ui/dateMenu.js:796-797`).
    f.niri().gnome_settings.quick_toggles.do_not_disturb = true;
    banner_notify(&mut f, banner_req("app", ":1.1"));
    assert!(f.niri().notifications.unseen_count() > 0);
    assert!(
        !f.niri().panel.messages_indicator_visible(),
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
    f.niri().notifications_emit = Some(tx);

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
    let output = f.niri_output(1);
    let origin = f.niri().panel_popover.content_location(&output);
    // All three cards are reachable now (the list scrolls); they render
    // newest-first.
    let cards = f.niri().panel_popover.date_menu().unwrap().card_rects();
    assert_eq!(
        cards.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        vec![pid, did, rid],
        "sources render newest-first; scrolling keeps every card reachable"
    );
    assert_eq!(
        f.niri().panel_popover.date_menu().unwrap().list().len(),
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
    assert!(f.niri().notifications.find(pid).is_none());
    assert_eq!(f.niri().panel_popover.date_menu().unwrap().list().len(), 2);
    assert!(f.niri().panel_popover.is_open(), "the popover stays open");
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::Closed { id, reason, .. } => {
            assert_eq!((id, reason.wire_code()), (pid, 2), "Dismissed on the wire");
        }
        _ => panic!("expected a Closed emission"),
    }

    // Body-click the default-action card: ActionInvoked('default') unicast +
    // destroyed (non-resident) — and the popover CLOSES (activation drops the
    // menu, `js/ui/notificationDaemon.js:370-382`).
    let cards = f.niri().panel_popover.date_menu().unwrap().card_rects();
    let (_, card, _) = cards[0];
    click(
        &mut f,
        smithay::utils::Point::from((card.loc.x + 30., card.loc.y + card.size.h - 10.)),
    );
    assert!(f.niri().notifications.find(did).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::ActionInvoked {
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
        !f.niri().panel_popover.is_open(),
        "activating a notification closes the calendar"
    );

    // Body-click the resident card (no default action): `source.open()`
    // destroys only non-resident notifications — it survives; the popover
    // closes here too.
    open_calendar(&mut f);
    let origin = f.niri().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: smithay::utils::Point<f64, smithay::utils::Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let cards = f.niri().panel_popover.date_menu().unwrap().card_rects();
    let (_, card, _) = cards[0];
    click(
        &mut f,
        smithay::utils::Point::from((card.loc.x + 30., card.loc.y + card.size.h - 10.)),
    );
    assert!(
        f.niri().notifications.find(rid).is_some(),
        "a resident notification survives activation"
    );
    f.settle_animations();
    assert!(!f.niri().panel_popover.is_open());

    // Clear: everything (resident included) closes; the placeholder is up.
    open_calendar(&mut f);
    let pill = f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .clear_pill_rect()
        .unwrap();
    click(&mut f, rect_center(pill));
    assert!(f.niri().notifications.sources.is_empty());
    assert!(f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .list()
        .is_empty());
    assert!(
        f.niri().panel_popover.is_open(),
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
    f.niri().notifications_emit = Some(tx);

    let mut req = banner_req("app-a", ":1.1");
    req.body = "a long body ".repeat(40).trim_end().to_owned();
    req.actions = vec![
        ("ok".to_owned(), "OK".to_owned()),
        ("no".to_owned(), "No".to_owned()),
    ];
    let id = banner_notify(&mut f, req);
    f.settle_animations();

    open_calendar(&mut f);
    let output = f.niri_output(1);
    let origin = f.niri().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: smithay::utils::Point<f64, smithay::utils::Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let rect_center = |rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        smithay::utils::Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
    };
    let dm = |f: &mut Fixture| {
        let card = f.niri().panel_popover.date_menu().unwrap().card_rects()[0];
        let expand = f
            .niri()
            .panel_popover
            .date_menu()
            .unwrap()
            .card_expand_rect(card.0);
        let actions = f
            .niri()
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
    assert!(f.niri().panel_popover.is_open());
    assert!(f.niri().notifications.find(id).is_some());
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
    let rects = f.niri().panel_popover.date_menu().unwrap().card_rects();
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
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(id)
        .unwrap();
    click(&mut f, rect_center(caret));
    let rects = f.niri().panel_popover.date_menu().unwrap().card_rects();
    assert_eq!(rects[1].1.size.h, collapsed_h);
    assert!(f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_action_rects(id)
        .is_empty());

    // Expand once more and invoke the second action: ActionInvoked('no')
    // unicast with a real token, the notification destroyed (non-resident),
    // and the popover closes (the app it raised takes over).
    let caret = f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(id)
        .unwrap();
    click(&mut f, rect_center(caret));
    // The expanded card's action row now sits below the second card, past the
    // fold — scroll the list down to bring it into view (as a user would).
    f.niri().panel_popover.pointer_scroll(
        &output,
        origin + smithay::utils::Point::from((30., 30.)),
        1000.,
    );
    let actions = f
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .card_action_rects(id);
    click(&mut f, rect_center(actions[1]));
    assert!(f.niri().notifications.find(id).is_none());
    match emitted.recv_blocking().unwrap() {
        crate::notifications::NiriToNotifications::ActionInvoked {
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
        crate::notifications::NiriToNotifications::Closed { reason, .. } => {
            assert_eq!(reason.wire_code(), 2);
        }
        _ => panic!("expected a Closed emission"),
    }
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
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
    let output = f.niri_output(1);
    let origin = f.niri().panel_popover.content_location(&output);
    let click = |f: &mut Fixture, pos: Point<f64, Logical>| {
        pointer_motion_to(f, origin.x + pos.x, origin.y + pos.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
    };
    let rect_center = |r: Rectangle<f64, Logical>| {
        Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
    };
    let groups = |f: &mut Fixture| f.niri().panel_popover.date_menu().unwrap().group_rects();

    // Collapsed: one group, not expanded, no per-card interactive rects.
    let g = groups(&mut f);
    assert_eq!(g.len(), 1, "both notifications collapse into one stack");
    assert!(!g[0].2, "the stack starts collapsed");
    assert!(f
        .niri()
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
    assert!(f.niri().panel_popover.is_open());
    assert!(groups(&mut f)[0].2, "clicking the stack expanded it");
    assert_eq!(
        f.niri()
            .panel_popover
            .date_menu()
            .unwrap()
            .card_rects()
            .len(),
        2,
        "expanded: both cards individually interactive"
    );
    // Expanding is pure UI — the store is untouched.
    assert!(f.niri().notifications.find(id1).is_some());
    assert!(f.niri().notifications.find(id2).is_some());

    // The header collapse button fans it back to a stack.
    let key = groups(&mut f)[0].0.clone();
    let collapse = f
        .niri()
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
        .niri()
        .panel_popover
        .date_menu()
        .unwrap()
        .stack_close_rect(&key)
        .expect("collapsed stack has a top-card close");
    click(&mut f, rect_center(stack_close));
    assert!(f.niri().notifications.find(id1).is_none());
    assert!(f.niri().notifications.find(id2).is_none());
    assert!(
        f.niri()
            .panel_popover
            .date_menu()
            .unwrap()
            .list()
            .is_empty(),
        "closing the group empties the list"
    );
    assert!(
        f.niri().panel_popover.is_open(),
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

    // Park the pointer where the banner is about to land.
    f.pointer_motion(1., 1.);
    pointer_motion_to(&mut f, 960., 80.);
    banner_notify(&mut f, banner_req("app", ":1.1"));
    f.settle_animations();
    assert!(!f.niri().notification_banner.is_expanded());

    // Hovering in place (it popped up under us) must NOT expand.
    pointer_motion_to(&mut f, 961., 80.);
    assert!(
        !f.niri().notification_banner.is_expanded(),
        "popped-under-pointer: hover without leaving first doesn't expand"
    );

    // Leave, come back: now it expands.
    pointer_motion_to(&mut f, 960., 400.);
    pointer_motion_to(&mut f, 960., 80.);
    assert!(f.niri().notification_banner.is_expanded());
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
    pointer_motion_to(&mut f, 960., 80.);
    assert!(!f.niri().notification_banner.is_expanded());

    f.settle_animations();
    assert!(
        f.niri().notification_banner.is_expanded(),
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
        f.niri().notification_banner.is_expanded(),
        "critical expands at show, before any hover"
    );
    f.settle_animations();

    // The action row is present in the hit-test (short body: one line, so the
    // row sits right below the 48px body block).
    let output = f.niri_output(1);
    let action_pos = smithay::utils::Point::from((960., 36. + 6. + 24. + 6. + 48. + 6. + 14.));
    assert_eq!(
        f.niri().notification_banner.hit_test(&output, action_pos),
        Some(crate::ui::notification_banner::BannerHit::Action(0))
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
        f.niri().app_system.installed().count(),
        0,
        "headless AppSystem must be inert"
    );
    assert!(f.niri().app_system.favorites().is_empty());

    let recorder = RecordingLauncher::default();
    let catalog = FakeCatalog::new(vec![AppEntry::fake("org.example.App.desktop", "App")]);
    f.niri().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder.clone()));

    f.niri()
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
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let recorder = RecordingLauncher::default();
    let apps = favorites
        .iter()
        .map(|id| AppEntry::fake(id, id))
        .collect::<Vec<_>>();
    f.niri().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.niri()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.niri().sync_dash_favorites();

    f.niri_state().do_action(Action::OpenOverview, false);
    assert!(f.niri().layout.is_overview_open(), "overview must open");

    (f, recorder)
}

/// The overview chrome's allocated boxes on output 1 — the same
/// `ControlsManagerLayout` the render and input paths consume.
fn overview_controls(f: &mut Fixture) -> crate::ui::overview_layout::ControlsLayout {
    let output = f.niri_output(1);
    f.niri()
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
    f.niri()
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    assert!(!f.niri().screen_shield.is_active(), "starts up");

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(true));
    assert!(f.niri().screen_shield.is_active(), "SetActive(true) blanks");
    assert!(
        !f.niri().screen_shield.is_locked(),
        "...but the screensaver is not a lock"
    );

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(false));
    assert!(!f.niri().screen_shield.is_active());

    // `Lock` also puts the shield down — the difference is what it takes to raise it.
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    assert!(f.niri().screen_shield.is_active(), "Lock blanks too");

    // The snapshot the bus reads deliberately lags the model: `active` is not published until the
    // curtain has landed, so the slide is not replaced by whatever gsd-power does on
    // `ActiveChanged` (our divergence from GNOME's beat — see `lock-screen-backlog.md` item H).
    assert!(
        !f.niri().shield_snapshot.lock().unwrap().active,
        "GetActive must not claim the screensaver is up while the curtain is still sliding"
    );

    f.niri().lock_screen.settle();
    f.niri().publish_shield_active();
    assert!(
        f.niri().shield_snapshot.lock().unwrap().active,
        "...and must say so the moment it lands, or gsd never blanks at all"
    );

    // Raising it publishes at once — there is nothing to wait for on the way out.
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(false));
    assert!(
        !f.niri().shield_snapshot.lock().unwrap().active,
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
    use crate::dbus::gnome_session_presence::{PresenceStatus, PresenceToNiri};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_presence_msg(PresenceToNiri::StatusChanged(PresenceStatus::Idle));
    assert!(
        !f.niri().screen_shield.is_active(),
        "idle fades to black first; it does not cover"
    );
    assert!(f.niri().fade_timer.is_some(), "the fade is running");
    assert!(f.niri().lock_timer.is_some(), "and so is the grace period");

    f.niri_state()
        .on_presence_msg(PresenceToNiri::StatusChanged(PresenceStatus::Available));
    assert!(
        f.niri().fade_timer.is_none(),
        "coming back drops the fade..."
    );
    assert!(
        f.niri().lock_timer.is_none(),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // What a build without D-Bus, or a gdm client that failed to start, actually looks like.
    f.niri_state().niri.gdm_requests = None;

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    assert!(f.niri().screen_shield.is_active(), "the screen is covered");
    assert!(!f.niri().screen_shield.is_locked(), "but never locked");
    assert!(
        f.niri().screen_shield.is_dismissible(),
        "and raising it must not wait on an answer nobody will send"
    );
}

/// A status we do not recognise is not idleness.
///
/// gnome-session can grow a new `PresenceStatus`, and mapping an unknown one onto idle would blank
/// the screen for a reason nobody chose.
#[test]
fn an_unknown_presence_status_does_not_blank_the_screen() {
    use crate::dbus::gnome_session_presence::{PresenceStatus, PresenceToNiri};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_presence_msg(PresenceToNiri::StatusChanged(PresenceStatus::Unknown(42)));
    assert!(!f.niri().screen_shield.is_active());
    assert!(f.niri().lock_timer.is_none());
    assert!(f.niri().fade_timer.is_none(), "and nothing starts fading");
}

/// logind's `PrepareForSleep(true)` locks before the machine goes down, with no grace period.
///
/// The delay inhibitor is what buys the time to do this at all, so the assertion that matters is
/// that the shield is *covered and asking to lock* by the time the handler returns — anything
/// deferred to a timer would run after the suspend.
#[test]
fn suspending_covers_the_screen_immediately() {
    use crate::dbus::freedesktop_login1::Login1ToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_login1_msg(Login1ToNiri::PrepareForSleep(true));
    assert!(f.niri().screen_shield.is_active());
    assert!(
        f.niri().lock_timer.is_none(),
        "a suspend has no grace period"
    );

    // Resuming wakes the screen but leaves the shield where it is.
    f.niri_state()
        .on_login1_msg(Login1ToNiri::PrepareForSleep(false));
    assert!(f.niri().screen_shield.is_active());
}

/// `disable-lock-screen` makes `Lock` a no-op — the shield does not even go down.
///
/// GNOME returns *before* `activate` (`screenShield.js:638-641`), so a locked-down session does
/// not get a blanked screen out of a `Lock` either. Getting that order wrong blanks a machine
/// whose administrator disabled locking.
#[test]
fn lockdown_makes_the_screen_saver_lock_a_no_op() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri()
        .screen_shield
        .set_settings(crate::screen_shield::ShieldSettings {
            disable_lock_screen: true,
            ..Default::default()
        });

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    assert!(!f.niri().screen_shield.is_active());
    assert!(!f.niri().shield_snapshot.lock().unwrap().active);

    // `SetActive` is a different call and is *not* gated by lockdown — the screensaver still
    // blanks, it just never becomes a lock.
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(true));
    assert!(f.niri().screen_shield.is_active());
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(true));
    assert!(f.niri().screen_shield.is_active());

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        !f.niri().screen_shield.is_active(),
        "a key press raises the shield"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
        "and the key that raised it must not also have reached the desktop's binds"
    );
    assert!(
        !f.niri().shield_snapshot.lock().unwrap().active,
        "GetActive follows the dismissal too"
    );

    // A second tap now behaves normally — the shield is not swallowing input forever.
    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "with the shield up, Super works again"
    );
}

/// A click raises the shield too, and **both** button edges are swallowed.
///
/// Forwarding the release alone would hand whatever is under the pointer a button-up it never saw
/// pressed — which is how a dismissing click ends up activating a panel button behind the curtain.
#[test]
fn a_click_raises_the_shield_and_neither_edge_reaches_the_desktop() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Park the pointer over the panel's Activities corner, whose click opens the overview — the
    // observable thing a leaked edge would trigger.
    pointer_motion_to(&mut f, 10., 10.);
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::SetActive(true));

    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        !f.niri().screen_shield.is_active(),
        "a click raises the shield"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;
    use crate::unlock_dialog::{Page, Status};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // `Lock` covers the screen at once, but must NOT claim to be locked — nothing can unlock it
    // until gdm answers.
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    assert!(f.niri().screen_shield.is_active(), "the screen is covered");
    assert!(
        !f.niri().screen_shield.is_locked(),
        "not locked until a verifier exists"
    );

    // gdm opens the channel. `epoch` is whatever the shield asked under; the model owns it, so
    // read it back rather than assuming 1.
    let epoch = 1;
    f.niri_state()
        .on_verifier_event(VerifierEvent::Ready(epoch));
    assert!(
        f.niri().screen_shield.is_locked(),
        "a live channel is what locks it"
    );

    // ...and it asks for the password.
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // A key on the clock page raises the prompt AND is kept.
    assert_eq!(f.niri().unlock_dialog.page(), Page::Clock);
    tap(&mut f, KEY_A);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Prompt);
    assert_eq!(
        f.niri().unlock_dialog.entry_display(),
        "\u{25cf}",
        "the first keystroke is not eaten by the page flip, and it is masked"
    );

    tap(&mut f, KEY_T);
    assert_eq!(f.niri().unlock_dialog.entry_display(), "\u{25cf}\u{25cf}");

    // Backspace, then Return sends the answer.
    tap(&mut f, KEY_BACKSPACE);
    assert_eq!(f.niri().unlock_dialog.entry_display(), "\u{25cf}");
    tap(&mut f, KEY_ENTER);
    assert_eq!(f.niri().unlock_dialog.status(), Status::Answered);
    assert_eq!(
        f.niri().unlock_dialog.entry_display(),
        "",
        "the buffer does not outlive the answer"
    );

    // A refusal keeps the shield down and lets the user try again.
    f.niri_state().on_verifier_event(VerifierEvent::Failed);
    assert!(f.niri().screen_shield.is_locked(), "still locked");
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    assert!(f.niri().unlock_dialog.is_entry_live(), "and can retry");

    // Only gdm's verdict raises it.
    f.niri_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.niri().screen_shield.is_locked());
    assert!(!f.niri().screen_shield.is_active(), "the shield is up");
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Onto the prompt, and let the crossfade finish so the return trip starts from a clean 1.
    tap(&mut f, KEY_A);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Prompt);
    f.niri().lock_screen.settle_page();
    let now = crate::utils::get_monotonic_time();
    assert_eq!(f.niri().lock_screen.page_progress(now), 1., "on the prompt");

    // Escape goes back to the clock — as an animation, not a jump.
    tap(&mut f, KEY_ESC);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Clock);
    let now = crate::utils::get_monotonic_time();
    assert!(
        f.niri().lock_screen.page_is_animating(now),
        "the way back owes frames"
    );
    let back = f.niri().lock_screen.page_progress(now);
    assert!(
        back > 0.,
        "the clock fades in from where the prompt was, it does not cut: {back}"
    );

    // Now unlock from the prompt. The page must stay put: the curtain carries it out.
    tap(&mut f, KEY_A);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Prompt);
    f.niri().lock_screen.settle_page();
    f.niri_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.niri().screen_shield.is_active(), "the shield is up");

    let now = crate::utils::get_monotonic_time();
    assert!(
        f.niri().lock_screen.is_covering(now),
        "but the curtain is still sliding away"
    );
    assert_eq!(
        f.niri().lock_screen.page_progress(now),
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
    use crate::dbus::gnome_screen_saver::{LockReply, ScreenSaverToNiri};

    /// Poll the caller's side of the reply channel without blocking.
    fn answered(rx: &async_channel::Receiver<()>) -> bool {
        !matches!(rx.try_recv(), Err(async_channel::TryRecvError::Empty))
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // --- A lock from a bare screen waits for the curtain to land. ---
    let (tx, rx) = async_channel::bounded(1);
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(Some(LockReply::for_test(tx))));
    assert!(
        !answered(&rx),
        "answered while the curtain was still on its way down"
    );

    // The slide finishing is what answers. Settling stands in for the 250 ms.
    f.niri().lock_screen.settle();
    f.niri().settle_lock_replies();
    assert!(
        answered(&rx),
        "the shield is up and the caller is still waiting"
    );

    // --- A second lock, with the screen already covered, answers at once. ---
    let (tx2, rx2) = async_channel::bounded(1);
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(Some(LockReply::for_test(tx2))));
    assert!(
        answered(&rx2),
        "a lock at an already-covered screen must not wait for an edge that cannot come"
    );

    // --- A refused lock answers rather than hanging. ---
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let mut settings = f.niri().screen_shield.settings();
    settings.disable_lock_screen = true;
    f.niri().screen_shield.set_settings(settings);

    let (tx3, rx3) = async_channel::bounded(1);
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(Some(LockReply::for_test(tx3))));
    assert!(!f.niri().screen_shield.is_active(), "lockdown refused it");
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Caps lock on the *clock* page warns about nothing: there is no entry yet.
    tap(&mut f, KEY_CAPSLOCK);
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.niri().lock_screen.caps_alpha(now),
        0.,
        "no warning on the clock page"
    );

    // Onto the prompt. The warning is owed the moment it appears, without another caps press —
    // the state is already on, and GNOME reads the keymap rather than waiting for an event.
    tap(&mut f, KEY_A);
    assert_eq!(
        f.niri().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt
    );
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.niri().lock_screen.caps_alpha(now),
        1.,
        "caps is on and the question is secret"
    );

    // Turning it off takes the warning with it.
    tap(&mut f, KEY_CAPSLOCK);
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(f.niri().lock_screen.caps_alpha(now), 0., "caps lock is off");

    // A non-secret question gets no warning even with caps on.
    tap(&mut f, KEY_CAPSLOCK);
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Username:".to_owned(),
            secret: false,
        });
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.niri().lock_screen.caps_alpha(now),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Caps lock goes on while the session is *unlocked*, so the shield never sees the key.
    tap(&mut f, KEY_CAPSLOCK);

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    // Raise the prompt by clicking, not typing.
    f.niri_state()
        .on_shield_click(smithay::utils::Point::from((960., 540.)));
    assert_eq!(
        f.niri().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt
    );
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.niri().lock_screen.caps_alpha(now),
        1.,
        "caps was on before the shield existed, and clicking is not a keystroke"
    );

    // Unlock, turn caps off while unlocked, lock again, and click.
    f.niri_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.niri().screen_shield.is_active());
    tap(&mut f, KEY_CAPSLOCK);

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(2));
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.niri_state()
        .on_shield_click(smithay::utils::Point::from((960., 540.)));
    f.niri().lock_screen.settle_caps();
    let now = get_monotonic_time();
    assert_eq!(
        f.niri().lock_screen.caps_alpha(now),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    f.niri_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });

    tap(&mut f, KEY_LEFTSHIFT);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Clock, "shift waits");
    tap(&mut f, KEY_CAPSLOCK);
    assert_eq!(f.niri().unlock_dialog.page(), Page::Clock, "so does caps");

    // Ctrl is not in GNOME's list: it raises the prompt like any other key.
    tap(&mut f, KEY_LEFTCTRL);
    assert_eq!(
        f.niri().unlock_dialog.page(),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.niri().screen_shield.is_locked());

    f.key_press(KEY_LEFTMETA);
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();

    assert!(
        f.niri().screen_shield.is_active(),
        "a locked shield does not raise on a keypress"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;
    use crate::unlock_dialog::Page;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.niri().screen_shield.is_locked());

    f.key_press(KEY_LEFTCTRL);
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F2);
    f.key_release(KEY_LEFTALT);
    f.key_release(KEY_LEFTCTRL);

    assert!(
        f.niri().screen_shield.is_locked(),
        "still locked, of course"
    );
    assert_eq!(
        f.niri_state().backend.headless().last_vt(),
        Some(2),
        "Ctrl+Alt+F2 must reach the VT switch from behind the curtain"
    );

    // The control: an ordinary key does NOT get through — it is typed at the shield instead.
    // Without this the assertion above would pass for a shield that forwarded everything.
    tap(&mut f, KEY_A);
    assert_eq!(
        f.niri().unlock_dialog.page(),
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
    use crate::dbus::freedesktop_login1::Login1ToNiri;
    use crate::dbus::gdm::VerifierEvent;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_login1_msg(Login1ToNiri::SessionLock(true));
    assert!(f.niri().screen_shield.is_active(), "Lock covers the screen");
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.niri().screen_shield.is_locked());

    // ...and gdm, having authenticated on its own VT, unlocks us.
    f.niri_state()
        .on_login1_msg(Login1ToNiri::SessionLock(false));
    assert!(
        !f.niri().screen_shield.is_locked(),
        "Unlock must actually unlock — otherwise gdm authenticates you into a locked screen"
    );
    assert!(!f.niri().screen_shield.is_active(), "and raise the shield");
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.niri().screen_shield.is_locked());

    f.niri_state().on_verifier_event(VerifierEvent::Lost);
    assert!(!f.niri().screen_shield.is_locked(), "the lock is dropped");
    assert!(
        f.niri().screen_shield.is_active(),
        "the screen stays covered"
    );

    // ...and it can now be dismissed, which is the whole point.
    tap(&mut f, KEY_A);
    assert!(!f.niri().screen_shield.is_active());
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![files, calc])),
        Box::new(recorder.clone()),
    );
    f.niri().app_system.set_favorites(vec![
        "org.example.Files.desktop".to_owned(),
        "org.example.Calc.desktop".to_owned(),
    ]);
    f.niri().sync_dash_favorites();

    // Files runs with two windows; Calc stays stopped.
    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.Files");
    let older = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Files");
    let newer = f.niri().layout.focus().unwrap().id();
    f.niri_complete_animations();

    let click = |f: &mut Fixture, i: usize| {
        f.niri_state().do_action(Action::OpenOverview, false);
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
    let older_window = f.niri().find_window_by_id(older).unwrap();
    f.niri_state().focus_window(&older_window);
    f.niri_state().update_keyboard_focus();
    f.double_roundtrip(client);
    f.niri_complete_animations();
    assert_eq!(f.niri().layout.focus().unwrap().id(), older);
    let _ = newer;

    // RUNNING, no modifier: focus its most recent window, and *do not launch*.
    click(&mut f, 0);
    f.niri_complete_animations();
    assert!(
        recorder.calls.borrow().is_empty(),
        "a running app must not be relaunched — that is what opens the spurious startup sequence"
    );
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
        older,
        "it activates the app's most recently used window"
    );
    assert_eq!(
        f.niri().app_system.app_state("org.example.Files.desktop"),
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
        f.niri().app_system.app_state("org.example.Calc.desktop"),
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
    f.niri().app_system = AppSystem::with_parts(
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
    f.niri_complete_animations();

    let can = |f: &mut Fixture, id: &str| f.niri().app_system.can_open_new_window(id);
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
    f.niri_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one favorite launched");
    assert_eq!(calls[0].0.id, "b.desktop", "the clicked favorite launched");
    assert_eq!(calls[0].1, ResolvedLaunch::Default);
    assert!(
        !f.niri().layout.is_overview_open(),
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
    f.niri_complete_animations();
    assert!(
        recorder.calls.borrow().is_empty(),
        "the press alone must not launch"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "and must not close the overview"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert_eq!(
        recorder.calls.borrow().len(),
        1,
        "the release completes the click"
    );
    assert!(!f.niri().layout.is_overview_open());
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
    f.niri_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "releasing over a different icon launches nothing"
    );
    assert!(
        f.niri().layout.is_overview_open(),
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.niri()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.niri().sync_dash_favorites();
    f.niri().sync_app_grid();

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri().layout.toggle_app_grid();
    assert!(f.niri().layout.is_app_grid_open(), "app grid must open");
    f.settle_animations();

    (f, recorder)
}

/// The favourites, in dash order — what `favorite-apps` would be written as.
fn dash_favorites(f: &mut Fixture) -> Vec<String> {
    f.niri()
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
        f.niri().app_grid.entry_id(0),
        Some("c.desktop"),
        "the grid should hold the one app that is not pinned"
    );

    let grid_area = overview_controls(&mut f).app_display;
    let from = f
        .niri()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    // Drag onto the *first* dash tile: dropping on its left half aims at slot 0.
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x - 20., first.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.niri().dash.drop_slot(),
        Some(0),
        "hovering the front of the dash must open the gap at slot 0"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_eq!(f.niri().dash.drop_slot(), None, "the drop closes the gap");
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
    f.niri().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder));
    f.niri()
        .app_system
        .set_favorites(vec!["a.desktop".to_owned()]);
    f.niri().sync_dash_favorites();

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_state().update_keyboard_focus();
    tap(&mut f, KEY_A);
    f.settle_animations();
    assert_eq!(
        f.niri().overview_search.result_id(0),
        Some("c.desktop"),
        "the search must list the app we are about to drag"
    );

    let area = overview_controls(&mut f).into();
    let from = f
        .niri()
        .overview_search
        .result_center(0, area)
        .expect("result tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x - 20., first.y);
    assert!(
        f.niri().app_drag.is_some(),
        "a search result must be draggable — it was not a drag source at all before"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
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
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.niri().dash.drop_slot(),
        None,
        "no gap opens before or after the dragged favourite itself"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
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
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.niri().dash.drop_slot(),
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
        f.niri().dash.drop_slot(),
        Some(3),
        "past the last favourite clamps to the end of them, not into the running zone"
    );
    assert!(
        !f.niri().app_drag.as_ref().unwrap().unpin,
        "the open gap pushed the show-apps button right, so this is the strip, not it"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

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
    f.niri_complete_animations();
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
        f.niri().panel_popover.is_app_menu(),
        "a right-click must open the menu on the PRESS, not wait for the release \
         (`recognize_on_press: true`, `appDisplay.js:2981-2986`)"
    );
    assert_eq!(
        f.niri().panel_popover.app_menu().unwrap().labels(),
        vec!["New Window", "Unpin"],
    );
    f.pointer_button(BTN_RIGHT, ButtonState::Released);
    assert!(
        f.niri().panel_popover.is_app_menu(),
        "and the release must not take it away again"
    );

    // The menu grabs, so nothing under it hovers — except its own icon, which stays
    // highlighted for as long as the menu is up.
    let second = dash_tile_center(&mut f, 1);
    pointer_motion_to(&mut f, second.x, second.y);
    assert_eq!(
        f.niri().dash.hovered_for_test(),
        Some(DashHit::App(0)),
        "the icon whose menu is open keeps its highlight, and the icon the pointer \
         moved to must NOT take it (the menu holds a grab)"
    );

    // A dash icon's menu opens *upward* — the dash is at the bottom of the screen, so
    // `popupMenuSide: St.Side.BOTTOM` (`dash.js:27`) puts the arrow under the box.
    let output = f.niri().global_space.outputs().next().unwrap().clone();
    let menu = f.niri().panel_popover.content_location(&output);
    let menu_h = f.niri().panel_popover.app_menu().unwrap().logical_size().h;
    let dash_area = overview_controls(&mut f).dash;
    let tile = f.niri().dash.tile_rect(0, dash_area).unwrap();
    assert!(
        menu.y + menu_h <= tile.loc.y,
        "the dash menu must sit entirely above its icon (bottom {}, icon top {})",
        menu.y + menu_h,
        tile.loc.y
    );

    // Dismiss, then the same on an app that is not pinned. The settle matters: a
    // popover stays `is_open` while it fades out, and a press during that window is a
    // dismissal (it lands on the still-grabbing menu), not a new menu.
    f.niri().panel_popover.close();
    f.settle_animations();
    let grid_area = overview_controls(&mut f).app_display;
    let unpinned = f
        .niri()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, unpinned.x, unpinned.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    assert_eq!(
        f.niri().panel_popover.app_menu().unwrap().labels(),
        vec!["New Window", "Pin to Dash"],
        "an app that is not pinned is offered the pin"
    );

    // A grid icon takes `AppIcon`'s default `St.Side.LEFT`, so its menu opens to the
    // icon's right instead (`appDisplay.js:2928`).
    let menu = f.niri().panel_popover.content_location(&output);
    let tile = f.niri().app_grid.entry_rect(0, grid_area).unwrap();
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

    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.niri().app_system.set_favorites(vec!["a.desktop".into()]);
    f.niri().sync_dash_favorites();

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
    assert_eq!(f.niri().app_system.running()[0].n_windows(), 2);

    f.niri_state().do_action(Action::OpenOverview, false);
    f.settle_animations();

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let labels = f.niri().panel_popover.app_menu().unwrap().labels();
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
    let output = f.niri().global_space.outputs().next().unwrap().clone();
    let origin = f.niri().panel_popover.content_location(&output);
    let row = f
        .niri()
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
        f.niri().layout.is_overview_open(),
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

    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.niri().app_system.set_favorites(vec!["a.desktop".into()]);
    f.niri().sync_dash_favorites();

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
        windows.push(f.niri().layout.focus().unwrap().window.clone());
    }
    // The second one is focused; the row must move focus to the first.
    assert_eq!(f.niri().layout.focus().unwrap().window, windows[1]);

    f.niri_state().do_action(Action::OpenOverview, false);
    f.settle_animations();
    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let output = f.niri().global_space.outputs().next().unwrap().clone();
    let origin = f.niri().panel_popover.content_location(&output);
    let row = f
        .niri()
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
        f.niri().layout.focus().unwrap().window,
        windows[0],
        "the row must raise the window it names"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
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

    let labels = f.niri().panel_popover.app_menu().unwrap().labels();
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
    let output = f.niri().global_space.outputs().next().unwrap().clone();

    let first = dash_tile_center(&mut f, 0);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_RIGHT, ButtonState::Pressed);
    f.pointer_button(BTN_RIGHT, ButtonState::Released);

    let origin = f.niri().panel_popover.content_location(&output);
    let row = f
        .niri()
        .panel_popover
        .app_menu()
        .unwrap()
        .row_center("Unpin")
        .expect("the menu has an Unpin row");
    let at = origin + row;
    pointer_motion_to(&mut f, at.x, at.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["b.desktop"],
        "the row must unpin the app"
    );
    assert!(
        !f.niri().panel_popover.is_open(),
        "activating any popup-menu item closes the menu"
    );
    assert!(
        f.niri().layout.is_overview_open(),
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
        f.niri().dash.item_id(0),
        None,
        "the dash must start empty for this to be the empty-dash path"
    );

    let area = overview_controls(&mut f).dash;
    let idle_w = f.niri().dash.pill_box(area).size.w;

    let grid_area = overview_controls(&mut f).app_display;
    let from = f
        .niri()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x, from.y + 40.);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");

    let pill = f.niri().dash.pill_box(area);
    assert_eq!(
        pill.size.w - idle_w,
        32.,
        "the drag must reserve `$dash_placeholder_size` of run for the drop target"
    );

    // Anywhere in that reserved run is slot 0.
    pointer_motion_to(&mut f, pill.loc.x + 15., pill.loc.y + 50.);
    assert_eq!(
        f.niri().dash.drop_slot(),
        Some(0),
        "an empty dash always drops at the start"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["a.desktop"],
        "the drop must pin the first favourite"
    );
    assert_eq!(
        f.niri().dash.pill_box(area).size.w,
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

    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert!(
        f.niri().app_drag.as_ref().unwrap().unpin,
        "the show-apps button must arm as the unpin target"
    );
    assert_eq!(
        f.niri().dash.drop_slot(),
        None,
        "the dash must not offer to pin and to unpin at once (`dash.js:444-445`)"
    );
    assert_eq!(
        f.niri().dash.hovered_for_test(),
        Some(DashHit::ShowApps),
        "the armed button lights up, which is the only feedback that it will remove"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_eq!(
        dash_favorites(&mut f),
        vec!["b.desktop"],
        "the drop must unpin the dragged app"
    );
    assert!(
        (0..3).any(|i| f.niri().app_grid.entry_id(i) == Some("a.desktop")),
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
        .niri()
        .app_grid
        .entry_center(0, grid_area)
        .expect("grid tile 0");
    let show_apps = dash_tile_center(&mut f, 2);
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, show_apps.x, show_apps.y);

    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert!(
        !f.niri().app_drag.as_ref().unwrap().unpin,
        "an app that is not pinned cannot be unpinned"
    );
    assert_eq!(
        f.niri().dash.hovered_for_test(),
        None,
        "so the button must not light up either — a drag grabs the pointer, and only \
         the unpin arming lights anything up (`dash.js:447-450`)"
    );

    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
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
        f.niri().app_drag.is_some(),
        "leaving the press box must start the drag"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "dragging must not launch on the way"
    );

    let ws = f.niri().layout.active_workspace().unwrap().id();
    pointer_motion_to(&mut f, 960., 400.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(f.niri().app_drag.is_none(), "the drop ends the drag");
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
        f.niri()
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")])),
        Box::new(RecordingLauncher::default()),
    );

    // A second workspace, and the target is *not* the active one.
    let _first = map_window_sized(&mut f, id, (800, 600), None);
    let first_win = f.niri().layout.focus().unwrap().window.clone();
    f.niri_state().do_action(Action::FocusWorkspaceDown, false);
    f.niri_complete_animations();
    let target = f.niri().layout.active_workspace().unwrap().id();
    f.niri_state().do_action(Action::FocusWorkspaceUp, false);
    f.niri_complete_animations();
    assert_ne!(
        f.niri().layout.active_workspace().unwrap().id(),
        target,
        "the target workspace must not be the active one"
    );

    f.niri()
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
    f.niri_complete_animations();

    let win = f
        .niri()
        .layout
        .windows()
        .map(|(_, m)| m.window.clone())
        .find(|w| *w != first_win)
        .expect("the second window must be mapped");
    let landed = f
        .niri()
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
        f.niri()
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
    f.niri_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "a drop on the dash itself launches nothing"
    );
    assert!(f.niri().layout.is_overview_open());
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
    f.niri_complete_animations();

    assert_eq!(recorder.calls.borrow().len(), 1, "middle-click launches");
    assert!(!f.niri().layout.is_overview_open());
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
        f.niri().layout.is_overview_open(),
        "right-click on the dash leaves the overview open"
    );
    assert!(
        !f.niri().seat.get_pointer().unwrap().is_grabbed(),
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
    let i = f.niri().dash.show_apps_index();
    let center = dash_tile_center(&mut f, i);
    // `pointer_motion` is relative; move onto the button once (from the origin) and
    // leave the pointer there — keyboard/actions below don't move it, so later
    // clicks land on the same spot without re-moving.
    f.pointer_motion(center.x, center.y);
    let click = |f: &mut Fixture| {
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.niri_complete_animations();
    };

    // Click show-apps → the app grid opens (no launch, overview stays open).
    click(&mut f);
    assert!(
        recorder.calls.borrow().is_empty(),
        "show-apps must not launch an app"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "show-apps keeps the overview open"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
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
    f.niri_state().update_keyboard_focus();
    tap(&mut f, KEY_ESC);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_app_grid_open(),
        "Escape returns to the window picker"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "…without closing the overview"
    );
    assert!(
        overview_controls(&mut f).app_display.loc.y >= 1080.,
        "the app grid must park below the work area again"
    );

    // Reopen the grid, then close the overview from it → the state resets on hide,
    // so reopening the overview starts in the window picker.
    click(&mut f);
    assert!(f.niri().layout.is_app_grid_open());

    f.niri_state().do_action(Action::CloseOverview, false);
    f.niri_complete_animations();
    assert!(!f.niri().layout.is_overview_open());

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_app_grid_open(),
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.niri()
        .app_system
        .set_favorites(favorites.iter().map(|s| s.to_string()).collect());
    f.niri().sync_dash_favorites();
    f.niri().sync_app_grid();

    f.niri_state().do_action(Action::OpenOverview, false);
    assert!(f.niri().layout.is_overview_open(), "overview must open");
    f.niri().layout.toggle_app_grid();
    f.niri_complete_animations();
    assert!(f.niri().layout.is_app_grid_open(), "app grid must open");

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
            .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };
    assert_eq!(
        ids(&mut f),
        vec!["a.desktop", "m.desktop", "z.desktop"],
        "with no saved layout the order is by name"
    );

    // Place two of them, out of name order and on separate pages; leave one unplaced.
    f.niri().gnome_settings.app_picker_layout = HashMap::from([
        ("z.desktop".to_owned(), (0, 7)),
        ("m.desktop".to_owned(), (1, 0)),
    ]);
    f.niri().sync_app_grid();
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
            .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };
    assert_eq!(ids(&mut f), vec!["a.desktop", "m.desktop", "z.desktop"]);

    let area = overview_controls(&mut f).app_display;
    let start = f.niri().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.niri().app_grid.entry_rect(2, area).expect("tile 2");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Aim just inside the leading edge of the third tile — within the 20px divider
    // leeway, so it is an insertion point and not the icon's body.
    pointer_motion_to(&mut f, third.loc.x + 5., third.loc.y + third.size.h / 2.);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
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
    let start = f.niri().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.niri().app_grid.entry_center(2, area).expect("tile 2");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // The centre of the third tile: its body, not the divider a reorder would take.
    pointer_motion_to(&mut f, third.x, third.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.niri().app_grid.entry_id(0),
        Some("m.desktop"),
        "the app that took no part stays where it was"
    );
    let members: Vec<&str> = f
        .niri()
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
    assert_eq!(f.niri().app_grid.entry_name(1), Some("Unnamed Folder"));
    assert_eq!(
        f.niri().app_grid.entry_id(2),
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();
    assert_eq!(f.niri().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.niri().app_grid.entry_id(1), Some("Utilities"));

    let area = overview_controls(&mut f).app_display;
    let app = f.niri().app_grid.entry_center(0, area).expect("tile 0");
    let folder = f.niri().app_grid.entry_center(1, area).expect("tile 1");

    // The folder onto the app: no drop, no reorder, nothing.
    pointer_motion_to(&mut f, folder.x, folder.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, app.x, app.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.niri().app_grid.drop_hover(),
        None,
        "a folder is not something another icon can swallow"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(f.niri().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.niri().app_grid.entry_id(1), Some("Utilities"));

    // The app onto the folder: a join.
    pointer_motion_to(&mut f, app.x, app.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, folder.x, folder.y);
    assert_eq!(
        f.niri().app_grid.drop_hover(),
        Some(1),
        "a folder lights up the moment the drag reaches it — no 500 ms preview"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let members: Vec<&str> = f
        .niri()
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
        f.niri().app_grid.entry_id(1),
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
    assert_eq!(f.niri().app_grid.page_count(area), 2, "30 apps paginate");
    assert_eq!(f.niri().app_grid.entry_id(0), Some("app00.desktop"));

    let start = f.niri().app_grid.entry_center(0, area).expect("tile 0");
    pointer_motion_to(&mut f, start.x, start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // Out to the right band. It only becomes a target once the previews have slid in.
    let right = area.loc.x + area.size.w - 20.;
    pointer_motion_to(&mut f, right, start.y);
    f.settle_animations();
    pointer_motion_to(&mut f, right - 1., start.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "the view must follow the app to its new page"
    );
    assert_eq!(
        f.niri().app_grid.entry_id(29),
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
        .niri()
        .app_grid
        .tile_center(0, area)
        .expect("grid tile 0 in range");

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, "m.desktop", "the clicked grid app launched");
    assert!(
        !f.niri().layout.is_overview_open(),
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
            .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
            .collect()
    };

    f.niri().gnome_settings.app_folders = vec![
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
    f.niri().sync_app_grid();

    assert_eq!(
        ids(&mut f),
        vec!["a.desktop", "Utilities"],
        "the folder's two apps left the top level, the folder took one slot, and the \
         folder that resolved to nothing is not displayed"
    );
    assert_eq!(
        f.niri()
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
        f.niri().app_grid.entry_folder(0).is_none(),
        "an app tile is not a folder"
    );

    // The folder id sorts through the same saved arrangement as a desktop id.
    f.niri().gnome_settings.app_picker_layout = HashMap::from([("Utilities".to_owned(), (0, 0))]);
    f.niri().sync_app_grid();
    assert_eq!(ids(&mut f), vec!["Utilities", "a.desktop"]);

    // Clicking it launches nothing and leaves the overview up — it opens instead.
    let area = overview_controls(&mut f).app_display;
    let center = f
        .niri()
        .app_grid
        .tile_center(0, area)
        .expect("the folder tile is in range");
    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "a folder launches nothing"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
        "clicking a folder must not close the overview"
    );
    assert_eq!(
        f.niri().folder_dialog.folder_id(),
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f.niri().app_grid.tile_center(1, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert_eq!(f.niri().folder_dialog.folder_id(), Some("Utilities"));

    // The edit button opens the entry, with the name selected whole.
    let edit = crate::ui::folder_dialog::layout(view).edit_button;
    let edit_center: Point<f64, smithay::utils::Logical> =
        Point::from((edit.loc.x + edit.size.w / 2., edit.loc.y + edit.size.h / 2.));
    pointer_motion_to(&mut f, edit_center.x, edit_center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(f.niri().folder_dialog.is_renaming());
    assert_eq!(f.niri().folder_dialog.rename_text(), Some("Utilities"));
    // Without this the overview never holds the key focus and the whole ladder is dead.
    f.niri_state().update_keyboard_focus();

    // Typing over the selection replaces the whole name, and the keys never reach the
    // search entry behind.
    tap(&mut f, KEY_T);
    tap(&mut f, KEY_O);
    assert_eq!(f.niri().folder_dialog.rename_text(), Some("to"));
    assert!(
        !f.niri().overview_search.is_active(),
        "the rename entry holds the key focus, so the search never engages"
    );

    // Enter commits: the label follows immediately.
    tap(&mut f, KEY_ENTER);
    assert!(!f.niri().folder_dialog.is_renaming());
    assert_eq!(f.niri().app_grid.entry_name(1), Some("Utilities"));
    assert_eq!(
        f.niri().folder_dialog.folder_name(),
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let open_it = |f: &mut Fixture| {
        let area = overview_controls(f).app_display;
        let center = f.niri().app_grid.tile_center(1, area).expect("folder tile");
        pointer_motion_to(f, center.x, center.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.niri_complete_animations();
    };
    let panel = crate::ui::folder_dialog::layout(view).panel;
    let outside: Point<f64, smithay::utils::Logical> =
        Point::from((panel.loc.x - 40., panel.loc.y + panel.size.h / 2.));

    // First, a drag that ends back inside the panel: nothing moves.
    open_it(&mut f);
    let member = f
        .niri()
        .folder_dialog
        .entry_center(0, view)
        .expect("member tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert_eq!(
        f.niri().app_grid.index_of("m.desktop"),
        Some(2),
        "the placeholder joins the grid for the duration of the drag"
    );
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.niri().app_grid.index_of("m.desktop"),
        None,
        "a drop back inside the folder withdraws the placeholder"
    );
    assert_eq!(f.niri().folder_dialog.member_count(), 2);

    // Then the real thing.
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.niri().folder_dialog.is_open(),
        "the dialog takes the drop and pops down"
    );
    assert_eq!(
        f.niri().app_grid.index_of("m.desktop"),
        Some(2),
        "the app is a top-level tile now, where its placeholder sat"
    );
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(2),
        "and it takes the key focus (`selectApp`)"
    );
    let members: Vec<&str> = f
        .niri()
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
        .niri()
        .folder_dialog
        .entry_center(0, view)
        .expect("the one member left");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, outside.x, outside.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert_eq!(
        f.niri().app_grid.index_of("Utilities"),
        None,
        "an emptied folder is deleted, not shown empty"
    );
    assert_eq!(f.niri().app_grid.index_of("z.desktop"), Some(2));
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
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
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f.niri().app_grid.tile_center(1, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert_eq!(
        f.niri().folder_dialog.member_ids(),
        vec!["m.desktop", "n.desktop", "o.desktop", "z.desktop"]
    );

    let at = |f: &mut Fixture, i: usize| {
        f.niri()
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
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert!(
        f.niri().folder_pending_move.is_some(),
        "the folder arms the same delayed move the grid does"
    );
    // A drop that beats the 200 ms timer still commits the move, as it does in the grid.
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.niri().folder_dialog.is_open(),
        "a drop inside the panel is the folder's, so the dialog stays up"
    );
    assert_eq!(
        f.niri().folder_dialog.member_ids(),
        vec!["n.desktop", "o.desktop", "m.desktop", "z.desktop"],
        "the dragged member took its new place"
    );
    assert_eq!(
        f.niri().app_grid.index_of("m.desktop"),
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
        f.niri().folder_dialog.member_ids(),
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
    let after_first = f.niri().folder_dialog.member_ids();

    // Wait out most of that timer's delay, so its firing lands *inside* the window the
    // second drag is watched over.
    f.dispatch_until(Duration::from_millis(150), |_| false);
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x + pitch * 0.4, third.y);
    assert!(f.niri().folder_pending_move.is_some(), "a move is armed");
    let moved_early = f.dispatch_until(Duration::from_millis(100), |state| {
        state.niri.folder_dialog.member_ids() != after_first
    });
    assert!(
        !moved_early,
        "nothing moves before the delay is out: {:?} became {:?}",
        after_first,
        f.niri().folder_dialog.member_ids()
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
        let output = f.niri_output(1);
        f.niri_state().do_action(Action::OpenOverview, false);
        f.niri_complete_animations();

        let controls = f
            .niri()
            .layout
            .controls_layout_for_output(&output)
            .expect("the output has a monitor");
        let icon = Dash::metrics(controls.dash).icon_px;
        let entry = f.niri().overview_search.entry_pill(controls.into()).size.w;
        let mon = f
            .niri()
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
    // The corner does *not* ramp here any more: since the search entry floats (approved
    // divergence) the picker got its 58px row back, so the preview on this canvas is
    // taller than the 520px the flat 30 is written for. The rule is unchanged — it just
    // bites lower down now.
    // Compared with a tolerance: `radius` is a ratio of the canvas, so it is exact only when
    // the panel strut divides evenly — at the default font it lands on 30.000000000000004.
    assert!(
        (s_radius - radius).abs() < 1e-9,
        "the preview is big enough to keep GNOME's corner: {s_radius} vs {radius}"
    );

    // 900x600, where the preview finally is smaller than that reference.
    let (_, _, t_radius, _) = measure((900, 600));
    assert!(
        t_radius < radius && t_radius >= 8.,
        "the corner follows the preview down but stays a corner: {t_radius}"
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
        f.niri().app_system.is_favorite("a.desktop"),
        "a.desktop starts pinned"
    );
    assert_eq!(
        f.niri().app_grid.index_of("a.desktop"),
        None,
        "…and so is not in the grid"
    );

    let controls = overview_controls(&mut f);
    let (area, dash) = (controls.app_display, controls.dash);
    let from = f.niri().dash.tile_center(0, dash).expect("the dash tile");
    let onto = f.niri().app_grid.entry_rect(1, area).expect("grid tile 1");

    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    // The leading edge of a tile: an insertion point, not the body a fold would take.
    pointer_motion_to(&mut f, onto.loc.x + 5., onto.loc.y + onto.size.h / 2.);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert!(
        f.niri().app_grid.index_of("a.desktop").is_some(),
        "a placeholder joins the grid for the duration of the drag"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.niri().app_system.is_favorite("a.desktop"),
        "the drop unpinned it"
    );
    assert_eq!(
        f.niri().app_grid.index_of("a.desktop"),
        Some(1),
        "and it stays in the slot it was dropped in, not at the name-ordered tail"
    );

    // Folding a favourite into a new folder unpins it too (`AppIcon.acceptDrop` reaches
    // the same `removeFavorite` via `AppDisplay.createFolder`, `appDisplay.js:1699-1751`).
    let from = f.niri().dash.tile_center(0, dash).expect("the dash tile");
    let onto = f
        .niri()
        .app_grid
        .entry_center(0, area)
        .expect("grid tile 0");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, onto.x, onto.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        !f.niri().app_system.is_favorite("b.desktop"),
        "the fold unpinned it"
    );
    // Only the unpin is asserted here: unpinning re-derives the grid from the settings
    // model, and this fixture has no settings writer for the new folder to come back
    // from. The fold itself is pinned by `overview_dropping_a_grid_icon_on_another_…`.

    // A drag that ends nowhere leaves the dash alone and withdraws the placeholder.
    let from = f.niri().dash.tile_center(0, dash).expect("the dash tile");
    let pinned = f.niri().dash.item_id(0).map(str::to_owned).expect("a tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, 4., 4.);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        f.niri().app_system.is_favorite(&pinned),
        "a drop on nothing keeps it pinned: {pinned}"
    );
    assert_eq!(
        f.niri().app_grid.index_of(&pinned),
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
        .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
        .collect();

    let first = f.niri().app_grid.entry_center(0, area).expect("tile 0");
    let third = f.niri().app_grid.entry_center(2, area).expect("tile 2");
    pointer_motion_to(&mut f, first.x, first.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, third.x, third.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");

    tap(&mut f, KEY_ESC);
    assert!(f.niri().app_drag.is_none(), "the drag is cancelled");
    assert!(
        f.niri().layout.is_app_grid_open(),
        "…and the grid it was over stays open — Escape went no further"
    );
    let after: Vec<String> = (0..3)
        .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
        .collect();
    assert_eq!(
        after, before,
        "the order the drag was reflowing is put back"
    );

    // The button is still down; releasing it must not now act as a drop.
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    let after: Vec<String> = (0..3)
        .filter_map(|i| f.niri().app_grid.entry_id(i).map(str::to_owned))
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
    let niri = f.niri();
    let dash = niri.dash.icon_uploads();
    for (name, other) in [
        ("the app grid", niri.app_grid.icon_uploads()),
        ("the search results", niri.overview_search.icon_uploads()),
        ("the drag proxy", niri.app_icon_uploads.clone()),
    ] {
        assert!(
            std::rc::Rc::ptr_eq(&dash, &other),
            "{name} shares the dash's upload map"
        );
    }

    // A folder's view is built when it opens, so it can only inherit the map if the
    // dialog was told about it — the one path that is not wired at construction.
    niri.folder_dialog
        .popup("Utilities", "Utilities", Vec::new());
    let folder = niri
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
        f.niri().app_system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(apps)),
            Box::new(RecordingLauncher::default()),
        );
        f.niri().sync_app_grid();
    }

    let order: Vec<String> = (0..4)
        .filter_map(|i| f.niri().app_grid.entry_name(i).map(str::to_owned))
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: apps.clone(),
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f.niri().app_grid.tile_center(0, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert_eq!(
        f.niri().folder_dialog.page_count(view),
        2,
        "twelve members make two pages of the 3x3 folder grid"
    );

    // Pick the first member up and hold it over the next-page band.
    let member = f
        .niri()
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
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    // The bands slide in over 150 ms and are not a drop target until they are there
    // (`hint_at` reads the peek). `niri_complete_animations` will not do: it flips the
    // clock's complete-instantly flag back off, so the peek reads 0 again the moment it
    // returns — the animation clock has to really move.
    f.settle_animations();
    assert_eq!(
        f.niri().folder_dialog.current_page(),
        0,
        "hovering a band does not switch the page on its own — that takes a beat"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.niri().folder_dialog.is_open(),
        "the folder took the drop, so the dialog stays up"
    );
    assert_eq!(
        f.niri().folder_dialog.current_page(),
        1,
        "and follows the member to the page it was sent to"
    );
    let members = f.niri().folder_dialog.member_ids();
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f.niri().app_grid.tile_center(1, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

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
        .niri()
        .folder_dialog
        .entry_center(0, view)
        .expect("tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, name.x, name.y);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        !f.niri().folder_dialog.is_open(),
        "the dialog takes the drop and pops down"
    );
    assert_eq!(
        f.niri().app_grid.index_of("m.desktop"),
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let center = f.niri().app_grid.tile_center(1, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    // Pick a member up and cross the drag threshold *inside* the panel.
    let from = f
        .niri()
        .folder_dialog
        .entry_center(0, view)
        .expect("tile 0");
    let to = f
        .niri()
        .folder_dialog
        .entry_center(1, view)
        .expect("tile 1");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, to.x, to.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    assert!(
        f.niri().folder_dialog.is_open(),
        "and the dialog is still up"
    );
    assert!(
        f.niri().grid_pending_move.is_none(),
        "the covered grid arms no move"
    );
    assert_eq!(
        f.niri().app_grid.drop_hover(),
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
    f.niri().gnome_settings.app_folders = vec![
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
    f.niri().sync_app_grid();
    assert_eq!(f.niri().app_grid.entry_id(0), Some("a.desktop"));
    assert_eq!(f.niri().app_grid.entry_id(1), Some("Office"));
    assert_eq!(f.niri().app_grid.entry_id(2), Some("Utilities"));

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let area = overview_controls(&mut f).app_display;
    let office = f
        .niri()
        .app_grid
        .entry_center(1, area)
        .expect("Office tile");

    // Open Utilities and pick its first member up.
    let center = f.niri().app_grid.tile_center(2, area).expect("folder tile");
    pointer_motion_to(&mut f, center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    let member = f
        .niri()
        .folder_dialog
        .entry_center(0, view)
        .expect("member tile 0");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);

    // Out of the panel, and hold there until the dialog gives up and pops down.
    pointer_motion_to(&mut f, office.x, office.y);
    assert!(f.niri().app_drag.is_some(), "the drag must have started");
    let popped = f.dispatch_until(Duration::from_millis(2000), |state| {
        !state.niri.folder_dialog.is_open()
    });
    assert!(
        popped,
        "the dialog pops down 500 ms after the drag leaves it"
    );
    // The pointer has not moved since, so nudge it to let the grid resolve the target it
    // has been ignoring.
    pointer_motion_to(&mut f, office.x, office.y);
    assert_eq!(
        f.niri().app_grid.drop_hover(),
        f.niri().app_grid.index_of("Office"),
        "Office is armed as the drop target"
    );
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    let members = |f: &mut Fixture, id: &str| -> Vec<String> {
        let i = f.niri().app_grid.index_of(id).expect("the folder is there");
        f.niri()
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
        f.niri().app_grid.index_of("m.desktop"),
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let view: Rectangle<f64, smithay::utils::Logical> =
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)));
    let open_it = |f: &mut Fixture| {
        let area = overview_controls(f).app_display;
        // The folder sorts after "a.desktop" by name, so it is tile 1.
        let center = f
            .niri()
            .app_grid
            .tile_center(1, area)
            .expect("the folder tile is in range");
        pointer_motion_to(f, center.x, center.y);
        f.pointer_button(BTN_LEFT, ButtonState::Pressed);
        f.pointer_button(BTN_LEFT, ButtonState::Released);
        f.niri_complete_animations();
    };

    open_it(&mut f);
    assert_eq!(f.niri().folder_dialog.folder_id(), Some("Utilities"));
    assert_eq!(
        (0..2)
            .filter_map(|i| f.niri().folder_dialog.entry_id(i).map(str::to_owned))
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
    f.niri_complete_animations();
    assert!(
        !f.niri().folder_dialog.is_open(),
        "a click outside pops down"
    );
    assert!(
        recorder.calls.borrow().is_empty(),
        "the modal swallowed the click; nothing under it launched"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
        "the grid is still there"
    );

    // Escape closes the folder first, leaving the grid open — the innermost tier of the
    // overview's Escape ladder. (The overview has to actually hold keyboard focus for the
    // ladder to be reachable at all.)
    open_it(&mut f);
    f.niri_state().update_keyboard_focus();
    assert!(f.niri().keyboard_focus.is_overview());
    tap(&mut f, KEY_ESC);
    assert!(
        !f.niri().folder_dialog.is_open(),
        "Escape pops the folder down"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
        "…and stops there rather than also leaving the grid"
    );

    // Launching from inside behaves exactly like a top-level tile: activate, then hide.
    open_it(&mut f);
    let grid_area = crate::ui::folder_dialog::layout(view).grid_area;
    let member = f
        .niri()
        .folder_dialog
        .tile_center(1, grid_area)
        .expect("the second member is in range");
    pointer_motion_to(&mut f, member.x, member.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(
        calls[0].0.id, "z.desktop",
        "the app inside the folder launched"
    );
    assert_eq!(calls[0].1, crate::app_system::ResolvedLaunch::Default);
    drop(calls);
    assert!(
        !f.niri().layout.is_overview_open(),
        "and the overview closed"
    );
    assert!(!f.niri().folder_dialog.is_open(), "with the folder");
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();

    let grid_area = overview_controls(&mut f).app_display;
    let members: Vec<crate::app_system::AppIconRef> = f
        .niri()
        .app_grid
        .entry_folder(1)
        .expect("tile 1 is the folder")
        .iter()
        .map(|m| m.icon.clone())
        .collect();
    assert_eq!(members.len(), 2);

    // Pick the folder tile up and move far enough to pass the drag threshold.
    let from = f
        .niri()
        .app_grid
        .entry_center(1, grid_area)
        .expect("the folder tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x + 120., from.y);
    let drag = f
        .niri()
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
        .niri()
        .app_grid
        .entry_center(0, grid_area)
        .expect("the app tile");
    pointer_motion_to(&mut f, from.x, from.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    pointer_motion_to(&mut f, from.x + 120., from.y);
    let drag = f
        .niri()
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
    f.niri().gnome_settings.app_folders = vec![AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec!["m.desktop".to_owned(), "z.desktop".to_owned()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();
    f.niri_state().update_keyboard_focus();
    assert!(f.niri().keyboard_focus.is_overview());

    // Reach the folder tile by keyboard too: it sorts after "a.desktop", so one Right
    // from the first tile lands on it, and Enter opens it rather than launching.
    tap(&mut f, KEY_RIGHT);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.niri().app_grid.focused(), Some(1));
    tap(&mut f, KEY_ENTER);
    f.niri_complete_animations();
    assert_eq!(
        f.niri().folder_dialog.folder_id(),
        Some("Utilities"),
        "Enter on a folder tile opens it"
    );
    assert!(recorder.calls.borrow().is_empty(), "…and launches nothing");

    // The arrows now belong to the dialog: the grid behind keeps the focus it had.
    let before = f.niri().app_grid.focused();
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.niri().folder_dialog.focused(), Some(0));
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.niri().folder_dialog.focused(), Some(1));
    assert_eq!(
        f.niri().app_grid.focused(),
        before,
        "the grid behind the modal did not move"
    );
    // Enter launches the focused member and takes the overview with it.
    tap(&mut f, KEY_ENTER);
    f.niri_complete_animations();
    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, "z.desktop", "the focused member launched");
    drop(calls);
    assert!(!f.niri().layout.is_overview_open());
    assert!(!f.niri().folder_dialog.is_open());
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
    f.niri_state().update_keyboard_focus();
    assert!(f.niri().keyboard_focus.is_overview());
    let area = overview_controls(&mut f).app_display;
    assert_eq!(
        f.niri().app_grid.page_count(area),
        2,
        "30 apps span two pages"
    );
    let center = |f: &mut Fixture, i: usize| {
        f.niri()
            .app_grid
            .entry_center(i, area)
            .expect("the tile is on the visible page")
    };

    // Nothing is lit until a key asks for it, and the first arrow takes the page's first
    // tile whichever way it points — our divergence: GNOME reaches the grid from the
    // search entry through a stage-wide focus chain we do not have.
    assert_eq!(f.niri().app_grid.focused(), None);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.niri().app_grid.focused(), Some(0));

    // Right moves along the row; Down drops a row in the same column.
    tap(&mut f, KEY_RIGHT);
    let right = f.niri().app_grid.focused().expect("Right moved the focus");
    assert!(right > 0);
    assert_eq!(
        center(&mut f, right).y,
        center(&mut f, 0).y,
        "Right stays in the row"
    );
    assert!(center(&mut f, right).x > center(&mut f, 0).x);

    tap(&mut f, KEY_DOWN);
    let down = f.niri().app_grid.focused().expect("Down moved the focus");
    assert_eq!(
        center(&mut f, down).x,
        center(&mut f, right).x,
        "Down stays in the column"
    );
    assert!(center(&mut f, down).y > center(&mut f, right).y);

    // …and back the way we came.
    tap(&mut f, KEY_UP);
    assert_eq!(f.niri().app_grid.focused(), Some(right));
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.niri().app_grid.focused(), Some(0));
    // Nothing lies left of the first column of the first page. The key is still consumed
    // — it must not fall through to the window binds behind the grid.
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.niri().app_grid.focused(), Some(0));
    assert!(f.niri().layout.is_app_grid_open());

    // Right off the end of a row crosses to the next page in the *same row*: the pages
    // sit side by side in one viewport, so that tile really is the nearest one to the
    // right. The view pages over to follow the focus.
    let row_y = center(&mut f, 0).y;
    for _ in 0..12 {
        tap(&mut f, KEY_RIGHT);
        if f.niri().app_grid.current_page() == 1 {
            break;
        }
    }
    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "the page followed the focus across"
    );
    let crossed = f.niri().app_grid.focused().unwrap();
    let per_page = f.niri().app_grid.items_per_page(area);
    assert!(crossed >= per_page, "…onto the second page");
    assert_eq!(center(&mut f, crossed).y, row_y, "…staying in its row");

    // The paging keys move the page and leave the focus where it was.
    tap(&mut f, KEY_PAGEUP);
    assert_eq!(f.niri().app_grid.current_page(), 0);
    assert_eq!(f.niri().app_grid.focused(), Some(crossed));
    tap(&mut f, KEY_END);
    assert_eq!(f.niri().app_grid.current_page(), 1);
    tap(&mut f, KEY_HOME);
    assert_eq!(f.niri().app_grid.current_page(), 0);
    tap(&mut f, KEY_PAGEDOWN);
    assert_eq!(f.niri().app_grid.current_page(), 1);

    // Enter launches the focused tile and closes the overview, exactly as a click does.
    tap(&mut f, KEY_ENTER);
    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one app launched");
    assert_eq!(calls[0].0.id, ids[crossed], "the focused app launched");
    drop(calls);
    assert!(
        !f.niri().layout.is_overview_open(),
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
        f.niri().app_grid.page_count(area),
        2,
        "30 apps span two pages"
    );
    assert_eq!(f.niri().app_grid.current_page(), 0);

    // A wheel notch over the grid pages forward.
    let tile = f.niri().app_grid.tile_center(0, area).unwrap();
    f.pointer_motion(tile.x, tile.y);
    f.scroll_wheel();
    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "a wheel notch pages the grid forward"
    );

    // Clicking the first page-indicator dot returns to page 0.
    let dot0 = f.niri().app_grid.indicator_center(0, area).unwrap();
    f.pointer_motion(dot0.x - tile.x, dot0.y - tile.y); // relative from the tile
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.niri().app_grid.current_page(),
        0,
        "clicking a dot jumps to its page"
    );

    // Clicking the next navigation arrow steps forward one page.
    use crate::ui::app_grid::PageArrow;
    let next = f
        .niri()
        .app_grid
        .arrow_center(PageArrow::Next, area)
        .unwrap();
    f.pointer_motion(next.x - dot0.x, next.y - dot0.y); // relative from the dot
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "clicking the next arrow advances a page"
    );
    // On page 1 the previous arrow exists; clicking it steps back to page 0.
    let prev = f
        .niri()
        .app_grid
        .arrow_center(PageArrow::Prev, area)
        .unwrap();
    f.pointer_motion(prev.x - next.x, prev.y - next.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        f.niri().app_grid.current_page(),
        0,
        "clicking the previous arrow steps back a page"
    );

    // A fresh overview open resets to page 0.
    f.niri().app_grid.set_page(1, area);
    f.niri_state().do_action(Action::CloseOverview, false);
    f.niri_complete_animations();
    f.niri().refresh_overview_search_state(); // falling edge
    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri().refresh_overview_search_state(); // rising edge → reset
    assert_eq!(
        f.niri().app_grid.current_page(),
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
    assert!(!f.niri().app_icon_cache.has_worker());
    f.niri().prewarm_app_icons(); // no panic, nothing to observe

    // Wire the async path to a test channel and prewarm: every fake app icon is
    // `Fallback`, so the requests dedup to one per surface size — the dash's 64px and
    // the grid's 96px.
    let rx = f.niri().app_icon_cache.wire_test_channel();
    assert!(f.niri().app_icon_cache.has_worker());
    f.niri().prewarm_app_icons();

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
    f.niri().prewarm_app_icons();
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

    let rx = f.niri().app_icon_cache.wire_test_channel();
    f.niri().prewarm_app_icons();
    let warmed: Vec<f64> = rx.try_iter().map(|req| req.scale()).collect();
    assert!(!warmed.is_empty(), "the fixture warms at its own scale");
    assert!(warmed.iter().all(|scale| *scale == 1.));

    let output = f.niri_output(1);
    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(2.)),
        None,
    );
    f.niri().output_resized(&output);

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

    f.niri_state().do_action(Action::CloseOverview, false);
    f.niri_complete_animations();
    assert!(!f.niri().layout.is_overview_open());

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
    f.niri_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    let center = dash_tile_center(&mut f, 0);

    // Give the cursor an output (screenshot capture is per-output-under-cursor), clear
    // of the dash so no hover is set yet.
    f.pointer_motion(960., 540.);
    // Raise the screenshot UI over the open overview (it doesn't close the overview).
    f.niri_state().open_screenshot_ui(false, None);
    assert!(f.niri().screenshot_ui.is_open(), "screenshot UI must open");
    assert!(
        f.niri().layout.is_overview_open(),
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
    assert_eq!(f.niri().dash.hovered_for_test(), None);
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
        f.niri().dash.hovered_for_test(),
        Some(DashHit::App(0)),
        "hovering a favorite marks it hovered"
    );

    // Move well clear of the dash (top-left corner): hover clears.
    f.pointer_motion(-center.x + 5., -center.y + 5.);
    assert_eq!(
        f.niri().dash.hovered_for_test(),
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
    f.niri().app_system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder.clone()));

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_state().update_keyboard_focus();
    assert!(
        f.niri().keyboard_focus.is_overview(),
        "the overview must hold keyboard focus so typing engages search"
    );
    (f, recorder)
}

/// Typing a printable engages search and lists the provider's results (in group
/// order); the entry becomes active.
#[test]
fn overview_search_types_query_and_lists_results() {
    let (mut f, _rec) = search_overview(&[&["a.desktop", "b.desktop"]]);

    assert!(!f.niri().overview_search.is_active());
    tap(&mut f, KEY_A);
    assert!(
        f.niri().overview_search.is_active(),
        "a printable key must start a search"
    );
    assert_eq!(f.niri().overview_search.result_id(0), Some("a.desktop"));
    assert_eq!(f.niri().overview_search.result_id(1), Some("b.desktop"));
    assert_eq!(f.niri().overview_search.result_id(2), None);
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_state().update_keyboard_focus();

    tap(&mut f, KEY_A);
    // hidden.desktop filtered → first result is app0; capped at 6.
    assert_eq!(f.niri().overview_search.result_id(0), Some("app0.desktop"));
    assert_eq!(
        f.niri().overview_search.result_id(6),
        None,
        "results are capped at MAX_RESULTS (6)"
    );
    assert_eq!(f.niri().overview_search.result_id(5), Some("app5.desktop"));
}

/// Enter launches the default (first) result and closes the overview, clearing search.
#[test]
fn overview_search_enter_launches_selected_and_closes() {
    let (mut f, recorder) = search_overview(&[&["a.desktop", "b.desktop"]]);

    tap(&mut f, KEY_A);
    tap(&mut f, KEY_ENTER);
    f.niri_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "a.desktop",
        "Enter launches the first result"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
        "launching from search closes the overview"
    );
    assert!(
        !f.niri().overview_search.is_active(),
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
        .niri()
        .overview_search
        .result_center(1, area)
        .expect("result tile 1");
    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    let calls = recorder.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.id, "b.desktop",
        "clicking a tile launches that app"
    );
    assert!(!f.niri().layout.is_overview_open());
}

/// Enter with an active query but zero results is consumed — it must NOT fall through
/// to the hardcoded Return→ToggleOverview bind and close the overview.
#[test]
fn overview_search_enter_with_no_results_keeps_overview_open() {
    let (mut f, recorder) = search_overview(&[]); // no groups → no results

    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());
    assert_eq!(f.niri().overview_search.result_id(0), None);
    tap(&mut f, KEY_ENTER);

    assert!(recorder.calls.borrow().is_empty(), "nothing to launch");
    assert!(
        f.niri().layout.is_overview_open(),
        "Enter with no results must not close the overview"
    );
}

/// Escape while active clears the search (overview stays open); a second Escape (now
/// inactive) falls through to the hardcoded bind and closes the overview.
#[test]
fn overview_search_escape_clears_then_closes() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());

    tap(&mut f, KEY_ESC);
    assert!(!f.niri().overview_search.is_active(), "first Escape clears");
    assert!(
        f.niri().layout.is_overview_open(),
        "clearing the search leaves the overview open"
    );

    tap(&mut f, KEY_ESC);
    f.niri_complete_animations();
    assert!(
        !f.niri().layout.is_overview_open(),
        "a second Escape (inactive) closes the overview via the hardcoded bind"
    );
}

/// Backspacing the query to empty deactivates the search (results cleared).
#[test]
fn overview_search_backspace_empties_deactivates() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());
    tap(&mut f, KEY_BACKSPACE);
    assert!(!f.niri().overview_search.is_active());
    assert_eq!(f.niri().overview_search.result_id(0), None);
}

/// A space as the first key does not engage search (the query tokenizes to empty),
/// mirroring GNOME's `_shouldTriggerSearch`.
#[test]
fn overview_search_space_first_stays_inactive() {
    let (mut f, _rec) = search_overview(&[&["a.desktop"]]);

    tap(&mut f, KEY_SPACE);
    assert!(
        !f.niri().overview_search.is_active(),
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    let catalog = FakeCatalog::new(vec![AppEntry::fake("a.desktop", "a.desktop")]);
    *catalog.search_result.borrow_mut() = vec![vec!["a.desktop".to_string()]];
    f.niri().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));

    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_state().update_keyboard_focus();
    f.settle_animations();

    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    let center = (rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);

    // Not searching: the preview is live and the fade is fully off.
    assert_eq!(f.niri().overview_search_fade(), 0.);
    pointer_motion_to(&mut f, center.0, center.1);
    assert!(
        f.niri().window_under_cursor().is_some(),
        "a preview must be clickable while not searching"
    );

    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());
    assert!(
        f.niri().window_under_cursor().is_none(),
        "a preview under the search results must not activate"
    );

    // The fade eases rather than snapping: armed on one frame, mid-way on the next.
    f.niri().advance_animations();
    {
        let niri = f.niri();
        let now = niri.clock.now_unadjusted();
        niri.clock.set_unadjusted(now + Duration::from_millis(60));
        niri.advance_animations();
    }
    let mid = f.niri().overview_search_fade();
    assert!(mid > 0. && mid < 1., "the search fade must ease, got {mid}");
    f.settle_animations();
    assert_eq!(f.niri().overview_search_fade(), 1.);

    // A click out on the covered picker (the pointer has not moved) is consumed by
    // the results strip.
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();
    assert!(
        f.niri().layout.is_overview_open(),
        "clicking the covered picker must not leave the overview"
    );
    assert!(
        f.niri().overview_search.is_active(),
        "and must not discard the search"
    );

    // Clearing brings the picker back — and its reactivity with it.
    tap(&mut f, KEY_ESC);
    f.settle_animations();
    assert!(!f.niri().overview_search.is_active());
    assert_eq!(f.niri().overview_search_fade(), 0.);
    assert!(
        f.niri().window_under_cursor().is_some(),
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.niri_state().update_keyboard_focus();
    f.settle_animations();

    let (tx, ty) = thumbnail_center(&mut f, 0);
    let active = f.niri().layout.active_workspace().unwrap().id();

    // Sanity: the same click switches workspace when nothing covers the strip —
    // otherwise this test could pass by simply missing the thumbnail.
    pointer_motion_to(&mut f, tx, ty);
    assert!(
        f.niri().thumbnail_workspace_under_cursor().is_some(),
        "the probe must actually be over a thumbnail"
    );

    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());

    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert_eq!(
        f.niri().layout.active_workspace().unwrap().id(),
        active,
        "a thumbnail under the search results must not switch workspace"
    );
    assert!(f.niri().layout.is_overview_open());
}

/// The idle entry pill is drawn too, so it consumes its clicks the same way: a
/// fall-through would land on the workspace behind it and leave the overview.
/// (gnome-shell focuses the entry on that click; we have no click-to-focus, but
/// the click must still not escape.)
#[test]
fn overview_search_idle_entry_body_consumes_clicks() {
    let (mut f, recorder) = search_overview(&[&["a.desktop"]]);
    assert!(!f.niri().overview_search.is_active());

    let area = overview_controls(&mut f).into();
    let pill = f.niri().overview_search.entry_pill(area);
    let center = (pill.loc.x + pill.size.w / 2., pill.loc.y + pill.size.h / 2.);

    f.pointer_motion(center.0, center.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.niri_complete_animations();

    assert!(
        f.niri().layout.is_overview_open(),
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
    assert!(f.niri().overview_search.is_active());

    let area = overview_controls(&mut f).into();
    let pill = f.niri().overview_search.entry_pill(area);
    let center = (pill.loc.x + pill.size.w / 2., pill.loc.y + pill.size.h / 2.);

    f.pointer_motion(center.0, center.1);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.niri().layout.is_overview_open(),
        "a click on the active entry must not fall through and close the overview"
    );
    assert!(
        f.niri().overview_search.is_active(),
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.niri_state().update_keyboard_focus();
    assert!(!f.niri().keyboard_focus.is_overview());

    tap(&mut f, KEY_A);
    assert!(
        !f.niri().overview_search.is_active(),
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
    f.niri().refresh_overview_search_state();
    tap(&mut f, KEY_A);
    assert!(f.niri().overview_search.is_active());

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri().refresh_overview_search_state(); // falling edge → clear
    assert!(
        !f.niri().overview_search.is_active(),
        "the search does not outlive the overview"
    );

    f.niri_complete_animations();
    f.niri().advance_animations();
    assert_eq!(
        f.niri().overview_search_fade(),
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
    assert!(f.niri().overview_search.is_active());

    // Prime the edge detector for the open state, then close and re-open.
    f.niri().refresh_overview_search_state();
    f.niri_state().do_action(Action::CloseOverview, false);
    f.niri_complete_animations();
    f.niri().refresh_overview_search_state(); // falling edge
    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri().refresh_overview_search_state(); // rising edge → clear

    assert!(
        !f.niri().overview_search.is_active(),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(recorder.clone()),
    );
    assert_eq!(
        f.niri().app_system.app_state("a.desktop"),
        AppState::Stopped
    );
    assert!(f.niri().app_system.can_open_new_window("a.desktop"));

    f.niri()
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
        f.niri().app_system.app_state("a.desktop"),
        AppState::Starting,
        "a launch opens a startup sequence"
    );
    assert!(
        f.niri().app_system.shows_running_dot("a.desktop"),
        "a starting app already shows the running dot (`appDisplay.js:3007`)"
    );
    assert!(
        !f.niri().app_system.can_open_new_window("a.desktop"),
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
        f.niri().app_system.app_state("a.desktop"),
        AppState::Running,
        "the mapping window completes the sequence"
    );
    assert_eq!(
        f.niri().app_system.starting_apps().count(),
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

    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    f.niri().app_system.set_favorites(vec!["a.desktop".into()]);
    f.niri().sync_dash_favorites();

    let dot = |f: &mut Fixture| f.niri().dash.item_shows_running_dot(0).unwrap();
    assert!(!dot(&mut f), "a stopped favorite shows no dot");

    f.niri()
        .app_system
        .launch(
            "a.desktop",
            LaunchMode::Activate,
            &LaunchContext::bare(get_monotonic_time()),
        )
        .expect("launch");
    assert!(
        f.niri().sync_running_apps(),
        "a state change must report as a change, or the dash never redisplays"
    );
    f.niri().sync_dash_favorites();
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

    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "a.desktop",
            "A",
            "a",
        )])),
        Box::new(RecordingLauncher::default()),
    );

    let cursor = |f: &mut Fixture| f.niri().cursor_manager.global_override();
    f.niri().sync_running_apps();
    assert_eq!(cursor(&mut f), None, "nothing starting, no override");

    f.niri()
        .app_system
        .launch(
            "a.desktop",
            LaunchMode::Activate,
            &LaunchContext::bare(get_monotonic_time()),
        )
        .expect("launch");
    f.niri().sync_running_apps();
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
    f.niri().sync_running_apps();
    assert_eq!(
        cursor(&mut f),
        None,
        "the window completing the sequence clears it"
    );

    // And a launch that never maps clears it on the timeout, rather than leaving the
    // pointer stuck as a watch forever.
    let now = get_monotonic_time();
    f.niri()
        .app_system
        .begin_startup("a.desktop", None, None, now);
    f.niri().sync_running_apps();
    assert_eq!(cursor(&mut f), Some(CursorIcon::Wait));
    f.niri()
        .app_system
        .expire_startups(now + STARTUP_TIMEOUT + Duration::from_millis(1));
    f.niri().sync_running_apps();
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")])),
        Box::new(RecordingLauncher::default()),
    );

    let now = get_monotonic_time();
    f.niri()
        .app_system
        .begin_startup("a.desktop", None, None, now);
    assert_eq!(
        f.niri().app_system.app_state("a.desktop"),
        AppState::Starting
    );

    assert!(
        !f.niri()
            .app_system
            .expire_startups(now + STARTUP_TIMEOUT - Duration::from_millis(1)),
        "the sequence outlives everything up to the timeout"
    );
    assert!(f
        .niri()
        .app_system
        .expire_startups(now + STARTUP_TIMEOUT + Duration::from_millis(1)));
    assert_eq!(
        f.niri().app_system.app_state("a.desktop"),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake_with_wm_class(
            "org.example.Editor.desktop",
            "Editor",
            "editor-instance",
        )])),
        Box::new(RecordingLauncher::default()),
    );
    assert!(
        f.niri().app_system.running().is_empty(),
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
        .niri()
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
    assert_eq!(f.niri().app_system.running()[0].n_windows(), 1);
    assert!(f.niri().app_system.is_running("org.example.Editor.desktop"));

    // Unmap: the app stops running.
    let window = f.client(id).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);

    assert!(
        f.niri().app_system.running().is_empty(),
        "unmapping the last window stops the app"
    );
    assert!(!f.niri().app_system.is_running("org.example.Editor.desktop"));
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
    f.niri().app_system =
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(recorder.clone()));
    f.niri()
        .app_system
        .set_favorites(vec!["fav.desktop".to_owned()]);
    f.niri().sync_dash_favorites();
    f.niri_state().do_action(Action::OpenOverview, false);

    let area = overview_controls(&mut f).dash;
    assert!(
        f.niri().dash.separator_box(area).is_none(),
        "one favorite and nothing running draws no divider"
    );
    assert_eq!(
        f.niri().dash.item_id(1),
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
        f.niri().dash.item_id(1),
        Some("runner.desktop"),
        "the running non-favorite joins the dash after the favorites"
    );
    let area = overview_controls(&mut f).dash;
    let sep = f
        .niri()
        .dash
        .separator_box(area)
        .expect("a favorite plus a running non-favorite draws the divider");

    // The divider sits between the two tiles and is itself inert.
    let fav = f.niri().dash.tile_center(0, area).unwrap();
    let run = f.niri().dash.tile_center(1, area).unwrap();
    assert!(sep.loc.x > fav.x && sep.loc.x < run.x);

    // Clicking the running app's tile is a live target — but it *activates* rather than
    // launching, since the app is RUNNING (`shell_app_activate_full`, `shell-app.c:528-530`).
    // This assertion used to expect a launch, which was the bug behind the busy cursor on every
    // dash click of an already-open app.
    pointer_motion_to(&mut f, run.x, run.y);
    assert_eq!(f.niri().dash.hovered_for_test(), Some(DashHit::App(1)));
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    assert!(
        recorder.calls.borrow().is_empty(),
        "a running app is activated, not relaunched"
    );
    assert!(
        !f.niri().layout.is_overview_open(),
        "and the click still took, closing the overview"
    );

    // Closing the window drops it back out of the dash, divider and all.
    let window = f.client(client).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(client);

    assert_eq!(f.niri().dash.item_id(1), None);
    let area = overview_controls(&mut f).dash;
    assert!(f.niri().dash.separator_box(area).is_none());
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
            "gnome-shell-rs-test-monitors-{}-{:?}.xml",
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
    f.niri_output(1).current_scale().fractional_scale()
}

/// The `ApplyMonitorsConfig` config for headless-1 at `scale`, the way the DBus handler builds it.
fn dbus_scale_config(scale: f64) -> HashMap<String, Option<niri_config::Output>> {
    HashMap::from([(
        "headless-1".to_owned(),
        Some(niri_config::Output {
            off: false,
            name: "headless-1".to_owned(),
            scale: Some(niri_config::FloatOrInt(scale)),
            position: Some(niri_config::Position { x: 0, y: 0 }),
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
    f.niri_state().apply_display_config(dbus_scale_config(2.0));
    assert_eq!(
        output_scale(&f),
        2.0,
        "the applied scale takes effect on the FIRST apply"
    );

    // Any later reload keeps the live-applied value; the store never overrides it.
    f.niri_state().reload_output_config();
    assert_eq!(
        output_scale(&f),
        2.0,
        "a reload must not resurrect the stored scale"
    );

    // Second apply after the first one persisted: what applies is THIS value, not the file's.
    store.write(&monitors_xml_with_scale(2.0));
    f.niri_state().apply_display_config(dbus_scale_config(1.5));
    assert_eq!(
        output_scale(&f),
        1.5,
        "the second apply must not land the first apply's value"
    );
}

/// `niri msg output set-scale` also outranks the store; `set-scale automatic` falls back to it.
#[test]
fn ipc_scale_beats_store_and_automatic_returns_to_it() {
    let _store = MonitorsXmlGuard::install(&monitors_xml_with_scale(2.0));

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    assert_eq!(output_scale(&f), 2.0);

    f.niri_state().apply_transient_output_config(
        "headless-1",
        niri_ipc::OutputAction::Scale {
            scale: niri_ipc::ScaleToSet::Specific(1.5),
        },
    );
    assert_eq!(
        output_scale(&f),
        1.5,
        "a live IPC apply beats the saved store scale"
    );

    f.niri_state().apply_transient_output_config(
        "headless-1",
        niri_ipc::OutputAction::Scale {
            scale: niri_ipc::ScaleToSet::Automatic,
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
    f.niri_state().do_action(Action::FocusWorkspaceDown, false);
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

    f.niri().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "picker -> app grid (middle active)");

    f.settle_animations();
    let grid = workspace_geo(&mut f);

    // ...and back down the same leg, which is the direction that used to swing
    // left before settling right.
    f.niri().layout.toggle_app_grid();
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "app grid -> picker (middle active)");
    f.settle_animations();
    let picker = workspace_geo(&mut f);
    assert!(
        picker[0].size.w > grid[0].size.w,
        "the picker row must be the larger of the two"
    );

    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    f.niri_state().do_action(Action::CloseOverview, false);
    let samples = f.sample_workspace_geo(1, Duration::from_millis(600), 32);
    assert_row_travels_monotonically(&samples, "app grid -> desktop (middle active)");
    assert_geo_eq(
        samples.last().unwrap(),
        &desktop,
        "the close must land back on the desktop layout it started from",
    );
}

/// The app-grid leg happens at a fully-open overview, so the workspace zoom must
/// be parked there for its whole length: closing from the grid re-fits the row
/// first and zooms up after, rather than doing both at once.
///
/// That ordering is not cosmetic. A fit-all row at a near-desktop zoom is
/// degenerate — the workspaces overflow the view, so the run pins to the left gap
/// instead of centering — and blending toward it is what threw the row sideways.
/// gnome-shell cannot reach that state: its single 0..2 adjustment passes through
/// `WINDOW_PICKER`, which unwinds the fit *before* the zoom starts
/// (`getStateTransitionParams`, `overviewControls.js:278-308`).
#[test]
fn overview_close_from_the_app_grid_refits_before_it_zooms() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _w = map_window_sized(&mut f, id, (800, 600), None);

    tap(&mut f, KEY_LEFTMETA);
    f.settle_animations();
    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    let grid = workspace_geo(&mut f);

    f.niri_state().do_action(Action::CloseOverview, false);
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

    // And the fit is fully unwound before the zoom finishes: by the time the
    // active workspace is back to full width, the row is the desktop row.
    let full = samples
        .iter()
        .position(|row| row[0].size.w >= 1919.)
        .expect("the close must reach full width");
    let pitch = samples[full][1].loc.x - samples[full][0].loc.x;
    assert!(
        pitch >= 1919.,
        "at full width the row must already be unfitted, got pitch {pitch} \
         (grid pitch was {})",
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

    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("b.desktop", "B"),
        ])),
        Box::new(RecordingLauncher::default()),
    );
    assert!(
        f.niri().app_grid.entry_id(0).is_none(),
        "the grid must start empty, or a reload proves nothing"
    );

    for _ in 0..8 {
        f.niri().queue_app_catalog_reload();
    }
    f.dispatch();

    assert!(
        f.niri().app_grid.entry_id(0).is_none(),
        "a ping reloaded the catalog inline instead of coalescing"
    );
    let first = f
        .niri()
        .app_catalog_reload_at
        .expect("the burst queued no reload at all — the change would be lost");

    // A later ping pushes the deadline out rather than arming a second timer: the reload
    // lands once the writes stop, not once the first one starts.
    f.niri().queue_app_catalog_reload();
    let second = f.niri().app_catalog_reload_at.expect("still pending");
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
        f.niri().app_system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(vec![AppEntry::fake("a.desktop", "a")])),
            Box::new(RecordingLauncher::default()),
        );
        f.niri().sync_app_grid();
        f.niri_state().do_action(Action::OpenOverview, false);
        f.niri().layout.toggle_app_grid();
        f.niri_complete_animations();

        let area = overview_controls(&mut f).app_display;
        let rendered = f.niri().app_grid.metrics_for(area).icon_px;
        let default = crate::ui::widget::TileMetrics::overview().icon_px;
        assert_eq!(
            rendered == default,
            expect_default,
            "{mode:?} should{} render at the default {default}, got {rendered}",
            if expect_default { "" } else { " NOT" }
        );

        let variants = f.niri().prewarm_variants();
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
    assert_eq!(f.niri().app_grid.page_count(area), 2);

    // Park the pointer over the grid — the swipe is only live there.
    let center = f
        .niri()
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
    let dragged = f.niri().app_grid.page_pos();
    assert!(
        (dragged - 0.4).abs() < 0.01,
        "the pages follow the finger 1:1 (160 px of 400), got {dragged}"
    );
    assert_eq!(
        f.niri().app_grid.current_page(),
        0,
        "…without committing to a page yet"
    );

    // Released slowly, it falls back to the page it is nearest.
    lift(&mut f);
    assert_eq!(f.niri().app_grid.current_page(), 0, "two fifths snaps back");
    assert_eq!(f.niri().app_grid.page_pos(), 0.);

    // Dragged past halfway just as slowly, it falls forward instead.
    swipe(&mut f, 13, 2., 50);
    lift(&mut f);
    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "past halfway, the nearest page is the next one"
    );

    // A flick: a short drag, but fast enough to clear the velocity threshold, so it
    // carries a whole page even though the drag itself covered a fifth of one.
    swipe(&mut f, 4, -2., 1);
    lift(&mut f);
    assert_eq!(
        f.niri().app_grid.current_page(),
        0,
        "a flick advances a page the drag never reached"
    );
    assert_eq!(f.niri().app_grid.page_pos(), 0.);

    // A vertical two-finger scroll is swallowed and moves nothing: GNOME's tracker is
    // horizontal, and letting it through would page the workspaces behind the grid.
    swipe(&mut f, 4, 0., 20);
    for _ in 0..4 {
        f.advance_input_time(20);
        f.scroll_finger(0., 4.);
    }
    assert_eq!(f.niri().app_grid.page_pos(), 0.);
    assert_eq!(f.niri().app_grid.current_page(), 0);
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
    assert_eq!(f.niri().app_grid.page_count(area), 2);

    // Grab the band well below the tiles — the background, not an icon: a press on an
    // icon belongs to that icon's own drag.
    // Near the right edge, so a long leftward drag does not run the pointer into the
    // side of the screen (which silently shortens the travel).
    let start_x = area.loc.x + area.size.w - 20.;
    let start_y = area.loc.y + area.size.h - 6.;
    assert!(
        f.niri()
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
    let dragged = f.niri().app_grid.page_pos();
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
    assert_eq!(f.niri().app_grid.current_page(), 1);
    assert_eq!(f.niri().app_grid.page_pos(), 1.);
    assert!(
        recorder.calls.borrow().is_empty(),
        "a drag on the background launches nothing"
    );
    assert!(
        f.niri().layout.is_app_grid_open(),
        "…and does not dismiss the grid"
    );

    // A press that never moves is just a click on the background: nothing happens.
    pointer_motion_to(&mut f, start_x, start_y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert_eq!(
        f.niri().app_grid.current_page(),
        1,
        "a click on the background does not page"
    );
    assert!(f.niri().layout.is_app_grid_open());
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
    f.niri_state().update_keyboard_focus();
    assert!(f.niri().keyboard_focus.is_overview());
    let area = overview_controls(&mut f).app_display;
    let per_page = f.niri().app_grid.items_per_page(area);
    assert_eq!(f.niri().app_grid.page_count(area), 2);

    // Nothing focused: Tab enters at the very first icon.
    assert_eq!(f.niri().app_grid.focused(), None);
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(0),
        "Tab enters at the start"
    );

    // …then steps one at a time, in catalog order — not spatially.
    tap(&mut f, KEY_TAB);
    assert_eq!(f.niri().app_grid.focused(), Some(1));

    // Entering is from the *start of the grid*, not of the page you happen to be looking
    // at: `navigate_focus(null, TAB_FORWARD)` walks the focus chain from its beginning,
    // and the page then follows the focus back.
    f.niri().app_grid.set_focused(None);
    assert!(f.niri().app_grid.set_page(1, area));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(0),
        "Tab enters at the grid's first icon even from another page"
    );
    assert_eq!(f.niri().app_grid.current_page(), 0, "…paging back to it");

    // Entering *backwards* takes the other end — `TAB_BACKWARD` reverses the focus chain
    // before taking its first entry (`st-widget.c:2089-2090`).
    f.niri().app_grid.set_focused(None);
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(29),
        "Shift+Tab enters at the grid's last icon"
    );

    // Back to the front for the wrap checks below.
    f.niri().app_grid.set_focused(Some(0));
    assert!(f.niri().app_grid.set_page(0, area));

    // Shift+Tab steps back, and off the front it wraps to the very last icon — which
    // pages the view with it. (Focus is on the first icon after the entry above.)
    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(29),
        "Shift+Tab off the front wraps to the last icon"
    );
    assert_eq!(
        f.niri().app_grid.current_page(),
        29 / per_page,
        "…and the page follows the focus there"
    );

    // Forward off the end wraps back to the start, paging back with it.
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(0),
        "and forward wraps too"
    );
    assert_eq!(f.niri().app_grid.current_page(), 0);

    // An open folder is its own focus group (`appDisplay.js:2516`), so Tab cycles inside
    // it and the grid behind keeps whatever focus it had.
    f.niri().app_grid.set_focused(Some(0));
    f.niri().gnome_settings.app_folders = vec![crate::gnome::AppFolder {
        id: "Utilities".to_owned(),
        name: "Utilities".to_owned(),
        apps: vec![ids[1].clone(), ids[2].clone()],
        ..Default::default()
    }];
    f.niri().sync_app_grid();
    let folder = f
        .niri()
        .app_grid
        .index_of("Utilities")
        .expect("the folder tile");
    f.niri().app_grid.set_focused(Some(folder));
    tap(&mut f, KEY_ENTER);
    f.niri_complete_animations();
    assert!(f.niri().folder_dialog.is_open());

    tap(&mut f, KEY_TAB);
    assert_eq!(f.niri().folder_dialog.focused(), Some(0));
    tap(&mut f, KEY_TAB);
    assert_eq!(f.niri().folder_dialog.focused(), Some(1));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().folder_dialog.focused(),
        Some(0),
        "Tab wraps inside the folder — it does not escape into the grid"
    );
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(folder),
        "…and the grid behind the modal kept its own focus"
    );
    tap(&mut f, KEY_ESC);
    assert!(!f.niri().folder_dialog.is_open());

    // Tab is a genuinely *different* traversal from the arrows, and the end of a row is
    // where they part: Tab takes the next icon in order (the row below), where Right
    // leaves for the same row of the next page.
    let row0_y = f.niri().app_grid.entry_center(0, area).unwrap().y;
    let cols = (1..per_page)
        .find(|&i| f.niri().app_grid.entry_center(i, area).unwrap().y != row0_y)
        .expect("the page has more than one row");
    let row_end = cols - 1;

    f.niri().app_grid.set_focused(Some(row_end));
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().app_grid.focused(),
        Some(row_end + 1),
        "Tab wraps onto the next row"
    );

    f.niri().app_grid.set_focused(Some(row_end));
    f.niri().app_grid.set_page(0, area);
    tap(&mut f, KEY_RIGHT);
    assert_eq!(
        f.niri().app_grid.focused(),
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
    use niri_config::OutputName;
    use smithay::output::{Output, PhysicalProperties, Subpixel};

    use crate::niri::AppliedDisplayConfig;

    let mut f = Fixture::new();

    // A 16" panel, so the mobile DPI target applies (utils/scale.rs, mutter's meta-monitor.c).
    let output = Output::new(
        "Virtual-1".to_owned(),
        PhysicalProperties {
            size: (344, 215).into(),
            subpixel: Subpixel::Unknown,
            make: "niri".to_owned(),
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
    let (hidpi, _) = f.niri().derive_output_scale_transform(&output, None);

    set_mode((3840, 2160));
    let (uhd, _) = f.niri().derive_output_scale_transform(&output, None);

    assert!(
        uhd > hidpi,
        "the same panel at a denser mode wants a bigger scale ({hidpi} -> {uhd})"
    );

    // The live-applied config (GNOME Settings' ApplyMonitorsConfig) still outranks the guess —
    // it is dropped on a *hardware* mode change, not consulted-and-ignored.
    f.niri().applied_display_config.insert(
        "Virtual-1".to_owned(),
        AppliedDisplayConfig {
            scale: Some(1.),
            transform: None,
        },
    );
    let (applied, _) = f.niri().derive_output_scale_transform(&output, None);
    assert_eq!(applied, 1.);

    f.niri().applied_display_config.remove("Virtual-1");
    set_mode((2048, 1330));
    let (back, _) = f.niri().derive_output_scale_transform(&output, None);
    assert_eq!(back, hidpi, "dropping the override re-derives for the mode");
}

/// With the app grid up, the shrunken workspaces are scenery, not a picker.
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
    let win = f.niri().layout.focus().unwrap().window.clone();

    f.niri_state().do_action(Action::ToggleOverview, false);
    f.niri_state().update_keyboard_focus();
    f.settle_animations();

    // In the picker, hovering a preview is live: it hovers, and a click would activate it.
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    let center = rect.loc + rect.size.downscale(2.).to_point();
    pointer_motion_to(&mut f, center.x, center.y);
    assert!(
        f.niri().window_under_cursor().is_some(),
        "the picker must be live before the app grid opens"
    );
    // The overlay fades in, so it is only on screen once its animation has run — and settling has
    // to come after the last input roundtrip (the headless animation-clock trap).
    f.settle_animations();
    let hovered = |f: &mut Fixture| {
        let out = f.niri_output(1);
        f.niri()
            .layout
            .monitor_for_output(&out)
            .unwrap()
            .preview_overlays()
            .into_iter()
            .filter(|(_, _, hover)| *hover > 0.)
            .count()
    };
    assert_eq!(hovered(&mut f), 1, "the hovered preview shows its overlay");

    f.niri().layout.toggle_app_grid();
    f.settle_animations();

    // Sample where the preview *now* is: the row shrank, so the old point would miss it and the
    // test would pass without proving anything. The hover and the click both resolve through
    // `Layout::window_under` — not through `Niri::window_under`, which would answer None here
    // anyway because the app grid covers the layout. This is the path that was still handing a
    // window over.
    let small = f.niri().layout.expose_drawn_rect(&win).unwrap();
    assert!(
        small.size.w < rect.size.w,
        "premise: the app grid shrinks the workspaces ({:?} -> {:?})",
        rect.size,
        small.size
    );
    let small_center = small.loc + small.size.downscale(2.).to_point();
    let out = f.niri_output(1);
    assert!(
        f.niri().layout.window_under(&out, small_center).is_none(),
        "a shrunken workspace must not hand a window to the pointer"
    );
    assert_eq!(
        hovered(&mut f),
        0,
        "the overlay must go with the state, even though the pointer never moved"
    );

    // …and it comes back when the app grid closes.
    f.niri().layout.toggle_app_grid();
    f.settle_animations();
    pointer_motion_to(&mut f, center.x, center.y);
    let out = f.niri_output(1);
    assert!(
        f.niri().layout.window_under(&out, center).is_some(),
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
    let mut a11y = f.niri().gnome_settings.a11y;
    a11y.always_show = true;
    f.niri().gnome_settings.a11y = a11y;
    f.niri().panel.set_a11y(a11y);

    let anchor = f.niri().panel.a11y_rect(ow).expect("indicator present");
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
        f.niri().panel_popover.is_open(),
        "clicking the a11y indicator opens its menu"
    );
    assert_eq!(f.niri().panel_popover.open_role(), Some(ROLE_A11Y));

    // The first row is High Contrast (`accessibility.js:45-46`). Its center comes from the
    // menu itself rather than a copy of its metrics, so changing the padding can't
    // silently retarget this click at a different row.
    let out = f.niri().global_space.outputs().next().unwrap().clone();
    let origin = f.niri().panel_popover.content_location(&out);
    let row0 = f.niri().panel_popover.a11y_row_center(0).unwrap();
    click(&mut f, origin.x + row0.x, origin.y + row0.y);

    assert!(
        f.niri().gnome_settings.a11y.get(A11yToggle::HighContrast),
        "the row must flip the backing a11y state"
    );
    // Before the fade finishes, the clicked switch must already show its NEW state:
    // GNOME's rows are `settings.bind`-ed, so the switch travels as the menu closes
    // rather than fading out still showing the old position. The gsettings echo cannot
    // do this — it arrives after the close has begun.
    assert_eq!(
        f.niri().panel_popover.a11y_row_state(0),
        Some(true),
        "the clicked switch must flip before the menu finishes closing"
    );

    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "a switch row closes the menu (popupMenu.js:539-550)"
    );

    // And the indicator's own predicate now holds without the pin.
    let mut a11y = f.niri().gnome_settings.a11y;
    a11y.always_show = false;
    f.niri().gnome_settings.a11y = a11y;
    f.niri().panel.set_a11y(a11y);
    assert!(
        f.niri().panel.a11y_rect(ow).is_some(),
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
    let out = f.niri_output(1);

    // No icon -> nothing at all.
    f.niri()
        .osd
        .show_one(&out, &[], Some("Volume"), OsdLevel::new(0.5, 1.));
    assert!(f.niri().osd.content(&out).is_none());

    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    assert!(f.niri().osd.content(&out).is_some());
    // The 100 ms fade is running, so it is not yet opaque.
    assert!(f.niri().osd.alpha(&out) < 1.);
    assert!(f.niri().osd.are_animations_ongoing());

    tick(&mut f, 120);
    assert_eq!(f.niri().osd.alpha(&out), 1.);
    // The deadline is armed at the Showing->Shown transition inside
    // advance_animations; the wake-up must be armed from the same place.
    assert!(f.niri().osd_timer.is_some());

    // Still up just before the timeout — which started at `show()`, concurrently
    // with the fade (`osdWindow.js:107-110`) — and gone after it plus the fade out.
    tick(&mut f, 1300);
    assert!(f.niri().osd.content(&out).is_some());
    tick(&mut f, 200);
    tick(&mut f, 200);
    assert!(f.niri().osd.content(&out).is_none());
}

/// A second OSD while one is up replaces its content in place and re-arms the
/// timeout, with **no re-fade** — the fade only runs on the hidden->visible edge
/// (`js/ui/osdWindow.js:94-111`).
#[test]
fn osd_replace_in_place_never_refades() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.niri_output(1);

    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.3, 1.));
    tick(&mut f, 120);
    assert_eq!(f.niri().osd.alpha(&out), 1.);

    // Just short of expiry, a new level arrives.
    tick(&mut f, 1300);
    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.6, 1.));
    assert_eq!(
        f.niri().osd.alpha(&out),
        1.,
        "replacing content must not restart the fade"
    );

    // The re-arm happens inside `show()`, between frames. Nothing else can notice
    // it, so the wake-up has to be re-armed against what the timer is actually set
    // to — otherwise the old timer fires early, drops itself, and the OSD hangs on a
    // damage-free desktop until unrelated damage happens by.
    let armed = f.niri().osd_timer_at;
    tick(&mut f, 0);
    let (now_armed, deadline) = {
        let niri = f.niri();
        (niri.osd_timer_at, niri.osd.next_wakeup())
    };
    assert_eq!(
        now_armed, deadline,
        "the wake-up must follow a deadline re-armed by show()"
    );
    assert_ne!(armed, now_armed, "and it moved");

    // The re-arm bought another full 1500 ms from *now*.
    tick(&mut f, 1300);
    assert!(f.niri().osd.content(&out).is_some());
    tick(&mut f, 200);
    tick(&mut f, 200);
    assert!(f.niri().osd.content(&out).is_none());
}

/// The level *eases* when the OSD is already visible and *snaps* when it is not
/// (`js/ui/osdWindow.js:71-84`) — which is what makes a held volume key look like a
/// bar sliding rather than teleporting.
#[test]
fn osd_level_eases_only_when_already_visible() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.niri_output(1);

    // First show: no ease, the bar is already at its value.
    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.2, 1.));
    assert_eq!(f.niri().osd.displayed_level(&out), Some(0.2));
    tick(&mut f, 120);

    // A step up while visible eases across 100 ms. Sampled *mid-flight*: read at zero
    // elapsed time it would still be exactly 0.2, which an implementation that just
    // applies the new value one frame later would also pass.
    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.8, 1.));
    tick(&mut f, 50);
    let mid = f.niri().osd.displayed_level(&out).unwrap();
    assert!(
        mid > 0.2 && mid < 0.8,
        "the bar should be strictly in flight at 50 ms, was at {mid}"
    );
    tick(&mut f, 120);
    assert_eq!(f.niri().osd.displayed_level(&out), Some(0.8));
    assert!(!f.niri().osd.are_animations_ongoing());

    // Up then straight back down inside one frame: the new target equals the value
    // the (stale) ease started from, so nothing new is armed — and if the old ease is
    // left running the bar climbs to 0.8 and then snaps back to 0.2.
    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.3, 1.));
    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.8, 1.));
    tick(&mut f, 50);
    assert_eq!(
        f.niri().osd.displayed_level(&out),
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
    let out = f.niri_output(1);

    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.4, 1.));
    tick(&mut f, 40);
    let mid = f.niri().osd.alpha(&out);
    assert!(mid > 0. && mid < 1., "still fading in, alpha was {mid}");

    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    let after = f.niri().osd.alpha(&out);
    assert_eq!(after, mid, "a show mid-fade must not snap the pill opaque");
    // ...and the fade still lands.
    tick(&mut f, 80);
    assert_eq!(f.niri().osd.alpha(&out), 1.);
}

/// The alt-tab switcher becoming visible hides every OSD
/// (`js/ui/switcherPopup.js:170-178`) — driven through the real keybind, not by
/// calling `hide_all` by hand.
#[test]
fn osd_hidden_by_the_window_switcher() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.niri_output(1);
    let id = f.add_client();
    let _first = map_focused_window(&mut f, id);
    let _second = map_focused_window(&mut f, id);

    f.niri()
        .osd
        .show_one(&out, VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.niri().osd.is_visible());

    // The real keybind, not a hand-call to `hide_all` — the wiring is the thing
    // under test.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.niri().switcher.is_open(), "Alt+Tab opened it");

    // The OSD goes when the popup *appears*, not when the key is pressed: `_showImmediately`
    // calls `osdWindowManager.hideAll()` (`switcherPopup.js:178`), and that is 150 ms after the
    // press. So the first tick reveals the popup and starts the OSD's fade, and the second lets
    // the fade finish. (niri's MRU hid it at keypress, which is why this used to need one tick.)
    tick(&mut f, 200);
    assert!(
        f.niri().switcher.is_visible(),
        "the popup is past its delay"
    );
    tick(&mut f, 200);
    assert!(
        !f.niri().osd.is_visible(),
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
    let a = f.niri_output(1);
    let b = f.niri_output(2);

    f.niri()
        .osd
        .show_all(VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.niri().osd.content(&a).is_some());
    assert!(f.niri().osd.content(&b).is_some());

    // Now show on `a` alone: `b` is cancelled, not left behind.
    f.niri()
        .osd
        .show_one(&a, VOL_ICON, None, OsdLevel::new(0.9, 1.));
    tick(&mut f, 200);
    assert!(f.niri().osd.content(&a).is_some());
    assert!(
        f.niri().osd.content(&b).is_none(),
        "an output missing from the level map must be cancelled"
    );

    // hideAll takes the rest (`switcherPopup.js:178`).
    f.niri().osd.hide_all();
    tick(&mut f, 200);
    assert!(!f.niri().osd.is_visible());
}

/// An output that goes away takes its OSD with it (`js/ui/osdWindow.js:157-160`);
/// one that appears gets its own. Nothing migrates.
#[test]
fn osd_follows_outputs() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let a = f.niri_output(1);
    f.niri()
        .osd
        .show_all(VOL_ICON, None, OsdLevel::new(0.5, 1.));
    tick(&mut f, 120);
    assert!(f.niri().osd.is_visible());

    f.add_output(2, (1280, 720));
    let b = f.niri_output(2);
    assert!(
        f.niri().osd.content(&b).is_none(),
        "a new output starts with a hidden OSD, it does not inherit one"
    );

    f.remove_output(1);
    assert!(f.niri().osd.content(&a).is_none());
    assert!(!f.niri().osd.is_visible());
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
#[cfg(feature = "dbus")]
#[test]
fn osd_show_osd_routes_by_connector() {
    use crate::dbus::gnome_shell::GnomeShellToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let a = f.niri_output(1);
    let b = f.niri_output(2);
    let a_name = a.name();

    let show = |f: &mut Fixture, connector: Option<&str>, level: Option<f64>, max: Option<f64>| {
        f.niri_state()
            .on_gnome_shell_msg(GnomeShellToNiri::ShowOsd {
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
    assert!(f.niri().osd.content(&a).is_some());
    assert!(f.niri().osd.content(&b).is_some());
    let content = f.niri().osd.content(&a).unwrap();
    assert_eq!(
        content.icon,
        vec!["audio-volume-high-symbolic", "audio-volume-high"],
        "the serialized GIcon becomes the candidate list"
    );
    assert_eq!(content.max_level, 1., "an absent max_level is 1");

    // A connector routes to that output alone, and cancels the other.
    show(&mut f, Some(&a_name), Some(0.5), None);
    tick(&mut f, 200);
    assert!(f.niri().osd.content(&a).is_some());
    assert!(
        f.niri().osd.content(&b).is_none(),
        "showOne cancels the monitors it did not name"
    );

    // An unknown connector is skipped, not applied to everything.
    f.niri().osd.hide_all();
    tick(&mut f, 200);
    show(&mut f, Some("does-not-exist"), Some(0.5), None);
    assert!(!f.niri().osd.is_visible());

    // Amplified volume: max_level > 1 is carried through.
    show(&mut f, None, Some(1.4), Some(1.5));
    assert_eq!(f.niri().osd.content(&a).unwrap().max_level, 1.5);

    // No level at all -> the OSD shows, but with no bar.
    f.niri().osd.hide_all();
    tick(&mut f, 200);
    show(&mut f, None, None, None);
    let content = f.niri().osd.content(&a).unwrap();
    assert!(content.level.is_none(), "an absent level means no bar");
    assert!(f.niri().osd.level_rect(&a).is_none());
}

/// An icon that is not a theme name leaves no candidates, and `show()` refuses
/// without an icon (`js/ui/osdWindow.js:90-92`) — so a ShowOSD carrying only a
/// file icon draws nothing rather than an empty pill.
#[cfg(feature = "dbus")]
#[test]
fn osd_show_osd_without_a_resolvable_icon_draws_nothing() {
    use crate::dbus::gnome_shell::GnomeShellToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out = f.niri_output(1);

    f.niri_state()
        .on_gnome_shell_msg(GnomeShellToNiri::ShowOsd {
            connector: None,
            label: Some("Volume".to_owned()),
            level: Some(0.5),
            max_level: None,
            icon: Some("/usr/share/pixmaps/whatever.png".to_owned()),
        });
    tick(&mut f, 120);
    assert!(f.niri().osd.content(&out).is_none());

    // ...and with no icon key at all.
    f.niri_state()
        .on_gnome_shell_msg(GnomeShellToNiri::ShowOsd {
            connector: None,
            label: Some("Volume".to_owned()),
            level: Some(0.5),
            max_level: None,
            icon: None,
        });
    tick(&mut f, 120);
    assert!(f.niri().osd.content(&out).is_none());
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

fn mpris_update(bus_name: &str, state: crate::mpris::PlayerState) -> crate::mpris::MprisToNiri {
    crate::mpris::MprisToNiri::PlayerUpdated {
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
    f.niri_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", state));

    assert!(
        !f.niri().panel_popover.is_open(),
        "the point of this test is that nothing has been opened"
    );
    assert!(
        f.niri().image_cache.is_loaded(
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
    f.niri_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", next));

    assert!(
        f.niri().image_cache.is_loaded(
            &second_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the new cover must load"
    );
    assert!(
        !f.niri().image_cache.is_loaded(
            &first_src,
            crate::render_helpers::icon::ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the previous cover must be evicted once no player claims it"
    );

    // And a player going away takes its cover with it.
    f.niri_state()
        .on_mpris_msg(crate::mpris::MprisToNiri::PlayerRemoved {
            bus_name: "org.mpris.MediaPlayer2.rb".to_owned(),
        });
    assert!(
        !f.niri().image_cache.is_loaded(
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
#[cfg(feature = "dbus")]
#[test]
fn the_account_picture_is_decoded_up_front_and_outlives_a_track_change() {
    use crate::dbus::accounts_service::{AccountIcon, AccountsToNiri, UserAccount};
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

    f.niri_state()
        .on_accounts_msg(AccountsToNiri::UserChanged(UserAccount {
            real_name: "Test User".to_owned(),
            icon_file: AccountIcon::read(face.clone()),
            ..Default::default()
        }));

    let face_src = ImageSource::File(face.clone());
    assert!(
        f.niri()
            .image_cache
            .is_loaded(&face_src, ImageFit::Cover, AVATAR_PX, 1.0),
        "the picture must decode as AccountsService answers, not on the frame that draws it"
    );

    // A player appears and then changes track: two `retain` passes over the shared cache.
    let mut state = mpris_state("Rhythmbox", None);
    state.art = Some(ImageSource::File(cover.clone()));
    f.niri_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", state));
    let mut next = mpris_state("Rhythmbox", None);
    next.title = "Blue in Green".into();
    next.art = None;
    f.niri_state()
        .on_mpris_msg(mpris_update("org.mpris.MediaPlayer2.rb", next));

    assert!(
        !f.niri().image_cache.is_loaded(
            &ImageSource::File(cover.clone()),
            ImageFit::Contain,
            BODY_ICON,
            1.0
        ),
        "the cover no player claims must still be evicted — the bound has to keep working"
    );
    assert!(
        f.niri()
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
#[cfg(feature = "dbus")]
#[test]
fn changing_the_account_picture_in_place_replaces_it() {
    use crate::dbus::accounts_service::{AccountIcon, AccountsToNiri, UserAccount};
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
        f.niri_state()
            .on_accounts_msg(AccountsToNiri::UserChanged(UserAccount {
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
        .niri()
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
        .niri()
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
#[cfg(feature = "dbus")]
#[test]
fn each_condition_alone_hides_the_switch_user_button() {
    use crate::dbus::accounts_service::AccountsToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Nothing has answered yet: the fail-closed direction is hidden, since a button offering a
    // switch we have not established is possible is the one that does nothing.
    assert!(
        !f.niri().switch_user_visible(),
        "the button must not appear before anything has said it can work"
    );

    f.niri_state()
        .on_accounts_msg(AccountsToNiri::CanSwitch(true));
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::MultipleUsers(true));
    assert!(
        f.niri().switch_user_visible(),
        "with a seat, another user, and default settings, the button shows"
    );

    // ...and each condition, alone, takes it away again.
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::CanSwitch(false));
    assert!(
        !f.niri().switch_user_visible(),
        "a seat that cannot host another session"
    );
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::CanSwitch(true));

    f.niri_state()
        .on_accounts_msg(AccountsToNiri::MultipleUsers(false));
    assert!(!f.niri().switch_user_visible(), "nobody else to log in as");
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::MultipleUsers(true));

    let base = f.niri().screen_shield.settings();
    let mut settings = base;
    settings.user_switch_enabled = false;
    f.niri().screen_shield.set_settings(settings);
    assert!(
        !f.niri().switch_user_visible(),
        "org.gnome.desktop.screensaver user-switch-enabled = false"
    );

    let mut settings = base;
    settings.disable_user_switching = true;
    f.niri().screen_shield.set_settings(settings);
    assert!(
        !f.niri().switch_user_visible(),
        "org.gnome.desktop.lockdown disable-user-switching = true"
    );

    f.niri().screen_shield.set_settings(base);
    assert!(f.niri().switch_user_visible(), "and back");
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
#[cfg(feature = "dbus")]
#[test]
fn the_switch_user_button_is_clickable_exactly_while_it_is_drawn() {
    use std::time::Duration;

    use crate::dbus::accounts_service::AccountsToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::CanSwitch(true));
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::MultipleUsers(true));

    let t0 = Duration::from_secs(1_000);
    let mid = t0 + Duration::from_millis(150);
    let after = t0 + Duration::from_millis(400);

    // Resting on the clock: nothing drawn, nothing to click.
    f.niri().lock_screen.set_page(false, t0);
    f.niri().lock_screen.settle();
    assert!(
        !f.niri().switch_user_reactive(t0),
        "the button must not be reactive with the clock up"
    );

    // The instant the prompt starts coming up, the button is still at alpha 0.
    f.niri().lock_screen.set_page(true, t0);
    assert!(
        !f.niri().switch_user_reactive(t0),
        "a click landed on the button on the frame it was still invisible"
    );
    assert!(
        f.niri().switch_user_reactive(mid),
        "mid-crossfade the button is on screen and must take a click"
    );
    assert!(f.niri().switch_user_reactive(after), "and once it settles");

    // Going back: the page is already the clock, but the button is still fading out.
    f.niri().lock_screen.set_page(false, after);
    assert!(
        f.niri()
            .switch_user_reactive(after + Duration::from_millis(150)),
        "the button was still drawn but had stopped taking clicks"
    );
    assert!(
        !f.niri()
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
#[cfg(feature = "dbus")]
#[test]
fn clicking_the_switch_user_button_cancels_the_prompt() {
    use smithay::utils::{Point, Rectangle, Size};

    use crate::dbus::accounts_service::AccountsToNiri;
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::CanSwitch(true));
    f.niri_state()
        .on_accounts_msg(AccountsToNiri::MultipleUsers(true));

    let raise = |f: &mut Fixture| {
        f.niri_state()
            .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
        f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
        f.niri_state()
            .on_verifier_event(VerifierEvent::AskQuestion {
                question: "Password:".to_owned(),
                secret: true,
            });
        f.niri_state().on_shield_key(None, Some('a'));
    };
    raise(&mut f);
    assert_eq!(
        f.niri().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt,
        "the fixture is on the prompt page"
    );

    let monitor = Rectangle::from_size(Size::from((1920., 1080.)));
    let rect = crate::ui::lock_screen::switch_user_rect(monitor);
    let centre = Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));

    // A click just outside the circle but inside its box: the corner of the bounding square.
    f.niri_state().on_shield_click(rect.loc);
    assert_eq!(
        f.niri().unlock_dialog.page(),
        crate::unlock_dialog::Page::Prompt,
        "a click in the button's corner must not have counted as the button"
    );

    // ...and one on the button itself drops back to the clock, the prompt cancelled.
    f.niri_state().on_shield_click(centre);
    assert_eq!(
        f.niri().unlock_dialog.page(),
        crate::unlock_dialog::Page::Clock,
        "the click did not cancel the authentication in flight"
    );
    assert!(
        f.niri().screen_shield.is_active(),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.gnome.Rhythmbox3.desktop",
            "Rhythmbox",
        )])),
        Box::new(RecordingLauncher::default()),
    );

    // `DesktopEntry` is the id WITHOUT `.desktop`, which is what gnome-shell appends
    // (`mpris.js:168`).
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.niri_state().on_mpris_msg(mpris_update(
        bus,
        mpris_state("Rhythmbox 3", Some("org.gnome.Rhythmbox3")),
    ));
    let player = f.niri().mpris.get(bus).expect("player is tracked").clone();
    assert_eq!(
        player.app.as_ref().map(|a| a.id.as_str()),
        Some("org.gnome.Rhythmbox3.desktop")
    );
    assert_eq!(player.source_name(), "Rhythmbox", "the app's name wins");
    assert_eq!(player.artists_line(), "Miles Davis");

    // A player whose DesktopEntry matches nothing installed -- or that sends none at all -- falls
    // back to Identity, and is still shown.
    let other = "org.mpris.MediaPlayer2.mystery";
    f.niri_state()
        .on_mpris_msg(mpris_update(other, mpris_state("Mystery Player", None)));
    let player = f.niri().mpris.get(other).unwrap().clone();
    assert!(player.app.is_none());
    assert_eq!(player.source_name(), "Mystery Player");
    assert_eq!(f.niri().mpris.visible().count(), 2);

    // A vanished bus name takes its player with it (`mpris.js:242-249`).
    f.niri_state()
        .on_mpris_msg(crate::mpris::MprisToNiri::PlayerRemoved {
            bus_name: bus.to_owned(),
        });
    assert!(f.niri().mpris.get(bus).is_none());
    assert_eq!(f.niri().mpris.visible().count(), 1);
}

/// `raise()` (`mpris.js:93-100`) prefers the app over the remote `Raise()`, because a remote raise
/// runs into focus-stealing prevention. With no resolvable app it falls back -- but only when the
/// player claims `CanRaise`.
#[test]
fn mpris_raise_prefers_the_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::mpris::NiriToMpris;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let recorder = RecordingLauncher::default();
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.gnome.Rhythmbox3.desktop",
            "Rhythmbox",
        )])),
        Box::new(recorder.clone()),
    );

    // Stand in for the watcher's inbound half so the calls we would make are observable.
    let (tx, rx) = async_channel::unbounded();
    f.niri().mpris_emit = Some(tx);

    // A resolvable app that is not running: activating it is a launch, and NOTHING goes on the bus.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.niri_state().on_mpris_msg(mpris_update(
        bus,
        mpris_state("Rhythmbox 3", Some("org.gnome.Rhythmbox3")),
    ));
    f.niri_state().raise_mpris_player(bus);
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
    f.niri_state().on_mpris_msg(mpris_update(other, state));
    f.niri_state().raise_mpris_player(other);
    assert_eq!(
        rx.try_recv().ok(),
        Some(NiriToMpris::Raise(other.to_owned()))
    );

    // No app and no CanRaise: there is nothing to do, and we must not invent a launch.
    f.niri_state()
        .on_mpris_msg(mpris_update(other, mpris_state("Mystery Player", None)));
    f.niri_state().raise_mpris_player(other);
    assert!(rx.try_recv().is_err());
    assert_eq!(recorder.calls.borrow().len(), 1);

    // A player that is not tracked at all is a no-op, not a panic -- the card can outlive it.
    f.niri_state()
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
    f.niri_state()
        .on_mpris_msg(mpris_update(bus, mpris_state("Rhythmbox", None)));
    let nid = banner_notify(&mut f, banner_req("app-a", ":1.1"));
    f.settle_animations();

    open_calendar(&mut f);
    let dm = f.niri().panel_popover.date_menu().unwrap();
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
    f.niri_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::ClearNotifications);
    let dm = f.niri().panel_popover.date_menu().unwrap();
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
    f.niri_state().on_mpris_msg(mpris_update(bus, stopped));
    let dm = f.niri().panel_popover.date_menu().unwrap();
    assert!(dm.media_card_rects().is_empty());
    assert!(dm.list().is_empty(), "now the placeholder is back");
}

/// The card's controls drive the player (`js/ui/messageList.js:778-791` → `mpris.js:73-91`) and,
/// unlike a menu item, leave the popover open. Its body raises the player and closes it
/// (`MediaMessage.vfunc_clicked`, `:799-804`), and an insensitive skip button is `reactive = false`
/// (`:836-838`) — so a click on it falls through to the body rather than being swallowed.
#[test]
fn media_card_controls_drive_the_player() {
    use crate::mpris::NiriToMpris;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.pointer_motion(1., 1.);
    let (tx, rx) = async_channel::unbounded();
    f.niri().mpris_emit = Some(tx);

    // Next is allowed, Previous is not — the state the fall-through case needs.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    let mut state = mpris_state("Rhythmbox", None);
    state.can_go_next = true;
    state.can_go_previous = false;
    state.can_raise = true;
    f.niri_state().on_mpris_msg(mpris_update(bus, state));

    open_calendar(&mut f);
    let output = f.niri_output(1);
    let origin = f.niri().panel_popover.content_location(&output);
    let (_, card, controls) = f
        .niri()
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
        Some(NiriToMpris::PlayPause(bus.to_owned()))
    );
    assert!(
        f.niri().panel_popover.is_open(),
        "a control is not a menu item"
    );
    click(&mut f, controls[2]);
    assert_eq!(rx.try_recv().ok(), Some(NiriToMpris::Next(bus.to_owned())));

    // Previous is insensitive: the click reaches the message, which raises the player. With no
    // app resolved and CanRaise set, raising is the remote `Raise()`.
    click(&mut f, controls[0]);
    assert_eq!(rx.try_recv().ok(), Some(NiriToMpris::Raise(bus.to_owned())));
    // `close()` starts the fade; the popover is open until it finishes.
    f.settle_animations();
    assert!(
        !f.niri().panel_popover.is_open(),
        "raising the player closes the calendar"
    );

    // The body does the same. Re-open and click the card away from every control.
    open_calendar(&mut f);
    click(
        &mut f,
        smithay::utils::Rectangle::new(card.loc, smithay::utils::Size::from((80., card.size.h))),
    );
    assert_eq!(rx.try_recv().ok(), Some(NiriToMpris::Raise(bus.to_owned())));
    f.settle_animations();
    assert!(!f.niri().panel_popover.is_open());
}

/// Only the VOLUME icon takes the scroll. gnome-shell connects `scroll-event` to that one
/// indicator's actor (`js/ui/status/volume.js:434-437,470-472`), so its neighbours in the status
/// cluster have no scroll behavior and a wheel notch over them falls through to whatever else
/// wants it — here, a plain wheel bind.
#[test]
fn only_the_volume_icon_consumes_a_panel_scroll() {
    use niri_config::binds::{Bind, Binds, Key, Modifiers, Trigger};

    let mut config = Config::default();
    // A no-modifier wheel bind, so "was the event consumed?" is observable without PipeWire.
    let bind = |trigger, action| Bind {
        key: Key {
            trigger,
            modifiers: Modifiers::empty(),
        },
        action,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    };
    config.binds = Binds(vec![
        bind(Trigger::WheelScrollDown, Action::FocusWorkspaceDown),
        // Up, so it is observable from where the wheel bind leaves us: with one window there are
        // only two workspaces, and workspace 1 is the last.
        bind(Trigger::TouchpadScrollDown, Action::FocusWorkspaceUp),
    ]);
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    // Two workspaces to move between, and a volume icon in the cluster to aim at.
    let id = f.add_client();
    let _surface = map_focused_window(&mut f, id);
    f.niri_state()
        .on_audio_status(Some(crate::audio::AudioStatus {
            volume: 0.5,
            muted: false,
        }));
    let active = |f: &mut Fixture| {
        f.niri()
            .layout
            .active_monitor_ref()
            .unwrap()
            .active_workspace_idx()
    };
    assert_eq!(active(&mut f), 0);

    let volume = f.niri().panel.volume_indicator_rect(1920.).unwrap();
    let centre_x = volume.loc.x + volume.size.w / 2.;

    // Over the volume icon: consumed, so the bind never runs. (Changing the volume itself needs a
    // PipeWire connection, which a headless fixture has none of -- see the OSD test below.)
    pointer_motion_to(&mut f, centre_x, 10.);
    f.scroll_wheel();
    f.niri_complete_animations();
    assert_eq!(
        active(&mut f),
        0,
        "a scroll over the volume icon belongs to the volume, not to the wheel bind"
    );

    // Just outside it -- still on the panel, still on the status cluster -- the bind fires.
    pointer_motion_to(&mut f, volume.loc.x + volume.size.w + 4., 10.);
    f.scroll_wheel();
    f.niri_complete_animations();
    assert_eq!(
        active(&mut f),
        1,
        "the icons beside the volume one have no scroll behavior of their own"
    );

    // A TOUCHPAD scroll over the icon is the volume's too: GNOME's SMOOTH branch turns the delta
    // into fractional steps (`volume.js:452-458`), where ours used to ignore anything but a wheel.
    pointer_motion_to(&mut f, centre_x, 10.);
    f.scroll_finger(0., 120.);
    f.niri_complete_animations();
    assert_eq!(
        active(&mut f),
        1,
        "a touchpad scroll over the volume icon must be consumed too"
    );

    // ... and off the icon it reaches the touchpad bind, proving the fixture's finger scroll does
    // fire it and the assertion above is not vacuous.
    pointer_motion_to(&mut f, volume.loc.x + volume.size.w + 4., 10.);
    f.scroll_finger(0., 120.);
    f.niri_complete_animations();
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
    let one = f.niri_output(1);
    let two = f.niri_output(2);

    f.niri_state().show_volume_osd(&crate::audio::AudioStatus {
        volume: 0.5,
        muted: false,
    });
    let content = f.niri().osd.content(&one).expect("output 1 shows the OSD");
    assert_eq!(content.icon, vec!["audio-volume-medium-symbolic"]);
    assert_eq!(content.label, None);
    assert_eq!(content.level, Some(0.5));
    assert_eq!(
        content.max_level,
        crate::audio::MAX_VOLUME,
        "the bar is scaled to the volume ceiling, not to 1.0 by accident"
    );
    assert!(
        f.niri().osd.content(&two).is_some(),
        "showAll, not showOne: every monitor gets it"
    );

    // Muting swaps the glyph, as the indicator's own icon does.
    f.niri_state().show_volume_osd(&crate::audio::AudioStatus {
        volume: 0.5,
        muted: true,
    });
    assert_eq!(
        f.niri().osd.content(&one).unwrap().icon,
        vec!["audio-volume-muted-symbolic"]
    );

    // The QS slider path is silent: it goes through `apply_popover_action`, which never asks for
    // an OSD.
    f.niri().osd.hide_all();
    f.settle_animations();
    f.niri_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetVolume(0.8));
    assert!(
        !f.niri().osd.is_visible(),
        "dragging the visible slider must not also raise an OSD"
    );
}

/// The whole panel-scroll chain end to end — pointer over the volume icon, wheel notch, a real
/// write reaching the backend, and the OSD (`js/ui/status/volume.js:452-458`).
///
/// This is what the [`crate::audio::AudioBackend`] seam bought: with a concrete PipeWire handle on
/// `Niri` the headless fixture had no backend at all, so the scroll path returned early and none of
/// this was observable — deleting the `show_volume_osd` call left the suite green.
#[test]
fn a_scroll_over_the_volume_icon_steps_the_backend_and_shows_the_osd() {
    use crate::audio::AudioWrite;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let output = f.niri_output(1);
    let audio = f.install_stub_audio(0.5);

    let volume = f.niri().panel.volume_indicator_rect(1920.).unwrap();
    let centre_x = volume.loc.x + volume.size.w / 2.;
    pointer_motion_to(&mut f, centre_x, 10.);

    // One notch down: a write of exactly one slider step, and an OSD saying so.
    f.scroll_wheel();
    f.niri_complete_animations();
    assert_eq!(
        audio.writes(),
        vec![AudioWrite::Volume(0.5 - crate::audio::SCROLL_STEP)],
        "a wheel notch is one SLIDER_SCROLL_STEP, written to the backend"
    );
    let content = f
        .niri()
        .osd
        .content(&output)
        .expect("the scroll shows an OSD");
    assert_eq!(content.level, Some(0.5 - crate::audio::SCROLL_STEP));
    assert_eq!(content.icon, vec!["audio-volume-medium-symbolic"]);
    assert_eq!(
        f.niri().audio.unwrap().volume,
        0.5 - crate::audio::SCROLL_STEP,
        "the model the panel icon reads follows the write, without waiting for an echo"
    );

    // At the ceiling the write still happens, but the value cannot move -- and GNOME gates the OSD
    // on `slider.step()` having returned true (`volume.js:457`), so a scroll that changes nothing
    // must not re-arm an OSD that says the same thing.
    f.niri_state()
        .apply_popover_action(crate::ui::popover::PopoverAction::SetVolume(
            crate::audio::MAX_VOLUME,
        ));
    f.niri().osd.hide_all();
    f.settle_animations();
    audio.clear_writes();

    f.scroll_wheel_up();
    f.niri_complete_animations();
    assert!(
        !f.niri().osd.is_visible(),
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
        f.niri().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        "the volume icon is part of the quick-settings button"
    );
    f.niri().osd.hide_all();
    f.settle_animations();
    audio.clear_writes();

    f.scroll_wheel();
    f.niri_complete_animations();
    assert_eq!(
        audio.writes(),
        vec![],
        "with its slider on screen, the scroll must not change the volume"
    );
    assert!(
        f.niri().osd.is_visible(),
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
    f.niri_state()
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
    f.niri_state()
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
    f.niri_state()
        .apply_popover_action(PopoverAction::SetOutputDevice(AudioDeviceKey::Node(
            "bluez_output.AA".to_owned(),
        )));
    assert_eq!(
        audio.writes(),
        vec![AudioWrite::DefaultSink("bluez_output.AA".to_owned())]
    );

    // The input side takes the same two shapes, against the source setters.
    audio.clear_writes();
    f.niri_state()
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
    let output = f.niri_output(1);
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
    f.niri_state().on_sink_list(sinks.clone());
    f.niri_state().on_audio_cards(cards("analog-output"));
    assert_eq!(f.niri().headphones, Some(false));
    assert!(
        !f.niri().osd.is_visible(),
        "the initial sync must not raise an OSD (`initializing`)"
    );

    // Plug headphones in: a change, so the OSD comes up — showing the LEVEL glyph, never the
    // headphone one (`showOSD` uses `getIcon()`, `volume.js:283-288`).
    f.niri_state()
        .on_audio_cards(cards("analog-output-headphones"));
    assert_eq!(f.niri().headphones, Some(true));
    let content = f
        .niri()
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
    f.niri().osd.hide_all();
    f.settle_animations();
    f.niri_state()
        .on_audio_cards(cards("analog-output-headphones"));
    assert!(
        !f.niri().osd.is_visible(),
        "an unchanged port must not re-arm the OSD"
    );

    // Unplugging is a change too, and the first answer's silence is long spent.
    f.niri_state().on_audio_cards(cards("analog-output"));
    assert_eq!(f.niri().headphones, Some(false));
    assert!(f.niri().osd.is_visible(), "unplugging speaks up as well");

    // A bluetooth headset arriving as the new default: no card, but a form factor. GNOME does not
    // reset `_hasHeadphones` across a stream swap, so this is a change and shows the OSD.
    f.niri().osd.hide_all();
    f.settle_animations();
    f.niri_state().on_sink_list(crate::audio::SinkList {
        sinks: vec![SinkInfo {
            name: "bluez".to_owned(),
            description: "Bluetooth Headset".to_owned(),
            card: None,
            form_factor: Some("headset".to_owned()),
        }],
        default_name: Some("bluez".to_owned()),
    });
    assert_eq!(f.niri().headphones, Some(true));
    assert!(
        f.niri().osd.is_visible(),
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

    f.niri_state()
        .apply_popover_action(PopoverAction::SetVolume(0.8));
    f.niri_state()
        .apply_popover_action(PopoverAction::ToggleMute);
    f.niri_state()
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
        f.niri().audio.unwrap().muted,
        "the mute toggle updates the model the panel icon reads"
    );
    assert_eq!(
        f.niri().sink_list.default_name,
        None,
        "the picker's check waits for the backend's echo -- a rejected write has none"
    );

    // The input side needs a bound source, exactly as the live backend does: with none, the mic
    // controls return None and the compositor leaves its model alone.
    audio.clear_writes();
    f.niri_state()
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
    f.niri().audio_backend = Some(Box::new(audio.clone()));
    f.niri_state()
        .apply_popover_action(PopoverAction::SetInputVolume(0.6));
    f.niri_state()
        .apply_popover_action(PopoverAction::ToggleInputMute);
    f.niri_state()
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
        f.niri().mic.muted,
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.Two");
    f.niri_complete_animations();

    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    assert!(f.niri().switcher.is_open(), "Super-Tab raises the switcher");
    assert_eq!(
        f.niri().switcher.selected(),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    let first = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    f.niri_complete_animations();

    // Hold Super for real, so the popup gets a modifier to commit on: driving the action with
    // nothing held makes it a *no-modifier* switcher, which commits on a timeout instead and
    // would quietly test the wrong path.
    const KEY_LEFTMETA: u32 = 125;
    f.key_press(KEY_LEFTMETA);

    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert!(f.niri().switcher.is_open());
    assert!(
        !f.niri().switcher.is_visible(),
        "nothing is drawn inside the open delay"
    );

    // Let go well inside the delay. The popup was never drawn, and the switch happens anyway --
    // the release reaches us because the grab was taken at open rather than at reveal.
    f.key_release(KEY_LEFTMETA);

    assert!(!f.niri().switcher.is_open(), "the release ends the session");
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
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
    f.niri().app_system = AppSystem::with_parts(
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
    let a = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.Two");
    f.niri_complete_animations();

    // Rest on "One" (item 1) and let the popup timer run out.
    let open_on_one = |f: &mut Fixture| {
        f.key_press(KEY_LEFTMETA);
        f.niri_state()
            .do_action(Action::SwitchApplications { backward: false }, false);
        assert_eq!(f.niri().switcher.selected(), Some(1), "opens on \"One\"");
    };
    let rest = |f: &mut Fixture, by: Duration| {
        let mut clock = f.niri().clock.clone();
        let now = clock.now_unadjusted();
        clock.set_unadjusted(now + by);
        f.niri().advance_animations();
    };

    open_on_one(&mut f);
    assert!(
        !f.niri().switcher.thumbnails_open(),
        "the sub-list is not instant — tabbing through a multi-window app must not flash it"
    );
    rest(&mut f, crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    assert!(
        f.niri().switcher.thumbnails_open(),
        "resting on a multi-window app pops its windows up"
    );
    assert_eq!(
        f.niri().switcher.thumbnail_selected(),
        None,
        "...with nothing picked in it"
    );

    // So the release still activates the app's most recent window.
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
        b,
        "a sub-list nobody picked from commits to the app's first window"
    );

    // Now descend into it and take the *other* window.
    open_on_one(&mut f);
    tap(&mut f, KEY_DOWN);
    assert!(
        f.niri().switcher.thumbnails_open(),
        "Down opens the sub-list at once"
    );
    assert_eq!(f.niri().switcher.thumbnail_selected(), Some(0));

    tap(&mut f, KEY_RIGHT);
    assert_eq!(
        f.niri().switcher.thumbnail_selected(),
        Some(1),
        "Right walks the previews, not the app row"
    );
    assert_eq!(
        f.niri().switcher.selected(),
        Some(1),
        "and the app row stays where it was"
    );

    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
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
    f.niri().app_system = AppSystem::with_parts(
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
    f.niri_complete_animations();

    f.key_press(KEY_LEFTMETA);
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    tap(&mut f, KEY_DOWN);
    assert!(f.niri().switcher.thumbnails_open());

    // Up hands the arrows back to the app row and does *not* re-open the sub-list on the timer.
    tap(&mut f, KEY_UP);
    assert!(!f.niri().switcher.thumbnails_open(), "Up closes it");

    let mut clock = f.niri().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.niri().advance_animations();
    assert!(
        !f.niri().switcher.thumbnails_open(),
        "and it stays closed — `forceAppFocus` does not re-arm the timer"
    );

    // With the arrows back on the row, Left/Right move apps again...
    tap(&mut f, KEY_LEFT);
    assert_eq!(
        f.niri().switcher.selected(),
        Some(0),
        "the arrows are back on the app row"
    );

    // ...and landing on the single-window app arms nothing at all.
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.niri().advance_animations();
    assert!(
        !f.niri().switcher.thumbnails_open(),
        "a one-window app has no sub-list to show"
    );

    f.key_release(KEY_LEFTMETA);
}

/// `switch-group` opens the app switcher *inside* the current app, on its window sub-list.
///
/// It is the same popup and the same item list as `switch-applications` — you can still tab out
/// to another app — but it starts at (app 0, window 1): app 0 is the app you are in, and window 1
/// is the one you are not, so a tap-and-release swaps between an app's two windows the way
/// tap-and-release swaps between two apps (`_initialSelection`, `altTab.js:117-137`).
///
/// Driven through the real `Above_Tab` key, which is a **keycode** match: mutter special-cases its
/// fake keysym to `KEY_GRAVE + 8` before consulting any layout
/// (`src/core/keybindings.c:385-392`), so the binding is the physical key above Tab whatever it
/// happens to type.
#[test]
fn switch_group_opens_inside_the_current_app_on_its_second_window() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    const KEY_GRAVE: u32 = 41;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    // "One" gets three windows so forward's "second" and backward's "last" are distinguishable.
    // Each maps focused, so within "One" the MRU order ends up [c, b, a].
    map_window_for_app(&mut f, client, "org.example.Two");
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let c = f.niri().layout.focus().unwrap().id();
    f.niri_complete_animations();

    // Super + the key above Tab, held.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_GRAVE);

    assert!(f.niri().switcher.is_open(), "Above_Tab raises the switcher");
    assert_eq!(
        f.niri().switcher.selected(),
        Some(0),
        "pinned to the app you are already in, not the previous one"
    );
    assert!(
        f.niri().switcher.thumbnails_open(),
        "with its windows already up — no waiting for the popup timer"
    );
    assert_eq!(
        f.niri().switcher.thumbnail_selected(),
        Some(1),
        "starting on the app's *second* window — the one you are not in"
    );

    // A second press walks that app's windows rather than moving to the next app.
    tap(&mut f, KEY_GRAVE);
    assert_eq!(f.niri().switcher.selected(), Some(0), "still the same app");
    assert_eq!(f.niri().switcher.thumbnail_selected(), Some(2));

    // And releasing commits to the window the sub-list had picked.
    f.key_release(KEY_LEFTMETA);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    let focused = f.niri().layout.focus().unwrap().id();
    assert_eq!(focused, a, "committed to the picked window");
    assert_ne!(focused, b);
    assert_ne!(focused, c);

    // Backward starts at the *end* of the app's windows instead (`cachedWindows.length - 1`).
    f.key_press(KEY_LEFTSHIFT);
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_GRAVE);
    assert_eq!(f.niri().switcher.selected(), Some(0));
    assert_eq!(f.niri().switcher.thumbnail_selected(), Some(2));
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
    let a = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.B");
    let b = f.niri().layout.focus().unwrap().id();
    f.niri_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_ESC);

    assert!(f.niri().switcher.is_open(), "Alt+Escape opens a cycler");
    assert!(
        f.niri().switcher.item_rect(0).is_none() && f.niri().switcher.footer_rect().is_none(),
        "...which has no list, so it measures no panel to hit-test or draw"
    );
    // `_initialSelection`: forward starts at 1, the window you are *not* on.
    assert_eq!(f.niri().switcher.cycler_window(), Some(a));
    let highlight = f.niri().cycler_highlight.expect("the window is framed");
    assert!(
        highlight.size.w > 0. && highlight.size.h > 0.,
        "and framed somewhere real: {highlight:?}"
    );

    // `<Alt>F6` is the *other* cycler's binding: `_keyPressHandler` matches one action and
    // propagates the rest, so it does not cross-drive this one.
    tap(&mut f, KEY_F6);
    assert_eq!(
        f.niri().switcher.cycler_window(),
        Some(a),
        "the group cycler's key does not drive the window cycler"
    );

    // A second press of its own key walks on; the frame follows.
    tap(&mut f, KEY_ESC);
    assert_eq!(f.niri().switcher.cycler_window(), Some(b));
    assert_ne!(f.niri().cycler_highlight, Some(highlight));

    // Releasing the modifier commits, like any other switcher.
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(f.niri().layout.focus().unwrap().id(), b);
    assert!(
        f.niri().cycler_highlight.is_none(),
        "and the frame goes with the session"
    );
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.Two");
    let other = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let a = f.niri().layout.focus().unwrap().id();
    map_window_for_app(&mut f, client, "org.example.One");
    let b = f.niri().layout.focus().unwrap().id();
    f.niri_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F6);
    assert!(f.niri().switcher.is_open());
    assert_eq!(f.niri().switcher.cycler_window(), Some(a));

    // "One" has exactly two windows, so any number of presses stays inside them.
    for expected in [b, a, b] {
        tap(&mut f, KEY_F6);
        assert_eq!(f.niri().switcher.cycler_window(), Some(expected));
        assert_ne!(
            f.niri().switcher.cycler_window(),
            Some(other),
            "the other app's window is not in this cycler at all"
        );
    }

    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(f.niri().layout.focus().unwrap().id(), b);
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.A.desktop",
            "A",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.A");
    map_window_for_app(&mut f, client, "org.example.A");
    let before = f.niri().layout.focus().unwrap().id();
    f.niri_complete_animations();

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_F6);
    assert!(f.niri().switcher.is_open(), "the group cycler is up");
    assert_ne!(f.niri().switcher.cycler_window(), Some(before));

    // Still holding Alt: `<Alt>Escape` is `cycle-windows`, which this popup does not match.
    tap(&mut f, KEY_ESC);
    assert!(
        !f.niri().switcher.is_open(),
        "it abandons rather than cycles"
    );
    assert!(f.niri().cycler_highlight.is_none());

    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(client);
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
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
    f.niri().app_system = AppSystem::with_parts(
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
    f.niri_complete_animations();

    let mut clock = f.niri().clock.clone();
    let rest = |f: &mut Fixture, clock: &mut crate::animation::Clock, by: Duration| {
        let now = clock.now_unadjusted();
        clock.set_unadjusted(now + by);
        f.niri().advance_animations();
    };

    // Rest on the multi-window app until its sub-list pops up on the timer, then let the fade
    // finish so "still animating" below can only mean a *new* fade.
    f.key_press(KEY_LEFTMETA);
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert_eq!(f.niri().switcher.selected(), Some(1), "opens on \"One\"");
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
    assert!(f.niri().switcher.thumbnails_open());
    assert_eq!(f.niri().switcher.thumbnail_selected(), None);
    assert!(
        !f.niri().switcher.are_animations_ongoing(),
        "the timer-opened list has finished fading in"
    );

    tap(&mut f, KEY_DOWN);

    assert_eq!(
        f.niri().switcher.thumbnail_selected(),
        Some(0),
        "Down picks the app's first window"
    );
    assert!(
        !f.niri().switcher.are_animations_ongoing(),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.One.desktop",
            "One",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    f.niri_complete_animations();

    f.key_press(KEY_LEFTMETA);
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    tap(&mut f, KEY_DOWN);

    assert!(f.niri().switcher.thumbnails_open());
    let alpha = f
        .niri()
        .switcher
        .thumbnail_alpha()
        .expect("an open sub-list");
    assert!(
        alpha < 1.,
        "the sub-list starts transparent and eases in, got {alpha}"
    );
    assert!(
        f.niri().switcher.are_animations_ongoing(),
        "and it must keep the redraw loop alive, or the fade never runs"
    );

    // Past the fade, it is fully drawn and asks for nothing more.
    let mut clock = f.niri().clock.clone();
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::FADE_TIME * 2);
    assert_eq!(f.niri().switcher.thumbnail_alpha(), Some(1.));
    assert!(!f.niri().switcher.are_animations_ongoing());

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
    f.niri().app_system = AppSystem::with_parts(
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
    f.niri_complete_animations();

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
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    assert_eq!(f.niri().switcher.selected(), Some(1), "opens on \"One\"");

    // `w` on the app row is a no-op: nothing is picked, so there is nothing to close.
    tap(&mut f, KEY_W);
    assert_eq!(
        asked(&mut f),
        (0, false),
        "`w` on the app row must not close anything — the key belongs to the sub-list"
    );
    assert!(
        f.niri().switcher.is_open(),
        "and it certainly must not end the session"
    );

    // Nor does it once the sub-list has merely *popped up* on its timer: it is up with nothing
    // picked, and there is still no window the key names.
    let mut clock = f.niri().clock.clone();
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::POPUP_TIME * 2);
    f.niri().advance_animations();
    assert!(f.niri().switcher.thumbnails_open());
    assert_eq!(f.niri().switcher.thumbnail_selected(), None);
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
    assert!(f.niri().switcher.is_open(), "without ending the session");

    // `q` quits the app: every window of it is asked to close, and no other app's.
    tap(&mut f, KEY_Q);
    assert_eq!(
        asked(&mut f),
        (2, false),
        "`q` asks every window of the selected app to close, and only that app's"
    );
    assert!(f.niri().switcher.is_open(), "still without ending it");

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
        f.niri().contents_under(over_window).surface.is_some(),
        "the window under the pointer normally receives pointer focus"
    );

    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.niri().switcher.is_open());

    // Still inside the popup's delay: the grab is already up, so the window is already cut off.
    assert!(
        !f.niri().switcher.is_visible(),
        "sampled inside the open delay, where the popup draws nothing"
    );
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.niri().contents_under(over_window).surface.is_none(),
        "no window under an open switcher receives pointer focus"
    );
    assert!(
        f.niri()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "the seat pointer focus is cleared while the switcher holds its grab"
    );

    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();

    // ...and it comes back when the session ends.
    pointer_motion_to(&mut f, over_window.x, over_window.y);
    assert!(
        f.niri().contents_under(over_window).surface.is_some(),
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
    let before = f.niri().layout.focus().unwrap().id();

    // Open on item 1 (the previously used window), then walk right and back.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert_eq!(
        f.niri().switcher.selected(),
        Some(1),
        "opens on the previous"
    );

    tap(&mut f, KEY_RIGHT);
    assert_eq!(f.niri().switcher.selected(), Some(2), "Right is _next");
    tap(&mut f, KEY_LEFT);
    tap(&mut f, KEY_LEFT);
    assert_eq!(f.niri().switcher.selected(), Some(0), "Left is _previous");

    // Escape abandons: the popup goes away and focus has not moved.
    tap(&mut f, KEY_ESC);
    assert!(!f.niri().switcher.is_open(), "Escape destroys the popup");
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_eq!(
        f.niri().layout.focus().unwrap().id(),
        before,
        "a cancelled switcher leaves focus where it was"
    );

    // Return commits without waiting for the modifier, which is what makes a no-modifier popup
    // usable at all.
    f.key_press(KEY_LEFTALT);
    tap(&mut f, KEY_TAB);
    assert!(f.niri().switcher.is_open());
    tap(&mut f, KEY_ENTER);
    assert!(!f.niri().switcher.is_open(), "Return finishes the popup");
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    f.double_roundtrip(id);
    assert_ne!(
        f.niri().layout.focus().unwrap().id(),
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.One.desktop",
            "One",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    f.niri_complete_animations();

    const KEY_LEFTALT: u32 = 56;
    const KEY_LEFTMETA: u32 = 125;

    f.key_press(KEY_LEFTALT);
    f.niri_state()
        .do_action(Action::SwitchWindows { backward: false }, false);
    let item = f.niri().switcher.item_rect(0).expect("an item");
    let footer = f
        .niri()
        .switcher
        .footer_rect()
        .expect("the window switcher has a title band");
    let panel = f.niri().switcher.panel_rect().expect("a panel");
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
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    let app_footer = f.niri().switcher.footer_rect();
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
    f.niri().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");

    // A second window on the next workspace down.
    f.niri_state().do_action(Action::FocusWorkspaceDown, false);
    map_window_for_app(&mut f, client, "org.example.Two");
    f.niri_complete_animations();

    // Sanity: the two really are on different workspaces, so the assertions below can differ.
    assert_eq!(
        f.niri().switcher_tab_list(false).len(),
        2,
        "both windows exist when nothing is filtered"
    );

    const KEY_LEFTALT: u32 = 56;
    const KEY_LEFTMETA: u32 = 125;

    f.key_press(KEY_LEFTALT);
    f.niri_state()
        .do_action(Action::SwitchWindows { backward: false }, false);
    let alt_tab_items = f.niri().switcher.item_count();
    f.key_release(KEY_LEFTALT);

    f.key_press(KEY_LEFTMETA);
    f.niri_state()
        .do_action(Action::SwitchApplications { backward: false }, false);
    let super_tab_items = f.niri().switcher.item_count();
    f.key_release(KEY_LEFTMETA);

    assert_eq!(
        alt_tab_items,
        Some(1),
        "stock Alt-Tab shows only this workspace's window"
    );
    assert_eq!(super_tab_items, Some(2), "stock Super-Tab spans workspaces");
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
    use crate::dbus::polkit_agent::{BeginRequest, PolkitRequest, PolkitToNiri};
    use crate::niri::KeyboardFocus;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Stand in for the agent, so what the dialog *sends* can be read back rather than assumed.
    let (to_agent, from_dialog) = async_channel::unbounded();
    f.niri().polkit_requests = Some(to_agent);
    let sent = move || from_dialog.try_recv().ok();

    let begin = |user: &str| {
        PolkitToNiri::Begin(Box::new(BeginRequest {
            action_id: "org.freedesktop.test.frobnicate".to_owned(),
            message: "Authentication is required to frobnicate".to_owned(),
            user_name: user.to_owned(),
            passwordless: false,
            avatar: None,
        }))
    };

    // polkitd asks. Nothing is on screen yet — PAM has not said it wants anything.
    f.niri_state().on_polkit_msg(begin("root"));
    assert!(
        !f.niri().polkit_is_open(),
        "the dialog must not appear before PAM asks"
    );
    assert!(
        matches!(sent(), Some(PolkitRequest::Initiate { .. })),
        "but the conversation has been started"
    );

    // PAM asks, and now it is on screen and holds the keyboard.
    f.niri_state().on_polkit_msg(PolkitToNiri::Request {
        prompt: "Password:".to_owned(),
        echo_on: false,
    });
    f.niri().polkit_ui.settle();
    assert!(f.niri().polkit_is_open());
    f.niri_state().refresh_and_flush_clients();
    assert!(
        matches!(f.niri().keyboard_focus, KeyboardFocus::PolkitDialog),
        "the dialog is modal, so it owns the keyboard: {:?}",
        f.niri().keyboard_focus,
    );

    // A real password off a real keyboard, masked on the way in.
    tap(&mut f, KEY_A);
    tap(&mut f, KEY_T);
    assert_eq!(f.niri().polkit_dialog.entry_display(), "\u{25cf}\u{25cf}");
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
        f.niri().polkit_dialog.entry_display(),
        "",
        "the buffer does not outlive the answer"
    );

    // PAM refuses. The dialog stays up and another conversation starts.
    f.niri_state().on_polkit_msg(PolkitToNiri::Completed(false));
    assert!(f.niri().polkit_is_open(), "a refusal is not the end");
    assert!(matches!(sent(), Some(PolkitRequest::Initiate { .. })));

    // Escape is a dismissal, which is a different answer from a failure: it tells the program that
    // asked to stop, rather than to try again.
    tap(&mut f, KEY_ESC);
    assert!(
        matches!(sent(), Some(PolkitRequest::Done { dismissed: true })),
        "Escape must reach polkitd as a dismissal"
    );
    f.niri().polkit_ui.settle();
    assert!(!f.niri().polkit_is_open());
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
    use crate::dbus::gnome_screen_saver::ScreenSaverToNiri;
    use crate::dbus::polkit_agent::{BeginRequest, PolkitRequest, PolkitToNiri};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (to_agent, from_dialog) = async_channel::unbounded();
    f.niri().polkit_requests = Some(to_agent);
    let sent = move || from_dialog.try_recv().ok();

    // Lock, with a live verifier behind it.
    f.niri_state()
        .on_screen_saver_msg(ScreenSaverToNiri::Lock(None));
    f.niri_state().on_verifier_event(VerifierEvent::Ready(1));
    assert!(f.niri().screen_shield.is_locked());

    f.niri_state()
        .on_polkit_msg(PolkitToNiri::Begin(Box::new(BeginRequest {
            action_id: "org.freedesktop.test.frobnicate".to_owned(),
            message: "Authentication is required to frobnicate".to_owned(),
            user_name: "root".to_owned(),
            passwordless: false,
            avatar: None,
        })));
    assert!(!f.niri().polkit_is_open(), "not over a lock screen");
    assert!(
        sent().is_none(),
        "and no conversation is started behind it either"
    );
    assert!(
        f.niri().polkit_deferred.is_some(),
        "the request is held, not dropped -- polkitd is still waiting on it"
    );

    // gdm accepts; the shield goes, and the held request gets its turn on the next refresh (which
    // a live compositor runs constantly).
    f.niri_state().on_verifier_event(VerifierEvent::Complete);
    assert!(!f.niri().screen_shield.is_active());
    f.niri_state().refresh_and_flush_clients();
    assert!(
        f.niri().polkit_deferred.is_none(),
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
#[cfg(feature = "dbus")]
#[test]
fn the_portal_window_list_carries_what_its_chooser_reads() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::dbus::gnome_shell_introspect::{IntrospectToNiri, NiriToIntrospect};

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();

    f.niri().app_system = AppSystem::with_parts(
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
    f.niri().sync_running_apps();

    let (tx, rx) = async_channel::unbounded();
    f.niri_state()
        .on_introspect_msg(&tx, IntrospectToNiri::GetWindows);
    let NiriToIntrospect::Windows(windows) = rx.try_recv().expect("a reply") else {
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
            Some(crate::niri::DYNAMIC_CAST_TARGET_LABEL),
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
    f.niri_state()
        .on_introspect_msg(&tx, IntrospectToNiri::GetRunningApplications);
    let NiriToIntrospect::RunningApplications(apps) = rx.try_recv().expect("a reply") else {
        panic!("wrong reply");
    };
    assert_eq!(
        apps.get("org.example.Editor.desktop")
            .and_then(|a| a.active_on_seats.as_deref()),
        Some(&[String::from("seat0")][..]),
        "the focused app is active on seat0"
    );
}
