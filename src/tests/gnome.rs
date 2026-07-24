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
const KEY_SPACE: u32 = 57;
const KEY_RIGHT: u32 = 106;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_Z: u32 = 44;
const KEY_LEFTALT: u32 = 56;
const KEY_F2: u32 = 60;
const KEY_F4: u32 = 62;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_DOWN: u32 = 108;
const KEY_PAGEDOWN: u32 = 109;
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

/// GNOME's `switch-windows` (default `<Alt>Tab`) cycles windows: holding Alt
/// and tapping Tab opens the switcher, releasing Alt commits, and focus lands
/// on the previously-used window. A second Alt+Tab returns to the first.
/// (Mapped onto niri's window MRU switcher; GNOME's app-grouped
/// `switch-applications` on `<Super>Tab` goes to the same switcher spanning
/// all workspaces — accepted divergence: no app grouping.)
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
        f.niri().window_mru_ui.is_open(),
        "Alt+Tab must open the window switcher"
    );
    f.key_release(KEY_LEFTALT);
    f.niri_complete_animations();
    // Let the focus change go through a refresh cycle, like in a real event
    // loop iteration, so the MRU bookkeeping sees it.
    f.double_roundtrip(id);

    assert!(
        !f.niri().window_mru_ui.is_open(),
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
    // within the work area, which the top panel insets to (0, 35, 1920, 1045).
    let slot = ((1920. % 101.) / 2., 35. + (1045. % 101.) / 3.);

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
    // below/right candidates all overflow the 1920×1045 work area (the top
    // panel insets it), so first-fit fails and every subsequent window cascades.
    let _w1 = map_window_sized(&mut f, id, (1000, 600), None);

    let _w2 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 35.),
        "the first cascaded window must sit at the work-area origin",
    );

    let _w3 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (50., 85.),
        "the next cascade slot is one 50px diagonal step down",
    );

    let _w4 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (100., 135.),
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
        @"size: 960 × 1045, bounds: 1920 × 1045, states: [Activated, TiledTop, TiledBottom, TiledLeft]"
    );

    // The client commits the tiled size; the tile sits at the left edge.
    let window = f.client(id).window(&surface);
    window.set_size(960, 1045);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 35.),
        "a left-tiled window must sit at the work-area origin",
    );

    // Toggle again: untile, restoring the saved geometry.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 800 × 600, bounds: 1920 × 1045, states: [Activated]"
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

    // Clamped to the work area (top panel insets it to 1920×1045):
    // scale = min(1920·√0.8/1800, 1045·√0.8/1000) ≈ 0.937 → 1687×937.
    let factor = 0.8f64.sqrt();
    let scale = f64::min(1920. * factor / 1800., 1045. * factor / 1000.);
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
        configures.contains("TiledLeft") && configures.contains("size: 960 × 1045"),
        "dropping in the left edge band must tile left, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(960, 1045);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 35.),
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
    // thumbnails band is reserved. Work area 1045 tall ⇒ spacing round(20.9) = 21,
    // round(21·0.6) = 13 above the (zero-height) band; the picker box is then
    //   y = 35 + 58 + 13                                = 106
    //   h = 1045 − 112(dash) − 21 − 58(entry) − 13     = 841
    let controls = overview_controls(&mut f);
    assert_eq!(controls.workspaces.loc.y, 106.);
    assert_eq!(controls.workspaces.size.h, 841.);

    // The row is fit by height into that box, and centered on what width is left.
    let zoom: f64 = 841. / 1080.;
    let ws_w = (1920. * zoom).ceil(); // 1496
    let offset_x = ((1920. - ws_w) / 2.).round(); // 212

    // Workspace-local slot (see expose::tests): 760 × 570 centered in the work
    // area — the top panel insets it to 1920×1045, so the slot sits at
    // (580, 35 + (1045−570)/2) = (580, 272).
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    assert_pos_eq(
        (rect.loc.x, rect.loc.y),
        (offset_x + 580. * zoom, 106. + 272. * zoom),
        "picker slot must sit in the allocated window-picker box",
    );
    assert!(
        (rect.size.w - 760. * zoom).abs() <= 1. && (rect.size.h - 570. * zoom).abs() <= 1.,
        "picker slot size must scale by the workspace zoom, got {rect:?}"
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
    // 35 + 58 + round(21 × 0.6) = 106, and 1080 × MAX_THUMBNAIL_SCALE = 54.
    assert_eq!((band.loc.y, band.size.h), (106., 54.));

    let (mon, _, _) = f.niri().layout.workspaces().next().unwrap();
    let strip = mon
        .expect("workspaces must be on a monitor")
        .thumbnail_strip()
        .expect("three workspaces must show the strip");
    assert_eq!(strip.thumbs[0].loc.y, band.loc.y);
    assert_eq!(strip.thumbs[0].size.h, band.size.h);
    assert!(
        strip.thumbs[0].loc.y >= crate::ui::panel::PANEL_HEIGHT,
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
    assert_eq!((expanded.loc.y, expanded.size.h), (168., 779.));

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
        mid.size.h > expanded.size.h && mid.size.h < 841.,
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
    assert_eq!((collapsed.loc.y, collapsed.size.h), (106., 841.));
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
    assert_eq!(collapsed.size.h, 841.);

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
    assert_eq!((expanded.loc.y, expanded.size.h), (168., 779.));
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
    // the active workspace spans 192..1728, spacing clamps to 24, so the
    // neighbor is visible from 1752 on (gnome-shell keeps the spacing at
    // its minimum exactly so neighbors peek in).
    f.pointer_motion(1800., 540.);
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

/// Two populated desktops with the overview open: window A stays on the
/// first workspace, window B is dragged to the trailing one (leaving a new
/// trailing empty third). Returns (A's window, B's window).
fn setup_two_desktops_in_overview(
    f: &mut Fixture,
    id: ClientId,
) -> (smithay::desktop::Window, smithay::desktop::Window) {
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
    f.pointer_motion(1800. - grab.0, 540. - grab.1 - 10.);
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

    // Click it again (now active): leave the overview.
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
    window.set_size(1920, 1045);
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
            (1920, 1045),
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
        configures.contains("size: 1920 × 1045"),
        "a maximized window must fill the work area below the panel, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(1920, 1045);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 35.),
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
    // origin (menu y = PANEL_HEIGHT + margin).
    let tile_x = origin_x + 12. + 75.;
    let tile_y = (35. + margin) + (12. + 44. + 8.) + (56. + 8.) + 28.;
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
    let tile_y = (35. + margin) + (12. + 44. + 8.) + (56. + 8.) + 28.;
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

    f.niri_state()
        .niri
        .emit_notification_action(id, "reply".to_owned());
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
    f.niri_state()
        .niri
        .emit_notification_action(id, "default".to_owned());
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
    f.niri_state().niri.open_notification_app(id);
    match gtk_emitted.recv_blocking().unwrap() {
        GtkToNotifications::Activate { app_id, .. } => assert_eq!(app_id, "org.example.App"),
        _ => panic!("expected Activate"),
    }
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

    // Collapsed: 90px card, live caret, no action row.
    let ((cid, card, _), expand, actions) = dm(&mut f);
    assert_eq!(cid, id);
    assert_eq!(card.size.h, 90.);
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
        90. + 5. * 18. + 28. + 6.,
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
        90. + 5. * 18. + 28. + 6.,
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
    assert_eq!(rects[1].1.size.h, 90.);
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
        .launch("org.example.App.desktop", LaunchMode::Activate)
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

/// A middle-click on a favorite also launches it (still `Activate`: `open_new_window`
/// is reserved for a *running* app, which S3 never tracks) and closes the overview.
#[test]
fn overview_dash_favorite_middle_click_launches() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let center = dash_tile_center(&mut f, 0);

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_MIDDLE, ButtonState::Pressed);
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

/// The trailing show-apps button consumes its click inertly in S3 (the app grid is
/// S8): no launch, and the overview stays open.
#[test]
fn overview_dash_show_apps_click_is_inert() {
    let (mut f, recorder) = dash_fixture(&["a.desktop"]);
    let i = f.niri().dash.show_apps_index();
    let center = dash_tile_center(&mut f, i);

    f.pointer_motion(center.x, center.y);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.niri_complete_animations();

    assert!(
        recorder.calls.borrow().is_empty(),
        "show-apps must not launch an app"
    );
    assert!(
        f.niri().layout.is_overview_open(),
        "show-apps is inert in S3 — the overview stays open"
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
        Some(DashHit::Favorite(0)),
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
    assert_eq!(f.niri().app_system.running()[0].n_windows, 1);
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
