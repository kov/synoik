# The window menu

gnome-shell's `WindowMenu` (`js/ui/windowMenu.js`) — the menu a right-click on a titlebar pops up.
Ours is `src/ui/window_menu.rs` (which rows, what each does) over `widget::Menu` (box model,
layout, hit-testing, painting), hosted by `PanelPopover` like every other menu.

## The three ways in

| Trigger | Reference | Anchor |
|---|---|---|
| A CSD client's titlebar right-click, arriving as `xdg_toplevel.show_window_menu` | `xdg_toplevel_show_window_menu`, `meta-wayland-xdg-shell.c:293-315` | the client's point, measured from the **buffer** origin |
| `activate-window-menu` (`<Alt>space`) | `handle_activate_window_menu`, `keybindings.c:1999-2021` | the window's top-left |
| Mod+RMB, mutter's passive button grab | `window.c:7743-7844` | the pointer |

On Wayland the titlebar belongs to the client, so the first one is the path that matters: the
toolkit recognizes the click itself and asks us for the menu. Dropping that request is what "right
clicking does nothing" was — smithay's `XdgShellHandler::show_window_menu` defaults to a no-op and
we never overrode it.

**The client's point is buffer-relative.** mutter adds it to `buffer_rect`, not to the geometry
rect, so a GTK window's invisible shadow margin counts: the anchor is
`geometry_rect.loc - geometry().loc + point`. Skipping the subtraction hangs the menu the width of
the shadow away from the click.

**The serial is not validated by smithay.** mutter checks the request against the seat's recorded
grab serial (`meta_wayland_seat_get_grab_info`) and drops stale ones; smithay passes it straight
through. The gate is focus instead — only the window the keyboard is on may summon its menu, the
same rule `XdgShellHandler::grab` applies to a toplevel popup grab. Without it a background client
takes the modal grab out from under whatever the user is using.

## Rows

Built in GNOME's order, each shown only when its target exists — the neighbour checks
`workspace.get_neighbor(dir) !== workspace` and `get_monitor_neighbor_index(…) !== -1`
(`windowMenu.js:110-181`) make:

- **Take Screenshot** — the window's own pixels, stored the way a keypress stores one (file,
  clipboard, notification) and without the pointer: `captureScreenshot(texture, null, 1, null)`
  passes a null cursor and a null geometry (`windowMenu.js:26-36`). Shares one capture with the
  `screenshot-window` action.
- **Hide** — minimize: the window leaves both layouts and is parked on its workspace, alive and
  still ours, so it stays in the switcher, the app system and every state surface. See
  `docs/fork/minimize-port.md`.
- **Maximize / Restore** — one row, whichever the window is not. Reads the *pending* sizing mode:
  `window.is_maximized()` is the compositor's own state, which flips when the request is made, not
  when the client acks the configure.
- **Move / Resize** — the keyboard grabs (`windowMenu.js:58-84`), insensitive when the window
  does not own its own geometry, which is `allows_move` / `allows_resize`. See "The keyboard
  grabs" below.
- **Always on Top** — `make_above` / `unmake_above` (`windowMenu.js:86-98`), ticked with
  `Ornament::Check` when set. Drawn **insensitive while the window is maximized**, which is not a
  nicety: a maximized window is in the normal layer even with the flag set
  (`meta_window_get_default_layer`, `window.c:6416-6432`), so an enabled row would promise an
  effect it does not have. gnome-shell's three other disabling cases are X11 window types that
  xdg-shell has no equivalent of. See "The always-on-top band" below.
- **Always on Visible Workspace** — `stick` / `unstick` (`windowMenu.js:105-114`), ticked when
  set. gnome-shell's one disabling case, `is_always_on_all_workspaces()`, is a window type
  xdg-shell has no equivalent of. See "Sticky windows" below.
- **Move to Workspace Left / Right** — GNOME's horizontal workspace axis is our vertical one, the
  same mapping `move-to-workspace-left` → `MoveWindowToWorkspaceUp` already uses. **Not shown for
  a sticky window**: it is on every workspace already, so there is nowhere to move it to, which
  is why gnome-shell builds these rows inside `if (!isSticky)` (`windowMenu.js:116`).
- **Move to Monitor Up / Down / Left / Right**.
- **Close**.

Every row acts on the window the menu was summoned on, not on the focus: gnome-shell's items close
over `window`, and a menu can outlive the focus that opened it. A menu whose window is unmapped
closes with it (`windowMenu.js:235-237`) — left open it would hold the modal grab over whatever
the focus fell back to.

The menu comes up with its first row focused (`navigate_focus TAB_FORWARD`, `windowMenu.js:247`),
and takes Up/Down/Tab, Enter/Space and Escape. It is the only popover with a keyboard way in, which
is why it is the only one with key navigation; the pointer-summoned menus (app, indicator) get it
when they get a keyboard trigger. Every row gnome-shell builds is built.

## Sticky windows

`stick` / `unstick` (`window.c:5333-5359`), a **carry**: the window is moved onto whichever
workspace you switch to, rather than drawn on all of them. That keeps the single-owner answer to
"which workspace holds this window" — the `has_window` / `holds_window` pair — which a
draw-everywhere model would give two answers to.

The carry runs at the top of `Monitor::activate_workspace_with_anim_config`, before anything else
moves: it reads the *outgoing* workspace's active window to decide whether the focus travels
along, so a sticky window you were using is still the focused one when the switch lands. A
carried window can empty the workspace it left, and the dynamic-workspace cull then reindexes, so
the target is re-found by `WorkspaceId` afterwards.

**Both ways of switching carry.** `workspace_switch_gesture_end` sets the active workspace itself
rather than going through `activate_workspace`, so a three-finger swipe needs the carry of its
own; without it a swipe strands the window. Nothing is culled underneath that one — a
`move_to_workspace` skips the cleanup while a switch is running — so the landing index stays
valid across it.

**The flag is on the tile**, like `is_above`, so it rides `RemovedTile` through a minimize or a
workspace move. **Membership is derived**: a window is carried if it is flagged *or* any ancestor
of it is. mutter stores the flag on each transient at stick time (`stick_foreach_func`), which
misses the dialog that maps after the stick; deriving it does not. Minimized windows are left out
— they are parked rather than laid out, and there is nothing on screen to carry.

**Unsticking sends the window home**, to the workspace it was stuck on, and can therefore take it
out of the view you are in. That is the behavior, not a bug: mutter deliberately leaves
`window->workspaces` untouched across a stick so the revert is exact (`window.c:5279-5299`). The
home is a `WorkspaceId`, never an index — dynamic workspaces cull, and an index would name a
different workspace by the time it was read. A home that no longer exists is no home, and the
window stays put. Whatever rode along with the window goes home with it: the set that leaves is
the set that stopped being derived-sticky.

**Divergences.** GNOME draws a sticky window in *every* workspace preview in the overview
(`located_on_workspace` is true everywhere); the carry draws it once, on the active workspace.
And during the switch animation gnome-shell holds sticky windows in a group that does not move
(`workspaceAnimation.js`), while ours slides out with the old workspace and back in with the new.
Sticky is not saved across a session restore, where minimized is; it is a "for now" state.

## The keyboard grabs

Move and Resize are one state machine, `src/input/keyboard_window_grab.rs`, driving a **virtual
pointer**: the arrows walk a delta and the layout's own interactive move and resize see the
numbers a real drag would give them. mutter's is `process_keyboard_move_grab` and
`process_keyboard_resize_grab` (`meta-window-drag.c:614-1070`), reached from `begin-move` /
`begin-resize` or from the menu rows.

- **The step is ten pixels, one under Ctrl**, and Shift — mutter's snap mode — also steps by one.
- **Releases and modifier presses are eaten and keep the grab.** Reaching for Ctrl to slow the
  step down must not cancel the drag.
- **Escape restores and ends**; any other key commits and ends. Either way the key is swallowed.
- **A resize starts with no edge** (`RESIZING_UNKNOWN`): the first arrow picks one and resizes
  nothing, and an arrow across the chosen edge's axis moves the grab to that edge, also resizing
  nothing. Only arrows along the current edge's axis drag it. Each edge change ends the layout's
  interactive resize and begins a new one, so the size it measures from is always current.
- The grab also ends on a pointer button press and when its window goes away. mutter needs
  neither rule because its keyboard grab holds the pointer too; ours does not, so without the
  first a click on another window would leave the grab driving an unfocused one.

Both are refused for a window that does not own its own geometry — mutter's `has_move_func` /
`has_resize_func` — which is the same gate the menu rows are dimmed by, so an enabled row always
does something. Floating only, for the reason the band is.

**Divergences.** Shift is only the smaller step: mutter's snap flag also turns on edge
resistance against other windows and the work area, which we have no model for. And mutter's
keyboard move is unconstrained but still snaps to a nearby edge when the increment would
overshoot it; ours walks the plain increment.

## The always-on-top band

`FloatingSpace::tiles` is the stacking order, topmost first, and always-on-top is a **partition**
of it: the flagged windows occupy a prefix, the ordinary ones the rest. Every raise clamps to its
own side of the boundary (`raise_target`), which is `meta_stack_raise` under mutter's layer
constraints — an ordinary window activated over an always-on-top one rises only as far as the
band's floor.

Three things follow from mutter that are easy to get wrong:

- **Both directions raise.** `meta_window_make_above` and `meta_window_unmake_above`
  (`window.c:3622-3639`) each end in `meta_window_raise`. Unmaking leaves the window at the top of
  the *normal* band rather than dropping it wherever the boundary falls.
- **The raise does not activate.** Making a window always-on-top while another has the focus
  leaves the focus alone; mutter's `always-on-top.metatest` asserts exactly that.
- **`raise-or-lower` uses two different scopes.** "Is it on top" asks the whole stack
  (`meta_stack_get_top`); "is it covered" asks only the window's own band (`meta_stack_get_above`
  with `only_within_layer`). With one scope a normal window under an always-on-top one would raise
  forever and never come back down.

**Membership is derived, never stored.** A tile is in the band if it is flagged, or if any
ancestor of it is — so the boundary cannot come down between a dialog and its parent. And a
*maximized* window is out of the band while it is maximized, flag and all, which is the rule the
menu row's insensitivity reflects.

Because membership is derived from a flag *and* a sizing mode *and* the transient chain, it can
change under code that never touches the stacking order — maximize is the obvious one. So the band
is **re-established, not maintained**: `resettle_band` is a stable partition, idempotent and free
on the common path, run once per frame and immediately after a flag change. `verify_invariants`
asserts the band is a prefix, so a path that manages to break it fails loudly.

Floating only, and that is not a gap: in GNOME mode every window is floating, and niri's scrolling
layout has no stacking order for a window to be on top *of*.

**Not ported: the map-time focus rule.** mutter denies focus to a window that would map with 40%
or less of itself visible under the always-on-top windows
(`window_would_mostly_be_covered_by_always_above_window`, `window.c:2213-2246`, checked at
`:2476-2480`). It needs the window's *placed* rect, and mutter runs `force_placement` before the
check; ours decides focus above the layer that decides position, so the predicate has no rect to
ask about yet. It is a focus-policy rule rather than part of the stacking model, and wiring it at
the wrong seam would apply it to unminimize and workspace moves as well as to a first map.
