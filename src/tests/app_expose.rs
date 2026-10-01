// SPDX-License-Identifier: GPL-3.0-only

//! App Exposé (`docs/fork/app-expose.md`): a divergence from GNOME, pinned the way the
//! conformance corpus pins a port — through `do_action` and real input against the fixture.

use smithay::desktop::Window;
use smithay::output::Output;
use synoik_config::Action;

use super::fixture::Fixture;
use super::gnome::{map_window_for_app, switcher_apps, tap, touchpad_swipe};

const ONE: &str = "org.example.One";
const TWO: &str = "org.example.Two";

fn focused(f: &mut Fixture) -> Window {
    f.synoik().layout.focus().unwrap().window.clone()
}

fn output(f: &mut Fixture, name: &str) -> Output {
    f.synoik()
        .layout
        .outputs()
        .find(|o| o.name() == name)
        .unwrap()
        .clone()
}

/// The windows in `output`'s grid, in no particular order.
fn shown(f: &mut Fixture, output: &Output) -> Vec<Window> {
    f.synoik()
        .layout
        .app_expose_slots(output)
        .into_iter()
        .map(|(window, _)| window)
        .collect()
}

fn same_set(mut a: Vec<Window>, mut b: Vec<Window>) -> bool {
    a.sort_by_key(|w| format!("{w:?}"));
    b.sort_by_key(|w| format!("{w:?}"));
    a == b
}

/// One display: "Two" and two windows of "One" on the first workspace, a third of "One" on the
/// second, focus back on the first workspace in "One". Returns One's three windows and Two's.
fn one_display_two_workspaces(f: &mut Fixture) -> (Vec<Window>, Window) {
    f.add_output(1, (1920, 1080));
    switcher_apps(f);
    let client = f.add_client();

    map_window_for_app(f, client, TWO);
    let two = focused(f);
    map_window_for_app(f, client, ONE);
    let a = focused(f);
    map_window_for_app(f, client, ONE);
    let b = focused(f);

    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    map_window_for_app(f, client, ONE);
    let c = focused(f);
    f.synoik_state().do_action(Action::FocusWorkspaceUp, false);
    f.settle();
    assert_eq!(
        focused(f),
        b,
        "precondition: focus is back in One, on the first workspace"
    );

    (vec![a, b, c], two)
}

/// The action brings up the focused app's windows from every workspace of the display in one
/// grid, and nothing of any other app's; pressed again, it goes away.
#[test]
fn the_focused_apps_windows_from_every_workspace_share_one_grid() {
    let mut f = Fixture::new();
    let (one, two) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();

    assert!(f.synoik().layout.is_app_expose_open());
    let windows = shown(&mut f, &out);
    assert!(
        same_set(windows.clone(), one.clone()),
        "exactly One's three windows, the one on the other workspace included: {windows:?}"
    );
    assert!(!windows.contains(&two), "never another app's window");

    // One decision over the lot: no two slots overlap, as they would if each workspace laid
    // itself out alone.
    let slots: Vec<_> = f
        .synoik()
        .layout
        .app_expose_slots(&out)
        .into_iter()
        .map(|(_, slot)| slot)
        .collect();
    for (i, a) in slots.iter().enumerate() {
        for b in &slots[i + 1..] {
            assert!(!a.overlaps(*b), "slots {a:?} and {b:?} overlap");
        }
    }

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(!f.synoik().layout.is_app_expose_open());
    assert!(
        f.synoik().layout.app_expose_slots(&out).is_empty(),
        "and once it has finished going away, nothing is left of it"
    );
}

/// A minimized window of the app is still one of its windows.
#[test]
fn minimized_windows_are_in_the_grid() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");

    f.synoik_state().do_action(Action::MinimizeWindow, false);
    f.settle();
    // Minimizing hands focus on to another window; put it back in One.
    f.synoik().layout.activate_window(&one[0]);

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(same_set(shown(&mut f, &out), one));
}

/// Each display lays out the app's windows that live on its own workspaces, and nothing else.
#[test]
fn each_display_shows_its_own_windows() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    switcher_apps(&mut f);
    let client = f.add_client();

    let left = output(&mut f, "headless-1");
    let right = output(&mut f, "headless-2");

    map_window_for_app(&mut f, client, ONE);
    let a = focused(&mut f);
    map_window_for_app(&mut f, client, ONE);
    let b = focused(&mut f);
    f.synoik_state()
        .do_action(Action::MoveWindowToMonitorRight, false);
    map_window_for_app(&mut f, client, TWO);
    f.synoik_state()
        .do_action(Action::MoveWindowToMonitorRight, false);
    f.synoik().layout.activate_window(&b);
    f.settle();
    assert_eq!(
        f.synoik().layout.active_output(),
        Some(&right),
        "precondition"
    );

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();

    assert_eq!(shown(&mut f, &left), vec![a]);
    assert_eq!(shown(&mut f, &right), vec![b]);
}

/// With nothing focused there is no app to show, and the action does nothing.
#[test]
fn nothing_focused_is_a_no_op() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(!f.synoik().layout.is_app_expose_open());
}

/// The overview and App Exposé are never up together: it does not open over the overview, and
/// opening the overview puts it away.
#[test]
fn the_overview_and_app_expose_exclude_each_other() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle();
    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(
        !f.synoik().layout.is_app_expose_open(),
        "not over the overview"
    );

    f.synoik_state().do_action(Action::CloseOverview, false);
    f.settle();
    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(f.synoik().layout.is_app_expose_open());

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.settle();
    assert!(f.synoik().layout.is_overview_open());
    assert!(
        !f.synoik().layout.is_app_expose_open(),
        "opening the overview puts it away"
    );
}

/// Cycling moves to the next running app, wrapping, and every display re-lays for it.
#[test]
fn cycling_moves_to_the_next_app_and_wraps() {
    let mut f = Fixture::new();
    let (one, two) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();

    f.synoik_state().cycle_app_expose(false);
    f.settle();
    assert_eq!(shown(&mut f, &out), vec![two.clone()], "on to Two");

    f.synoik_state().cycle_app_expose(false);
    f.settle();
    assert!(
        same_set(shown(&mut f, &out), one.clone()),
        "and round to One"
    );

    f.synoik_state().cycle_app_expose(true);
    f.settle();
    assert_eq!(shown(&mut f, &out), vec![two], "and back");
}

/// Activating a window that lives on another workspace makes that workspace its display's active
/// one, focuses the window, and puts App Exposé away.
#[test]
fn activating_a_window_on_another_workspace_switches_to_it() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let c = one[2].clone();

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();

    assert!(f.synoik().layout.activate_app_expose_window(&c));
    f.settle();

    assert!(!f.synoik().layout.is_app_expose_open());
    assert_eq!(focused(&mut f), c);
    let ws = f.synoik().layout.active_workspace().unwrap();
    assert!(ws.holds_window(&c), "its workspace is the active one");
}

/// Locking puts App Exposé away: its input is the shell's, and a locked session has none.
#[test]
fn locking_puts_it_away() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(f.synoik().layout.is_app_expose_open());

    f.synoik_state().on_screen_saver_msg(
        crate::dbus::gnome_screen_saver::ScreenSaverToSynoik::Lock(None),
    );
    f.settle();
    assert!(!f.synoik().layout.is_app_expose_open());
}

/// The dash has neither of its homes in App Exposé: there is no dock to summon.
#[test]
fn there_is_no_dock() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");

    f.synoik().dock.show(&out);
    assert!(
        f.synoik().dash_area(&out).is_some(),
        "precondition: the dock is out"
    );

    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert_eq!(f.synoik().dash_area(&out), None);
}

const KEY_ESC: u32 = 1;
const KEY_TAB: u32 = 15;
const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;

fn open(f: &mut Fixture) {
    f.synoik_state().do_action(Action::ToggleAppExpose, false);
    f.settle();
    assert!(f.synoik().layout.is_app_expose_open(), "precondition: up");
    assert!(
        f.synoik().keyboard_focus.is_app_expose(),
        "it holds the keyboard focus, so no window and no search sees a key"
    );
}

fn click_at(f: &mut Fixture, x: f64, y: f64) {
    use smithay::backend::input::ButtonState;
    super::gnome::pointer_motion_to(f, x, y);
    f.pointer_button(super::gnome::BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(super::gnome::BTN_LEFT, ButtonState::Released);
    f.settle();
}

/// Escape leaves, and the focus is where it was.
#[test]
fn escape_leaves() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    open(&mut f);

    tap(&mut f, KEY_ESC);
    f.settle();
    assert!(!f.synoik().layout.is_app_expose_open());
    assert_eq!(focused(&mut f), one[1], "the desktop as it was");
}

/// Tab moves on to the next app, Shift+Tab back.
#[test]
fn tab_cycles_apps() {
    let mut f = Fixture::new();
    let (one, two) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    open(&mut f);

    tap(&mut f, KEY_TAB);
    f.settle();
    assert_eq!(shown(&mut f, &out), vec![two]);

    f.key_press(KEY_LEFTSHIFT);
    tap(&mut f, KEY_TAB);
    f.key_release(KEY_LEFTSHIFT);
    f.settle();
    assert!(same_set(shown(&mut f, &out), one));
}

/// There is no search to type into: a letter neither engages the overview's nor leaves.
#[test]
fn typing_does_not_search() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);
    open(&mut f);

    tap(&mut f, KEY_A);
    f.settle();
    assert!(f.synoik().layout.is_app_expose_open());
    assert!(!f.synoik().overview_search.is_active());
}

/// Clicking a preview goes to its window, its workspace first when it lives on another one.
#[test]
fn clicking_a_preview_goes_to_its_window() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    open(&mut f);

    let c = one[2].clone();
    let slot = f
        .synoik()
        .layout
        .app_expose_slots(&out)
        .into_iter()
        .find(|(w, _)| *w == c)
        .unwrap()
        .1;
    click_at(
        &mut f,
        slot.loc.x + slot.size.w / 2.,
        slot.loc.y + slot.size.h / 2.,
    );

    assert!(!f.synoik().layout.is_app_expose_open());
    assert_eq!(focused(&mut f), c);
    assert!(f
        .synoik()
        .layout
        .active_workspace()
        .unwrap()
        .holds_window(&c));
}

/// A click on no preview leaves, the desktop as it was.
#[test]
fn clicking_beside_the_previews_leaves() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    open(&mut f);

    // The output's bottom-left corner, which no slot reaches.
    let slots = f.synoik().layout.app_expose_slots(&out);
    let (x, y) = (2., 1078.);
    assert!(
        slots
            .iter()
            .all(|(_, s)| !s.contains(smithay::utils::Point::from((x, y)))),
        "precondition: no preview there"
    );
    click_at(&mut f, x, y);

    assert!(!f.synoik().layout.is_app_expose_open());
    assert_eq!(focused(&mut f), one[1]);
}

/// The pointer on a preview raises its overlay — the close button and caption — once App
/// Exposé is fully up, as the picker's does in the overview.
#[test]
fn hovering_a_preview_shows_its_overlay() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    open(&mut f);

    let slot = f
        .synoik()
        .layout
        .app_expose_slots(&out)
        .into_iter()
        .find(|(w, _)| *w == one[0])
        .unwrap()
        .1;
    super::gnome::pointer_motion_to(
        &mut f,
        slot.loc.x + slot.size.w / 2.,
        slot.loc.y + slot.size.h / 2.,
    );
    f.settle();

    let overlays = f
        .synoik()
        .layout
        .monitor_for_output(&out)
        .unwrap()
        .preview_overlays();
    assert_eq!(
        overlays.iter().map(|(w, _, _)| w).collect::<Vec<_>>(),
        vec![&one[0]],
        "the hovered preview, and only it, shows its overlay"
    );
}

/// Three fingers down from the desktop bring App Exposé up over the focused app; up again takes
/// it down. (The fixture's swipes are post-natural-scrolling: positive is up.)
#[test]
fn swiping_down_brings_it_up_and_up_takes_it_down() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    // Off the hot corner, which would open the overview by itself.
    super::gnome::pointer_motion_to(&mut f, 960., 540.);

    // Slowly, and far past its one leg.
    touchpad_swipe(&mut f, 3, (0., -10.), 80, 50);
    assert!(f.synoik().layout.is_app_expose_open());
    assert!(!f.synoik().layout.is_overview_open());
    assert!(same_set(shown(&mut f, &out), one.clone()));

    touchpad_swipe(&mut f, 3, (0., 10.), 80, 50);
    assert!(!f.synoik().layout.is_app_expose_open());
    assert!(
        !f.synoik().layout.is_overview_open(),
        "one swipe up from App Exposé reaches the desktop and no further"
    );
    assert_eq!(focused(&mut f), one[1], "the desktop as it was");
}

/// A swipe that turns back before half way leaves the desktop alone, by the overview's release
/// rule.
#[test]
fn a_swipe_that_turns_back_leaves_the_desktop() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);
    super::gnome::pointer_motion_to(&mut f, 960., 540.);

    f.swipe_begin(3);
    for _ in 0..10 {
        f.advance_input_time(50);
        f.swipe_update(0., -10.);
    }
    assert!(
        f.synoik().layout.is_app_expose_open(),
        "mid-swipe it is up, as far as input goes"
    );
    for _ in 0..8 {
        f.advance_input_time(50);
        f.swipe_update(0., 10.);
    }
    f.advance_input_time(1);
    f.swipe_end(false);
    f.settle_animations();
    assert!(!f.synoik().layout.is_app_expose_open());
}

/// Down from the desktop with nothing focused has no app to show, and is swallowed: it does not
/// open the overview either.
#[test]
fn swiping_down_over_nothing_focused_does_nothing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    switcher_apps(&mut f);
    super::gnome::pointer_motion_to(&mut f, 960., 540.);

    touchpad_swipe(&mut f, 3, (0., -10.), 80, 50);
    assert!(!f.synoik().layout.is_app_expose_open());
    assert!(!f.synoik().layout.is_overview_open());
}

/// Down from the overview is still the overview's: it closes it, and does not carry on into App
/// Exposé.
#[test]
fn swiping_down_from_the_overview_only_closes_it() {
    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);
    super::gnome::pointer_motion_to(&mut f, 960., 540.);

    touchpad_swipe(&mut f, 3, (0., 10.), 80, 50);
    assert!(
        f.synoik().layout.is_overview_open(),
        "precondition: up opens the overview"
    );

    touchpad_swipe(&mut f, 3, (0., -10.), 200, 50);
    assert!(!f.synoik().layout.is_overview_open());
    assert!(!f.synoik().layout.is_app_expose_open());
}

/// A sideways swipe in App Exposé steps to the next app, one per swipe; the other way steps
/// back.
#[test]
fn a_sideways_swipe_steps_to_the_next_app() {
    let mut f = Fixture::new();
    let (one, two) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    super::gnome::pointer_motion_to(&mut f, 960., 540.);
    open(&mut f);
    let ws = f.synoik().layout.active_workspace().unwrap().id();

    // However far it goes, one swipe is one app.
    touchpad_swipe(&mut f, 3, (10., 0.), 80, 50);
    assert!(f.synoik().layout.is_app_expose_open());
    assert_eq!(shown(&mut f, &out), vec![two.clone()]);

    touchpad_swipe(&mut f, 3, (-10., 0.), 80, 50);
    assert!(same_set(shown(&mut f, &out), one));
    assert_eq!(
        f.synoik().layout.active_workspace().unwrap().id(),
        ws,
        "and it is not a workspace switch"
    );
}

const KEY_ENTER: u32 = 28;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;

/// The arrow keys pick a preview — the focused window's first, then its neighbours — showing it
/// as hovered, and Enter goes to it.
#[test]
fn arrows_pick_a_preview_and_enter_goes_to_it() {
    let mut f = Fixture::new();
    let (one, _) = one_display_two_workspaces(&mut f);
    let out = output(&mut f, "headless-1");
    open(&mut f);

    let overlaid = |f: &mut Fixture| -> Vec<Window> {
        f.synoik()
            .layout
            .monitor_for_output(&out)
            .unwrap()
            .preview_overlays()
            .into_iter()
            .map(|(w, _, _)| w)
            .collect()
    };

    tap(&mut f, KEY_RIGHT);
    f.settle();
    assert_eq!(
        f.synoik().app_expose_key_selection,
        Some(one[1].clone()),
        "the first press picks the focused window"
    );
    assert_eq!(overlaid(&mut f), vec![one[1].clone()], "shown as hovered");

    // Three previews in a grid: some arrow reaches another one.
    let picked = [KEY_RIGHT, KEY_LEFT, KEY_DOWN, KEY_UP]
        .into_iter()
        .find_map(|key| {
            tap(&mut f, key);
            f.settle();
            f.synoik()
                .app_expose_key_selection
                .clone()
                .filter(|sel| *sel != one[1])
        })
        .expect("an arrow moves the pick");
    assert_eq!(overlaid(&mut f), vec![picked.clone()]);

    tap(&mut f, KEY_ENTER);
    f.settle();
    assert!(!f.synoik().layout.is_app_expose_open());
    assert_eq!(focused(&mut f), picked);
}

/// `synoik msg input swipe-*` drives the same swipe a touchpad does, so a live session can be
/// walked into App Exposé and back out without one.
#[test]
fn an_injected_swipe_brings_it_up_and_takes_it_down() {
    use synoik_ipc::InjectedEvent;

    use crate::input::synthetic::inject;

    let mut f = Fixture::new();
    let _ = one_display_two_workspaces(&mut f);
    super::gnome::pointer_motion_to(&mut f, 960., 540.);

    let swipe = |f: &mut Fixture, dy: f64| {
        inject(f.synoik_state(), &InjectedEvent::SwipeBegin { fingers: 3 }).unwrap();
        for _ in 0..20 {
            inject(f.synoik_state(), &InjectedEvent::SwipeUpdate { dx: 0., dy }).unwrap();
        }
        inject(
            f.synoik_state(),
            &InjectedEvent::SwipeEnd { cancelled: false },
        )
        .unwrap();
        f.settle_animations();
    };

    // Natural scrolling is off on the synthetic device, so negative is towards App Exposé.
    swipe(&mut f, -20.);
    assert!(f.synoik().layout.is_app_expose_open());

    swipe(&mut f, 20.);
    assert!(!f.synoik().layout.is_app_expose_open());
    assert!(!f.synoik().layout.is_overview_open());
}
