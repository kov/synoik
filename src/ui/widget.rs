//! Reusable widget-construction helpers shared by the popover/panel UIs.
//!
//! Every baking UI component (`input_source_menu`, `calendar`, `quick_settings`,
//! the dialogs, the panel bar, …) used to hand-roll the same offscreen-bake dance
//! — `create_buffer` → `bind` → `render` → `clear`/draw → `finish` →
//! `make_offscreen_sampleable` — behind its own subtly-different texture cache.
//! [`bake`] absorbs that dance once, keyed by `(scale, physical_size, revision)`
//! so a size change auto-invalidates (the calendar-height-freeze class of bug,
//! commit `128d112e`, cannot recur). See `docs/fork/widget-layer-design.md`.
//!
//! The `paint` closure draws in **physical** pixels for now; a later slice threads
//! a logical/pt `Painter` through it so no call site touches `scale` (H2 in the
//! design doc, the structural fix for the HiDPI-glyph bug class).

use std::collections::HashMap;

use anyhow::Context as _;
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{GlyphRun, VkTexture, VulkanFrame, VulkanRenderer};
use crate::utils::to_physical_precise_round;

/// Straight-alpha RGBA, the color type every draw verb takes (glyph coverage /
/// SDF alpha modulates it). Matches the `[f32; 4]` the frame primitives want.
pub type Rgba = [f32; 4];

/// Shared visual tokens, so the same color/icon-set is not re-declared per widget
/// (they drifted before: `HOVER_WASH` in 3 files, `TEXT`/`CHECK_ICONS` in more).
/// Only genuinely-shared, identically-valued tokens live here; widget-specific or
/// divergent values (a menu bg vs a tile bg, the separator alphas) stay local until
/// a port reconciles them against `docs/fork/gnome-style-reference.md`.
pub mod style {
    use super::Rgba;

    /// Fully transparent — the clear color for a rounded (transparent-corner) surface.
    pub const TRANSPARENT: Rgba = [0., 0., 0., 0.];
    /// Primary foreground (opaque white); glyph coverage modulates the alpha.
    pub const TEXT: Rgba = [1., 1., 1., 1.];
    /// Dimmed foreground (secondary labels, e.g. a row's short code).
    pub const MUTED: Rgba = [0.6, 0.6, 0.6, 1.];
    /// The hover highlight wash over a row/tile (a subtle lighten). NOTE: the
    /// lighten-vs-darken *direction* is per-widget (read from the SCSS cascade); this
    /// is the standard lighten used by menu rows / QS tiles / calendar days.
    pub const HOVER_WASH: Rgba = [1., 1., 1., 0.1];
    /// Icon-name fallback chain for an "active/selected" check mark.
    pub const CHECK_ICONS: &[&str] = &["object-select-symbolic", "emblem-ok-symbolic"];
}

/// Composite a symbolic icon — the first of `candidates` that resolves — centered
/// at `center` (relative to the element `origin`), sized `logical_px`, tinted
/// `color`. The single home for the icon-compositing helper that was copy-pasted
/// across the popover/panel UIs (`icon_element` ×2 + 3 inline sequences). Returns
/// `None` (logging on a GPU upload error) if no candidate resolves or the upload
/// fails, so callers can `if let Some(el) = …` and simply skip a missing glyph.
#[allow(clippy::too_many_arguments)]
pub fn icon_element<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Option<TextureRenderElement<VkTexture>> {
    let buffer = candidates
        .iter()
        .find_map(|name| icons.buffer(name.as_ref(), logical_px, scale, color))?;
    let tb = match TextureBuffer::from_memory_buffer(renderer, &buffer) {
        Ok(tb) => tb,
        Err(err) => {
            tracing::error!("error uploading widget icon: {err:#}");
            return None;
        }
    };
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        1.,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// A per-widget offscreen-texture cache for [`bake`], keyed by `(scale,
/// physical_size, revision)`. One lives (behind a `RefCell`) on each baking
/// widget; it clears itself when the renderer context changes.
#[derive(Default)]
pub struct BakeCache {
    context: Option<ContextId<VkTexture>>,
    // key: (scale, physical_w, physical_h) -> (revision, texture)
    textures: HashMap<(NotNan<f64>, i32, i32), (u64, VkTexture)>,
}

impl BakeCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Convert a widget's logical size to the physical buffer size at `scale`
/// (clamped to at least 1×1). The single home for that rounding.
pub fn physical_size(scale: f64, logical: Size<f64, Logical>) -> Size<i32, Physical> {
    Size::from((
        to_physical_precise_round::<i32>(scale, logical.w).max(1),
        to_physical_precise_round::<i32>(scale, logical.h).max(1),
    ))
}

/// Bake a widget's chrome into a scale-sized offscreen `VkTexture`, caching by
/// `(scale, physical_size, revision)`. On a cache hit the stored texture is
/// cloned (the GPU image is `Arc`-shared) and **neither** closure runs.
///
/// The physical buffer is `round(logical_size × scale)`. Two phases, run only on a
/// cache miss:
/// - `prepare(renderer)` shapes every `GlyphRun` and returns them (or any bake inputs). Glyph
///   shaping needs `&mut VulkanRenderer` and cannot run while the frame is alive, so it must happen
///   here, before the frame opens.
/// - `paint(frame, phys, prepared)` clears + draws everything into the bound [`VulkanFrame`] (the
///   full-buffer rect is `Rectangle::from_size(phys)`). Widgets clear with their own color —
///   transparent for rounded popovers, a border color for square dialogs.
pub fn bake<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    logical_size: Size<f64, Logical>,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<P>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;
    let phys = physical_size(scale, logical_size);
    let key = (scale_key, phys.w, phys.h);

    // The renderer context changing invalidates every cached GPU texture.
    let context = renderer.context_id();
    if cache.context.as_ref() != Some(&context) {
        cache.textures.clear();
        cache.context = Some(context);
    }

    let fresh = matches!(cache.textures.get(&key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let prepared = prepare(renderer)?;
        let tex = bake_uncached(renderer, scale, logical_size, |frame, phys| {
            paint(frame, phys, &prepared)
        })?;
        cache.textures.insert(key, (revision, tex));
    }

    Ok(cache.textures.get(&key).map(|(_, t)| t.clone()).unwrap())
}

/// A cache for [`bake_content`] — a content-sized bake whose physical size is not
/// known until its text is shaped, so it is keyed by `(scale, revision)` alone
/// (the revision determines the content, hence the size). Clears on context change.
#[derive(Default)]
pub struct ContentCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl ContentCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Bake a **content-sized** widget (a dialog/notification box whose size is derived
/// from its shaped text's ink, not known up front). Cached by `(scale, revision)`.
///
/// `prepare(renderer)` shapes the text and returns the physical buffer size plus a
/// layout value `P`; `paint(frame, phys, prepared)` draws it. Both run only on a
/// cache miss. The caller reads the returned texture's own size to place it (these
/// widgets center themselves on screen from the baked size).
pub fn bake_content<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut ContentCache,
    scale: f64,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<(Size<i32, Physical>, P)>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;

    let context = renderer.context_id();
    if cache.context.as_ref() != Some(&context) {
        cache.textures.clear();
        cache.context = Some(context);
    }

    let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let (phys, prepared) = prepare(renderer)?;
        let tex = bake_uncached_sized(renderer, phys, |frame| paint(frame, phys, &prepared))?;
        cache.textures.insert(scale_key, (revision, tex));
    }

    Ok(cache
        .textures
        .get(&scale_key)
        .map(|(_, t)| t.clone())
        .unwrap())
}

/// The offscreen dance for an already-known **physical** size (no logical→physical
/// step). Shared by [`bake_content`] and [`bake_uncached`].
fn bake_uncached_sized(
    renderer: &mut VulkanRenderer,
    phys: Size<i32, Physical>,
    paint: impl FnOnce(&mut VulkanFrame) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let (w, h) = (phys.w.max(1), phys.h.max(1));
    let mut target =
        renderer.create_buffer(Fourcc::Abgr8888, Size::<i32, BufferCoord>::from((w, h)))?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
        paint(&mut frame)?;
        let _sync = frame.finish()?;
    }
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}

/// Bake once with no caching — for widgets that re-draw every frame while
/// animating and bypass their cache (the panel workspace-dot morph, the QS pill
/// fill-fade). Same contract as [`bake`]'s `paint`.
pub fn bake_uncached(
    renderer: &mut VulkanRenderer,
    scale: f64,
    logical_size: Size<f64, Logical>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let phys = physical_size(scale, logical_size);
    bake_uncached_sized(renderer, phys, |frame| paint(frame, phys))
}

// --- H2: logical/pt drawing --------------------------------------------------------------------
//
// A widget describes its chrome in LOGICAL units and GNOME points; `TextShaper`
// and `Painter` perform the one and only `× scale` conversion internally. No
// widget draw site multiplies by scale again — the multiply that got forgotten
// (the minuscule-text bug `3c7473be`) no longer exists at any call site.

/// A text style: a GNOME point size (routed through [`crate::ui::pt_to_px`]) and
/// weight. Color is chosen at draw time (the same shaped run can be drawn in more
/// than one color), so it is not part of the style.
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    /// GNOME point size (e.g. 11 for `%heading`). NOT pixels.
    pub pt: f64,
    pub bold: bool,
}

impl TextStyle {
    pub fn new(pt: f64) -> Self {
        Self { pt, bold: false }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

/// A shaped, rasterized run ready to draw — produced by [`TextShaper::shape`] at a
/// specific scale, drawn by [`Painter::text`]. Opaque wrapper over the physical
/// [`GlyphRun`] so callers never touch physical glyph metrics directly.
pub struct ShapedText {
    run: GlyphRun,
}

/// One span of a styled paragraph: its text, family, weight, and GNOME point size.
/// The pt → physical conversion happens in [`TextShaper::paragraph`].
#[derive(Debug, Clone, Copy)]
pub struct ParagraphSpan<'a> {
    pub text: &'a str,
    pub mono: bool,
    pub bold: bool,
    /// GNOME point size for this span (spans may differ, e.g. a title vs body).
    pub pt: f64,
}

impl<'a> ParagraphSpan<'a> {
    /// A plain sans span at `pt`.
    pub fn new(text: &'a str, pt: f64) -> Self {
        Self {
            text,
            mono: false,
            bold: false,
            pt,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

/// A shaped, wrapped, multi-span paragraph — the dialog/notification text block.
/// Its ink metrics are **physical** (content-sized widgets lay out in physical px
/// directly); draw it with [`Painter::paragraph`].
pub struct ShapedParagraph {
    run: GlyphRun,
}

impl ShapedParagraph {
    /// Ink bounding box of the whole block, physical px: `(x, y, w, h)`.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        self.run.ink_bounds()
    }

    /// Ink bounding box of span index `i` (physical px) — e.g. to draw a keycap
    /// patch behind a monospace command span.
    pub fn span_ink_bounds(&self, i: u32) -> (i32, i32, i32, i32) {
        self.run.span_ink_bounds(i)
    }

    /// The shaped run, for drawing with [`Painter::paragraph`] or (content-sized
    /// widgets laying out in physical px) `VulkanFrame::render_glyphs` directly.
    pub(crate) fn run(&self) -> &GlyphRun {
        &self.run
    }
}

/// Shapes text at physical (`× scale`) pixels — the miss-only prepare phase (it
/// needs `&mut VulkanRenderer`, which the live frame holds, so shaping must happen
/// before the frame opens). Hand one to a widget's `prepare` closure.
pub struct TextShaper<'a> {
    renderer: &'a mut VulkanRenderer,
    scale: f64,
}

impl<'a> TextShaper<'a> {
    pub fn new(renderer: &'a mut VulkanRenderer, scale: f64) -> Self {
        Self { renderer, scale }
    }

    /// Shape one line. `style.pt` → logical px (`pt_to_px`) → physical px
    /// (`× scale`) — the single font-size conversion.
    pub fn shape(&mut self, text: &str, style: TextStyle) -> anyhow::Result<ShapedText> {
        let px = (crate::ui::pt_to_px(style.pt) * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_run_weighted(text, px, style.bold)?;
        Ok(ShapedText { run })
    }

    /// Shape a wrapped, center-aligned, multi-span paragraph. `wrap` is the wrap
    /// width in **logical** px; `base_pt` is the line-height reference point size.
    /// Every span's pt is converted the same way as [`Self::shape`] — no call site
    /// touches a physical font size.
    pub fn paragraph(
        &mut self,
        spans: &[ParagraphSpan],
        wrap: f64,
        base_pt: f64,
    ) -> anyhow::Result<ShapedParagraph> {
        use niri_vk::text::{SpanFamily, TextSpan};
        let to_px = |pt: f64| (crate::ui::pt_to_px(pt) * self.scale) as f32;
        let vk_spans: Vec<TextSpan> = spans
            .iter()
            .map(|s| TextSpan {
                text: s.text,
                family: if s.mono {
                    SpanFamily::Mono
                } else {
                    SpanFamily::Sans
                },
                bold: s.bold,
                px: to_px(s.pt),
            })
            .collect();
        let wrap_px = (wrap * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_paragraph(&vk_spans, wrap_px, to_px(base_pt))?;
        Ok(ShapedParagraph { run })
    }
}

/// Horizontal placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// How [`Painter::text`] anchors a run's ink box to its `at` point.
#[derive(Debug, Clone, Copy)]
pub struct Align {
    pub h: HAlign,
    pub v: VAlign,
}

impl Align {
    /// Left edge at `at.x`, vertically centered on `at.y` (a row label).
    pub const LEFT_MIDDLE: Align = Align {
        h: HAlign::Left,
        v: VAlign::Middle,
    };
    /// Right edge at `at.x`, vertically centered on `at.y` (a right-aligned label).
    pub const RIGHT_MIDDLE: Align = Align {
        h: HAlign::Right,
        v: VAlign::Middle,
    };
    /// Centered both ways on `at`.
    pub const CENTER: Align = Align {
        h: HAlign::Center,
        v: VAlign::Middle,
    };
}

/// A scale-correct drawing surface over a bound [`VulkanFrame`]. Every verb takes
/// **logical** coordinates/sizes (and points, for text); the single `× scale`
/// conversion lives here. Construct one inside a [`bake`] `paint` closure.
pub struct Painter<'a, 'frame, 'buffer> {
    frame: &'a mut VulkanFrame<'frame, 'buffer>,
    scale: f64,
    full: Rectangle<i32, Physical>,
}

impl<'a, 'frame, 'buffer> Painter<'a, 'frame, 'buffer> {
    /// `phys` is the full baked-buffer size (as handed to the `paint` closure); it
    /// scopes every draw's damage.
    pub fn new(
        frame: &'a mut VulkanFrame<'frame, 'buffer>,
        scale: f64,
        phys: Size<i32, Physical>,
    ) -> Self {
        Self {
            frame,
            scale,
            full: Rectangle::from_size(phys),
        }
    }

    fn px(&self, v: f64) -> i32 {
        to_physical_precise_round::<i32>(self.scale, v)
    }

    fn rect_px(&self, r: Rectangle<f64, Logical>) -> Rectangle<i32, Physical> {
        Rectangle::new(
            Point::from((self.px(r.loc.x), self.px(r.loc.y))),
            Size::from((self.px(r.size.w), self.px(r.size.h))),
        )
    }

    /// Clear the whole buffer to `color` (a transparent clear for rounded popovers,
    /// a border color for square dialogs).
    pub fn clear(&mut self, color: Rgba) -> anyhow::Result<()> {
        self.frame.clear(Color32F::from(color), &[self.full])?;
        Ok(())
    }

    /// Fill `rect` (logical) with `color`, corners cut by `radius` (logical; 0 = a
    /// plain rectangle, e.g. a separator rule).
    pub fn fill_rounded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        self.frame
            .render_rounded_rect(color, r, self.rect_px(rect), &[self.full])?;
        Ok(())
    }

    /// Draw a shaped run, anchoring its ink box to `at` (logical) per `align`,
    /// tinted `color`.
    pub fn text(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let (ix, iy, iw, ih) = shaped.run.ink_bounds();
        let ax = self.px(at.x);
        let ay = self.px(at.y);
        let ox = match align.h {
            HAlign::Left => ax - ix,
            HAlign::Center => ax - ix - iw / 2,
            HAlign::Right => ax - ix - iw,
        };
        let oy = match align.v {
            VAlign::Top => ay - iy,
            VAlign::Middle => ay - iy - ih / 2,
            VAlign::Bottom => ay - iy - ih,
        };
        self.frame.render_glyphs(
            &shaped.run,
            Point::from((ox, oy)),
            color,
            self.full,
            &[self.full],
        )?;
        Ok(())
    }
}

/// Test-only scale-sweep harness (H4 in the design doc). Bakes a widget at scales
/// {1.0, 1.5, 2.0} and asserts the buffer is physically `round(logical × scale)`
/// and that the glyph **ink area grows with the square of the scale** — the
/// assertion that would have caught the input-source popover's minuscule text
/// (`3c7473be`), where text shaped at logical px kept a constant glyph size at
/// every scale.
///
/// Ink *area* (bright-pixel count), not the ink bounding box: a widget's ink bbox
/// spans its top row to its bottom row, so its height tracks the buffer (row
/// layout) regardless of per-glyph size and cannot see shrunk glyphs. Glyph ink
/// area scales as `font_px²`, i.e. `scale²` when the text is correctly sized — so
/// scale-1→2 area ≈ 4×; the bug (constant font_px) leaves it ≈ 1×.
///
/// `bake_at` bakes the widget at the given scale and returns its texture; pass the
/// widget's scale-independent `logical_size`.
#[cfg(test)]
pub fn assert_scale_correct(
    vk: &mut VulkanRenderer,
    logical_size: Size<f64, Logical>,
    mut bake_at: impl FnMut(&mut VulkanRenderer, f64) -> VkTexture,
) {
    use smithay::backend::renderer::{ExportMem, Texture};
    use smithay::utils::Rectangle;

    // (scale, bright-pixel count) collected across the sweep.
    let mut ink: Vec<(f64, u64)> = Vec::new();

    for scale in [1.0, 1.5, 2.0] {
        let expected = physical_size(scale, logical_size);
        let mut tex = bake_at(vk, scale);

        let size = tex.size();
        assert_eq!(
            (size.w, size.h),
            (expected.w, expected.h),
            "scale {scale}: buffer size {size:?} != round(logical × scale) {expected:?}",
        );

        // Read the baked pixels back and count "ink" — pixels clearly brighter than
        // the dark widget background (text is near-white; the dark rounded bg and
        // the low-alpha separator stay well under the threshold).
        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let count = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count() as u64;
        assert!(
            count > 20,
            "scale {scale}: expected visible glyph ink, got {count} bright pixels",
        );
        ink.push((scale, count));
    }

    // The regression pin: ink area must grow ~scale². Correct text quadruples from
    // scale 1 to 2; text shaped at logical px (the bug) stays ~flat (≈1×), far below
    // the band. A wide band absorbs glyph-hinting/anti-aliasing noise while leaving
    // the 4×-vs-1× gap unmissable (per the review — no reliance on exact linearity).
    let ratio = ink[2].1 as f64 / ink[0].1 as f64;
    assert!(
        (2.5..=6.0).contains(&ratio),
        "ink area should grow ~4× (scale²) from scale 1 to 2, got {ratio:.2} \
         (counts {ink:?}) — a ratio near 1 means text was shaped at logical px \
         instead of physical (the HiDPI bug class)",
    );
}
