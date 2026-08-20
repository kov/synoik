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

Two rules keep the inputs still, and neither is retention:

- **A settled rect is read, never recovered.** It comes from `tiles_with_offsets`, unrounded
  and frame-stable, not from the rounded render position with the animation subtracted back
  off — at fractional scale `round(round(X + R) − R) != X`, so recovering it that way moves a
  rect that is by construction animation-free by a physical pixel for the life of `R`.
- **The layout order is creation order.** `stable_sequence`, our
  `MetaWindow::get_stable_sequence`, never the floating stack, which `raise_window` reorders
  on every activation. GNOME lays out `_sortedWindows` in that order for exactly this reason
  (`workspace.js:811-817`, consumed at `:541`), while `syncStacking` sorts only a *local*
  array for z-order (`:862-872`), so a restack recomputes to the identical assignment.

Retention is GNOME fidelity, and insurance: a tie-break-free sort stays fragile against every
*genuine* sub-pixel input change, and holding the decision makes those non-events rather than
lucky ones.

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

Narrower in practice than it sounds, and knowingly so: a free-floating window's position is
stored as a fraction of the working area (`floating.rs:192-227`, mutter's rule), so a real
strut *moves* most windows, which changes an input and re-decides anyway. The re-fit is what
happens to the windows a strut leaves where they are. We therefore re-decide on more events
than GNOME, which never dirties on a bare position change — the safe direction, and not
worth diverging from mutter's placement to narrow.

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

## The drag

A window picked up in the picker leaves its workspace to ride along with the move, and
membership is a layout input — so without help, picking one up is removing one, and the
previews around it close the gap. The drag therefore **reserves its layout input**: the
`(stable_sequence, settled rect)` it had at pickup stays in the list, keeps its place in the
order, and resolves to a slot that no tile asks for.

That is GNOME's mechanism, which has nothing to suppress: a preview drag reparents the actor
and leaves the window in `_sortedWindows` (`windowPreview.js:643-670`), so the layout keeps
computing a slot with no actor in it, and `addWindow` reflows around it (`workspace.js:824`).
`_layoutFrozen` exists for something else entirely — a removal settling under a still pointer
(`:1152-1183`) and overview exit (`:1300`).

It reserves the *input*, not the slot it resolved to. A captured slot comes through the slide
post-pass, so replaying one pins a preview wherever it had got to: a drag begun during a
settling drop stopped the picker dead mid-flight for the length of the drag. And a slot
capture cannot answer for a window that arrives mid-drag, which is why one used to collapse
the reserved gap and reflow everything.

A pickup reserves the input *before* the removal takes the tile away, so for that stretch the
window is both a live tile and the reservation. The splice drops the reservation while that
lasts: laying it out twice would decide a grid over a window that does not exist, and poison
the held inputs with it.

## The close

A window leaving re-decides the layout — membership is an input — so the picker would reflow
out from under a pointer that is still working in it. It **holds still** instead, for 750 ms of
pointer stillness, and reflows with a 200 ms `EaseOutQuad` when the hold runs out. Both halves
are gnome-shell's `_doRemoveWindow` (`workspace.js:1140-1183`): clearing `layout_frozen` emits
`layout_changed()` (`:937`) onto the `_needsLayout` the removal set, so the reflow arrives
through `animateAllocation` (`:759-766`). **The ease belongs to the release, not to the close.**

A freeze holds the **whole input list**, not the departed window's entry. One vacant entry is
not enough: a removal in the scrolling layout shifts every column after it, so the survivors'
own settled rects move and the grid re-decides over them anyway — and, with the ease at the
release, snaps them. Substituting the list does not bypass retention, it feeds it: the held
decision is still validated bit-for-bit against exactly what it is handed.

Armed in `Layout::remove_window` — the one site every close funnels through, a client
destroying its toplevel, `Action::CloseWindow`, the preview's own close button — and at the
source of a move (below). Two conditions:

- **Only while something shows the spread**, `overview_open || peek_open`.
- **Not on a drag pickup**, which removes the window too but reserves its place first.

### What ends a hold

- **750 ms with the pointer still**, which is the `Some(hold)` case. That hold is an `Animation`
  and so claims a running frame — which is what guarantees a frame arrives to notice it, at a
  bounded cost of 750 ms of static redraws per close. gnome-shell's GLib timeout pays nothing
  there; expiring on the animation clock is what makes the hold testable under a frozen one,
  and no calloop timer is.
- **Never, while the pointer rests on one of this workspace's previews** — gnome-shell's second
  disjunct at `:1170`, and the point of the whole mechanism: a close button must stay under the
  cursor for a second click. That hold is `None`, claims no animation, and so costs nothing
  however long it lasts. Only a *live* preview counts: a close-button click leaves the pointer
  over a corpse, and `Workspace::remove_tile` drops the departed window's hover entry.
  Unreachable during a peek, whose hover is never computed — a peek's hold is only ever the
  timed kind, though pointer motion still re-arms it.
- **A window arriving**, per workspace (`_doAddWindow`, `:1245-1251`). Fidelity and invariant
  both: a freeze holds a fixed list, and a tile with no entry in it has no slot to land in.
  Every insertion funnels through `Workspace::add_tile`, `add_tile_to_column` or `add_column`
  for that reason — the third was missed once, and reached the `debug_assert` in
  `expose_layout` through a column move onto a workspace with a close still settling.
- **The overview fully hiding**, not the start of its exit. The exit interpolates each preview
  between its window rect and its slot, so reflowing there shuffles the picker on its way out.
  A hold that merely *ran out* mid-exit would do the same, so leaving converts every hold to
  the indefinite kind first — which is gnome-shell exactly: `prepareToLeaveOverview` removes
  the timeout and *sets* `layout_frozen` (`:1295-1303`), and only `_doneLeavingOverview` clears
  it (`:1316-1318`).
- **The overview opening**, with `forget_expose_layout`.

Animations being off skips the hold along with the ease, so a close reflows at once. GNOME's
timeout is not an animation and would survive; this is deliberate, and the alternative is a
750 ms freeze followed by a snap.

Two hazards this shape has and a poll does not, both closed: an indefinite hold needs a way
back to a timed one, or a pointer that leaves a preview for another output freezes the picker
for good; and a hold that expires with no motion since the close would reflow under a still
pointer, so the expiry re-checks hover rather than trusting the motion feed alone.

One residue of holding a *list* rather than an allocation: a view-size change during the hold
misses retention and re-decides — over the frozen list, so the hole survives it. Everything
else is held, gnome-shell's frozen allocation included.

## Every other door

A window can also join or leave without either of those funnels, and each such door used to
leave previews to snap:

- **A fresh map.** `Layout::add_window` brackets the insertion with a `picker_slots_now`
  snapshot and `ease_picker_from`. The arriving preview appears in its slot rather than easing
  — it is missing from the snapshot, which is what tells the ease to skip it. gnome-shell pops
  the added clone up from scale 0 instead (`:1233-1243`); not ported.
- **Move-to-workspace and move-to-output**, which bypass `Layout::remove_window` and so snapped
  *both* sides. The source now holds still like any other removal (gnome-shell's
  `_windowRemoved`, `:1260-1262`) and the target eases apart around the arrival.
- **The dragged window's own client dying mid-drag.** Dropping the reservation is what closes
  the gap the drag was holding, so it freezes first. It must ask whether *a* reservation is
  held, not whether this window's removal is the pickup that took it: mid-flight the tile lives
  in the move state and no workspace can answer the second question, so the freeze it gated on
  never armed.

The snapshot forces a layout *decision* on a workspace that has not made one, so it is taken
only while the picker is up; outside that it is empty and the ease is a no-op.

Still snapping: toggling a window between floating and tiling reaches the sub-layouts directly
(`workspace.rs`, `toggle_window_floating`) rather than `add_tile`/`remove_tile`.

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
  under a client commit, and the view rescaling. The rescale leg is confounded and known to
  be: it moves the working area too, so it would pass even with the view size out of the
  compared set. The reason both view *dimensions* are compared — a 1920x1080 to 3440x1080
  mode change leaves the height bit-identical — is unwitnessable headless, where mode changes
  no-op.
- `a_window_mapping_mid_drag_keeps_the_dragged_preview_a_place` and
  `a_drag_begun_mid_settle_leaves_the_other_previews_travelling` — the two things a captured
  slot could not do. The first reads how many windows the decision was made over, because a
  reserved slot reaches no caller and is otherwise unobservable; a count alone would pass on
  a reservation spliced at the wrong index, so it also pins the arriving preview to the right
  of the picker's middle, which is where the hole in front of it puts it. The second must
  distinguish *travelling* from *teleporting*: a pickup that re-decided and jumped the
  bystander to its final slot moves it just as far as the slide does, so intermediate samples
  are pinned off both endpoints.

  Both sample across a live animation and so must **freeze the clock** over it.
  `run_until_settled` unfreezes on the way out when it froze on the way in, and a slide left
  running on the wall clock finishes during whatever the test does next — reading as a snap
  that never happened. The freeze is only worth as much as its writers respect: `Synoik::redraw`
  used to pin the clock at each frame's real-time target regardless, which put the machine back
  in charge of a frozen clock at every redraw.
- `a_window_closing_in_the_picker_eases_the_survivors` — both halves: nothing has moved at
  700 ms, and the reflow past the hold is sampled across its 200 ms. It must settle **frame by
  frame** beforehand, not with `settle_animations` — that jumps the clock a second ahead and
  the roundtrip puts it back, arming the ease at a time no sample reaches, which reads exactly
  like a snap.
- `a_pointer_resting_on_a_preview_holds_the_close_freeze`, plus
  `the_closed_windows_own_preview_does_not_hold_the_freeze` and
  `a_window_arriving_releases_the_close_freeze` — the three ends of a hold. The first advances
  past the hold **in steps**: one jump lands the release and the reading on the same instant,
  where the ease has covered nothing and a wrongly-reflowed picker is indistinguishable from a
  held one. It passed the mutation that way before the steps were split.
- `a_fresh_window_eases_the_previews_it_displaces` and
  `moving_a_window_to_another_workspace_settles_both_pickers` — the other doors. The first must
  map with `map_window_unsettled`: `map_window_sized` settles, which runs the ease to its end
  before anything can sample it. The second reads the two sides at *different* times, because
  they behave differently — the target eases immediately and is over by 200 ms, the source is
  still untouched at 300 ms.
- `a_client_dying_mid_drag_holds_the_gap_it_left`, `a_close_freeze_does_not_run_out_during_the_overview_exit`
  and `a_column_arriving_releases_the_close_freeze` — three mechanisms that were each written
  and shipped inert. The first must **not** settle after the pickup: a held drag never settles.

A hold is only pinned by a test that asserts *nothing moved* while it runs. A test that only
watches for the reflow passes on a hold that never armed, on one that released early, and on
the old ease-at-close alike — three of these tests were that shape until the assertions were
added.
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
`dropped_preview_flies_back_into_its_slot`, `overview_drag_freezes_the_other_previews`, and
`leaving_the_overview_drops_the_picker_overlay`.

**What the corpus cannot see.** `Fixture::run_until_settled` dispatches and refreshes but
never renders, so anything that only `render_expose` does — its hover restack, say — is
invisible here. Tests reach the picker through `expose_slot` and `expose_slots_now`, the same
entry the render path uses, which is why they see the layout at all.

## Known gap

A held grid packed into a much smaller area drives `additional_vertical_scale` negative,
where a fresh decision would have scored its way to a single row and avoided it. It needs an
area shorter than `(rows − 1) × spacing()` — `spacing()` is about 30px, so a held two-row
grid needs roughly 30 logical pixels of height left, which on a 1080-logical view means a
mid-visit layer surface reserving over 500px. A view-size change would have re-decided, so
only a strut appearing during a visit reaches it.

What happens: negative slot sizes reach `expose_tile_render` as a negative tile scale, and
`RescaleRenderElement` builds its geometry from corners, so damage intersection treats the
result as empty. The previews go invisible and stop hit-testing until the next input change
or overview entry. No panic — `verify_invariants` is test-only — and self-healing.

Left alone deliberately. GNOME divides the same way at every one of these sites
(`workspace.js:284-285`, `:320`, `:329`) and has the same hole, and clamping here would put
a fudge factor in a ported formula to paper over a state reached only by an OSK-sized strut
opening over an already-shrunken picker. If it is ever worth closing, close it by
re-deciding when a re-fit would come out negative — not by clamping the scale.
