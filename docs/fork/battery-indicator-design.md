<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Dynamic battery indicator — design

A **divergence we are choosing**, from the GNOME design team's "Design Dreams"
post (2026-08-11) and the `battery-status/battery-status.png` mockup in
`Teams/Design/os-mockups`. It is not shipped GNOME behavior — 50.3 still draws a
16px `battery-level-*-symbolic` glyph — so this doc is the spec, and the mockup
plus the reference metrics below are the ground truth it derives from.

Assessment and rationale for taking this one: `dreams-assessment.md` §1.

## What replaces what

**Today (both GNOME 50.3 and us).** The battery is one square symbolic icon in
the status cluster. GNOME's `Indicator` (`js/ui/status/system.js:308-360`) sets
`gicon` from the power toggle and shows a separate `St.Label` of the percentage
beside it when `org.gnome.desktop.interface show-battery-percentage` is on;
`.power-status.panel-status-indicators-box { spacing: 0 }`
(`_panel.scss:150-152`) is the only styling. Ours is thinner still:
`system_status::battery_icon()` (`src/system_status.rs:336`) returns UPower's
`IconName` and a derived `battery-level-{0,10,..,100}[-charging]-symbolic`
fallback, and `src/ui/panel.rs:451-456` drops that into the generic cluster. We
do not read `show-battery-percentage` at all today (the only hit in `src/` is a
font-size comment, `quick_settings.rs:86`).

**Proposed.** A wide, self-painted indicator whose fill bar tracks the charge
continuously, with colour and shape carrying the warning state.

The point of the change: charge is a *continuous* quantity currently quantised
into ten glyphs that differ by a few pixels of interior fill. A fill bar reads at
a glance; the ten-step glyph does not.

## As built (2026-08-13) — read this before the sections below

Everything below was written *before* the thing existed. The seat corrected it in four ways, and
those corrections are the design now:

1. **Colour lives in the charge, not the housing.** The housing takes the panel foreground like
   every neighbouring glyph; only the fill is tinted. The first build tinted the whole indicator,
   which made being on mains — permanent on a desktop — the brightest thing in the panel. Critical
   is the single exception: it reddens everything.
2. **"On AC" is two states, not one.** `Charging` (current flowing) gets the bolt and the green;
   `PendingCharge` and `FullyCharged` get a **mains plug** and no tint at all. A laptop held at
   ~80% by a charge limit sits in `PendingCharge` indefinitely, so a bolt there claims a current
   that is not flowing.
3. **Every overlay is a composited icon with a rim and a shadow**, sized to *cross* the body's
   outline. A white glyph over a white fill is invisible — measured, not predicted: a plug over a
   charged battery could not be seen at all. The rim (a dilated silhouette, a second asset) fixes
   the contrast; the shadow makes the rim read as an outline rather than as part of the shape; the
   size stops the glyph reading as debris trapped in the charge.
4. **The critical glyph is an icon too**, not shaped text. That is what let it share the path
   above, and it took the text shaping out of the bake entirely.

Settled numbers, tuned by looking at rendered pixels at scale 1.25 (see `~/battery-states/`):
body 26x13, radius 4.5, stroke 1.5, **inset 3** (2.5 welded the fill to the shell), nub 2.5x6 at
**gap 0.5** (1.5 read as a detached tick), overlays **19** logical (**18** for the plug), shadow
1.14x at +1px, rim `#444C44` — midway between black and the grey the neighbouring icons' dim parts
read as (panel fg at Adwaita's 0.35 over the plate); black was the only hard outline in the cluster
— shadow the same `#444C44` at α0.3, green `$green_4` (`$green_3` shouted).

Assets: `battery-{bolt,cord,alert}-symbolic.svg` and a `-halo-` rim for each, all ours
(GPL-3.0-only), because no theme ships these alone — every themed battery bakes them into a whole
battery, and we draw our own body. The bolt and alert rims are the same path **stroked in the fill
colour**, since fill+stroke is a uniformly dilated silhouette; the plug's rim is hand-dilated rects
from before that trick was found, and should switch if that shape is ever touched.

**Validate with `debug-set-battery`**, not by waiting for hardware — and let the icon worker settle
(~1.5s) before screenshotting, or a glyph that merely has not rasterized yet reads as a missing
one. See `RUNNING.md`.

## Geometry

Anchored to the existing panel metrics so it sits correctly among its neighbours:
`QS_ICON` = 16 (GNOME's `$scalable_icon_size`), `QS_ICON_MARGIN` = 4,
`QS_BOX_SPACING`, `INDICATOR_H_PADDING` — all `src/ui/panel.rs:107,321-332`.

| | Logical px | Derivation |
|---|---|---|
| Body width | 26 | ~1.6 × `QS_ICON`; matches the mockup's ~2:1 body ratio at our icon height |
| Body height | 13 | Fits inside `QS_ICON`'s 16px box with 1.5px of air top and bottom |
| Body radius | 4.5 | Rounded rect, not a stadium — the mockup's corners are visibly tighter than h/2 |
| Shell stroke | 1.5 | `Painter::stroke_rounded` |
| Fill inset | 2.5 from the shell's outer edge | leaves a 1px gap inside the stroke |
| Fill radius | 2 | body radius − inset (4.5 − 2.5), so the fill's corners stay concentric |
| Nub | 2 × 5, radius 1, 1.5px to the right of the body | centred vertically |
| Total slot width | 29.5 | body + gap + nub |

**These numbers are a starting point, not a citation.** The mockup is a picture,
not a spec, and the only honest way to settle them is to render it and look:
build, run on the `gsrs` seat, screenshot the panel at native resolution, and
compare against the neighbouring wifi and volume glyphs for optical weight. Ratios
in a 26×13 box are the sort of thing that reads wrong by 1px and nobody can say
why. Expect to tune.

The fill bar's width is `inner_w × percentage`, floored at the fill *diameter*
(2 × 2 = 4px — a rounded rect narrower than its own corner diameter can't exist),
so a 1% charge is a visible lozenge rather than a degenerate sliver, and clamped
so 100% exactly fills.

## Colour

Panel foreground indicators on our dark bar take the *lighter* palette step, the
way `$privacy_indicator_color` does (`if($variant=='light', $orange_4, $orange_3)`,
`_panel.scss:4`). So step 3, not the `$warning_color`/`$error_color` background
tokens:

| State | Shell + fill | Palette |
|---|---|---|
| Discharging, normal | `TEXT` (panel white) | existing |
| Charging | `#33d17a` | `$green_3` |
| Low | `#f6d32d` | `$yellow_3` |
| Very low / critical | `#e01b24` | `$red_3` |

The shell and the fill take the same colour; the shell is never tinted
independently. At critical the *whole* indicator is red, per the mockup.

**Keep the "!" glyph at very-low.** My first instinct was to drop it as redundant
with the red — that is wrong, and worth writing down so it doesn't get
re-litigated. Colour as the sole channel is the accessibility anti-pattern; the
shape change is what carries the state for colour-blind users. Red is the
redundant half, not the glyph.

## State thresholds

Do **not** hardcode percentages. UPower already computes this, and its thresholds
are system policy, configurable per device.

Both enums below are read off `/usr/include/libupower-glib/up-types.h` on this
machine, not from memory:

- `WarningLevel`: 0 Unknown, 1 None, 2 Discharging, **3 Low**, **4 Critical**,
  **5 Action**, 6 Normal, 7 High, 8 Full.
- `State`: 0 Unknown, **1 Charging**, **2 Discharging**, 3 Empty,
  **4 FullyCharged**, **5 PendingCharge**, 6 PendingDischarge.

`BatteryStatus` (`src/system_status.rs:297`) is currently `{icon_name,
percentage}` and `read_battery()` (`src/dbus/system_status.rs:306`) reads only
`IsPresent`/`IconName`/`Percentage`. Add `warning_level: u32` and `state: u32`
(the latter distinguishes charging from discharging without string-matching
`icon_name`, which is what `battery_icon()` does today).

**Full state table.** Colour is decided by `State` first, then `WarningLevel`:

| `State` | Colour | Bolt | Note |
|---|---|---|---|
| 1 Charging | green | yes | actively charging |
| 5 PendingCharge | green | no | on AC, not drawing — **this VM's actual state right now** |
| 4 FullyCharged | green | no | on AC at 100% |
| 2 Discharging, 6 PendingDischarge | by `WarningLevel` | no | the interesting path |
| 3 Empty | red + "!" | no | |
| 0 Unknown | `TEXT` | no | fall back to plain |

Discharging, by `WarningLevel`: 3 Low → yellow; 4 Critical / 5 Action → red +
"!"; everything else → `TEXT`.

The on-AC rows are not hypothetical: this VM reports `state: pending-charge,
warning-level: none` at 79%. A naive `state == 1` test would render a
plugged-in machine as plain discharging white — which is exactly the bug the
table-driven conformance test exists to catch, so write the table first.

## OSD messages — BLOCKED on a decision, do not build yet

The mockup's three pills ("Low Battery 15%", "Battery Almost Empty 5%", "Device
Will Power Off Soon 1%") are the same `WarningLevel` transitions, and
`src/ui/osd.rs` already draws exactly this shape.

**But we would be the second source.** In stock GNOME these warnings are not the
shell's — gnome-settings-daemon's power plugin emits them as ordinary
notifications and owns the Action-level suspend. `gsd-power` **is running in
kov's live synoik session** (verified 2026-08-13; so are 16 other gsd
components, and our own `read_network` comment notes gsd-rfkill already owns
airplane mode). Build the pills naively and a low battery produces a gsd-power
banner *and* our OSD pill, from two processes, on the same transition.

So this needs a decision before any code:

- **A.** synoik owns the pills, and we filter gsd-power's battery notifications
  out of the notification stack by app-id. Nicer UI, but we're now suppressing
  another process's notifications by name, which is fragile.
- **B.** gsd-power keeps the warnings; we ship the indicator only. Zero risk,
  and the indicator's colour change already carries the state.
- **C.** Stop running gsd-power and own the whole power-warning path, including
  the critical-action suspend. Consistent, but that is a subsystem, not a pill.

Recommendation: **B for now**, revisit if the indicator alone proves too quiet.
The pills are the least valuable part of this design and the only part with a
cross-process interaction.

If we ever do A or C: fire on the **transition into** a level (UPower emits
`g-properties-changed` continuously), latch per level so a percentage wobble
across a boundary doesn't re-fire, reset the latch on charging, don't stack —
a new level supersedes the previous pill — and do **not** respect Do Not Disturb;
these are system-critical. The mockup's pills embed the live percentage and a
miniature of the same indicator, so reuse the widget at OSD scale rather than
adding a second drawing path.

## The actual prerequisite: the cluster has no per-element width

This is the work, and it is not the widget.

`qs_icon_x()` (`src/ui/panel.rs:472`) is index arithmetic over fixed-width slots
— `rect_x + INDICATOR_H_PADDING + QS_ICON_MARGIN + i * (QS_ICON + QS_ICON_GAP)`
— and its doc comment declares it the single source of truth for both the render
loop and the hit test, "or a scroll lands on a neighbour of the icon it looks
like it is over". `qs_cluster_width()` likewise assumes `n` identical icons.
Consumers: `panel.rs:1247` (app-indicator hit), `1389` (volume hit), `1740`
(app-indicator draw), `1766` (cluster draw), and the pinned test at `2200`.

A variable-width element breaks the *indexing scheme*, not one call site. So:

1. Replace `qs_indicator_icons()`'s `Vec<(candidates, color)>` with a
   `Vec<QsSlot>`, where a slot is `{ kind: Icon { candidates, color } |
   Battery(BatteryStatus), width: f64 }`.
2. Replace `qs_icon_x(rect_x, i)` with `qs_slot_x(&slots, rect_x, i)` — a prefix
   sum over slot widths. Same single-source-of-truth property, same doc comment,
   now correct for mixed widths.
3. `qs_cluster_width()` sums slot widths instead of multiplying.
4. Port the five call sites and rewrite the pinned geometry test.

Do this as its own commit, with the cluster still all-icons and the tests still
green, *before* adding the battery. If the refactor is landed separately, a
regression in the new indicator can't be confused with a regression in the
cluster arithmetic — and the volume-scroll hit test is exactly the kind of thing
that fails silently and gets noticed a week later.

## Toolkit

Per the toolkit-first rule, this is a `widget::` type, not a one-off in
`panel.rs`: a `widget::BatteryIndicator` carrying the metrics, with
`Painter::battery(&BatteryIndicator, ...)`. It composes from verbs we already
have — `stroke_rounded` for the shell, `fill_rounded` for the bar and the nub,
`text_px` for the "!" and the optional percentage.

### The charging bolt — DECIDED (2026-08-13): C, an embedded SVG

The bolt is the one part with no obvious home, and the first draft of this doc
was too glib about it ("add `Painter::bolt`"). The renderer's whole primitive set
is `render_rounded_rect`, `render_rounded_rect_faded`, `stroke_rounded_rect`,
`render_triangle` and the glyph paths (`vulkan/frame.rs:615-853`) — there is no
arbitrary-polygon verb. So there are three real options, not one:

- **A. Two `Painter::triangle` calls.** No new machinery, but a bolt assembled
  from two triangles is exactly the fill-full-plus-inner class of fake the
  toolkit-first rule warns about: it can't take a knockout outline, and the joint
  will show at some scale.
- **B. A new renderer primitive** (an SDF or polygon verb) behind
  `Painter::bolt`. Honest, reusable, and the only option that makes the bolt a
  first-class shape — but it is renderer work, so it owes a
  `SYNOIK_VK_VALIDATION=1` run.
- **C. Ship a bolt asset and load it through the icon path.** `IconCache::texture`
  already rasterizes and *tints* symbolic SVGs, which is how every other glyph in
  this cluster is drawn, so the bolt would compose exactly like its neighbours for
  free. Adwaita has no standalone bolt — every charging glyph bakes the bolt into
  a whole battery (`battery-level-*-charging-symbolic`) — so this means adding one
  small asset of our own.

**Decided: C.** It is not a deviation, it is the path we already have.
`resources/icons/` already holds 13 symbolic SVGs compiled in with
`include_bytes!`, resolved by `embedded_icon()` (`src/render_helpers/icon.rs:541`)
against the `EMBEDDED_ICON_NAMES` table a test walks, rasterized by **resvg** —
our own crate, no librsvg and no GObject — and tinted by `IconCache`. The bolt is
entry 14 on machinery that is built, tested and in the daily driver.

B would be justified if we wanted arbitrary shapes *generally*. We don't: we want
a glyph, and glyphs have a home. Adding a polygon rasterizer to the renderer to
draw one lightning bolt is the opposite of growing the toolkit for what the port
needs — and it would owe a `SYNOIK_VK_VALIDATION=1` run for the privilege.

**One genuinely new thing**, small but worth naming: all 13 current assets are
*bundled* from gnome-shell's gresource (icons in no theme on disk), carrying
upstream's GPLv2+. The bolt is the first icon we **author**, so it takes our
GPL-3.0-only terms and its own provenance note rather than the inherited ones.

Nothing else in the design needs a new verb: the shell is `stroke_rounded`, the
bar and nub are `fill_rounded`, the "!" and the percentage are `text_px`.

Everything is painted, so there is no offscreen and **no rounded clipping** — this
design does not reopen `clip_to_geometry` (`433d255f`).

## Percentage

The mockup does not show the percentage integrated into the indicator; the prose
asks for it. Keep it as GNOME has it — a separate label beside the widget — and
gate it on `org.gnome.desktop.interface show-battery-percentage`, which we must
start reading. Inline-in-the-pill makes the indicator very wide on a laptop
panel; if we want it later it is a variant of the same widget, not a redesign.

## Conformance

The corpus can't see colour, so pin the model, not the pixels:

- `(state, warning_level)` → colour role and glyph presence: a pure function,
  table-driven, **every row of the state table above**, including PendingCharge
  and FullyCharged.
- Fill fraction → bar width, including the 0%, 1% and 100% edges and the
  minimum-diameter floor.
- Slot layout: a cluster of `[icon, battery, icon]` places and hit-tests every
  element at the right x — the regression the refactor is most likely to cause.
- OSD latching, *only if* we take option A or C above.

Colour and optical weight are settled on the seat with a native-resolution
screenshot, not in the corpus.

## Order of work

1. Per-element-width cluster refactor, tests green, no behavior change.
2. `warning_level` + `state` onto `BatteryStatus` and `read_battery()`.
3. `widget::BatteryIndicator` + `Painter::bolt`, wired as a slot kind.
4. Screenshot on `gsrs`, tune the metrics.
5. `show-battery-percentage`.

Steps 1–4 are the feature; 5 is separable. **OSD pills are not on this list** —
they're blocked on the gsd-power decision above, and the recommendation is not
to build them.

Two open items to settle: the gsd-power question, and the metrics, which can
only be settled by looking at rendered pixels.
