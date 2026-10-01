// SPDX-License-Identifier: GPL-3.0-only

//! App Exposé (`docs/fork/app-expose.md`): a divergence from GNOME, pinned the way the
//! conformance corpus pins a port — through `do_action` and real input against the fixture.

use smithay::desktop::Window;
use smithay::output::Output;
use synoik_config::Action;

use super::fixture::Fixture;
use super::gnome::{map_window_for_app, switcher_apps};

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
