use std::cell::OnceCell;

use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
#[cfg(feature = "vulkan")]
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::memory::MemoryBuffer;
#[cfg(feature = "vulkan")]
use super::{encompassing_geo, render_to_vec};
use super::{render_to_encompassing_texture, ToRenderElement};
use crate::render_helpers::{RenderCtx, RenderTarget};

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

    /// Contents rendered into a texture (lazily).
    pub texture: OnceCell<Option<(GlesTexture, Rectangle<i32, Physical>)>>,

    /// Contents with blocked-out bg rendered into a texture (lazily).
    pub texture_with_blocked_out_bg: OnceCell<Option<(GlesTexture, Rectangle<i32, Physical>)>>,

    /// Blocked-out contents rendered into a texture (lazily).
    pub blocked_out_texture: OnceCell<Option<(GlesTexture, Rectangle<i32, Physical>)>>,

    /// Non-blocked-out contents rendered into a renderer-neutral CPU buffer, captured eagerly at
    /// snapshot time through the owned Vulkan renderer (`Mapped::capture_neutral_vulkan`). The
    /// Vulkan resize crossfade has no GLES renderer in its render path to lazily rasterize the
    /// GLES `texture` above, so it uploads this buffer to a `VkTexture` instead. `(buffer,
    /// encompassing geo)`; `None` unless captured — a Vulkan session leaves `contents` empty
    /// and fills only this.
    pub neutral: OnceCell<Option<(MemoryBuffer, Rectangle<i32, Physical>)>>,
}

/// A captured variant: its pixels, and the encompassing geometry they were rendered at.
pub type CapturedVariant = (MemoryBuffer, Rectangle<i32, Physical>);

/// A snapshot captured as renderer-neutral CPU buffers — one per block-out variant, each with its
/// own encompassing geometry (the blocked-out variant is a bare rect, so its box is genuinely
/// smaller than the contents' box, which includes the shadow).
///
/// The owned Vulkan renderer can't sample the GLES textures a [`RenderSnapshot`] bakes, so a Vulkan
/// session captures the same three variants this way instead and uploads them on demand.
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
    /// Mirrors the GLES selection in `ClosingWindow::render`.
    ///
    /// `None` means there is nothing safe to draw — draw nothing. Notably it never falls back to
    /// the unblocked `contents` for a target that must be blocked out.
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

impl<C, B, EC, EB> RenderSnapshot<C, B>
where
    C: ToRenderElement<RenderElement = EC>,
    B: ToRenderElement<RenderElement = EB>,
    EC: RenderElement<GlesRenderer>,
    EB: RenderElement<GlesRenderer>,
{
    pub fn texture(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        scale: Scale<f64>,
    ) -> Option<&(GlesTexture, Rectangle<i32, Physical>)> {
        if ctx.target.should_block_out(self.block_out_from) {
            self.blocked_out_texture.get_or_init(|| {
                let _span = tracy_client::span!("RenderSnapshot::texture");

                let elements: Vec<_> = self
                    .blocked_out_contents
                    .iter()
                    .map(|baked| {
                        baked.to_render_element(Point::from((0., 0.)), scale, 1., Kind::Unspecified)
                    })
                    .collect();

                match render_to_encompassing_texture(
                    ctx.renderer,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    &elements,
                ) {
                    Ok((texture, _sync_point, geo)) => Some((texture, geo)),
                    Err(err) => {
                        warn!("error rendering blocked-out contents to texture: {err:?}");
                        None
                    }
                }
            })
        } else if ctx.target != RenderTarget::Output && self.contents_with_blocked_out_bg.is_some()
        {
            let contents = self.contents_with_blocked_out_bg.as_ref().unwrap();
            self.texture_with_blocked_out_bg.get_or_init(|| {
                let _span = tracy_client::span!("RenderSnapshot::texture");

                let elements: Vec<_> = contents
                    .iter()
                    .map(|baked| {
                        baked.to_render_element(Point::from((0., 0.)), scale, 1., Kind::Unspecified)
                    })
                    .collect();

                match render_to_encompassing_texture(
                    ctx.renderer,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    &elements,
                ) {
                    Ok((texture, _sync_point, geo)) => Some((texture, geo)),
                    Err(err) => {
                        warn!("error rendering contents with blocked-out bg to texture: {err:?}");
                        None
                    }
                }
            })
        } else {
            self.texture.get_or_init(|| {
                let _span = tracy_client::span!("RenderSnapshot::texture");

                let elements: Vec<_> = self
                    .contents
                    .iter()
                    .map(|baked| {
                        baked.to_render_element(Point::from((0., 0.)), scale, 1., Kind::Unspecified)
                    })
                    .collect();

                match render_to_encompassing_texture(
                    ctx.renderer,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    &elements,
                ) {
                    Ok((texture, _sync_point, geo)) => Some((texture, geo)),
                    Err(err) => {
                        warn!("error rendering contents to texture: {err:?}");
                        None
                    }
                }
            })
        }
        .as_ref()
    }
}

/// Vulkan-native neutral capture: import a surface tree through the owned Vulkan renderer and
/// render it into a renderer-neutral CPU [`MemoryBuffer`], at snapshot time.
///
/// The Vulkan analogue of [`RenderSnapshot::capture_neutral`] — used on a Vulkan session so the
/// resize crossfade's pre-resize snapshot never touches GLES. Rather than re-render the GLES-baked
/// `contents` (whose `PrimaryGpuTextureRenderElement` is a no-op on Vulkan), it re-imports the
/// surface tree directly through the Vulkan renderer via the already-generic
/// [`push_elements_from_surface_tree`] (a cache hit — the window has been compositing through this
/// renderer). `buf_pos` is the window-geometry origin (logical, negated), matching
/// [`RenderSnapshot::capture_neutral`]'s geometry so the resulting `(buffer, geo)` places
/// `tex_prev` identically. Returns `None` on empty geometry or a render error, leaving the caller
/// free to fall back to the GLES path.
#[cfg(feature = "vulkan")]
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
