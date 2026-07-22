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
use smithay::backend::renderer::{Bind, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Size, Transform};

use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::utils::to_physical_precise_round;

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
    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((phys.w, phys.h)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
        paint(&mut frame, phys)?;
        let _sync = frame.finish()?;
    }
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
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
