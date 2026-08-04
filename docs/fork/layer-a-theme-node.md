# Layer A — the theme node (St box model, no CSS parser yet)

**Status:** design, approved to build (Gustavo 2026-07-24: "let's do layer A, migrate the dash").
**Relationship:** this is the next step of [`widget-layer-design.md`](widget-layer-design.md) — H1
(`bake`), H2 (`Painter` logical/pt verbs), H3 (`style` tokens + atoms) are done; Layer A is the
box-model node those verbs were always meant to feed. Advisor (Fable) sizing of the 50.1 St surface
drove the scope below.

## Why (the concrete trigger)

Porting each widget re-derives GNOME's box model by hand, per widget, from the SCSS + the Clutter/St
allocation semantics. That hand-derivation is the bug source. The running-dot fix (`1495990b`) is the
poster child: the dot is `y_align: END` against the icon button that **`y_expand`s to fill the whole
dash-background**, lifted `-$dash_padding` — so its reference edge is the *pill*, not the 76px icon
tile. Getting that wrong drew the dot on the icon; I mis-derived it four times before measuring a real
screenshot. A theme-node that models "this child fills its parent, that padding reserves a band" once,
correctly, removes the whole class.

## What St actually is (so scope stays bounded)

Per the 50.1 source (`~/Projects/gnome-shell/src/st/`): St is **not** a general CSS engine. Layout is
Clutter's job (BoxLayout/BinLayout — which we already hand-roll); CSS contributes only **border +
padding + sizing** into size negotiation, plus **paint** (background/border/shadow). Selectors are
trivial (type, `.class`, `#id`, descendant, `:pseudo`); the property set is small and closed
(`st-theme-node.c`); transitions are a whole-node paint crossfade; SCSS is compiled offline. So Layer A
is small, and the parser (Layer B) is a separable fast-follow.

## Layer A — the node

A resolved, typed **style bag** + the **box-model math** that consumes it. No parser, no cascade, no
retained tree, no signals — those are Layer B / later.

```rust
/// top, right, bottom, left — logical px (St resolves these per-side).
pub struct Edges { pub top: f64, pub right: f64, pub bottom: f64, pub left: f64 }

/// A resolved `.overview-icon`-style node: the subset of `st-theme-node.c` we
/// actually consume. Built today from typed constants; Layer B will produce the
/// same struct from the real stylesheet.
pub struct ThemeNode {
    pub padding: Edges,
    pub border: Edges,           // per-side width (usually uniform)
    pub border_color: Rgba,
    pub border_radius: f64,
    pub background: Option<Rgba>,
    // sizing hints St resolves (min/nat height, fixed width/height, spacing)
    pub width: Option<f64>,
    pub height: Option<f64>,
}

impl ThemeNode {
    /// Allocation → content box: subtract border + padding (St
    /// `_st_theme_node_adjust_for_border`/`_for_padding`).
    pub fn content_box(&self, alloc: Rect<Logical>) -> Rect<Logical>;
    /// Content size → allocation size: add border + padding (the preferred-size hook).
    pub fn allocation_for(&self, content: Size<Logical>) -> Size<Logical>;
    /// Paint background + border into `alloc` via the existing `Painter` verbs
    /// (`fill_rounded` / `stroke_rounded`). Replaces per-widget `fill_rounded` calls.
    pub fn paint(&self, p: &mut Painter, alloc: Rect<Logical>) -> anyhow::Result<()>;
}
```

Plus the **minimal cross-axis placement** helper the dash needs (and every future icon strip): given a
parent box, a child's natural size, and `{Fill, Start, Center, End}` on each axis (`+ y_expand`),
return the child's box. This is the ~40 lines of Clutter allocation semantics I kept getting wrong —
modelled once, tested against `st/test-theme.c`-style pins.

### Deferred from Layer A (cited, so we don't creep)
Transitions (St's is a paint crossfade — later), `background-image`/gradient slicing, box-shadow (we
already have `Painter::drop_shadow` + `bake_card_shadow`; fold in when a node needs it), the em/pt
resolution against `StThemeContext` scale (we have `ui::pt_to_px`/`PX_PER_PT`; generalize when a
node carries a font). Layout managers beyond the single-strip cross-axis helper.

## First migration — the dash

Model the dash's node stack explicitly, deriving every number `dash.rs` now hand-computes:

| GNOME node            | ThemeNode facts (cited)                                              |
|-----------------------|---------------------------------------------------------------------|
| `.dash-background`    | padding 12 t/b, radius `$dash_border_radius`, bg overlay (`_dash.scss:19-25`) |
| icon button (`.overview-tile`, reset) | `y_expand` fills the pill; padding-bottom 12 reserves the dot band (`_dash.scss:50-55`, `dash.js:150`) |
| `.overview-icon` (`%tile`) | padding 6, radius 16, flat button bg (`_common.scss:84-90`, `_dash.scss:60-63`) |
| icon glyph            | 64 (`iconSize`, `dash.js:321`)                                      |
| `.app-grid-running-dot` | 5×5, `y_align: END` in the pill-filling button, `offset-y: -12` (`_app-grid.scss:45-51`, `_dash.scss:72-78`) |
| `.dash-separator`     | 1px, side margin 4, height = iconSize (`_dash.scss:83-92`, `dash.js:813`) |

The migration replaces `PILL_H`/`tile_top`/`icon_center`/`dot_box`/`separator` hand-math with
derivations from these nodes + the cross-axis helper. **Behaviour must stay pixel-identical to the
now-correct `dash.rs`** (dot in the gap, icon centered, divider between groups) — this is a refactor
that proves Layer A, not a visual change. Gate: the existing dash unit + `vulkan_dash_*` render tests
stay green unchanged; add `ThemeNode::content_box`/cross-axis unit pins ported from `st/test-theme.c`.

## Layer B (fast-follow) — where it slots in

Once the `ThemeNode` bag shape has stabilized across the dash **and one more widget** (candidate: QS
tiles or the panel button — both already on `style` tokens), add a parser+cascade
(`cssparser` + `selectors`, mature Servo crates) that produces the **same** `ThemeNode` from the real
`gnome-shell.css`, and start deleting the typed-constant bags selector-by-selector. Not before: the
parser without a stable node target fixes nothing. Nesting: Layer B is additive — it only changes how a
`ThemeNode` is *sourced*, never how widgets consume it, so no widget re-touch.

## Rollout (slices, each a commit, all gates green)
- **A1.** ✅ DONE (`b12037d7`). `ThemeNode` + `Edges` + `content_box`/`allocation_for`/`paint` +
  `allocate_1d`/`allocate_align`, unit-pinned against the St formulas (`src/ui/theme_node.rs`). No
  widget wired; no visual change.
- **A2.** ✅ DONE (`d78086cc`). Dash pill modelled as a `DASH_BACKGROUND` `ThemeNode`: pill size from
  `allocation_for`, run origin from `content_box`, dot + divider from `allocate_1d`, pill background
  through `ThemeNode::paint`. Pixel-identical — existing dash unit + `vulkan_dash_*` render tests are
  the guardrail (pass unchanged) + a node⇄const pin. The icon tile stays `widget::AppIcon` (already the
  `.overview-icon` primitive); only the pill was unmodelled.
- **A3+.** Opportunistic: next widget touched (QS tile / panel button) adopts `ThemeNode`, stabilizing
  the bag shape for Layer B.
- **B1.** Parser+cascade feeding `ThemeNode` from `gnome-shell.css`; delete constants incrementally.

## Gates (per project policy)
`cargo fmt --all` / `cargo clippy --workspace --all-targets -D warnings` / `cargo test --workspace` /
`SYNOIK_VK_VALIDATION=1 cargo test --workspace` (exit 0, no `VULKAN ERROR`) on every render-touching
commit. Port commits assert **behavioural** invariants (geometry, alignment, pixel probes), not
byte-identical bakes.
