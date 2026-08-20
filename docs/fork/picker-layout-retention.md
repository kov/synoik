<!-- SPDX-License-Identifier: GPL-3.0-only -->
# The overview picker keeps its layout

The picker's layout is a decision, held until its inputs change. It is not re-derived per
frame, and a change to the room available does not reopen it.

## Why holding it matters

`expose::compute_grid` assigns rows by `center().y` and columns by `center().x`, both stable
sorts with no tie-break. It is an *ordering over its inputs*, so anything that disturbs an
input re-seats previews — and centred auto-placement makes exact ties ordinary: two
same-sized windows each mapped alone on an empty workspace and then brought together have
*identical* centres, resolved by nothing but input order.

Two inputs were unstable, and both are fixed:

- **The settled rect was rounded twice.** `tiles_with_render_positions` rounds
  `logical_pos + render_offset` to physical pixels, and the picker rounded again after
  subtracting `render_offset` back off. At fractional scale `round(round(X + R) − R) != X`,
  so a rect that is by construction animation-free wobbled by a physical pixel for the life
  of a decaying move spring. It is now read from its own source (`tiles_with_offsets`),
  unrounded and frame-stable.
- **The sort input was reordered by stacking.** GNOME lays out `_sortedWindows`, held in
  `get_stable_sequence()` order (`workspace.js:811-817`, consumed at `:541`); `syncStacking`
  sorts a *local* array for z-order (`:862-872`), so a restack recomputes to the identical
  assignment. Ours sorted the floating stack, which `raise_window` reorders on every
  activation. It now sorts by `stable_sequence`.

Retention is not what fixed those. It is GNOME fidelity, and insurance: a tie-break-free
sort stays fragile against every *genuine* sub-pixel input change, and holding the decision
makes those non-events rather than lucky ones.

## The split

`expose::compute_slots` is the composition of two halves, and only the first is dangerous:

| Half | Reads | Can re-order |
|---|---|---|
| `compute_grid` — the assignment | positions, sizes, the area, the view height | **yes** |
| `pack_grid` — the packing | the grid, sizes, the area | no |

`pack_grid` never consults a position, so re-packing a held grid can move and re-scale
previews but can never hand one a different cell. That is what makes the per-frame path
safe, and it is gnome-shell's own split: `vfunc_allocate` guards `_layout` with
`_needsLayout` while `_windowSlots` additionally recomputes on `containerAllocationChanged`
(`workspace.js:668-681`).

GNOME in fact scores the grid against `this._workarea` and packs into `_windowSlotsBox`, the
actor allocation — two different rectangles, because its open/close transition expands the
container every frame. We zoom in the render instead, so `expose_area` plays both roles.
Do not "fix" that conflation into GNOME's two rectangles without the animation that
motivates it.

`compute_slots` remains for `src/ui/screenshot_ui.rs`, an independent consumer that computes
once at construction.

## Validity

**The held decision is valid while the inputs it was reached from are bit-identical**, tested
per call. The inputs are the ordered `(stable_sequence, settled rect)` list plus the view
size. Not the id — an id is a `smithay::desktop::Window` handle, and holding one keeps a
closed window alive until the next overview visit.

Bits rather than `==`: `-0.` and `0.` compare equal but sort apart under the `total_cmp` the
grid orders with, so a value that flipped the sign of its zero really can re-seat a preview.

**Comparison rather than dirty flags set by mutators.** Under-invalidation is this design's
one real hazard — a permanently wrong picker is worse than a transient wobble — and the
mutation surface cannot be closed by audit: `tiles_with_offsets_mut` and
`tiles_with_render_positions_mut` hand out `&mut Tile` past every named mutator, an
interactive resize reaches a window's size through one, and the set stays open to every
future caller. Comparison makes a missed event unrepresentable; the worst it can do is
decide again. It also subsumes what a hook inventory would have had to enumerate —
membership, client-commit size, position — and is *stronger* than GNOME on stacking, which
dirties the layout in `syncStacking` where we recompute nothing at all.

The view size is compared because the monitor is frozen into the decision: `window_scale`'s
enlargement of small windows is read again at packing time from the height the grid was
summed at. GNOME freezes the same thing, constructing a fresh layout strategy around
`Main.layoutManager.monitors[this._monitorIndex]` on every decision (`workspace.js:521-522`,
read back at `:173`). Both dimensions, not just the height the grid needs — a 1920×1080 →
3440×1080 mode change leaves the height bit-identical while the area doubles in width.

**The area is deliberately not compared.** That is the whole point of holding a decision, and
it is GNOME's: `workareas-changed` calls `layout_changed()` without ever setting
`_needsLayout` (`workspace.js:594-597`). A strut appearing mid-overview shifts and re-scales
the previews together rather than re-seating them.

### What bounds the staleness

Only one thing goes stale: the area a decision was searched for. Since `additional_scale`
caps at 1, a re-fit can only shrink, so a decision made for a small area stays small in a
large one.

**Entering the overview decides afresh.** That is GNOME's scope exactly —
`prepareToEnterOverview` runs `_updateWorkspacesViews` (`workspacesView.js:998`), which
rebuilds every `Workspace`, and a fresh `WorkspaceLayout` starts with `_needsLayout` set
(`workspace.js:430`). So staleness is bounded by a single visit, with no threshold constant
to justify.

An empty workspace decides nothing at all. The overview renders every workspace and dynamic
workspaces keep a trailing empty one, so a workspace with nothing to lay out must not read
as a decision — otherwise the count climbs every frame the overview is open.

## What stays per frame

`slide_slot` and `expose_slides`, unchanged — a live post-pass over the held slots, and what
makes a re-layout animate rather than jump. The interpolated slot must never reach
`compute_grid`. The hover growth in `expose_tile_render`. And the `rect` half of
`ExposeLayout`, the tile's live render rect; only the `slot` half is held.

`expose_frozen` sits beside the held decision rather than being it: while an overview drag is
in flight the remaining tiles keep their captured slots and the dragged window's slot stays
vacant. A window the freeze does not know about falls through to the held decision, which is
stable for the duration of a drag.

## Tests

Retention is a claim about *when* work happens, so the suite asserts both halves: a
disturbance must not re-decide, **and** every real change must still re-decide. A suite that
only asserts stability passes trivially on a picker frozen forever.

`Layout::expose_recompute_count` is the observation the claim is made of — a held decision
and a freshly derived one compute the same slots, which is the entire contract, so nothing
about the picker's *output* can witness retention directly.

**Pins.**

- `a_settled_position_does_not_move_while_the_window_animates` — the double-rounding defect,
  stated directly. Its fixture geometry must be **off the physical-pixel grid**: on it,
  `round(round(X + R) − R) = X` for every `R` and the wobble is unreachable.
- `raising_a_window_does_not_move_the_previews` — the stacking defect. The windows must tie
  **exactly** or the sort never consults the order and the test is decorative.
- `a_strut_refits_the_previews_and_does_not_reseat_them` — the split. Its lever must move the
  area *alone*: a rescale moves the view size and re-decides for a different and correct
  reason, and a free-floating window's position is stored as a fraction of the working area,
  so a strut moves it unless it sits at the origin. It asserts a fresh decision over the same
  area differs from the re-fitted one — without that, re-fitting and re-deciding would be
  indistinguishable and the test would prove nothing.
- `the_picker_decides_again_when_its_inputs_change` — a window arriving, moving, resizing
  under a client commit, and the view rescaling.
- `the_picker_decides_its_layout_once_and_holds_it` — forward-only, and says so. The count it
  asserts on did not exist before the decision was held, so it cannot have been red. It
  queries *every* workspace, which is what the render path does and the only way an empty
  workspace's behaviour is visible.

**Geometry, `src/layout/expose.rs`.** A golden table pinned on bit patterns, covering the
finite degenerates: a zero extent, an empty middle row (which exercises both the negative
spacing term and the quirk where a row's height is bumped before the window that raised it is
turned away), and a window tall enough to drive `window_scale` negative. Inputs that make the
arithmetic itself NaN are not pinned — NaN bit patterns are not portable. Packing is pinned
idempotent across a foreign area, which is what licenses re-packing a held grid.

**Nets, not pins.** `a_preview_settling_into_the_picker_never_doubles_back` asserts a real
trajectory invariant but passes on either side of the fix — its geometry lands on the
physical-pixel grid. Kept as a class net.

**Must keep passing unchanged.** `overview_drag_does_not_reflow_the_picker_on_pickup`,
`overview_drop_does_not_reflow_the_picker_while_the_window_settles`,
`dropped_preview_flies_back_into_its_slot`, and `leaving_the_overview_drops_the_picker_overlay`.

## Known gap

A held grid packed into a pathologically shrunken area can drive
`additional_vertical_scale` negative, where a fresh decision would have scored its way to a
single row and avoided it. It needs an area shorter than `(rows − 1) × spacing()`, which a
view-size change would have re-decided, so only an extreme mid-visit strut reaches it.
GNOME divides the same way at every one of these sites (`workspace.js:284-285`, `:320`,
`:329`) and has the same hole. Do not clamp — the geometry is pinned, but whether a
negative-size slot survives the render path is unverified.
