<!-- SPDX-License-Identifier: GPL-3.0-only -->
# The overview picker keeps its layout

The picker's layout is a decision, made when something changes and honoured until
something changes again. It is not re-derived per frame.

## The three defects

They are separable, and land in this order. Retention is last, and is not the bug fix.

### 1. The settled rect is rounded twice

`tiles_with_render_positions` rounds `logical_pos + render_offset` to physical pixels
(`src/layout/floating.rs:426`), and `expose_layout` rounds again after subtracting
`render_offset` (`src/layout/workspace.rs:2092`). At fractional scale
`round(round(X + R) − R) != X`, so while a move spring decays, a settled rect that is *by
construction* animation-free wobbles by a physical pixel.

`compute_slots` assigns rows by `center().y` and columns by `center().x`
(`src/layout/expose.rs:261`, `:154`) with no tie-break, and centred auto-placement makes
exact ties ordinary — two same-sized windows each centred on an empty workspace and then
brought together have *identical* centres. One pixel of wobble swaps their slots; they
charge at each other's positions and swap back when it passes.

**Fix:** feed the layout the unrounded position from `tiles_with_offsets`
(`floating.rs:408`), which is frame-stable for the whole decay. This removes the measured
bug's mechanism outright.

### 2. Our sort input is reordered by stacking; GNOME's is not

GNOME lays out `_sortedWindows`, held in `get_stable_sequence()` order
(`~/Projects/gnome-shell/js/ui/workspace.js:811-817`, consumed at `:541`). `syncStacking`
sorts a *local* array for z-order and never touches it (`:862-872`), so a restack
recomputes to the identical assignment and no preview moves. Ours sorts
`tiles_with_render_positions()` — the floating stack — which `raise_window`
(`floating.rs:799`) reorders on every activation. Tied windows swap slots when one is
raised.

**Fix:** sort the layout input by a stable per-window key before ordering, our analogue of
`get_stable_sequence`. Restacks then become no-ops, as in GNOME.

### 3. The layout is recomputed every frame

`render_expose` calls `expose_layout` once per workspace per frame, from both the main row
(`monitor.rs:3770`) and each thumbnail (`monitor.rs:3419`) — in the overview with N
workspaces, 2N full grid searches a frame, plus one per window through `preview_rects`,
plus a full recompute inside every `expose_slot` (`workspace.rs:2351`) and two per drop in
`slide_expose_slots_from`.

With 1 and 2 fixed the recompute is deterministic and the picker is correct. Retention is
then what it actually is: GNOME fidelity and cost, landed without a correctness gun to its
head — and insurance, because a tie-break-free sort stays fragile against every *genuine*
sub-pixel input change (an interactive resize, a client-driven move). Retention makes
those non-events rather than lucky ones.

## Shape

Three tiers, following `Workspace.vfunc_allocate`
(`~/Projects/gnome-shell/js/ui/workspace.js:668-681`, 50.3):

| Tier | Value | Recomputed when |
|---|---|---|
| 1 | the **grid** — row partition, cell assignment, and its scale | an invalidating event |
| 2 | the **slots** — concrete rects in workspace coordinates | tier 1 changed, or the area changed |
| 3 | slide and hover | every frame |

A container resize re-fits and **never reconsiders which window goes where**. That is the
whole point of the split, and it is GNOME's: `_layout` is guarded by `_needsLayout` while
`_windowSlots` additionally recomputes on `containerAllocationChanged`.

### Where to cut `compute_slots`

Tier 1 is **not** the vertical sort alone. The row-count search
(`src/layout/expose.rs:269-286`) scores candidate layouts against `area`, so re-running it
on an area change can pick a different `num_rows` and move windows between cells; and the
within-row x-sort (`:153-154`) re-reads live centres. Retaining only a `Vec<usize>` order
would leave both re-orderings on the tier-2 path — the defect this design exists to remove.

The cut is between `expose.rs:287` and `:289`:

- **Tier 1** = `:254-287` — sort, row-count search, row partition, and the resulting
  `layout.scale`. Retained as rows of window ids plus the scale, and carrying `view_h`,
  because `window_scale(window, monitor_height)` is consulted during packing (`:349`).
  GNOME likewise freezes the monitor into the strategy at `_createBestLayout` time.
- **Tier 2** = `:289-375` — `_computeRowSizes` and `computeWindowSlots`. This half reads
  only window **sizes** (`:349-355`), never positions, and a size change is a tier-1
  trigger — so a re-pack can never consult a jitterable input.

`compute_slots` stays as the composition of the two: `src/ui/screenshot_ui.rs:813` is an
independent consumer that computes once at construction and needs no retention.

### The retained value

```rust
/// The picker's standing decision for this workspace.
struct RetainedLayout<W: LayoutElement> {
    /// Rows of window ids, and the scale the grid was searched at. Keyed by id, never by
    /// index: a restack reorders the tiles vec, and an order stored by index would
    /// silently re-associate.
    grid: Grid<W::Id>,
    /// The slots that grid packed into, and the area they were packed for. Dropped when
    /// the area moves, which re-packs without re-ordering.
    packed: Option<(Rectangle<f64, Logical>, Vec<(W::Id, Rectangle<f64, Logical>)>)>,
}
```

`expose_frozen` (`workspace.rs:121`, `:180`) already stores `Vec<(W::Id, Rectangle)>` keyed
by id, so it becomes this rather than sitting beside it.

## Invalidation

GNOME dirties on five things: `addWindow` (`workspace.js:824`), a window's `size-changed`
(`:806`), `removeWindow` (`:857`), `syncStacking` (`:876`), and `set spacing` (`:921`).

**Put the hooks inside `FloatingSpace`, not on the `Workspace` wrappers.**
`Workspace::descendants_added` (`workspace.rs:2584`) reaches
`FloatingSpace::bring_up_descendants_of` (`floating.rs:1495`) without passing through any
Workspace mutator, so a dialog mapping would restack past a wrapper-level set. Hooking the
owner of the state means the wrapper inventory does not have to be exhaustive.

- **Membership** — `FloatingSpace::add_tile`/`add_tile_above` (`floating.rs:640`),
  removals, and the layer moves behind `set_window_floating` (`workspace.rs:1831`).
- **Size and position** — `FloatingSpaceData::update` (`floating.rs:238`), which already
  guards on `if self.size == size { return; }` and calls `recompute_logical_pos`. This is
  the client-commit hook, and it is GNOME's `size-changed` exactly: a commit reaches it via
  `Layout::update_window` (`mod.rs:1433`). Invalidating in `Workspace::refresh` instead
  would fire once per frame for any redrawing client and quietly rebuild the per-frame
  recompute this design removes. Plus `set_logical_pos` (`floating.rs:248`), `move_by`
  (`:1132`), `recompute_logical_pos` (`:192`), `refit_to_working_area` (`:342`).
- **Order** — with defect 2 fixed these recompute to the same answer, so they are dirty-flag
  parity rather than load-bearing: `raise_window` (`floating.rs:799`),
  `bring_up_descendants_of` (`:1495`), and the `floating_is_active` flips behind
  `focus_floating` (`workspace.rs:1842`).
- **Tier 2 only** — `set_view_size` (`workspace.rs:737`, which already early-returns when
  nothing changed) and `FloatingSpace::update_config`'s `area_changed` (`floating.rs:315`).

`render_offset` is **not** in the set, and never was: it is subtracted before the sort.

Invalidation is a flag and recompute is lazy at the next `expose_layout`, so
over-invalidating outside the overview costs nothing. **Under-invalidation is the only real
hazard, so err generous.**

## The freeze

GNOME does **not** unfreeze on window-added. `addWindow` sets `_needsLayout` (`:824`) but
allocate skips both recomputes while frozen (`:668`), so a window mapped mid-drag simply
has no slot until unfreeze; `removeWindow` splices its slot out immediately (`:846-849`).
Adopt that: freeze suppresses recompute, a removal splices, and the layout is recomputed on
unfreeze.

Our renderer must still draw a mid-drag-mapped window somewhere, which GNOME's actor model
does not have to answer. Keep today's throwaway fallback (`workspace.rs:2105-2122`) for
that case **only while frozen** — its inputs are static during a drag, so it is stable
per-frame — and store on unfreeze.

One behaviour change to expect: `freeze_expose` (`workspace.rs:2337`) captures through
`expose_layout`, which applies `slide_slot`, so today it freezes *mid-slide interpolated*
positions. Retained, it freezes the targets while slides keep running — better (a drag
begun during a settling drop no longer pins previews mid-flight), but visible to
`overview_drag_freezes_the_other_previews`.

## What stays per frame

`slide_slot` (`workspace.rs:2141`) and `expose_slides`, unchanged — a live post-pass over
the retained slots, and what makes a re-layout animate rather than jump. The hover growth
in `expose_tile_render` (`:244`). And the `rect` half of `ExposeLayout`, the tile's live
render rect; only the `slot` half is retained.

`slide_expose_slots_from` (`:2184`) gets simpler: it takes a snapshot before and calls
`expose_slots_now` again inside, two full layouts per drop. With the previous slots already
in hand, arming a slide becomes a diff.

## Decisions for review

1. **Work-area changes.** GNOME updates `_workarea` on `workareas-changed` (`:592`) and
   does not dirty the layout, tolerating a stale one — with the consequence that growth is
   shrink-only until the next tier-1 event, since `additional_scale` caps at 1
   (`expose.rs:303`, `:313`). We invalidate tier 2 on it. A deliberate divergence.
2. **Position moves.** GNOME has no in-overview move that isn't a drag, so it never dirties
   on a bare move. We have `move_floating_window`, so it is in our set.

## Guard

Once defect 1 is fixed the inputs are frame-stable, which makes a **debug recompute-and-
compare** sound: behind a test build or a `SYNOIK_DEBUG_*` knob, recompute fresh and assert
it matches the retained value. Any divergence is by construction a missed invalidation.
That converts this design's worst failure — a permanently wrong picker, which is worse than
a transient wobble — into a red test.

## Tests

Retention is a claim about *when* work happens, so the suite asserts both halves: a
disturbance must not re-sort, **and** every real change must still re-sort. A suite that
only asserts stability passes trivially on a picker frozen forever.

**Pins — red before their fix, green after.**

- *Defect 1*: at fractional scale, two tied same-size windows, a move spring armed on a
  third; assert the tied pair's slots never change while it decays. This is the measured
  bug stated directly.
- *Defect 2*: two tied windows; raise one; assert neither slot moves.
- *Defect 3*: perturb the settled rects with no invalidating event; assert the assignment
  is identical.
- *The tier split*: change the area only; assert the slots re-pack and the assignment is
  preserved. Windows must be **exactly tied** or this is decorative — untied windows sort
  identically at any scale. This test fails against the `compute_order` split that an
  earlier draft of this document proposed, which is why it earns its place.

**Per trigger.** One test each asserting the layout *does* change: membership, client-commit
size, position, and area. These are the other half; without them a frozen picker passes.

**Geometry, `src/layout/expose.rs`.** Table-driven over 1..=8 windows across several aspect
mixes: slots inside the area, non-overlapping, spatial order preserved, and tier 2 over a
retained tier 1 reproducing `compute_slots` exactly.

**Nets, not pins.** `a_preview_settling_into_the_picker_never_doubles_back`
(`src/tests/gnome.rs`) asserts a real trajectory invariant with a travel precondition, but
it passes today — its geometry lands on the physical-pixel grid, where
`round(round(X + R) − R) = X` exactly, so the wobble is unreachable in its own fixture.
Keep it as a class net; it is not the pin. Same for "opening and closing the overview
re-sorts nothing" and "a drop eases the previews it displaces".

**Must keep passing unchanged.**
`overview_drag_does_not_reflow_the_picker_on_pickup` (`gnome.rs:10012`),
`overview_drop_does_not_reflow_the_picker_while_the_window_settles` (`:27681`),
`dropped_preview_flies_back_into_its_slot` (`:27796`, which asserts `expose_target_rect` is
`None` mid-drag), and `leaving_the_overview_drops_the_picker_overlay`
(`src/layout/tests.rs:3706`, whose `clear_expose_hover` also clears `expose_slides`).
