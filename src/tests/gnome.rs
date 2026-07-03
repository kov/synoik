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
use std::time::Instant;

use insta::assert_snapshot;
use niri_config::{Action, Config};
use smithay::backend::input::ButtonState;
use smithay::input::keyboard::Keysym;
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
const KEY_LEFTCTRL: u32 = 29;
const KEY_A: u32 = 30;
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
    // grid of same-size windows, halved horizontally, third-ed vertically.
    let slot = ((1920. % 101.) / 2., (1080. % 101.) / 3.);

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
    // below/right candidates all overflow the 1920×1080 work area, so
    // first-fit fails and every subsequent window cascades.
    let _w1 = map_window_sized(&mut f, id, (1000, 600), None);

    let _w2 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 0.),
        "the first cascaded window must sit at the work-area origin",
    );

    let _w3 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (50., 50.),
        "the next cascade slot is one 50px diagonal step down",
    );

    let _w4 = map_window_sized(&mut f, id, (1000, 600), None);
    assert_pos_eq(
        focused_window_pos(&mut f),
        (100., 100.),
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
        @"size: 960 × 1080, bounds: 1920 × 1080, states: [Activated, TiledTop, TiledBottom, TiledLeft]"
    );

    // The client commits the tiled size; the tile sits at the left edge.
    let window = f.client(id).window(&surface);
    window.set_size(960, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 0.),
        "a left-tiled window must sit at the work-area origin",
    );

    // Toggle again: untile, restoring the saved geometry.
    f.key_press(KEY_LEFTMETA);
    tap(&mut f, KEY_LEFT);
    f.key_release(KEY_LEFTMETA);
    f.double_roundtrip(id);

    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 800 × 600, bounds: 1920 × 1080, states: [Activated]"
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

    // scale = min(1920·√0.8/1800, 1080·√0.8/1000) ≈ 0.954 → 1717×954.
    let factor = 0.8f64.sqrt();
    let scale = f64::min(1920. * factor / 1800., 1080. * factor / 1000.);
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
        configures.contains("TiledLeft") && configures.contains("size: 960 × 1080"),
        "dropping in the left edge band must tile left, got: {configures}"
    );

    let window = f.client(id).window(&surface);
    window.set_size(960, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    assert_pos_eq(
        focused_window_pos(&mut f),
        (0., 0.),
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

/// GNOME (40+) overview geometry: workspaces form a horizontal row with the
/// active one centered at 85% of the monitor (gnome-shell WorkspacesView).
/// The picker slot of a lone window pins both the workspace scale and its
/// centering.
#[test]
fn overview_workspace_is_centered_at_gnome_scale() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _w = map_window_sized(&mut f, id, (800, 600), None);
    let win = f.niri().layout.focus().unwrap().window.clone();

    tap(&mut f, KEY_LEFTMETA);
    f.niri_complete_animations();

    // Workspace-local slot (see expose::tests): (580, 255) 760 × 570, then
    // through zoom 0.8 with the workspace centered at (192, 108).
    let rect = f.niri().layout.expose_target_rect(&win).unwrap();
    assert_pos_eq(
        (rect.loc.x, rect.loc.y),
        (192. + 580. * 0.8, 108. + 255. * 0.8),
        "picker slot must reflect the centered 0.8-scale workspace",
    );
    assert!(
        (rect.size.w - 760. * 0.8).abs() <= 1. && (rect.size.h - 570. * 0.8).abs() <= 1.,
        "picker slot size must scale by the workspace zoom, got {rect:?}"
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

/// An edge-tiled window's preview drag re-tiles it on the drop workspace:
/// the overview drag moves the window between workspaces, nothing else
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

    // Drag the preview onto the trailing workspace's peeking edge. The
    // pick-up untiles to the restore size, like any interactive move; the
    // roundtrip flushes that configure so the re-tile one is observable.
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

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("TiledLeft"),
        "the drop must re-tile the window on the target workspace, got: {configures}"
    );

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
/// shake-loose is for dragging the real window, not the picker — and a drop
/// re-maximizes it on the target workspace (gnome-shell moves the window
/// between workspaces without changing its state).
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
    window.set_size(1920, 1080);
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

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("Maximized"),
        "the drop must re-maximize the window on the target workspace, got: {configures}"
    );

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
}
