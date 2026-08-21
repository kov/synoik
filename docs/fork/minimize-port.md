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
desktop just showed.

**Where a hidden window lives is not the same question as whether the user watched it go there.**
`dest` is always known; `animate` is separate and says only whether the desktop shrink runs. A
session restore parks a window that was never on screen and a minimize with the overview up has no
desktop to cross — neither shrinks, but both still grow out of the dock when the picker opens.
Answering the first question with "nowhere" made a restored window appear at full size in a place
it had never been.

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

**The picker's from-rect is always the tile's natural size.** The draw scale is derived from it
(`slot.size.w / rect.size.w`) and then applied to the tile's own natural-size elements, so a rect
sized as anything else scales the window by exactly that ratio. Pointing it at a dock-icon-sized
destination drew a window some forty times too big, one corner of it covering the workspace, while
every position assert stayed green. A preview that starts somewhere it was never drawn says so
with a separate **from-scale** — the scale it has at progress 0 — not by lying about its size.

A parked tile has no rect on screen to interpolate from, so on the open/close leg it uses the
destination's *position* at natural size, with the destination's own scale as its from-scale: it
grows out of the place the window went. Without a destination it uses the rect it is laid out over. gnome-shell instead gives a window that is not
`showing_on_its_workspace` the work-area origin at **zero size** (`workspace.js:709-720`) and fades
it in over that; ours does not, because this leg interpolates a *scale* (`slot.size.w / rect.size.w`,
`expose_tile_render`), so a zero-width `from` divides by zero and the preview never draws at any
progress. A dock icon is a real rect and a real place, so the preview has somewhere honest to come
from without needing a per-preview opacity ramp — which on this leg would cost an offscreen per
preview per frame.

**`has_window` is the wrong question for the picker.** It means *laid out here*, so the five
`Layout::expose_*` dispatchers, the touch tap-to-activate grab, `window_workspace_position`,
`set_expose_hover` and `expose_hovers_a_live_preview` all ask `holds_window` instead. The two hover
ones were missed in the first sweep and cost a minimized preview both its growth and its close
button, since each is gated on the hover being armed. A picker query that asks the positional question finds nothing and returns
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

## The two halves of the animation

Minimize and unminimize are one mechanism, mirrored — gnome-shell's `MINIMIZE_WINDOW_ANIMATION_TIME`
and `_MODE` (`windowManager.js:28-29`) are shared by `_minimizeWindow` and `_unminimizeWindow`. Both
ease scale, position **and** opacity together, and so do we: `minimize_animation_config()` is the
one clock all four animations run on, because an animation whose halves land at different times
reads as two animations.

Geometry and opacity ride different mechanisms, because the tile is in a different place on each
leg:

- **Out.** The parked tile is outside both layout halves, so `Workspace::render_minimizing` draws
  it itself, wrapping the tile's elements in a rescale. Its opacity is the tile's own
  `alpha_animation` — legal at a non-1 target only because the visible-tile invariant walks
  `tiles_with_render_positions()`, which parked tiles are not in.
- **Back.** The returning tile is **in** the layout from the first frame — focusable, raisable,
  hit-testable — and only its drawing comes from elsewhere. That is `Tile::grow_animation`: the
  space that positions the tile asks `grow_transform(pos)` and rescales, and the target is read
  fresh each frame, so a tile relaid out mid-flight still lands on its real place. Opacity is
  `alpha_animation` again, to 1, which the invariant allows.

The transform is deliberately **not** applied inside `Tile::render`. The picker derives its own
scale from the tile's natural size and applies it to natural-size elements, so a transform baked
into the tile would compose with the picker's and draw the window at the product of the two — the
forty-times-too-big class above, reinstated.

**Parked tiles are advanced by `Workspace::advance_animations` explicitly.** They are in neither
half, so nothing else walks them: without it the shrink's fade is never cleared and every picker
preview draws at the alpha the shrink ended on, which is zero.

## Divergences, for now

- **The workspace row shows minimized windows; GNOME's thumbnail strip hides them.** GNOME's strip
  is a positional miniature of the real workspace, so a hidden window has nothing to draw there.
  Ours renders each row entry through the same `render_expose` the picker uses, off the same
  retained layout decision — so excluding them would mean deciding two different grids per
  workspace and the row disagreeing with the picker it miniaturizes.
- **The picker preview does not fade with the overview state.** GNOME ramps a minimized preview's
  opacity (`_syncOpacity`, `workspace.js:448-451`), which is what lets its geometry start at zero
  size. Ours starts at the dock rect instead — a real place, and the one the desktop shrink just
  used — so the ramp buys only the last few pixels of the growth, at the price of an offscreen per
  preview per frame. Not worth it; the growth is what the eye follows.
- **`Tile::grow_animation` is honored by the floating half only.** In GNOME mode every window is
  floating, so the scrolling half has no returning tile to draw. Wiring it there is a copy of the
  same six lines if that ever changes.
- **Minimized windows still get frame callbacks.** `windows_for_output_mut` reaches them through
  `Workspace::tiles_mut`, so a hidden window keeps drawing. Convenient — the picker preview has a
  live texture — but it is work GNOME throttles.
