# Minimize

GNOME's minimize (`meta_window_minimize`, `window.c:2734`; the shell's `Hide` row,
`windowMenu.js:40-45`). A minimized window is **alive, still ours, and out of the layout**: it
keeps its surface, its place in the switcher and the app system, and comes back to the workspace it
left.

## The model

`Workspace::minimized: Vec<RemovedTile<W>>` — the same record `remove_tile` produces for an
interactive move, parked on the workspace instead of carried by the pointer. That is what makes the
round trip exact: width, full-width and floating-vs-scrolling all ride the record, so unminimize is
a `put back`, not a re-place.

Minimizing goes through a **real** `remove_tile`, never a flag flip, so the focus fixup, the column
collapse and the workspace bookkeeping all happen the way they do for any other take-out.

The bucket lives on the `Workspace`, not on the `Layout`, because everything that walks workspaces —
`windows()`, `session_snapshot`, `has_windows_or_name` — must still see the window. `Workspace::tiles`
and `tiles_with_ipc_layouts` chain it for exactly that reason.

**`has_window` means *laid out here*; `holds_window` means *on this workspace at all*.** The ~60
`Layout` dispatchers that ask "which workspace owns this?" before doing something positional want
the first: a parked window has no column to collapse and no tile to animate. Removal and lookup want
the second.

`LayoutElement::is_minimized` is a **cache** the layout writes on the window; the bucket is the
truth. Nothing may set it directly.

### `Home`

Three places a window can be on a workspace, so `Workspace` dispatches on

```rust
enum Home { Scrolling, Floating, Minimized }
```

with `home_of` / `target_home` and **exhaustive matches, no catch-all** — the boolean this replaced
sent a minimized window down the scrolling path and panicked on an unwrap. A fourth home must not be
addable without the compiler naming every site.

## The four ways in

| Trigger | Reference |
|---|---|
| The window menu’s **Hide** row | `windowMenu.js:40-45` |
| `minimize` (`<Super>h`) | `handle_minimize`, `keybindings.c:2183` |
| A client's own minimize button, `xdg_toplevel.set_minimized` | `meta-wayland-xdg-shell.c` |
| wlr-foreign-toplevel `set_minimized` / `unset_minimized` | — |

`set_minimized` was the second silent drop of the same shape as `show_window_menu`: smithay's
`XdgShellHandler::minimize_request` defaults to a no-op, so a GTK window's own minimize button did
nothing at all.

**Activating unminimizes.** `activate_window` restores before it focuses (`window.c:3908`), so
the switcher, the dash and an activation token all bring a hidden window back without knowing about
minimize.

## State surfaces

Every consumer that answers "what windows are there, and can you see them" carries the bit:

- **IPC** `Window.is_minimized`, read off `Mapped` (not off an acked configure) and in the
  change-detection chain.
- **wlr-foreign-toplevel** — the `Minimized` state in the array, alongside focus, in `ShellState`.
- **The switcher** keeps minimized windows in the list, sorted last: key `(minimized,
  Reverse(focus_timestamp))`.
- **The app system** — `RunningWindow.minimized`, and `RunningApp::is_minimized()` is *all* windows
  minimized (`shell_app_is_minimized`).
- **Session save/restore** — the record carries it, and a restored window is re-minimized after it
  maps.
- **`is_hidden`** now means minimized *or* on an inactive workspace.

## The shrink

400ms on `EASE_OUT_EXPO` — gnome-shell's `MINIMIZE_WINDOW_ANIMATION_TIME` and
`..._MODE` (`windowManager.js:28-29`).

**One rect does both motions.** The window shrinks into `Parked::dest` on the desktop, and the
picker grows the preview back out of that same rect, so the overview cannot contradict what the
desktop just showed. `dest` is `None` for a window that was never seen to go anywhere — a session
restore parks one that was never on screen, and a minimize with the overview already up has no
desktop to cross — and then there is no shrink and the picker falls back to the layout input.

**Where it goes — a divergence.** gnome-shell aims at `meta_window_get_icon_geometry`, and at the
monitor's top-left corner at scale 0 when nothing set one (`_minimizeWindow`,
`windowManager.js:1178-1197`). Nothing in gnome-shell 50.3 ever calls `set_icon_geometry`, so its
shipped behavior is always that corner. We have a dock, which is what icon geometry was for: the
window aims at its own app icon there, or at the dock's home edge (the bottom centre of the work
area) when the app is not in the dash or the dock is slid away. `State::minimize_destination`
resolves it and passes it in — the layout never learns about the dock.

The destination is clamped to `MIN_DEST_SIZE`. The picker divides by the from-rect's width, so a
zero-sized destination reproduces the blank-preview defect of `b8078c6f`.

**Cancellation is structural.** Unminimizing removes the whole `Parked` entry, so a shrink
interrupted by an activation goes with it; there is no half-state to unwind.

## The overview

GNOME's split, and ours: the **window picker shows** minimized windows — `_isOverviewWindow` is
`!win.skip_taskbar` with no minimized check (`workspace.js:1332`) — while the **thumbnail strip
hides** them, its own `_isOverviewWindow` adding `showing_on_its_workspace()`
(`workspaceThumbnail.js:461-463`), which is false when minimized; both `_addWindowClone` sites
connect `notify::minimized` to add and remove clones live (`:275`, `:374`).

A parked tile can produce the picker's layout input without any new state. The input is
`(stable_sequence, rect)` where `rect` is the settled position plus the tile size
(`Workspace::expose_live_inputs`), and removal stamps both onto the tile:
`floating_pos` (a size-fraction of the working area) and `floating_window_size`
(`FloatingSpace::remove_tile_by_idx`). Those are the same fields `stored_or_default_tile_pos`
reads back to put the window where it was, which is what makes unminimize exact.

Because the rect comes from the position the tile *had*, minimizing with the overview open leaves
the picker's input unchanged and the grid does not shuffle. The one place the two can disagree is
off-screen windows: `Data::recompute_logical_pos` clamps the fraction to keep a window mostly
on-screen and `stored_or_default_tile_pos` does not.

The picker is GNOME-mode-only and GNOME mode keeps every window in the floating layout, so there is
no scrolling-layer case with no stored position.

`Workspace::expose_layout` is the single seam: it chains the parked tiles into both the layout
inputs and the render list, and every picker query — slots, hit-testing, hover, the close button,
activation — reads through it, so nothing else needed a minimized case.

A parked tile has no rect on screen to interpolate from, so on the open/close leg it uses the same
rect it is laid out over — the one it had. gnome-shell instead gives a window that is not
`showing_on_its_workspace` the work-area origin at **zero size** (`workspace.js:709-720`) and fades
it in over that; ours cannot, because this leg interpolates a *scale* (`slot.size.w / rect.size.w`,
`expose_tile_render`), so a zero-width `from` divides by zero and the preview never draws at any
progress. Reaching zero size means having the opacity ramp that hides the degenerate scale, which
is the fade below.

**`has_window` is the wrong question for the picker.** It means *laid out here*, so all five
`Layout::expose_*` dispatchers, the touch tap-to-activate grab and `window_workspace_position` ask
`holds_window` instead. A picker query that asks the positional question finds nothing and returns
a **vacant** slot — a zero rect, not `None` — which reads as a real answer to any check that only
tests for overlap.

## Backlog: a minimized strip instead of picker slots

Preferred over GNOME's behavior (kov, 2026-08-21): show minimized windows on a **smaller
affordance** — a preview strip across the bottom of the workspace area — rather than as
full picker slots, the way macOS keeps minimized windows in the Dock rather than in Exposé.

Deferred because it is **more** work than folding them into the picker, not less:

- `overview_layout::layout` is pure geometry, so the strip is a new `Measured` height, a new
  `ControlsLayout` box, and a subtraction from the picker box — cheap and pinnable, but it makes
  the picker box **depend on whether any window is minimized**. Minimizing with the overview open
  would then relayout the whole picker, which is exactly the shuffle that folding avoids.
- The strip needs its own render path, hit-testing and click-to-restore. Folding needs none: a
  minimized window that is an ordinary picker entry gets slots, hover, the close button and
  activation for free.
- It competes with the dash for the bottom of the work area.

What carries over when it is built: the preview chrome (`ui::window_preview::PreviewChrome`
takes a rect), the tile-into-slot scaling, and the synthesized-rect reading above. What is thrown
away is small. The strip **replaces** the picker slots when it lands — it is not additive.

## Divergences, for now

- **The workspace row shows minimized windows; GNOME's thumbnail strip hides them.** GNOME's strip
  is a positional miniature of the real workspace, so a hidden window has nothing to draw there.
  Ours renders each row entry through the same `render_expose` the picker uses, off the same
  retained layout decision — so excluding them would mean deciding two different grids per
  workspace and the row disagreeing with the picker it miniaturizes.
- **No fade, so no growth from a corner either.** GNOME ramps a minimized preview's opacity with
  the overview state (`_syncOpacity`, `workspace.js:448-451`), which is what lets its geometry
  start at zero size. The two go together: without the ramp, ours has to interpolate from a
  non-degenerate rect. Landing the fade is what would let the growth follow.
- **Unminimize animates, but not as the mirror.** gnome-shell's `_unminimizeWindow` runs the
  *same* ease backwards, growing the window out of the icon geometry
  (`windowManager.js:1222-1260`). Ours reuses the window-open effect instead, so something happens
  rather than the window popping back into existence, but it does not come out of
  [`Parked::dest`]. The true mirror needs a tile that is **in** the layout and drawn scaled, and
  the normal render path has no way to do that — the shrink can only be drawn because a parked
  tile is outside both halves. Closing this means giving `Tile` a from-rect the layout render
  honors, which is also what the picker's missing fade would use.
- **The shrink has no fade.** gnome-shell eases opacity alongside the scale and position; ours is
  the geometry only, so the window is still faintly visible at destination size when it stops
  being drawn. The smaller the destination, the less this shows.
- **Minimized windows still get frame callbacks.** `windows_for_output_mut` reaches them through
  `Workspace::tiles_mut`, so a hidden window keeps drawing. Convenient — the picker preview has a
  live texture — but it is work GNOME throttles.
