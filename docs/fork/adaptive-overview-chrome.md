# Adaptive overview chrome

**Status: implemented 2026-07-28** (pieces 1-5; the panel stays exempt by Gustavo's call —
"begin by leaving the panel alone", to be revisited once the rest has shrunk around it). The mode
gate landed as `ce9325c1`; the canvas this was written for turned out to be the one the fork runs
on daily (the internal display, 2048x1330 @ 2 = **1024x665 logical**), so it was judged there
rather than hypothetically. Trigger and evidence: `overview-port.md` §S11.

**Open questions settled at implementation.** (A) Reference canvas: **1280x800** as proposed —
the live canvas lands at ramp 0.8, which steps the dash one ladder rung and reads right. (B) Ramp
shape: linear, not snapped — no mushiness observed. (C) Layer B: untouched, as planned.

**Method note.** Judging this needs a canvas, and `--headless` hardcoded 1920x1080; it now takes
`SYNOIK_HEADLESS_MODE=WxH`, so the harness reproduces any display exactly (mode + `synoik msg output
… scale`) and the chrome can be eyeballed on the shape that is actually wrong.

## Decision

Everything in the **overview** adapts to the output's logical canvas **except the top panel and
text sizes**. gnome-shell itself is *not* adaptive — its chrome is fixed logical constants from
the SCSS (`$base_border_radius: 8px`, `_common.scss:33`), a fixed dash icon
(`this.iconSize = 64`, `dash.js:321`), a fixed 30px workspace-background radius
(`workspace.js:30`), fixed spacing clamps (`workspacesView.js:22-23`) — and it looks fine only
because GNOME assumes a logical canvas ≥ roughly 1280×720. The S11 incident put us on a 1024×665
canvas and produced: near-circular workspace corners (30px radius on a ~290-logical-tall
preview), picker gaps pegged at their 80px max, a 24px app-grid icon under a 64px dash icon, and
center-clipped grid labels. The mode gate makes that canvas unlikely, not impossible; this design
makes any canvas degrade gracefully.

Why panel and fonts are exempt:
- **Panel**: user decision — it is the one fixed landmark, and its height is already a work-area
  input everything else measures against.
- **Fonts**: text size is a readability constant per scale, not a proportion of the screen;
  GNOME's knob for it is `text-scaling-factor` (accessibility), never canvas size. Ramping text
  would make small canvases *less* usable. Labels keep their size and instead ellipsize (see
  piece 6).

## Mechanism: one ramp, two derivation rules

A single **chrome ramp** factor per output, computed in one place (`ui/overview_layout.rs`, next
to the band layout it feeds) and carried in the layout struct so it is inspectable through the
same model as the rest of the overview state (control-plane tenet, STRATEGY §2):

```
r = clamp(min(logical_w / 1280, logical_h / 800), 0.5, 1.0)
```

- Never exceeds 1.0: a huge canvas gets GNOME's exact constants; this divergence only *shrinks*.
- Floor 0.5 keeps hit targets and rings from vanishing on absurd canvases.
- Reference canvas 1280×800 = the smallest shape GNOME's fixed constants visibly tolerate
  (open question A below).

Two rules decide how each piece adapts — do not mix them per-widget ad hoc:

1. **Self-derived**: an element whose *box already scales with the canvas* derives its radii and
   internal spacing from its own box, not from `r`. (A workspace thumbnail is 5% of the screen by
   construction; its corner radius should be a fraction of the thumb height so the shape is
   scale-invariant.)
2. **Ramped**: standalone chrome whose box is a fixed logical constant multiplies that constant
   by `r`. (The dash, the entry, the spacing clamps.)

## Piece inventory

### Already adaptive — no change

- **App grid modes + icon ladder** — `ui/app_grid.rs:70-76,344` (`iconGrid.js` port): page mode by
  aspect, largest of `[96,64,48,32,24,16]` that fits, spacing distributes slack. Verified working
  in the S11 measurements (ladder correctly collapsed 96→24 on the tiny canvas).
- **Workspace preview slot fitting** — the `overviewControls`/`workspacesView` allocation port;
  the preview is computed from the actual boxes.
- **Thumbnail height** — 5% of the porthole (`layout/thumbnails.rs:13`, `MAX_THUMBNAIL_SCALE`).
- **Picker gap formula core** — `layout/monitor.rs:1562` `workspace_gap` is allocation-driven;
  only its clamps are fixed (piece 3).

### Piece 1 — workspace background corner radius (small)

`WORKSPACE_BACKGROUND_CORNER_RADIUS = 30.` (`layout/monitor.rs:69`, faithful to
`workspace.js:30`; SCSS `.workspace-background` `_window-picker.scss:56-60`). Rule 1
(self-derived): radius = `30 × (preview_h / 800)` clamped to `[8, 30]`, where `preview_h` is the
settled picker-state preview height — computed inside the existing single choke point
`workspace_background_radius()` (`monitor.rs:1207`), which the wallpaper AND the shadow both
already read (they once diverged and left pointy shadow tabs; keep it that way). The matching
box-shadow ramps for free.

### Piece 2 — thumbnail strip roundness + spacing (small)

- Wallpaper/thumb radius `6.` logical (`monitor.rs:2580`, `radius = 6. / strip.scale`) and the
  indicator ring radius that it is defined as half of. GNOME: `$base_border_radius * 0.5`
  (`_workspace-thumbnails.scss:12`). Rule 1: radius = `6 × (thumb_h / 54)` (54 = 5% of the 1080
  reference), derived from the strip's own scale so a thumb is the same *shape* on every canvas.
- Inter-thumb `SPACING = 8.` (`layout/thumbnails.rs:20`): same self-derivation from thumb height.
- The indicator border width stays fixed (it is a focus affordance, like a focus ring — minimum
  visibility beats proportionality). Decide during implementation if it looks heavy at small
  thumb sizes.

### Piece 3 — picker gap clamps (small)

`WORKSPACE_MIN_SPACING = 24.` / `WORKSPACE_MAX_SPACING = 80.` (`layout/monitor.rs:64-65`,
faithful to `workspacesView.js:22-23`; the S11 canvas pegged the gap at 80 → "comical" spacing).
Rule 2: clamp bounds × `r` in `workspace_gap` (`monitor.rs:1571`). The vertical branch
(`view_h × 0.1 × zoom`, `monitor.rs:1573`) is already proportional — leave it.

### Piece 4 — dash (medium)

`ICON_PX = 64.` (`ui/dash.rs:56`, `dash.js:321`) plus the interlocked constants that feed layout:
`TILE = icon+2·pad`, `ITEM_ADVANCE`, `PILL_PAD_H/V`, `PILL_H`, `PILL_RADIUS = 28.`,
`PREFERRED_HEIGHT` (consumed by `overview_layout`), `SEPARATOR_H`, `DOT_PX`. GNOME's own ladder
`baseIconSizes = [16, 22, 24, 32, 48, 64]` (`dash.js` `_adjustIconSize`) only engages on
*overflow*; we additionally pick the largest ladder step ≤ `64 × r`, then let overflow shrink
further exactly as GNOME does. Pill paddings/radius/dot/separator derive proportionally from the
chosen icon size (rule 1, with the icon as the box). This piece is "medium" because
`PREFERRED_HEIGHT` flows into `overview_layout`'s band reservation and the existing dash tests
pin the 64-px geometry — the constants become functions of the icon size, and the tests gain a
second canvas shape. Note `dash.rs:76-78` documents that we take GNOME's height cap but keep the
icon size (pill may overflow on short screens) — piece 4 subsumes that divergence with a real
shrink.

### Piece 5 — search entry width (trivial)

`ENTRY_WIDTH = 352.` (`ui/overview_search.rs:65`) and `Entry::HEIGHT` via
`PREFERRED_ENTRY_HEIGHT` (`overview_search.rs:74`). Rule 2: `width = min(352, 352 × r … canvas_w
− margins)`, height × `r` with the entry's font *not* ramped (exempt) — so the entry shrinks to
its text plus padding at the floor. `STATUS_CARD_W = ENTRY_WIDTH` (`overview_search.rs:103`)
follows automatically; verify the results-strip layout at the floor.

### Piece 6 — app-grid label ellipsize (small, do regardless)

Labels currently center-clip (`waita De` for `Adwaita Demo` in the S11 grid screenshot); GNOME
end-ellipsizes. Change the grid tile label from centered-clip to end-ellipsis in
`ui/app_grid.rs`. Not a ramp item — correct at every canvas — but it is the remaining "squished
grid" symptom, so it ships with slice 1.

### Explicitly exempt

- Top panel (height, font, indicator sizes) — user decision.
- All text (entry text, grid/dash labels, search results) — `text-scaling-factor` is the knob.
- Interactive hit targets keep a floor: nothing interactive may ramp below ~24 logical px even at
  `r = 0.5` (window-preview close button, dash dots as click affordances).

## Testing

Headless `Fixture` pins at three canvas shapes (the harness reproduces arbitrary mode+scale — see
S11 method note; scale-sweep tests are the established pattern):

1. **1920×1080 @ 2** (the 4K seat): `r = 1` — every constant must equal GNOME's; this pins that
   the divergence is invisible on normal canvases (byte-stable against today's rendering).
2. **1638×1064 @ 1.25** (the internal screen post-mode-gate): mild ramp; sanity ratios.
3. **1024×665 @ 2** (the S11 degenerate shape): assert shape-invariance — thumb radius/thumb_h
   and preview radius/preview_h match shape 1's ratios; picker gap < the un-ramped 80 max; dash
   ladder stepped down; entry ≤ ramped width; grid labels ellipsized not clipped.

Guard the ramp itself with a unit test (monotone, clamped, `r(1280×800)=1`).

## Sequencing

- **Slice 1 (the trio + clamps, ~a day incl. tests):** pieces 1, 2, 3, 6. Purely visual, each a
  localized constant→function change; no layout-band interactions.
- **Slice 2 (dash + entry, 1–2 days):** pieces 4, 5 — the ones that feed `overview_layout`'s
  band reservations; land together with the shape-2/3 pins.
- Both after the mode-gate merge, so live judgement happens on real canvases first.
- Standing cost to accept: every future overview widget port must decide "ramped, self-derived,
  or exempt" — add that line to the port checklist when reviewing.

## Open questions (decide at implementation)

- **A. Reference canvas** 1280×800 vs 1366×768 — pick after eyeballing the post-fix internal
  screen; only changes where the ramp starts biting.
- **B. Ramp shape**: linear in `min(w-ratio, h-ratio)` is proposed; if mid-ramp canvases look
  mushy, snap `r` to quarter steps so element families move together.
- **C. Layer B interaction**: when the cssparser cascade (layer-a-theme-node.md B1) lands, the
  ramp could become a theme-node input so SCSS-derived paddings/radii ramp uniformly; until then
  it stays a plain factor in `overview_layout` with one call site per constant. Do not block on
  this.

## Addendum: the app grid fills the canvas (approved 2026-07-31)

**Status: implemented.** `AppGrid::fill_scale` (`src/ui/app_grid.rs`), pinned by
`a_roomy_canvas_scales_the_page_mode_up`, `a_short_library_does_not_stretch_into_a_strip` and
`folders_keep_gnomes_fixed_3x3`.

The ramp above only ever *shrinks* chrome, so it says nothing about a canvas with room to spare.
The app grid had the opposite problem. GNOME picks one of four fixed `(columns, rows)` modes
(`defaultGridModes`, `iconGrid.js:30-47`) and turns whatever is left into spacing — which is
capped at `max-column-spacing: $base_padding * 6`. Past a certain canvas the remainder is simply
dead margin.

**Decision.** Scale the chosen mode up, bounded by **two** ceilings, taking the lower:

1. *What the canvas allows* — one factor for both axes, so the mode keeps its shape.
2. *What the content needs* — `sqrt(n / mode_capacity)`, square-rooted because the factor applies
   to both axes, so capacity grows with its square.

Clamped at 1, so filling only ever adds tiles. The icon ladder is untouched: filling adds tiles,
it never inflates or shrinks one.

**Both ceilings are load-bearing, and each was learned the hard way.**

- **Dropping the shared factor** (filling each axis independently) maximises tile count and ruins
  the shape. On a canvas with far more horizontal than vertical room — a 4K at scale 1 — the
  columns run away while the rows do not.
- **Dropping the content bound** produces the same strip from the other direction, and this is the
  subtler one: a 4K page sized only to the canvas becomes 16x11, but a normal 34-app library fills
  three rows of it, leaving ~1400px of dead space between the grid and the dash. *Capacity is not
  free.* A page far larger than its contents is worse than a second page, because the layout
  reserves the full row count and the content sits at the top of it.

A corollary that looks like a bug but is the rule working: where one axis has no room for another
cell, the factor is 1 and nothing changes. A 1920x1080 band is exactly that — it fits a ninth
96px column but no fourth row, so it stays at GNOME's 8x3.

**Watch the fit asymmetry.** Cell fitting is inclusive across and *strict* down, matching
`iconGrid.js:395`. Getting that wrong offers a row the icon step then refuses, and the grid
answers by shrinking every icon a rung — which reads as "the fill made my icons smaller".

**Folders are exempt.** `FolderGrid` deliberately never re-flows to its box
(`appDisplay.js:2077-2082`) and its dialog is a fixed modal, not a canvas to fill.

**Consequence to know about:** a denser grid has less vertical slack between rows, so a hovered
tile's expanded caption grows about the tile's centre rather than only downward. Tests that sampled
a caption row computed from the *resting* tile top now miss it — count lines of ink instead.
