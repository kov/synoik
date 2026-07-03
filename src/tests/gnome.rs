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

use niri_config::{Action, Config};
use smithay::backend::input::ButtonState;
use smithay::input::keyboard::Keysym;
use wayland_client::protocol::wl_keyboard::KeyState as WlKeyState;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::gnome::{Accel, AccelMods, AccelTrigger, GnomeKeyAction};

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
