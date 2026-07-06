use std::cell::OnceCell;

use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::memory::MemoryBuffer;
use super::{encompassing_geo, render_to_encompassing_texture, render_to_vec, ToRenderElement};
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
    /// snapshot time (see [`RenderSnapshot::capture_neutral`]). The Vulkan resize crossfade has no
    /// GLES renderer in its render path to lazily rasterize the GLES `texture` above, so it
    /// uploads this buffer to a `VkTexture` instead. `(buffer, encompassing geo)`; `None`
    /// unless captured.
    pub neutral: OnceCell<Option<(MemoryBuffer, Rectangle<i32, Physical>)>>,
}

impl<C, B, EC, EB> RenderSnapshot<C, B>
where
    C: ToRenderElement<RenderElement = EC>,
    B: ToRenderElement<RenderElement = EB>,
    EC: RenderElement<GlesRenderer>,
    EB: RenderElement<GlesRenderer>,
{
    /// Render the non-blocked-out contents into a renderer-neutral CPU buffer, once. Called at
    /// capture time (when a GLES renderer is still on hand, even on a Vulkan session) so the
    /// Vulkan resize crossfade — which has no GLES renderer at render time — can later upload it
    /// to a `VkTexture`. Mirrors the `else` branch of [`Self::texture`], minus the target-dependent
    /// blocked-out / blocked-out-bg variants (unwired on Vulkan; the crossfade uses plain
    /// contents).
    pub fn capture_neutral(&self, renderer: &mut GlesRenderer, scale: Scale<f64>) {
        self.neutral.get_or_init(|| {
            let _span = tracy_client::span!("RenderSnapshot::capture_neutral");

            let elements: Vec<_> = self
                .contents
                .iter()
                .map(|baked| {
                    baked.to_render_element(Point::from((0., 0.)), scale, 1., Kind::Unspecified)
                })
                .collect();

            let geo = encompassing_geo(scale, elements.iter());
            if geo.size.is_empty() {
                return None;
            }

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
                    let buffer =
                        MemoryBuffer::new(data, fourcc, buffer_size, scale, Transform::Normal);
                    Some((buffer, geo))
                }
                Err(err) => {
                    warn!("error capturing neutral snapshot buffer: {err:?}");
                    None
                }
            }
        });
    }

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
