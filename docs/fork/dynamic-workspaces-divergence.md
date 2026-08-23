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
`picker_alpha` in `Synoik::render`). The fit-single ↔ fit-all blend in
`Monitor::workspaces_strip_axis` went with that trip.

### The two legs must be traversed in order

Every app-grid blend reads `Monitor::app_grid_leg` — the state-derived
`WINDOW_PICKER → APP_GRID` leg — and **not** the raw `app_grid_fraction`. The raw scalar is
deliberately *frozen* across a close (the overview must not animate the grid shut and close at the
same time), so a blend that reads it directly sees "grid fully in" for the entire close.

That was the shape of the jarring close Gustavo reported on 2026-08-03: the grid stayed up, the
picker behind it stayed at `alpha 0`, and the whole return to the desktop — the previews
un-spreading, the workspace zooming out — ran invisibly behind it before the desktop popped in.

The other half is ordering. `Monitor::open_fraction` **saturates at 1 across the app-grid leg**,
so a close from the grid parks the zoom while the grid slides out and only zooms into the active
workspace once it is gone; `expose_progress` rides the same leg, so the previews stay spread until
then. This is what gnome-shell's single 2 → 0 adjustment gives for free by passing *through*
`WINDOW_PICKER`. (The saturation was briefly removed earlier the same day, when the picker still
travelled on this leg and it bought a dead zone; with the picker parked there is nothing to bend,
and the ordering is the whole point.) Pinned by
`overview_close_from_the_app_grid_unwinds_the_grid_before_it_zooms`.

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

### The row opens for a new workspace *during* the drag (2026-08-11)

gnome-shell moves nothing while you drag a window over the strip: it shows a fixed-width
`.placeholder` pill at the insertion point (`_dropPlaceholderPos`, `workspaceThumbnail.js:1352-1390`),
and only *after* the drop does the new thumbnail expand — `acceptDrop` puts it in
`ThumbnailState.NEW` with `collapse_fraction = 1, slide_position = 1` (`:888-930`) and
`_updateStates` runs two sequential 200ms `EASE_OUT_QUAD` eases, collapse then slide (`:1144-1181`).
So the row snaps open under a drop that has already happened.

Ours runs the same two stages, but the **drag** drives them instead of a timer: the reveal is a
function of how close the pointer has come to the trailing workspace, the slot widens over the
first half of the approach and the workspace materializes over the second. A drag that wanders off
eases the slot shut over the same 200ms.

Three things this is careful about:

- **The slot grows off the right end, and the row does not move.** It is placed after the run is
  centered and takes no part in that centering (`thumbnails::strip_geometry`), so every real
  thumbnail is exactly where it was when the drag began. The row owes itself half a slot of
  recentring the moment the workspace is real, and pays it then, in one 200ms ease from the
  snapshotted pre-drop positions (`Monitor::OpenSlide`).
- **The row must be still for the whole drag**, and that is the reason for the above. A row that
  recentered continuously was both a moving target to aim at and a feedback loop: the run shifts
  under the pointer, which changes which drop the pointer is asking for, which changes the reveal.
  At the gap before the trailing workspace that loop was visible as a shake. The reveal is
  additionally measured against the row *at rest*, never the row as drawn — same rule
  `thumb_drag_target` takes its answer by.
- **Only the trailing workspace arms by proximity.** An interior gap is a target you have to aim
  into, and a distance ramp there would have the row breathing a gap open under the pointer wherever
  it went; those keep the pill. The ramp is measured to the trailing thumbnail and the slot
  *together*, because both drops grow the row — which also means a drop can only ever land on a
  slot that is already full width, so the real thumbnail takes it over with no jump.

The workspace is *not* in the model while the slot is open — `Monitor::render_phantom_thumb` draws
an empty desktop's chrome and nothing else, for the same reason `CloseSlide` keeps a removed
workspace out of it.

## Accepted losses

**Empty workspaces do not survive their output going away** — the shipped behavior, and a
**decided reversal, not yet built** (`multi-display.md` §2). `Monitor::into_workspaces` and
`Layout::add_output` both filter to `has_windows_or_name()` when migrating workspaces between
monitors, so unplugging a display drops its empty desktops rather than piling them onto the
primary, and a workspace emptied while its display is away can never come home. The reason for the
filter — a display plugged and unplugged repeatedly must not accumulate anonymous empties on
whichever output is left — is kept by parking an absent display's empty workspaces in `Layout`
rather than materializing them on the primary at all.

**niri's `empty-workspace-above-first` is gone** (config field, ~50 special-case sites, its
tests, and its wiki section). It is niri's way of doing workspaces, GNOME has no equivalent, and
it complicated every invariant this change touches. Two of its tests — `add_and_remove_output`
and `move_window_to_different_output` — were generic invariant checks that merely happened to set
the flag; they were kept, flagless.

## 4. DIVERGENCE — every display owns its own workspaces

**Approved by Gustavo 2026-08-22.** Each monitor has its own independent stack of workspaces, and
switching workspaces on one monitor leaves the others alone — macOS Mission Control's model, and
the one the niri-inherited `Layout` already implements (`MonitorSet::Normal { monitors }`, a
`Vec<Workspace>` per `Monitor`).

GNOME's model is the opposite, and it is not a setting we are declining to honor — it is the
shape of mutter's core. `MetaWorkspaceManager` holds **one** global workspace list and **one**
`active_workspace` (`meta-workspace-manager-private.h:37-39`). There is no per-monitor current
workspace to have: gnome-shell hands every monitor's view the same `_scrollAdjustment`
(`workspacesView.js:610,749`) and a swipe on one monitor starts the gesture on all of them
(`:943`), so switching to workspace 3 switches *every* display to 3.

`workspaces-only-on-primary` (`org.gnome.mutter`, **default `false`**) is a different axis and
neither of its values reaches per-monitor stacks:

- **`true`** forces every window not on the primary to `on_all_workspaces`
  (`meta-window.c` `should_be_on_all_workspaces`, `:5191-5195`) — secondary monitors go *static*
  while the primary switches.
- **`false`** (the default) lets non-primary monitors carry workspaces too — but in **lockstep**
  with every other monitor, because there is still only the one `active_workspace`.

The axis the key controls is *whether secondaries participate*, never *whether they switch
independently*. Independent per-display switching is not expressible in mutter's data model, which
is why this is a divergence rather than a choice of setting.

So this is a real divergence in both directions, and it is the one the rest of this document
already assumes. §3's promise — that `Super+3` means the same desktop all day — is a per-monitor
promise here: the index is an index into *that display's* stack. And "Accepted losses" above,
where empty workspaces do not survive their output going away, is only a question that exists
because workspaces belong to a monitor in the first place.

**What a display going away and coming back costs, and what it should cost, is
`multi-display.md`** — display identity, home groups, the pointer-decides chooser, moving a
workspace between displays, and the geometry a window owes a smaller work area. The two backlog
entries below are that document's §2, §3 and §6.

### Consequence: `<primary>` is nearly inert

With per-monitor workspaces, the primary monitor stops being the thing that owns the desktops.
What the layout still calls `primary_idx` (`src/layout/mod.rs:468`) is only the destination for
workspaces orphaned by an unplug. Restoring `monitors.xml`'s `<primary>` into it is therefore a
small, low-stakes change rather than a policy decision — it no longer decides where workspace
switching happens, because nothing global does.

### Backlog: dragging a workspace across monitors

**Wanted, not built.** The thumbnail strip reorders workspaces within one display
(`ThumbGrab`, `src/input/thumb_grab.rs`); it should also be able to carry a workspace to
*another* display's strip. This is the affordance per-monitor workspaces owe the user: with one
global list there is nothing to move a workspace *to*, but with a stack per display, moving a
desktop between displays is the natural operation and there is currently no way to express it.

Designed in `multi-display.md` §6, which puts a workspace **context menu** — rename, close, send to
a display — ahead of the drag: it is the keyboard- and screen-reader-reachable way to express the
same move, and it does not wait on either problem below.

Two things the drag itself runs into, both already documented here and worth reading before
starting:

- The layout does not know monitor positions (`src/layout/mod.rs:4887`), which is why a
  cross-output *window* drag teleports rather than animating. A cross-output workspace drag will
  want the same knowledge, so the two are one problem.
- The row must be still for the whole drag, and a row being aimed at must not move under the
  pointer (§ "The row opens for a new workspace *during* the drag"). Two rows, on two displays,
  both potentially reacting to one drag, makes that rule harder to keep, not easier.

### Backlog: workspace state should survive an unplug and a replug

**Designed, not built — `multi-display.md` §2 and §3.** Unplugging a display appends its
workspaces to the primary (`Monitor::append_workspaces`), and `Layout::add_output` does take back
the ones whose `original_output` matches when it returns (`src/layout/mod.rs:1038-1074`), with
`Layout::last_active_workspace_id` (`:427`) restoring which was active. Three things are missing,
and a laptop that docks daily meets all three: an emptied workspace is filtered out by
`has_windows_or_name()` and never comes home; nothing pins *where* in the returning strip a
workspace sat, so a reorder made while it was homeless is unrecoverable; and the home tag is a name
(`OutputId`, `src/layout/workspace.rs:192`) rather than the identity the session store already
matches by (`session_state::OutputIdentity`).

The answer is per-workspace, not per-configuration: a home `OutputIdentity` plus a home ordinal,
stamped at detach and rewritten only by an explicit move. Keying the *whole configuration* the way
`monitors.xml` does was considered and rejected — it would restore a stale arrangement and fight the
edits the user made while the display was away, where per-workspace tags degrade one workspace at a
time. The live layout and the session store share the one identity, and the store persists no stack
of its own: restore derives an ordering from the whole record set at load and materializes a
workspace only when a window actually lands in it (`multi-display.md` §3).

Related and adjacent: a window whose display is gone currently keeps its size and loses its
position. `multi-display.md` §5 is that machinery — mutter's two-branch move, a per-axis shrink, and
a remembered `displaced_rect` — and a workspace moved between displays uses the same path.

## Amendment 2026-08-13 — GNOME's drag & drop workspace concept: IGNORED

The design team's *Shell Design Dreams* post (2026-08-11) revives a GNOME-40-era
concept (`window-dnd/window-dnd.png`): **remove the workspace minimap**, and when
a window drag starts, scale the *real* workspaces down so they become large drop
targets — one representation of a workspace instead of two. Plus drag-to-edge to
half-tile, drag past the edge to create a workspace, and drag-and-hold at the
edge to advance. Cleo Menezes Jr. has an extension prototyping it.

**Disposition: ignore.** Not deferred pending upstream — if GNOME ships it, we
record the divergence and move on.

The reason is this document. Our overview is already answering the same question
from a different direction, and the answers are load-bearing:

- the strip is **always shown**, and it is the **same row** as the app grid
  (§2, §2b) — so we do not have the two-representations problem their concept
  exists to remove;
- empty workspaces are closed **by hand**, macOS-style (§3), so a drag past the
  end means something different in our model than in theirs;
- the dash is a dock (`dock-divergence.md`), which changes what the bottom edge
  of a drag means.

Adopting their concept would mean unpicking all three. The core insight — that
two representations of one workspace on screen at once is the actual problem — is
correct, and we agree with it; we just already act on it.

**Separable, if we ever want them:** the edge behaviours (half-tile on edge drag,
drag-and-hold to advance) do not require the minimap to go away and could be
judged on their own merits later.

Assessment context: `dreams-assessment.md` §6.
