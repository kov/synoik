# Reusable widget helpers — design (descoped after review)

**Status (2026-07-25):** H4/H1/H2/H3 are **built and shipped** (slices A–C, §5); D+ ports are
ongoing. Sections §1–§4 below are the *design intent* written before implementation and are kept
for the reasoning; where the built code differs, **§9 is authoritative**.

> **Looking for how the bake actually works?** Read **§9 — The bake as built**. It covers the
> API family, the cache key and what each term is scar tissue for, the invalidations, why
> `prepare`/`paint` are split, and the measured cost model. §4/H1 is the older sketch and its
> signature and "makes it sampleable" step are both stale.

**Decision (2026-07-22):** build the **narrow, St-aware** fix — four focused, reusable helpers
that close the three recurring bug classes and de-duplicate the layer, shaped so they do **not**
foreclose a future St API for extensions, but **without** a retained widget tree, an allocate/paint
protocol, a signal model, or a unit-newtype system now. Those were dropped: they addressed a
speculative "St-shaped" constraint, not the actual bugs, and adopting them now would let a deferred
concern drive core architecture.

## 1. Why (the three bug classes)

Every UI component in `src/ui/` (16 files, ~19k lines; ~14 of them bake textures) is hand-built
from the raw Vulkan bake primitives — `create_buffer` → `bind` → `render` → `clear` →
`render_rounded_rect` / `render_glyphs` → `make_offscreen_sampleable` → wrap in a
`TextureRenderElement` — plus a per-widget `(scale, revision)` texture cache. There is no layer
between "the model" and "raw GPU verbs". Three failure modes follow, and we keep paying for all:

1. **Scale-correctness is per-call-site manual discipline.** Each widget hand-writes a `px()`
   closure (`let px = |v| to_physical_precise_round(scale, v)`) and multiplies font size by `scale`
   (`font_px = TEXT_PX * scale`, ~18 sites). Miss one multiply and you get a DPI bug — exactly the
   input-source popover's minuscule text (`3c7473be`), which shipped because the headless harness
   defaults to scale 1 so the bug is invisible in CI. Root cause: font sizes and glyph metrics
   travel as bare `f32`, and layout must *measure* at **unscaled** px while paint *rasterizes* at
   **scaled** px — same type, so the wrong one type-checks fine.

2. **Atoms are copy-pasted and drift.** The icon-compositing helper exists as 2 named
   `fn icon_element` (`input_source_menu.rs:368` — its own comment says "Mirrors the shared helper
   in quick_settings" — `quick_settings.rs:2219`) plus 3 inline sequences (`notification_card.rs:653`,
   `calendar.rs:1650`, `calendar.rs:2205`). `CHECK_ICONS` is declared identically twice
   (`input_source_menu.rs:58`, `quick_settings.rs:155`); `HOVER_WASH=[1,1,1,0.10]` three times;
   `TEXT=[1,1,1,1]` / `BOX_BG=[0.1,0.1,0.1,1]` across 3–10 files each. Close buttons, 1px
   separators (three impls, three color literals), pills, and rows are near-but-not-identical copies.

3. **Bake+cache lifecycle is re-implemented per widget.** The 6-step bake dance is copy-pasted in
   ~13 files, behind **seven** distinct cache-key schemes / five invalidation philosophies (§7).
   The calendar background froze at open-time height because its key omitted the popover height
   (`128d112e`); the fix had to be made locally because the cache is local to each widget.

We now have enough components to see the shared vocabulary. Extracting it buys visual + behavioral
consistency as the UI grows and closes the three modes by construction, not discipline.

## 2. St-aware, not St-shaped

Extensions may eventually be offered an St-style API (a future GJS binding). Per the user
(2026-07-22): St-shape is **not a hard constraint**, but we must not make that future
*unnecessarily* hard or impossible. Per `STRATEGY.md` (lines 55, §4): extensions are deferred and
"never drive core [architecture]." So the posture is **don't foreclose**, not **design around it**:

- Express styling as **data** (a small style/color struct passed in), not hardcoded color literals
  at each draw site — so a future CSS cascade can feed the same widgets.
- Keep a clean **model → layout → paint** separation inside each helper — so a future retained
  shell can wrap them.
- That is the whole cost. No retained tree, no allocate protocol, no signal model, no CSS engine,
  no AccessKit tree **now**.

**Strategy note (stale):** `STRATEGY.md` §3.6 imagined `st-toolkit` as a separate crate assembled
from Taffy + Vello + Parley + AccessKit + cssparser, and "prefer libcosmic/iced until it earns its
place." None of those crates are in the tree — we built our own hand-rolled Vulkan + cosmic-text
stack, and `accesskit` is already wired behind the `dbus` feature. So a future St API most
plausibly sits on **our** Vulkan bakes — the layer this doc cleans up — not on a Vello assembly.
`STRATEGY.md` §3.6 should be revisited to reflect that (out of scope here; noted).

## 3. Substrate facts (so the design is grounded)

- **One render context, uniform.** Every baking component — incl. the panel bar
  (`panel.rs:1490`) — bakes into an offscreen `create_buffer` texture and composites a
  `TextureRenderElement`. There is no direct-to-screen widget path to special-case.
- **Shape and paint are sequential, never simultaneous.** `build_glyph_run_weighted` takes
  `&mut VulkanRenderer` and returns an *owned* `GlyphRun` (Arc-shared atlas);
  `VulkanFrame` holds `renderer: &'frame mut VulkanRenderer` (`frame.rs:45`), so `render_glyphs`
  cannot run while shaping. Widgets already shape-all-then-draw (`input_source_menu.rs:211-230` →
  `:236-281`). **A nested offscreen sub-bake also needs `&mut renderer`, so it cannot open inside a
  live parent frame** — the calendar bakes its scrolled list as a *sibling* texture composited with
  a clip (`calendar.rs:1568`), never a child draw. The helpers must respect both: a `bake()` closure
  runs one frame to completion; anything needing a sub-texture bakes it *before* opening the outer
  frame and composites it as a sibling. (This is why we are **not** building a Painter-walks-a-tree
  model — it would break at the first clipping container.)
- **Some animations re-bake per frame and bypass the cache.** The panel morphs workspace dots every
  frame during a switch and "skip[s] the cache" (`panel.rs:1162`); the QS pill fill-fade is
  similar. So the bake helper must support an **uncached** mode, not only cache-on-miss.
- **Font sizing has one correct knob:** `ui::pt_to_px(pt)` (`ui/mod.rs:28`, `PX_PER_PT = 4/3`) →
  *logical* px. The missing, forgettable second step is logical → physical (`× scale`).

## 4. The four helpers

### H1 — `bake()`: the offscreen-bake + cache lifecycle (closes bug class #3)
One helper absorbs the 6-step dance and the cache. A component supplies its logical size, a
revision, and a paint closure; `bake()` sizes the physical buffer, binds, clears, runs the closure,
finishes, makes it sampleable, caches, and returns the `VkTexture` / `TextureRenderElement`.

```rust
// cache key = (scale, physical_size, revision); an uncached() variant skips the cache
fn bake(renderer, cache, scale, logical_size, revision, paint: impl FnOnce(&mut Painter)) -> VkTexture
fn bake_uncached(renderer, scale, logical_size, paint: impl FnOnce(&mut Painter)) -> VkTexture
```

- **Cache key = `(scale, physical_size, revision)`.** Size is folded in so a size change
  auto-invalidates (the `128d112e` freeze cannot recur). The review noted this can mask a broken
  `revision`; mitigation: `revision` should be **derived** (a hash/counter over every input that
  affects the bake) rather than hand-bumped, so it is correct on its own and size is pure insurance.
  **Landed 2026-07-26 as `widget::Revision`** — a builder (`.of()` / `.px()` / `.color()` /
  `.each()`) that folds the values the paint closure reads into the `u64`, so adding an input to the
  paint and adding it to the key are one edit at one place. Migrate opportunistically, starting with
  the sites that hand-pack bits; leave `end_session`'s `Sig` alone, since an equality tuple is
  strictly stronger than a hash where a widget already has one.
  Widgets that pre-bake variants (mru's 3 scope panels `:1729`, screenshot's show/hide `:109`) keep
  their own path — `bake()` does not force a single-variant model on them.
- **`uncached()`** covers the panel/QS per-frame animation bypass (§3) as a first-class mode.
- Absorbs the duplicated "context changed? clear" guard once.

### H2 — logical/pt drawing on `Painter` (closes bug class #1, structurally)
`Painter` wraps `(scale, &mut VulkanFrame)`. **Every verb takes logical units; the single `× scale`
conversion lives inside it, once.** Font sizes are **GNOME points**; `Painter` routes
pt → logical (`pt_to_px`) → physical (`× scale`). No component multiplies by scale again — the
multiply that got forgotten no longer exists at any call site.

```rust
impl Painter<'_> {
    fn fill_rounded(&mut self, rect: Rect<Logical>, radius: Logical, style: &Fill);
    fn separator(&mut self, rect: Rect<Logical>, style: &Fill);
    fn text(&mut self, run: &ShapedText, at: Point<Logical>, align: Align, style: &TextStyle);
    fn icon(&mut self, icon: &ResolvedIcon, at: Point<Logical>, tint: Rgba);
}
```

- **`Align` + `place_ink`** centralize the ink-bounds arithmetic (`cy - lh/2 - ly`, right-align
  `right - sw - sx`) re-derived at ~10 sites today.
- **Two-phase text.** Because shaping needs `&mut renderer` before the frame exists (§3), text is
  pre-shaped into a `ShapedText` (a `GlyphRun` + logical metrics) *before* `bake()` opens the frame,
  via a `TextShaper` (`shape(str, TextStyle) / measure(str, TextStyle) -> Size<Logical> /
  paragraph(...)`). `measure` returns **logical** size (unscaled); shaping bakes at **physical** —
  different methods, so the measure/raster asymmetry is no longer a silent `f32` footgun.
- **No unit-newtype trio.** A pt-only (logical-only) public API performs the one multiply
  internally; that closes the bug class without a `Pt`/`LogicalPx`/`PhysicalPx` system that would
  fight Smithay's existing `Logical`/`Physical` geometry markers. (Reviewer-endorsed.)
- **Line height derived from the font** (cosmic-text `round(px*1.25)`), retiring hardcoded
  `LINE_H=18` (`notification_card.rs:53`) and the `1.3`-vs-`1.25` drift (`calendar.rs:2303`).
- **Style as data** (`Fill`/`TextStyle` carry color, radius, weight) — the St-aware hook (§2): a
  future cascade feeds these structs; today they're built from the same consts, now shared (H3).

### H3 — shared atoms + a style/color module (closes bug class #2)
Small free helpers + one constants module — **not** widget objects with lifecycle:

- `icon_element(...)` — the single copy replacing the 2 fns + 3 inline sequences.
- `separator(...)`, `close_button(...)`, `checkmark(...)`, `pill(...)`, `hover_wash(...)` (carrying
  the per-widget lighten/darken *direction* — the hover-direction is read from the SCSS cascade, not
  assumed; see the hover-direction memory).
- `style` module: `TEXT`, `MUTED`, `BOX_BG`, `HOVER_WASH`, `TRANSPARENT`, `CHECK_ICONS`, the em
  helper, radius tokens — one cited home, replacing the 3–10× duplicated literals. Cross-referenced
  to `docs/fork/gnome-style-reference.md`.

### H4 — the scale-sweep test harness (the regression pin; the one part the review praised)
The DPI bug shipped because tests run at scale 1. A shared helper bakes any component at scales
**{1.0, 1.5, 2.0}** and asserts:
- buffer is physically `round(logical × scale)`;
- glyph ink is present and its bounding **height is scale-proportional, not scale-inverse** — a
  coarse band (scale-2 ink height in `[1.5×, 2.5×]` of scale-1), which cleanly catches the 4×-ratio
  DPI bug (2× expected vs ½× actual) without depending on sub-pixel hinting linearity;
- no panic; alignment invariants (centered stays centered, right-aligned stays flush) hold.

This lands **first**, wired to `input_source_menu`, as a permanent regression pin for `3c7473be`:
it fails on the pre-H2 path and passes after.

## 5. Rollout (tentative slices, each a commit, all gates green)
- **A. H4 + H1.** ✅ DONE (`a6027351`). Scale-sweep harness + `bake()` helper. Ported
  `input_source_menu` onto both; `popover_text_scales_with_output` is the `3c7473be` pin (verified
  it fails on a reinjected logical-px bug). No visual change.
- **B. H2.** ✅ DONE (`a17fd8f6`). `Painter` logical/pt verbs + `TextShaper`/`ShapedText` + `Align`.
  `input_source_menu` now describes its chrome purely in logical units + points; the last manual
  `× scale` is gone (pixel-identical by construction). `place_ink` is folded into `Painter::text`
  via `Align`.
- **C. H3.** ✅ DONE (`f2f20599`). `widget::icon_element` (one helper, replacing 2 fns + 3 inline
  copies) + `widget::style` token module (`TRANSPARENT`/`TEXT`/`MUTED`/`HOVER_WASH`/`CHECK_ICONS`).
  Ported **both** `input_source_menu` and `quick_settings` onto them (reuse exercised across two
  call sites); only identically-valued tokens promoted, divergent ones (menu vs tile bg, separator
  alphas) left local pending reference reconciliation. More atoms (`separator`, `checkmark`, `pill`,
  a `hover_wash` helper carrying the per-widget lighten/darken direction) remain to extract as
  further components are ported.
- **D+. Opportunistic ports.** calendar, quick_settings (fully), mru, dialogs — one commit each,
  only when otherwise touched. Un-ported components keep working on their current path (no
  big-bang).

## 6. Testing / gates
- Every ported component gets the H4 scale-sweep for free.
- Port commits assert **behavioral** invariants across scales (hit rects, alignment, ink presence)
  — **not** byte-identical bakes (immediate→helper rounding differs sub-pixel, and scale-1
  byte-equality can't see the DPI bug anyway).
- `cargo fmt` / clippy (`--features dbus,pipewire -D warnings`) / `cargo test --workspace` /
  `SYNOIK_VK_VALIDATION=1 cargo test --workspace` (exit 0, no `VULKAN ERROR`) on each render-touching
  commit, per project policy.

## 7. Inventory of what exists today (full sweep of `src/ui/*.rs`)

**Shared primitives everyone calls:** `ui::pt_to_px` (`mod.rs:28`); `build_glyph_run_weighted`
(single line, `renderer.rs:591`) / `build_glyph_paragraph` (multi-span wrapped, `renderer.rs:627`);
GPU-free `measure_line_width_weighted` / `wrap_lines_weighted` (`synoik-vk/text.rs:77,138`); frame ops
`render_rounded_rect` (`frame.rs:421`), `render_glyphs` (`:525`), `render_glyphs_spans` (`:541`),
`clear`; `GlyphRun::ink_bounds`. Two *unused-by-widgets* offscreen facilities exist
(`OffscreenBuffer`, `render_to_texture`); only the calendar list uses `render_to_texture`
(`calendar.rs:1568`) — everyone else hand-rolls the bake.

**Bake + cache.** 6-step dance copy-pasted in ~13 files. Seven cache-key schemes / five
philosophies: scale-keyed+`revision` counter (input_source_menu, run_dialog, calendar, quick_settings,
banner); scale-keyed no-revision + explicit `.clear()` (config_error `:60`); content-signature tuple
(end_session `Sig` `:101`); structural tuple + animating bypass (panel `(scale,width,count,active)`
`:1182`); bake-all-variants indexed by state (mru `:1729`, screenshot `:109`). Calendar `DateMenu`
packs `is_empty | clear_hover<<1 | height_key<<2` into the revision slot (`:2077`) — the height term
is the `128d112e` fix. Every file re-implements the "context changed? clear" guard.

**Text.** `build_glyph_run_weighted` for individually-placed colored runs; `build_glyph_paragraph`
for styled wrapped blocks (dialogs, hotkey, screenshot, mru scope). Divergence: end_session builds
one paragraph per element (one color per `render_glyphs` call) vs run_dialog/config_error's single
multi-span paragraph. Per-file font-size consts; `calendar.rs:61 ARROW_PX=18` is raw logical px, not
pt-derived. `render_glyphs_spans` used only by mru (`:1869`). Real wrap+ellipsis only in
notification_card; others use a `WRAP=100_000.` never-wrap sentinel.

**Rounded rects / backgrounds.** Two idioms: *rounded transparent-corner* (`clear(TRANSPARENT)` +
`render_rounded_rect`) for popover content; *square bordered box* (two `clear`s faking a border) for
modal dialogs (config_error, exit_confirm, end_session, hotkey, mru scope, screenshot — the last
notes these *could* move to `render_rounded_rect`). Circles/discs via `render_rounded_rect(_,
D/2*scale)`; panel derives radius from the already-physical `rect.h/2` — a different derivation.
Radius consts 14/16/18/20/24/36 + `_/2`, all independent. `BOX_BG=[0.1,0.1,0.1,1]` verbatim in ~9
files; quick_settings `[0.12,…]`; notification_card SCSS `#51515a`.

**Hit-testing.** Five approaches: row-index loop (input_source_menu); `Layout`/free rect fns
(calendar, quick_settings — click *order* matters); card-relative `CardLayout` shared banner/list
with divergent zone enums; static two-button (end_session, hover=focus); popover router
(`popover.rs:538`). Hover storage varies; the change-detect + `revision+=1` idiom is copy-pasted at
3+ sites. No explicit `(0,0)` warm-up inside `src/ui` — hover-clear is `pointer_hover(None)`.

**Layout/sizing.** `px`/`rect_px` closures duplicated verbatim (input_source_menu `:201-209` ⇄
notification_card `:402-408`). Glyph-centering re-hand-rolled everywhere (the `place_ink`
candidate). `em = pt_to_px(11)` thrice (banner 34em, calendar 29em, qs 12em). Hardcoded line
pitches coupled to font metrics. Three "even border" formulas.

**Revision.** Present in input_source_menu, both calendars, quick_settings, banner, run_dialog;
absent in config_error/exit_confirm (clear/static), end_session (sig tuple), panel (structural +
animating bypass), mru/screenshot (variant bake), hotkey (scale+context+config-clear).

**Atoms.** icon helper ×2 fns + ×3 inline; close button ×3 sizes; 1px separator ×3 impls/colors;
hover wash overlay (×3) vs bg-swap (notification_card) — two hover models; `CHECK_ICONS` ×2 verbatim;
pills/stadiums ×3 radius derivations; scrollbar only in calendar (reuses
`notification_card::stack_shadow_element` as thumb); rounded-corner opaque-region two-band
computation duplicated verbatim (`calendar.rs:2260` ⇄ `quick_settings.rs:1560`).

**Latent bug watchlist:** measure(unscaled)/raster(scaled) `f32` asymmetry (H2 fixes); font-metric-
coupled magic constants (H2 line-height fixes); screenshot's odd border formula at fractional
scales; mru/hotkey caches with no content revision (a new mutable field wouldn't invalidate).

## 8. Explicitly dropped after review (and why)
- **Retained widget tree + preferred-size/allocate protocol** — addresses none of the three bugs;
  needs a dirty-propagation protocol we'd have to invent; the nested-bake constraint (§3) breaks a
  Painter-walks-a-tree model at the first clipping container.
- **Signal/event model, CSS cascade engine, AccessKit tree** — St-shape scaffolding; deferred and
  charter-forbidden as core drivers now. (accesskit is already a dep for the D-Bus a11y bridge; a
  widget-tree a11y emission is a separate future effort.)
- **`Pt`/`LogicalPx`/`PhysicalPx` newtype trio** — fights Smithay's geometry markers; pt-only public
  API closes the bug class without it.
- **Byte-identical port gate** — won't hold across rounding changes and tests the un-broken scale-1
  case; replaced by cross-scale behavioral invariants (§6).

## 9. The bake as built (2026-07-25)

Authoritative description of the shipped helper. §4/H1 is the pre-implementation sketch; two of
its details did not survive contact (the `paint` signature, and a `make_offscreen_sampleable`
step that turned out to be unnecessary).

### 9.1 What a bake is

A **bake** renders a widget's chrome once into its own private GPU texture; every later frame
composites that texture as a single quad. `bake_uncached_sized` (`src/ui/widget.rs`) is the whole
operation:

```rust
let mut target = renderer.create_buffer(Abgr8888, phys)?;   // an offscreen texture
let mut fb     = renderer.bind(&mut target)?;
let mut frame  = renderer.render(&mut fb, phys, Normal)?;    // a VulkanFrame over it
paint(&mut frame)?;                                          // the caller draws
let _sync = frame.finish()?;                                 // submit + fence wait
```

**Why bake at all.** GNOME's chrome is expensive to draw and almost never changes. A quick-settings
popover is dozens of rounded rects, hairlines and shaped text runs whose pixels are identical frame
after frame. Redrawing per frame re-runs every draw call and re-shapes every string. Baking pays
that on a cache miss and reduces the steady state to one textured quad.

There is deliberately **no `make_offscreen_sampleable` call** afterwards: finishing a frame that
targets an offscreen already leaves it in `SHADER_READ_ONLY_OPTIMAL`, with the layout transition
riding that submit. The separate transition used to cost its own command buffer, submit and fence
wait — and as §9.5 shows, round trips are essentially the entire cost of a bake.

### 9.2 The family, and which widget uses which

Every entry point funnels into `bake_uncached_sized`, so the counter and timer
(`frame_log::time_bake`) live there and catch all of them.

| entry point | cache | used by |
|---|---|---|
| `bake` | `BakeCache`, key `(scale, phys_w, phys_h)` | app_grid, end_session_dialog, input_source_menu, overview_search, dash, window_preview |
| `bake_content` | `ContentCache`, key `(scale)` | run_dialog, config_error_notification, exit_confirm_dialog, hotkey_overlay |
| `bake_uncached` | none | notification_card |
| `bake_uncached_sized` | caller's own | calendar, screenshot_ui, mru, quick_settings, panel |
| `bake_card_shadow` / `_border` / `_fill` | `BakeCache` | popover, notification_banner |

`bake_content` exists because a dialog's size is **derived from its shaped text** and is not known
until `prepare` has run — so size cannot be part of its key, and the revision carries the whole
content identity instead. `bake_uncached` exists because some animations re-bake every frame by
design (the panel workspace-dot morph, the QS pill fill-fade); a cache would only be overhead.

### 9.3 The cache key, and what each term is scar tissue for

`bake`'s key is `(scale, physical_width, physical_height)`, with the stored value carrying
`(revision, texture)` — so a revision change **overwrites** its entry rather than accreting one.

- **`scale`** — a texture baked at scale 1 is simply wrong at scale 2.
- **physical size** — this term is the fix for `128d112e`. The calendar popover's background keyed
  on content shape alone, so when notifications arrived while it was open the background stayed
  frozen at its open-time height and the new cards drew *below* it. Folding the physical size in
  makes that class structurally impossible: a size change cannot hit a stale entry. It also means a
  broken `revision` is partly masked, which is why a revision should be *derived* from everything
  that affects the bake rather than hand-bumped.
- **`revision`** — content changes the size does not capture (hover state, text, counts).

### 9.4 The two invalidations that are not in the key

Both live on the cache, checked on every call, and both are bug scars:

- **`context: ContextId`** — textures belong to a renderer. A recreated renderer invalidates every
  one of them, and handing out the old handle samples an image destroyed with its device. Every
  texture cache in the tree carries this guard (`Wallpaper` was the last exception; fixed
  `b555d52f`).
- **`text_epoch`** — if a glyph upload fails, the atlas residency index is thrown away and
  re-rasterized. Anything baked *before* that holds **blank text**, under a key its widget has no
  reason to change — so a dialog title would stay blank for the life of the cache entry. The epoch
  moves on that recovery and drops the lot. See `VulkanRenderer::invalidate_glyphs`.

### 9.5 Why `prepare` and `paint` are separate

```rust
prepare: impl FnOnce(&mut VulkanRenderer)               -> Result<P>,
paint:   impl FnOnce(&mut VulkanFrame, Size<Physical>, &P) -> Result<()>,
```

Text shaping needs `&mut VulkanRenderer`, and a live `VulkanFrame` holds exactly that borrow
(`renderer: &'frame mut VulkanRenderer`). So all shaping must complete **before** the frame opens.
Rather than leave that as a rule to remember, the signature enforces it: `prepare` gets the
renderer, `paint` never sees it, and the borrow checker rejects the mistake. The same borrow is
what makes several other things safe by construction — nothing anywhere can upload a texture or
shape a run while any frame is open.

The same constraint means **a bake cannot nest**: a sub-bake also needs `&mut renderer`, so it
cannot open inside a live parent frame. Widgets that need a sub-texture (the calendar's scrolled
list) bake it as a **sibling** first and composite it with a clip — never as a child draw. This is
why the layer is not a Painter-walks-a-tree model; that shape breaks at the first clipping
container.

### 9.6 The cost model — a bake is round trips, not drawing

Measured on the seat, 2026-07-25 (`SYNOIK_FRAME_LOG`; see
[`foundation.md`](./foundation.md)). `time_bake()` wraps all of
`bake_uncached_sized`, which contains **two synchronous GPU round trips**:

1. `renderer.render(...)` → `VulkanFrame::begin` → `flush_glyph_uploads()` — a standalone submit
   **and fence wait** putting newly-shaped glyphs into the atlas.
2. `frame.finish()` — the offscreen submit, which CPU-waits.

| overview frame | bake total | glyph flush | offscreen submit | remainder (real CPU) |
|---|---|---|---|---|
| 13:45:00 | 23.32 ms | 10.71 | 10.77 | ~1.8 ms |
| 13:45:06 | 7.96 ms | 3.43 | 2.46 | ~2.1 ms |
| 13:45:07 | 7.78 ms | 3.57 | 2.04 | ~2.2 ms |

**A bake does ~2 ms of work and spends 6–21 ms waiting.** The 13:45:06/07 pair are one second
apart on an otherwise idle GPU, so their ~2–3.5 ms per round trip is Venus submit overhead with no
queued work behind it — that is the floor. Both frames land over the 16.67 ms budget *entirely* on
round trips.

Consequences worth knowing before optimising anything here:

- **Making `paint` cheaper is close to pointless.** The drawing is ~8% of a bake.
- **Cache hits are what matter**, because a hit costs zero round trips. This is why
  `hover_does_not_bump_the_bake_revision` (`c5336421`, `d396bd30`) mattered so much: hover was
  invalidating label bakes and re-shaping ~24 strings per pointer motion.
- **The remaining fix is structural, not local** — eliminate or defer the two submits. The glyph
  copy can ride the frame's own command buffer the way `record_pending_dmabuf_acquires` already
  does (zero submits instead of one); the offscreen fence wait is slice 1 in
  [`foundation.md`](./foundation.md). Note the bake runs during
  element *collection*, so its wait sits at the **start** of building a frame — the same pipelining
  the scanout deferral bought back at the end.

**Update, 2026-07-26 — both structural fixes landed, and the model's "why" was wrong.** Glyph
uploads now ride the frame's command buffer, and offscreen finishes defer to the in-flight list; on
a live seat the `offscreen` submit site's *wait* is now 0.00 ms (722 frames, 0.02 s parked in total
against 7.67 s over a comparable synchronous session). The 2–3.5 ms was never "Venus submit
overhead" in any GPU sense: a guest `vkWaitForFences` never enters the kernel, it is mesa's
`vn_relax` userspace poll — 160 µs rungs, ~291 µs actually slept after guest timer slack, against a
complete host round trip of 0.076 ms (`foundation.md` §5). So the cost was the *waiting*, not the
submitting, and not waiting removes it outright.

What survives unchanged is the conclusion that matters here: **a cache hit is the whole game, and
making `paint` cheaper is close to pointless.** If anything that is now stronger — a controlled
sweep on 2026-07-26 puts a draw call's fixed GPU cost under ~1.5 µs
(`perf_probe_what_does_a_draw_call_cost`), so a widget's *drawing* is not where its milliseconds
are. They are in shaping, in allocation, and in whatever forces a re-bake.

## 10. What the 2026-07-26 session added (challenges, and what they argue for)

A day of frame-cost work on the live seat, most of which turned into widget-layer findings. Recorded
here because each one is an argument about the *layer*, not about the widget it surfaced in.

### 10.1 A hand-bumped revision is a proxy, kept somewhere else, checked by nothing

§H1 said `revision` should be derived rather than hand-bumped and left it as a mitigation. The
inventory is worse than that phrasing suggests: **21 revision fields across `src/ui/`, bumped at 36
call sites**, in the seven schemes §7 catalogues. Every cache bug this project has had is the same
defect — the counter and the paint disagreeing — and it has now bitten in both directions:

- **Bumped when nothing baked changed** → a full GPU round trip on every frame of an animation. The
  panel re-baked its entire bar for the length of *every* overview animation, because opening the
  overview checks the Activities button and its fill fade made `are_animations_ongoing()` true; the
  code comment still described the intent the code had lost (`009213dd`). The app grid and the
  overview search re-shaped every visible label on every pointer motion because hover bumped
  `content_rev` (`c5336421`, `d396bd30`).
- **Not bumped when something did** → a stale texture living as long as its content. The calendar
  popover's background froze at open-time height while the list grew under it (`128d112e`).

`widget::Revision` (`df8deb7a`) folds the values the paint closure reads into `bake`'s `u64`:

```rust
let revision = Revision::new().of(!list.is_empty()).of(hover).of(height_key).done();
```

It does not make either mistake impossible. It moves the mistake to where it is visible — the inputs
are listed immediately above the closure that reads them, so adding an input to the paint and adding
it to the key are one edit in one place. `px()` normalizes NaN (a NaN hashing as itself would miss
the cache on *every* frame — the expensive failure, arriving silently) and folds `-0.0` to `0.0`.
Migrate opportunistically; leave `end_session`'s `Sig` alone, since an equality tuple is strictly
stronger than a hash where a widget already has one.

### 10.2 The per-frame re-bake is a bug *class*, and it is invisible to every ordinary test

Pixels are identical — a bake and its cache produce the same image. Frame counts are identical.
End-state tests never sample a running animation. The only thing that differs is how many GPU round
trips ran, so the instrument had to be built before the class could be tested at all
(`728c2394`: `time_bake` is `#[track_caller]`, so a frame line reads
`1 bakes in 1.39ms (ui/panel.rs:1540 ×1)`, and sites are recorded even with logging off so tests can
read them).

Guarded so far: the overview animation, the workspace switch (with the panel dot morph pinned as a
*known* exception — its geometry interpolates, so no cached texture can serve it), the
quick-settings and calendar open fades, and the app-grid open. **All are currently clean.** The
`#[track_caller]` chain through every `ui::widget` helper is load-bearing and degrades silently — a
break anywhere collapses every widget onto one helper line — which is why
`bake_sites_name_the_widget_not_the_helper` exists.

### 10.3 Two ways a widget test proves nothing, both of which were live

Both were caught by mutation-testing tests that were already green, and both are now checked inside
the shared helpers rather than left to each test:

1. **Sampling static frames.** Six frames with no animation running satisfy "nothing re-baked"
   perfectly. A `step` larger than the animation, or an action that never started one, makes the
   whole family green forever. `bake_sites_per_frame` now asserts that at least two sampled frames
   had an animation in flight.
2. **The subject never renders.** A widget that draws nothing re-bakes nothing on every frame. The
   app-grid guardrail passed — including against a mutation that changed its label bake key on
   *every call* — because the grid was empty. Each test now asserts its subject baked at least
   once while open, and prints the sites it *did* see when it did not.

### 10.4 A data-driven widget has an invisible empty state, and nothing in the render path objects

The grid was empty because **`AppGrid` does not read `app_system`**. `Synoik::sync_app_grid()` copies
the entries across, and it runs from app-system *change events* — so installing a catalog straight
onto `Synoik` leaves the grid holding nothing. An empty grid lays out no tiles, contributes no
elements, and is indistinguishable from a healthy one that happens not to be on screen.

That is the layer speaking. Three sibling widgets source their content three different ways with
three different sync triggers (`sync_app_grid`, `sync_dash_favorites`, `sync_overview_search`), and
there is no shared notion of "this widget has content" that a test, the renderer, or a frame log
could observe. Every consumer has to know each widget's private wiring to know whether silence means
*correct* or *broken*.

### 10.5 What this argues about §8

§8 dropped the retained tree and the preferred-size/allocate protocol on the grounds that they
address none of the three bug classes and would need a dirty-propagation protocol we would have to
invent. Two of those grounds have moved:

- The **dirty protocol is no longer hypothetical** — 21 hand-rolled revision counters *are* one,
  written 21 times, and §10.1 is the bill for that.
- A **fourth bug class** now has evidence behind it (§10.4): content ownership and the empty state
  are per-widget conventions, which is exactly what a base class exists to remove.

This is recorded as evidence, not as a reversal — the nested-bake constraint in §3 that broke a
Painter-walks-a-tree model is unchanged, and none of the above is an argument for a CSS cascade, a
signal system, or an accessibility tree. If the retained tree is argued back in, it should be argued
on §10.1 and §10.4, and it should say what it does about §3.

## 11. Which retained model, if one comes back: CoreAnimation, not Clutter (2026-07-27)

§10.5 left the retained tree open and set the bar: argue it on §10.1 and §10.4, and say what it does
about §3. This section does not schedule the work — it settles **which model we would copy**, so
that when a piece of it lands it lands facing the right direction.

**The boundary, agreed with Gustavo:** CoreAnimation for the **layer tree, animation and
compositing**; St/Clutter for **layout and style**. Not a compromise — the two halves are answering
different questions, and each reference is better at the one it keeps.

### 11.1 Why CA for the animation half

Clutter is what St is written against, so the instinct is to copy Clutter throughout. But we are not
porting Clutter's *implementation*, we are porting GNOME's *behavior*, and on the animation side
Clutter's API is the part St works around rather than the part it relies on. CA's model earns its
place by killing bug classes we have actually paid for, not by being tidier:

- **Model layer vs presentation layer** is the structural fix for the animated-geometry class: a lerp
  whose endpoints depend on the running progress bends its path, and we have fixed that by hand each
  time it appeared. Put the target on the model layer and let only the presentation layer move, and
  the endpoints are frozen *by construction* rather than by remembering to freeze them.
- **Animatable properties live apart from contents** (opacity, transform, position vs the backing
  store). That is precisely the fix pattern §10.2 applies by hand — "put the animated alpha on the
  element, not in the bake" — promoted from a thing you must know to a thing the type system says.
  The per-frame re-bake class exists because a widget's animated property and its baked content are
  the same bag of fields; CA's split is that bag, separated.
- **Property setters imply the dirty**, which is §10.1's twenty-one hand-bumped revision counters
  written once, in the substrate, instead of twenty-one times in the widgets.
- **Content ownership stops being a per-widget convention** (§10.4): a layer either has contents or
  it does not, and that is observable from outside the widget.

### 11.2 What it does about §3 — the constraint that killed the Painter tree

§3 rejected a Painter-walks-a-tree model because a nested sub-bake needs `&mut renderer` and cannot
open inside a live parent frame, so the model breaks at the first clipping container. **CA does not
have that problem, because CA never draws a tree in one pass.** A CALayer owns its own backing store,
drawn independently; the tree is walked by the *compositor*, over already-backed layers. Clipping is
`masksToBounds` on a layer that is composited, not a painter state pushed mid-walk.

That is a description of what our widgets already do — the calendar bakes its scrolled list as a
sibling texture and composites it with a clip. So the CA model is the one retained model that fits
§3's substrate rather than fighting it, which is the strongest single argument here: it is not a new
constraint to satisfy, it is a name for the shape the code already has.

### 11.3 What stays St, and why

Layout and style do not move. The box model, theme nodes, `allocate_1d`/cross-axis placement, the
CSS-ish cascade feeding one struct — that is where GNOME fidelity lives, it is what
[`layer-a-theme-node.md`](./layer-a-theme-node.md) is already building against the real
`st-theme-node.c`, and CA has no equivalent to translate onto. A layer tree underneath, St on top.

This does not weaken the fork tenet: "GNOME's way replaces niri's" is about observable behavior and
the config surface, not about which internal architecture we borrow to produce that behavior.

### 11.4 The one thing to watch

CA's implicit animations are *implicit* — a property set inside a transaction animates with the
transaction's defaults. GNOME's timings are explicit and specified (durations and easing curves per
widget, cited in [`gnome-style-reference.md`](./gnome-style-reference.md)). If we adopt the
mechanism and inherit CA's defaults with it, we drift from the reference in exactly the way that is
hardest to see: everything still animates, nothing looks obviously wrong, and every duration is
subtly not GNOME's. **The transaction defaults must be ours.** Whatever lands first should pin a
GNOME-cited duration and curve in a test, so the default cannot quietly become Apple's.
