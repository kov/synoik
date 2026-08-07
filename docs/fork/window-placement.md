<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Initial window placement

Where a newly-mapped floating toplevel lands, what mutter 50.3 does, where our port
diverges, and which knobs are worth turning.

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

Gaps, in the order I'd fix them:

1. **`center-new-windows` is never read, and we assume the wrong default.** `place_new_tile`'s
   doc comment says it reproduces mutter "with the default preferences (`center-new-windows` …
   off)" — that default flipped to `true` in mutter 48. Our own vendored copy of the schema
   already carries `<default>true</default>`
   (`resources/schemas/org.gnome.mutter.gschema.xml:96`), and we already bind that schema
   (`gnome.rs:2478`) for `overlay-key` and `edge-tiling`; nothing reads the key. **This is the
   single fix for the symptom in §1** — see §4 for the behavior owed. Needs the
   `window_place_centered` branch and the `place_centered` cascade variant (place.c:206-244).
2. **Monitor selection is the active monitor, not the pointer monitor.** `AddWindowTarget::Auto`
   resolves to `*active_monitor_idx` (`layout/mod.rs:1016`); mutter uses the pointer's monitor for
   first-shown windows (place.c:954). Real, user-visible difference on multi-monitor: launch
   something from a keybinding with the pointer parked elsewhere and it lands on a different
   screen than GNOME would pick. Worth deciding deliberately rather than inheriting.
3. **`auto-maximize` is unconditional.** `auto_maximize_if_too_big` always runs in GNOME mode
   (`handlers/compositor.rs:362-368`); mutter gates it on the pref (schema line 86, default true).
   One-line fix, same binding as (1).
4. **Our obstacle set is every tile.** `find_first_fit` builds `others` from all
   `self.data` (`floating.rs:1712-1717`); mutter's `rectangle_overlaps_some_window`
   (place.c:503-548) counts only `NORMAL`, `UTILITY`, `TOOLBAR`, `MENU` — dialogs, modal dialogs,
   docks and splash screens are **not** obstacles. With an open dialog we fall through to the
   cascade where mutter would still find a fit, which makes the top-left pile-up in §1 *worse*
   than upstream.
5. **The denied-focus placement path (step H) is not ported.** We compute `denied_focus_steal`
   (`handlers/compositor.rs:176`) but only use it to set urgency (`:349`); there is no re-place
   against the focus window and no `find_most_freespace`. Low priority — it only bites when a
   background app maps a window while you're working.
6. **No test coverage.** `place_new_tile`, `find_first_fit` and `find_next_cascade` have no
   conformance test; `src/tests/floating.rs` covers sizing only. The table in §1 came from a
   throwaway probe. Any change here should land the probe as a real test first.

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

**The default.** Ship GNOME's (`true`, §4) and stop there? Or keep the current behavior reachable
and diverge?

- **A. Pure fidelity.** Read the key, default `true`. Windows centre. Turning it off in
  dconf/Tweaks gets today's behavior back, bugs 2-5 fixed.
- **B. Fidelity plus a better `false` branch.** Same as A, but also change the *non-centred*
  cascade fallback to seed from the centred corner instead of the work-area origin (i.e. use
  `place_centered = TRUE`'s seeding with `find_first_fit` still in front of it). Gives a mode
  GNOME does not have: try not to overlap, and when that fails pile up from the middle rather
  than the corner. Costs one bool and a divergence note.
- **C. Minimal-overlap placement.** Replace the cascade fallback with a search scoring candidates
  by total overlap area (KWin's "smart" placement). Best results, most code, most drift, hardest
  to pin with a conformance test.

My recommendation: **A now** — it is a bug fix, it needs no approval, and it may well be the
whole answer. Live with it for a few days; if the loss of overlap-avoidance (§4, second bullet)
grates, **B** is a small follow-up and only affects the non-default branch.

**Monitor selection** (gap 2) is independent and does need a call: adopt mutter's pointer monitor
for first-shown windows, or keep our active monitor deliberately? Ours is arguably better with
focus-follows-keyboard workflows; mutter's is what a GNOME user's muscle memory expects.
