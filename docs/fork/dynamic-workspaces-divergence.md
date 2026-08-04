# Dynamic workspaces: manual close, one always-on workspace row

**Status: implemented 2026-08-03.** Approved by Gustavo in the session that wrote it, with three
sub-decisions taken up front (named workspaces stay un-closable; closing animates the strip
closed; niri's `empty-workspace-above-first` goes).

**Validated on the headless harness** the same day: a fresh session comes up with two workspaces
and the strip already showing; emptying a desktop leaves it in place; its thumbnail draws the
close button inset in its top-right corner; clicking it dismisses the desktop, closes the strip
up to two, and leaves the overview open. (Judge *structure* from IPC there, never window
contents — GPU clients composite empty headless.)

Three changes, only two of which are divergences.

## 1. Two workspaces at startup — *not* a divergence, a missed behavior

gnome-shell's `WorkspaceTracker._checkWorkspaces` enforces `MIN_NUM_WORKSPACES = 2`
(`js/ui/windowManager.js:42`) in two places: it appends until the count reaches 2
(`:273-276`), and the reap loop breaks the moment the count is back down to 2 (`:286`). A fresh
GNOME session therefore has **two** workspaces, not one.

Ours had one: `Monitor::new` pushed exactly one trailing empty workspace and nothing set a floor.
`MIN_NUM_WORKSPACES` now lives in `src/layout/monitor.rs` and is enforced in `Monitor::new` and
in `clean_up_workspaces`, in gnome-shell's order: ensure a trailing empty, then pad to the
minimum.

This is invisible on its own — GNOME hides the strip at 2 workspaces — which is why it went
unnoticed until (2).

## 2. DIVERGENCE — the thumbnail strip is always shown

gnome-shell's `ThumbnailsBox._updateShouldShow` (`js/ui/workspaceThumbnail.js:697-706`) shows the
strip only when `nWorkspaces > NUM_WORKSPACES_THRESHOLD` (2, `:16`), and eases `expandFraction`
0↔1 as the count crosses that line. Since the trailing empty always counts, GNOME's strip appears
only once a *second* desktop is populated.

We always show it. The row is the desktop switcher; one that appears and disappears as a
side effect of what you happen to have open is not one you can aim at, and with (3) the count now
reflects a deliberate user choice rather than a transient. `Monitor::thumbnails_visible` is now
just "is the overview open", and the count-crossing ease is gone — as is
`ui::overview_layout`'s `expand_fraction` parameter, which had nothing left to interpolate.

## 2b. DIVERGENCE — the strip and the app-grid row are the *same* row

**Approved 2026-08-03.** gnome-shell has two unrelated rows of workspaces in the overview: the
`ThumbnailsBox` strip in the window-picker state (`MAX_THUMBNAIL_SCALE`, 5% specks, raw window
positions, its own band under the search entry), and the window picker *itself* shrunk into
`_computeWorkspacesBoxForState(APP_GRID)` in the app-grid state (`SMALL_WORKSPACE_RATIO`, 15%,
exposé previews, full width). The show-apps transition cross-fades one into the other:
`overviewControls.js:512-548` eases the strip's opacity to 0 while `fitModeAdjustment` slides the
picker down into the row's place.

Ours is one row, `ControlsLayout::workspace_row`, drawn identically in both states:

- **One box.** Full width, top on the search puck's midline
  (`overview_search::ENTRY_CONTROL_MID_Y`), one `small_workspace_height` tall. It does not depend
  on the overview state, so the show-apps transition never moves it.
- **One layout.** `thumbnails::strip_geometry` is gnome-shell's fit-all row
  (`_getFirstFitAllWorkspaceBox`) — run centered, `WORKSPACE_MIN_SPACING` between, scrolling to
  follow the active workspace once it overflows.
- **One content.** `Monitor::render_thumbnails` draws `Workspace::render_expose` — the picker's
  own spread previews — over the same rounded wallpaper, at the row's zoom.
- **One shadow.** The workspace shadow every picker workspace casts, through the miniature's own
  transform. The **active** workspace gets that shadow in the system accent color
  (`workspace::accent_workspace_shadow_config`), which is our replacement for gnome-shell's
  `.workspace-thumbnail-indicator` border ring. The window picker's big workspace does *not* wear
  it: there the active one is already the centered, whole one.
- **One set of affordances.** Reorder-by-drag and dismiss-an-empty-desktop are the row's, so they
  work in the app-grid state too — pinned by
  `the_workspace_row_closes_and_reorders_in_the_app_grid_too`.

The picker therefore has nowhere to travel to on the `WINDOW_PICKER → APP_GRID` leg: it keeps its
own box and simply **fades away** over the row that is already there (`row_alpha` vs
`picker_alpha` in `Niri::render`). Two pieces of machinery went with that trip — the fit-single ↔
fit-all blend in `Monitor::workspaces_strip_axis`, and `open_fraction`'s saturation across the
app-grid leg, which existed only to park the zoom while the row re-fitted.

The point is a UX one, Gustavo's: *the user cannot tell the strip and the app-grid workspaces
apart*, because there is nothing to tell apart.

## 3. DIVERGENCE — empty workspaces are closed by hand, macOS-style

gnome-shell reaps: `_checkWorkspaces` removes every empty workspace that is not the active one
and not the last (`windowManager.js:278-291`), so closing your last window on a desktop makes
that desktop vanish and renumbers everything after it. That is the behavior we are dropping.

Instead, an emptied workspace **stays**, and grows a close button on hover in the overview —
Mission Control's model. The consequences:

- **Workspace indices are stable.** `Super+3` keeps meaning the same desktop across a day of
  opening and closing windows, which is the actual point of the change.
- `clean_up_workspaces` no longer reaps. It keeps only the invariants the reaper also
  maintained: a trailing empty workspace, and at least `MIN_NUM_WORKSPACES` of them. Every call
  site is unchanged — the policy moved, not the plumbing.
- The layout invariant "no non-last non-active empty workspace" is **deleted**
  (`Monitor::verify_invariants`). Its violation is now the feature.

### What is closable

`Monitor::workspace_is_closable`: windowless, **unnamed**, not the last workspace, and not if
closing it would drop below `MIN_NUM_WORKSPACES`.

- *Unnamed*: naming a workspace is how you say you want it kept — it is already what made a
  workspace un-reapable (`has_windows_or_name`). A named empty workspace shows no close button.
- *Not the last*: the trailing empty is re-appended the instant it is removed, so a close button
  there would be a no-op that flickers.
- *Not below the minimum*: same reason GNOME's reap loop breaks at 2.

A fresh session therefore shows two thumbnails, neither closable. That is intended: the second
desktop is scratch space, not clutter to tidy away.

### The button

Geometry in `layout::thumbnails::close_rect`, paint in `ui::thumbnail_chrome`. It is
`widget::IconButton` — the toolkit's circular glyph button — so its hover wash and its *round*
hit test are the ones every other icon button in the shell uses, and it carries the window
picker's `preview-close-symbolic` glyph so the two "dismiss this" affordances in the overview
read as one control at two sizes.

Two things it does differently from the window preview's close button
(`ui::window_preview::close_rect`), both forced by the strip:

- **Inset, not corner-centred.** A preview's button half-overhangs its top-right corner
  (`windowPreview.js:203-218`). The row clips everything it draws to its band, and the band is
  exactly one thumbnail tall, so an overhanging button would be sliced along its top edge — and
  the half sticking out would not be hittable either, since the hit test clips to the same band.
- **Ramped, not fixed at 32px.** Thumbnails are a fraction of the work area, so they shrink a
  long way on a small canvas; the button follows via `overview_layout::chrome_ramp`, the same
  factor the strip's own spacing uses.

The press is tested **before** `ThumbGrab` takes it (`input/mod.rs`): the button sits inside its
thumbnail's body, so the reorder drag would otherwise swallow every click aimed at it.

### Closing animates; showing does not

Dismissing a desktop eases the survivors into the gap over 250ms (`CloseSlide`,
`layout/monitor.rs`). The workspace itself is removed from the model *immediately* — nothing may
keep a removed workspace alive for an animation's sake, or every index in the layout is a lie for
a quarter second. What is animated is only where the survivors are *drawn*: the row is laid out
twice, at the old count and the new, and the thumbnails interpolate between the two.

Since 2026-08-03 the row centers the *run* rather than the active workspace (fit-all, see 2b), so
a close shifts everything after the gap and re-centers what is left.

**Known gap:** the button itself pops in and out with the hover rather than fading, unlike the
window preview's, whose alpha rides the picker's hover state. A per-workspace fade wants an
animation per thumbnail; deferred until it actually reads badly on the seat.

## Accepted losses

**Empty workspaces do not survive their output going away.** `Monitor::into_workspaces` and
`Layout::add_output` both filter to `has_windows_or_name()` when migrating workspaces between
monitors, so unplugging a display drops its empty desktops rather than piling them onto the
primary. Keeping them would mean a monitor that is plugged and unplugged repeatedly accumulates
anonymous empties on whichever output is left. The persistent-desktop promise is a within-session,
within-output one.

**niri's `empty-workspace-above-first` is gone** (config field, ~50 special-case sites, its
tests, and its wiki section). It is niri's way of doing workspaces, GNOME has no equivalent, and
it complicated every invariant this change touches. Two of its tests — `add_and_remove_output`
and `move_window_to_different_output` — were generic invariant checks that merely happened to set
the flag; they were kept, flagless.
