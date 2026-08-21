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

- **Maximize / Restore** — one row, whichever the window is not. Reads the *pending* sizing mode:
  `window.is_maximized()` is the compositor's own state, which flips when the request is made, not
  when the client acks the configure.
- **Move to Workspace Left / Right** — GNOME's horizontal workspace axis is our vertical one, the
  same mapping `move-to-workspace-left` → `MoveWindowToWorkspaceUp` already uses.
- **Move to Monitor Up / Down / Left / Right**.
- **Close**.

Every row acts on the window the menu was summoned on, not on the focus: gnome-shell's items close
over `window`, and a menu can outlive the focus that opened it. A menu whose window is unmapped
closes with it (`windowMenu.js:235-237`) — left open it would hold the modal grab over whatever
the focus fell back to.

The menu comes up with its first row focused (`navigate_focus TAB_FORWARD`, `windowMenu.js:247`),
and takes Up/Down/Tab, Enter/Space and Escape. It is the only popover with a keyboard way in, which
is why it is the only one with key navigation; the pointer-summoned menus (app, indicator) get it
when they get a keyboard trigger.

## Not built

Each of these is missing a *subsystem*, not a per-window capability, so they are omitted rather
than drawn permanently insensitive: GNOME dims a row when `can_minimize()` is false for *this*
window, and a row that could never enable teaches the reader nothing. In the order they appear in
`_buildMenu`:

| Row | Needs |
|---|---|
| Take Screenshot | per-window capture — `window.paint_to_content()` into the screenshot pipeline (`windowMenu.js:26-36`) |
| Hide | minimized window state in the layout |
| Move | keyboard interactive-move grab (`Meta.GrabOp.KEYBOARD_MOVING`) |
| Resize | keyboard interactive-resize grab (`Meta.GrabOp.KEYBOARD_RESIZING_UNKNOWN`) |
| Always on Top | a stacking model — `make_above` / `unmake_above` |
| Always on Visible Workspace | sticky windows — `stick` / `unstick` |

The same six are the `deferred` rows in `docs/fork/keybindings-port.md` (`minimize`,
`begin-move` / `begin-resize`, `always-on-top` / `toggle-above`, `toggle-on-all-workspaces`);
landing a subsystem there adds its row here, and the ornament (`Ornament::Check`) the two toggles
want is already in `widget::Menu`.
