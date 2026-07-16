use std::cell::OnceCell;

use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::memory::MemoryBuffer;
use super::{encompassing_geo, render_to_vec};
use crate::render_helpers::RenderTarget;

/// Snapshot of a render.
#[derive(Debug)]
pub struct RenderSnapshot<C, B> {
    /// Contents for a normal render.
    ///
    /// Relative to the geometry.
    pub contents: Vec<C>,

    /// Contents that are not blocked out, but the background is blocked out.
    ///
    /// If `None` then the background doesn't have any blocked-out surfaces, and normal `contents`
    /// can be used instead.
    pub contents_with_blocked_out_bg: Option<Vec<C>>,

    /// Blocked-out contents.
    ///
    /// Relative to the geometry.
    pub blocked_out_contents: Vec<B>,

    /// Where the contents were blocked out from at the time of the snapshot.
    pub block_out_from: Option<BlockOutFrom>,

    /// Visual size of the element at the point of the snapshot.
    pub size: Size<f64, Logical>,

    /// Non-blocked-out contents rendered into a renderer-neutral CPU buffer, captured eagerly at
    /// snapshot time through the owned Vulkan renderer (`Mapped::capture_neutral_vulkan`). The
    /// resize crossfade uploads this buffer to a `VkTexture`. `(buffer, encompassing geo)`; `None`
    /// unless captured.
    pub neutral: OnceCell<Option<(MemoryBuffer, Rectangle<i32, Physical>)>>,
}

/// A captured variant: its pixels, and the encompassing geometry they were rendered at.
pub type CapturedVariant = (MemoryBuffer, Rectangle<i32, Physical>);

/// A snapshot captured as renderer-neutral CPU buffers — one per block-out variant, each with its
/// own encompassing geometry (the blocked-out variant is a bare rect, so its box is genuinely
/// smaller than the contents' box, which includes the shadow).
///
/// Captured eagerly at snapshot time and uploaded to `VkTexture`s on demand.
#[derive(Debug)]
pub struct NeutralSnapshot {
    /// Contents for a normal render.
    pub contents: CapturedVariant,

    /// Contents that are not blocked out, but the background is.
    ///
    /// `None` means **not needed** (the background has no blocked-out surfaces) — never "the
    /// capture failed". A failed capture of a needed variant must throw the whole snapshot away:
    /// if the two were conflated, [`Self::variant`] would fall through to the unblocked `contents`
    /// and leak into a screencast exactly what block-out exists to hide.
    pub contents_with_blocked_out_bg: Option<CapturedVariant>,

    /// Blocked-out contents. `None` means **not needed**: `block_out_from` is `None`, so
    /// `should_block_out` is never true and this is never selected. Same rule as above.
    pub blocked_out_contents: Option<CapturedVariant>,

    /// Where the contents were blocked out from at the time of the snapshot.
    pub block_out_from: Option<BlockOutFrom>,

    /// Visual size of the element at the point of the snapshot.
    pub size: Size<f64, Logical>,
}

impl NeutralSnapshot {
    /// The variant to draw on `target`, and its index (stable, for keying an upload cache).
    ///
    /// This is the single point where the fail-closed block-out rule is enforced: `None` means
    /// there is nothing safe to draw — draw nothing. Notably it never falls back to the
    /// unblocked `contents` for a target that must be blocked out, and callers must never
    /// substitute another variant when this returns `None` or when their own upload of the
    /// chosen variant fails.
    pub fn variant(&self, target: RenderTarget) -> Option<(usize, &CapturedVariant)> {
        if target.should_block_out(self.block_out_from) {
            return Some((2, self.blocked_out_contents.as_ref()?));
        }

        if target != RenderTarget::Output {
            if let Some(contents) = &self.contents_with_blocked_out_bg {
                return Some((1, contents));
            }
        }

        Some((0, &self.contents))
    }
}

/// Vulkan-native neutral capture: import a surface tree through the owned Vulkan renderer and
/// render it into a renderer-neutral CPU [`MemoryBuffer`], at snapshot time.
///
/// Re-imports the surface tree directly through the Vulkan renderer via the already-generic
/// [`push_elements_from_surface_tree`] (a cache hit — the window has been compositing through this
/// renderer). `buf_pos` is the window-geometry origin (logical, negated), chosen so the resulting
/// `(buffer, geo)` places `tex_prev` at the same position the window occupied pre-resize.
///
/// Returns `None` on empty geometry or a render error; a needed variant that returns `None` must
/// fail the whole snapshot closed rather than be substituted (see [`NeutralSnapshot::variant`]).
pub fn capture_neutral_from_surface_tree(
    renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    buf_pos: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Option<(MemoryBuffer, Rectangle<i32, Physical>)> {
    use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;

    use crate::render_helpers::surface::push_elements_from_surface_tree;
    use crate::render_helpers::vulkan::VulkanRenderer;

    let _span = tracy_client::span!("capture_neutral_from_surface_tree");

    let mut elements: Vec<WaylandSurfaceRenderElement<VulkanRenderer>> = Vec::new();
    push_elements_from_surface_tree(
        renderer,
        surface,
        buf_pos.to_physical_precise_round(scale),
        scale,
        1.,
        Kind::Unspecified,
        &mut |elem| elements.push(elem),
    );

    let geo = encompassing_geo(scale, elements.iter());
    if geo.size.is_empty() {
        return None;
    }

    // Reverse + relocate exactly as `capture_neutral`: elements are pushed front-to-back but
    // `render_to_vec` draws front-to-back too, so the front-to-back push is drawn back-to-front
    // via `.rev()`, and the whole tree is shifted so `geo.loc` becomes the origin.
    let relocated = elements.iter().rev().map(|ele| {
        RelocateRenderElement::from_element(ele, geo.loc.upscale(-1), Relocate::Relative)
    });

    let fourcc = Fourcc::Abgr8888;
    match render_to_vec(
        renderer,
        geo.size,
        scale,
        Transform::Normal,
        fourcc,
        relocated,
    ) {
        Ok(data) => {
            let buffer_size = geo.size.to_logical(1).to_buffer(1, Transform::Normal);
            let buffer = MemoryBuffer::new(data, fourcc, buffer_size, scale, Transform::Normal);
            Some((buffer, geo))
        }
        Err(err) => {
            warn!("error capturing neutral snapshot buffer via Vulkan: {err:?}");
            None
        }
    }
}
