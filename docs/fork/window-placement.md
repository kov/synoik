<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Initial window placement

Where a newly-mapped floating toplevel lands, what mutter 50.3 does, and where our port still
diverges. The six fidelity gaps found in the first pass are closed (§3); §5 records the one
open design question.

Reference: `~/Projects/mutter` @ 50.3, `src/core/place.c`. Ours: `src/layout/floating.rs`.

## 1. The complaint, measured

"Windows tend to open at the top left." The perception is **correct**, and the mechanism is a
faithful port of mutter's *non-centering* path — but see §4: that path is no longer what GNOME
runs. `center-new-windows` has defaulted to `true` since mutter 48, and we pinned the pre-48
default. So this is a fidelity bug after all, not an inherited quirk. Measured on the headless
fixture, one 1920×1080 output,
GNOME mode, work area `(0, 32) 1920×1048` (top panel strut), windows mapped in order:

| # | committed size | placed at | via |
|---|---|---|---|
| 1 | 900×600 | (59, 181) | first-fit, "centered tile" slot |
| 2 | 900×600 | (959, 181) | first-fit, beside window 1 |
| 3 | 1000×700 | **(0, 32)** | cascade, work-area origin |
| 4 | 500×400 | (50, 82) | cascade, +1 step |
| 5 | 640×480 | (100, 132) | cascade, +2 steps |

Two separate reasons the result reads as top-left:

**a. The "centered tile" slot is not centered.** `find_first_fit`'s first candidate
(place.c:603-622 `center_tile_rect_in_area`) is the top-left cell of a hypothetical grid of
same-size windows; the offset is the grid *remainder*, halved horizontally and **divided by
three** vertically:

```
x = area.x + (area.w % (size.w + 1)) / 2
y = area.y + (area.h % (size.h + 1)) / 3
```

The modulo makes the x offset effectively arbitrary in the window's width: 900 wide on a 1920
work area gives `118 / 2 = 59` px from the left edge; 1000 wide gives `919 / 2 = 459`. A window
whose width divides the work area cleanly lands flush left. The `/ 3` guarantees the vertical
result is always in the upper third of the leftover.

**b. Once first-fit fails, everything cascades from the top-left corner.** `find_first_fit`
requires a candidate that fits the work area *and* overlaps nothing. With two windows open,
almost nothing qualifies, so `find_next_cascade` takes over — and its LTR origin is
`MAX(0, work_area.x), MAX(0, work_area.y)` (place.c:236-240), i.e. the top-left corner, stepping
50 px diagonally per collision. Window 3 above landed at exactly `(0, 32)`. From the third window
onward, **the top-left corner is the default**, and the first-fit "grid" phase rarely fires again.

Both mechanisms are now gone, by different routes: (a) does not run at all under GNOME's default
(§4), and (b) no longer exists in either branch — option B (§5) re-seeded the cascade at the
work-area centre. The table above is the *diagnosis*, kept as measured; nothing in it is current
behavior.

This also confirms placement runs against the client's *committed* size, not a pre-commit
placeholder — every measured position matches the hand-computed modulo for the final size.

## 2. What mutter does, in order

`meta_window_place` (place.c:839-1106):

- **A. Type deny-list** (861-887). Only `NORMAL`, `DIALOG`, `MODAL_DIALOG`, `SPLASHSCREEN` are
  placed. Docks, menus, tooltips, all override-redirect types: "the app knows best", never placed.
- **B. Position hints** (889-949). X11 only. With the default (workarounds enabled) *either*
  `PPosition` or `USPosition` skips placement outright. **Wayland windows never set these**
  (`meta-window-wayland.c:1188-1192`), so every Wayland toplevel runs the full algorithm.
- **C. Monitor** (951-971). First-time-shown windows go to
  `meta_backend_get_current_logical_monitor` — **the monitor with the pointer**. Not the focused
  window's, not primary (primary is only the NULL fallback). Already-shown windows keep
  `window->monitor`.
- **D. Work area** for that monitor, strut-reduced. gnome-shell contributes the panel strut via
  `layout.js:283` `affectsStruts: true` → `set_builtin_struts`; it does **not** override placement
  policy anywhere in JS.
- **E. Center on transient parent** (977-1010). `DIALOG`, `MODAL_DIALOG`, **and Wayland `NORMAL`
  with a `transient_for`** (979-980 — this is how GTK4/libadwaita dialogs get centered). Math is
  identical for modal and non-modal: horizontally centered on the parent *frame*, vertically
  `parent.y + (parent.h - h) / 3`. Then straight to auto-maximize, skipping everything below.
  Modal-only extra: `avoid_being_obscured_as_second_modal_dialog` (461-501), X11-only in practice.
- **F. Peers** = `find_windows_relevant_for_placement` (810-837): showing, on this workspace
  (or all workspaces, if the new window is sticky).
- **G. Centered vs origin** (1018-1045). `window_place_centered()` (448-459) is true for dialogs,
  modal dialogs, splash screens, or `NORMAL` when the **`org.gnome.mutter center-new-windows`
  gsetting** is on — **and that key defaults to `true` since mutter 48**
  (`9fe83c736c`, "schemas: Center windows by default", Feb 2025, closing gnome/mutter#246,
  #1662, #2123). Centering is therefore *the* GNOME behavior, not an opt-in; §4 spells the path
  out. Otherwise: first-fit, then origin cascade.
- **H. Denied focus** (1052-1086). When the window was denied focus-steal and overlaps the focus
  window, placement restarts against a one-element obstacle list; failing that,
  `find_most_freespace` puts it on whichever side of the focus window has the most room.
- **I. Auto-maximize** (1088-1101), gated on the `auto-maximize` pref: first-shown +
  `has_maximize_func` + area > 80% of the work area (`MAX_UNMAXIMIZED_WINDOW_AREA`).

Constants: `CASCADE_FUZZ 15`, `CASCADE_INTERVAL 50` (place.c:44-47),
`META_WINDOW_TITLEBAR_HEIGHT 50` (window-private.h:46, the diagonal step).

Not reached by any of this: `xdg_popup` and anything with a `placement.rule`, which go through
`meta_window_process_placement` (759-808) instead.

## 3. Our port, and where it differs

Entry: `Workspace::add_tile` → `FloatingSpace::add_tile_at` (`floating.rs:557-563`) →
`stored_or_default_tile_pos` (stored size-fraction position, then a `default_floating_position`
rule) → `place_new_tile` (`floating.rs:1684-1701`).

Faithfully ported: the transient centering (`+ (pw-w)/2, (ph-h)/3`), `find_first_fit`'s three
phases and its centered-tile modulo, `find_next_cascade` with all three constants, the
off-screen clamp (`Data::recompute_logical_pos`, `min_on_screen = clamp(size/4, 10, 75)`), the
80% auto-maximize with the `sqrt(0.8)` restore-size clamp, and the top-panel strut. Tile size
carries no border/shadow padding in GNOME mode, so the modulo math matches mutter's frame rect.

All six gaps found in the first pass are now closed. What each one was, and what closed it:

1. **`center-new-windows` was never read, and we assumed the wrong default.** The pref rides
   `layout::Options` down to every `FloatingSpace` (`Layout::set_gnome_center_new_windows`), is
   read in `load_mutter` (`gnome.rs`) and pushed at both GSettings sites in `synoik.rs`.
   `find_next_cascade` takes mutter's `place_centered` argument.
   → `new_windows_center_by_default`.
2. **Monitor selection was the active monitor, not the pointer monitor.** mutter seeds
   `window->monitor` from the pointer for a window with no position hint (`window.c:1245-1259`),
   and placement reuses it (`place.c:951-955`). Fixed at the *initial configure*
   (`xdg_shell.rs send_initial_configure`), not at map: the output recorded there is what the map
   path reuses, and it also picks the workspace the initial size is resolved against. The active
   monitor stays the fallback for no-outputs / pointer-off-all-outputs, and niri's scrolling mode
   keeps it outright. → `new_windows_open_on_the_pointer_monitor`. **Headless proof only** — no
   multi-monitor seat validation yet.
3. **`auto-maximize` was unconditional.** Now gated inside `Layout::auto_maximize_if_too_big`,
   sharing gap 1's plumbing. → `auto_maximize_can_be_disabled`.
4. **Our obstacle set was every tile.** `rectangle_overlaps_some_window` (place.c:503-548) counts
   only `NORMAL`, `UTILITY`, `TOOLBAR`, `MENU`; dialogs, docks and splash screens are not
   obstacles. Candidate positions still come from every window (place.c:698, :724 walk the
   unfiltered list) — only the overlap test filters. xdg-shell has no types, so
   `LayoutElement::is_transient` stands in for "dialog"; the approximation is one-sided, since
   mutter still treats a `UTILITY` window as an obstacle. → `dialogs_do_not_block_first_fit`.
5. **The denied-focus placement path (step H) was not ported.** `FloatingSpace::avoid_focus_window`
   re-runs first-fit against the focus window alone, then `find_most_freespace` (place.c:332-423).
   It runs just after the tile lands rather than inside placement, so it re-checks that the window
   was auto-placed — a stored position or a `default_floating_position` rule skips placement in
   mutter too. Ordered before auto-maximize, matching mutter's H then I.
   → `denied_focus_window_moves_off_the_focus_window`.
6. **Test coverage.** `src/tests/gnome.rs` now pins the centred path, the first-fit path's grid slot,
   downward chain, *beside* phase and cascade, the cascade's column overflow, transients, both
   prefs, the pointer monitor and the denied-focus move. Each new assertion was checked to fail
   with its fix reverted, so none of them is decoration.

Known remaining divergences, deliberate:

- `find_windows_relevant_for_placement` (place.c:810-837) excludes windows not showing on their
  workspace; our obstacle and candidate lists are whatever is in the floating space. If a
  minimized tile stays in `self.data`, placement avoids an invisible window.
- `find_most_freespace` gives up on `max_area == 0` in mutter; we give up on `<= 0` as well,
  because a negative product means the focus window is already outside the work area and mutter's
  chosen side would push the new window off-screen.
- RTL is not ported anywhere in placement (mutter mirrors the grid slot, the cascade origin and
  the *beside* candidate).
- **Option B (approved 2026-08-07).** `find_next_cascade` is always mutter's
  `place_centered = TRUE` shape — seeded at the work-area centre, walked nearest-centre-first.
  mutter only pairs that with *skipping* first-fit, so the `center-new-windows = false` branch is
  a mode GNOME does not have: first-fit still runs in front, and when nothing fits the pile grows
  from the middle instead of the top-left corner. The seeding and the walk order move together,
  as they do in mutter; taking one without the other would sort peers by a distance to a corner
  the slot no longer sits at. mutter's origin shape (slot at the work-area origin, northwest
  `x + y` order) now has no caller and was deleted rather than left as a dead branch — §5 below
  describes it, and it is six lines in `db520323` if it is ever wanted back.

Two things found along the way that are **not** placement bugs, recorded so the next reader
does not re-derive them:

- The overview picker used to re-flow when a drag was picked up: `interactive_move_update` writes
  a rubberband offset onto the tile *before* deciding the drag has started, the tile is still in
  the workspace at that point, and `expose_layout` feeds render positions to `compute_slots`,
  whose row assignment sorts by `center().y`. Both the pickup slot and `freeze_expose` are taken
  after that write, so the shuffle was captured and held for the whole drag. **Fixed
  2026-08-07:** the offset is no longer written when `in_expose`, where it was pure noise — the
  overview has no shake-loose threshold, so `started` is unconditionally true and the offset was
  written and cleared inside one call, having been resistance nobody saw. gnome-shell's
  `WindowPreview` drag never moves the window in the workspace layout at all.
  → `overview_drag_does_not_reflow_the_picker_on_pickup`. Two things the first attempt at that
  test got wrong, worth not repeating: the drag promotes on the **first** update in the overview,
  not the second, so only the first motion event's delta ever perturbs anything; and the
  rubberband damps small deltas to sub-pixel, so it takes one large motion (~400px) to move a
  slot. `overview_drag_freezes_the_other_previews` never caught it — two windows there gave an
  ordering the re-flow could not change.
- `src/tests/vulkan_render.rs`'s shared `window_fixture` now pins the first-fit path: those
  tests count a colour over the whole output, so a centred window hides under centred chrome
  (the switcher panel) and the measurement stops meaning anything.

## 4. What `center-new-windows = true` actually does

This is the behavior we owe, since it is GNOME's default. `window_place_centered()` returns true,
so step G takes the centered branch (place.c:1018-1032) and **`find_first_fit` never runs at
all** — no grid slot, no "below/beside an existing window", none of §1's mechanism (a) or (b).

The whole placement is one call, `find_next_cascade (…, place_centered = TRUE)`
(place.c:167-330):

1. **Slot = the centre of the work area.** `cascade_origin_x = work_area.x + work_area.width/2 -
   window_width/2`, `cascade_y = MAX (0, work_area.y + work_area.height/2 - window_height/2)`
   (place.c:225-244). (The `x`/`y` computed just above the call at place.c:1021-1022 are dead —
   `find_next_cascade` always overwrites them. Note it also recomputes the centre as
   `w/2 - size/2` rather than `(w - size)/2`, so the two disagree by 1px when exactly one of the
   two is odd. Match mutter's version, not the readable one.)
2. **Peers are visited in order of distance from that slot** — `window_distance_cmp`
   (place.c:64-101), squared euclidean distance from the centred corner, not northwest order.
3. **Fuzzy collision, 15 px.** A peer counts as sitting on the slot only when *both*
   `|peer.x - slot.x| < CASCADE_FUZZ` and `|peer.y - slot.y| < CASCADE_FUZZ`. When it does, the
   slot moves to `peer.x + 50, peer.y + 50` — one titlebar height diagonally (place.c:272-278) —
   and the walk *continues down the same list* from the next element. So a run of already-cascaded
   windows is chased down the diagonal in a single pass.
4. **Overflow starts a new column right of centre.** If the stepped slot leaves the work area, the
   slot resets to `centre + CASCADE_INTERVAL * ++stage` horizontally, `centre_y` vertically, and
   the scan restarts from the head of the list (place.c:281-312). When even that has no room, the
   result is the exact centre and windows stack there.

Then steps H (denied focus) and I (auto-maximize) apply unchanged.

Consequences worth being explicit about, because they are not "windows are centred, done":

- **Two same-size windows do not stack.** The second lands on the first, trips the 15 px test, and
  cascades to `centre + (50, 50)`; the third to `centre + (100, 100)`. The pile is diagonal, from
  the middle of the screen instead of from the corner.
- **Different-size windows overlap freely.** Nothing avoids overlap any more — a 500×400 opened
  over a 1000×700 sits in the middle of it, because their centred corners are >15 px apart so no
  cascade fires. The non-centred path would have tried to find it a clear spot. This is the real
  trade: centring buys a predictable, reachable position and gives up the tiling attempt.
- **Dialogs are unaffected by the key**, both ways: with a transient parent they take step E
  (centred on the parent, `+ (ph - h) / 3`); without one they were already centred, since
  `window_place_centered()` returns true for `DIALOG`/`MODAL_DIALOG`/`SPLASHSCREEN` regardless of
  the pref (place.c:448-459).

Applied to §1's measured sequence, on the same work area `(0, 32) 1920×1048`:

| # | size | today (measured) | with centring (hand-computed) |
|---|---|---|---|
| 1 | 900×600 | (59, 181) | (510, 256) |
| 2 | 900×600 | (959, 181) | (560, 306) — cascaded off #1 |
| 3 | 1000×700 | **(0, 32)** | (460, 206) — no peer within 15 px |

## 5. Options

Gaps (1)-(6) in §3 are all fidelity; none of them needs a decision. In particular gap 1 —
honoring `center-new-windows` at GNOME's own default of `true` — is the fix for the reported
symptom, and is *not* a divergence. The remaining questions:

**The default.** **A and B are landed.** Windows centre; turning the key off in dconf/Tweaks gets
first-fit back, now with gaps 2-5 fixed underneath it and a centre-seeded cascade behind it.
Whether to go further is still open:

- ~~**A. Pure fidelity.** Read the key, default `true`.~~ **DONE.**
- ~~**B. A better `false` branch.** Same as A, but also change the *non-centred*
  cascade fallback to seed from the centred corner instead of the work-area origin (i.e. use
  `place_centered = TRUE`'s seeding with `find_first_fit` still in front of it).~~ **DONE**
  (2026-08-07). It turned out to be a deletion rather than an addition: with both call sites
  wanting the centred shape, `find_next_cascade`'s `centered` parameter had one live value, so the
  parameter and mutter's origin branch went with it. For the record, what was removed is a slot at
  `(max(0, work_area.x), max(0, work_area.y))` and a peer walk sorted by `x + y` (northwest-first)
  instead of by squared distance from the centred corner. Pinned by
  `placement_cascades_when_nothing_fits` and
  `placement_cascade_starts_a_new_column_when_it_overflows`, both of which now assert
  centre-seeded runs; the divergence note is in §3.
- **C. Minimal-overlap placement.** Replace the cascade fallback with a search scoring candidates
  by total overlap area (KWin's "smart" placement). Best results, most code, most drift, hardest
  to pin with a conformance test. **Not taken**, and B is the reason it can wait: the corner pile
  was the visible half of §1's complaint, and B moved it to the middle without inventing a
  placement algorithm GNOME's source could not be used to check. Revisit only if overlapping
  windows in the `false` branch actually grate in use.

Note both B and C only affect the branch taken when `center-new-windows` is `false`, which is not
GNOME's default — neither changes what a stock session does.

The one thing still owed is a real multi-monitor check of gap 2 on a seat with two outputs.
The **monitor selection** question that used to sit here is settled: gap 2 adopted mutter's
pointer monitor, so a window opens where the mouse is, matching a GNOME user's muscle memory.
