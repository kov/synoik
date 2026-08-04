use std::array;
use std::cell::RefCell;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::backend::renderer::{Color32F, Texture as _};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use synoik_config::CornerRadius;
use synoik_vk::render::PostprocessPush;

use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::effect_buffer::EffectBuffer;
use crate::render_helpers::vulkan::{pack_mat3, VulkanError, VulkanFrame, VulkanRenderer};
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::region::TransformedRegion;

#[derive(Debug)]
pub struct Xray {
    // The buffers are per-render-target to avoid constant rerendering when screencasting.
    pub background: [Rc<RefCell<EffectBuffer>>; RenderTarget::COUNT],
    pub backdrop: [Rc<RefCell<EffectBuffer>>; RenderTarget::COUNT],
    pub backdrop_color: Color32F,
    pub workspaces: Vec<(Rectangle<f64, Logical>, Color32F)>,
}

/// Position for drawing xray background.
#[derive(Debug, Clone, Copy)]
pub struct XrayPos {
    /// Position of geometry relative to the backdrop in zoomed coordinates.
    ///
    /// Should be upscaled by `zoom` to get position in backdrop coordinates.
    pub pos_in_backdrop: Point<f64, Logical>,

    /// Zoom factor between backdrop coordinates and geometry.
    pub zoom: f64,
}

impl XrayPos {
    pub fn new(pos_in_backdrop: Point<f64, Logical>, zoom: f64) -> Self {
        Self {
            pos_in_backdrop: pos_in_backdrop.downscale(zoom),
            zoom,
        }
    }

    pub fn offset(mut self, offset: Point<f64, Logical>) -> Self {
        self.pos_in_backdrop += offset;
        self
    }

    /// Scale the geometry this describes by `factor` **about its own origin**, which
    /// stays put in backdrop coordinates.
    ///
    /// For a caller that draws an element and then wraps it in a
    /// `RescaleRenderElement` anchored at the same origin — the overview's window
    /// picker does exactly that, per preview. Without this the xray samples the
    /// backdrop for the element's *unscaled* size: the backdrop then shows at the
    /// wrong scale, and where the unscaled rect overhangs the workspace,
    /// [`Xray::render`]'s crop drops that part entirely and the element draws no
    /// backdrop at all there.
    ///
    /// A non-positive or non-finite factor would put an infinity into the position,
    /// so it is left alone: that only arises from a zero-width slot, whose preview
    /// has nothing to draw anyway.
    pub fn scale(mut self, factor: f64) -> Self {
        let zoom = self.zoom * factor;
        if !zoom.is_finite() || zoom <= 0. {
            return self;
        }
        // Hold the origin fixed in backdrop coordinates while the zoom changes.
        self.pos_in_backdrop = self.pos_in_backdrop.upscale(self.zoom).downscale(zoom);
        self.zoom = zoom;
        self
    }
}

impl Default for XrayPos {
    fn default() -> Self {
        Self {
            pos_in_backdrop: Point::new(0., 0.),
            zoom: 1.,
        }
    }
}

#[derive(Debug)]
pub struct XrayElement {
    buffer: Rc<RefCell<EffectBuffer>>,
    id: Id,
    geometry: Rectangle<f64, Logical>,
    src: Rectangle<f64, Buffer>,
    subregion: Option<TransformedRegion>,
    input_to_clip_geo: Mat3,
    clip_geo_size: Vec2,
    corner_radius: CornerRadius,
    scale: f32,
    blur: bool,
    noise: f32,
    saturation: f32,
    bg_color: Color32F,
}

/// Prepare an [`EffectBuffer`] through the renderer `ctx` wraps. Returns whether the offscreen is
/// ready to be sampled by the pushed [`XrayElement`]s.
fn prepare_effect_buffer(
    renderer: &mut VulkanRenderer,
    buffer: &mut EffectBuffer,
    blur: bool,
) -> bool {
    buffer.prepare_vulkan(renderer, blur)
}

impl Xray {
    pub fn new() -> Self {
        Self {
            background: array::from_fn(|_| Rc::new(RefCell::new(EffectBuffer::new()))),
            backdrop: array::from_fn(|_| Rc::new(RefCell::new(EffectBuffer::new()))),
            backdrop_color: Color32F::TRANSPARENT,
            workspaces: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        ctx: RenderCtx,
        params: RenderParams,
        xray_pos: XrayPos,
        blur: bool,
        noise: f32,
        saturation: f32,
        push: &mut dyn FnMut(XrayElement),
    ) {
        let zoom = xray_pos.zoom;
        let pos_in_backdrop = xray_pos.pos_in_backdrop.upscale(zoom);

        let (clip_geo, corner_radius) = params
            .clip
            .unwrap_or((params.geometry, CornerRadius::default()));

        let clip_offset = clip_geo.loc - params.geometry.loc;
        let clip_pos_in_backdrop = pos_in_backdrop + clip_offset.upscale(zoom);

        let geo_in_backdrop = Rectangle::new(pos_in_backdrop, params.geometry.size.upscale(zoom));

        let mut backdrop = self.backdrop[ctx.target as usize].borrow_mut();
        let backdrop_geo = Rectangle::from_size(backdrop.logical_size());
        let intersection_with_backdrop = backdrop_geo.intersection(geo_in_backdrop);

        let mut skip_backdrop = intersection_with_backdrop.is_none();

        let mut background = self.background[ctx.target as usize].borrow_mut();
        let prev = background.commit();
        if prepare_effect_buffer(ctx.renderer, &mut background, blur) {
            if background.commit() != prev {
                trace!("background damaged");
            }

            let clip_geo_size = Vec2::new(clip_geo.size.w as f32, clip_geo.size.h as f32);
            let buf_size = background.logical_size();

            for (ws_geo, bg_color) in &self.workspaces {
                // If the background color is opaque, check if the workspace fully covers the
                // element. In this case, we will skip the backdrop element since it's fully
                // covered.
                //
                // FIXME: also implement some way to check if the background elements are fully
                // covered in opaque regions, and not just the synoik background color is opaque
                let crop = if bg_color.is_opaque() && ws_geo.contains_rect(geo_in_backdrop) {
                    skip_backdrop = true;
                    // No need to intersect, we know it's fully covered.
                    Some(geo_in_backdrop)
                } else {
                    ws_geo.intersection(geo_in_backdrop)
                };

                let Some(crop) = crop else {
                    continue;
                };

                // If crop contains the intersection with backdrop, then the workspace fully
                // covers the backdrop, so we can skip the backdrop.
                //
                // This can happen when the overview is closed (so workspaces align left/right with
                // the backdrop) and the window is peeking out off screen to the side. In this
                // case, this off-screen part is on top of nothing, neither workspace nor backdrop,
                // but since the window doesn't fully cover the workspace, the check above doesn't
                // skip the backdrop.
                if bg_color.is_opaque()
                    && intersection_with_backdrop
                        .is_some_and(|backdrop| crop.contains_rect(backdrop))
                {
                    skip_backdrop = true;
                }

                // This can be different from zoom for surfaces that do not scale with
                // workspaces, e.g. layer-shell top and overlay layer.
                let ws_zoom = ws_geo.size / buf_size;

                let src = Rectangle::new(crop.loc - ws_geo.loc, crop.size).downscale(ws_zoom);
                let src = src.to_buffer(background.scale(), Transform::Normal, &buf_size);

                let buf_size = Vec2::new(buf_size.w as f32, buf_size.h as f32);
                let pos_against_buf = (clip_pos_in_backdrop - ws_geo.loc).downscale(ws_zoom);
                let pos_against_buf = Vec2::new(pos_against_buf.x as f32, pos_against_buf.y as f32);
                let ws_zoom_vec = Vec2::new(ws_zoom.x as f32, ws_zoom.y as f32);
                let input_to_clip_geo = Mat3::from_scale(ws_zoom_vec / zoom as f32)
                    * Mat3::from_scale(buf_size / clip_geo_size)
                    * Mat3::from_translation(-pos_against_buf / buf_size);

                let mut geometry =
                    Rectangle::new(crop.loc - geo_in_backdrop.loc, crop.size).downscale(zoom);
                geometry.loc += params.geometry.loc;

                let elem = XrayElement {
                    buffer: self.background[ctx.target as usize].clone(),
                    id: background.id().clone(),
                    geometry,
                    src,
                    subregion: params.subregion.clone(),
                    input_to_clip_geo,
                    clip_geo_size,
                    corner_radius,
                    scale: params.scale as f32,
                    blur,
                    noise,
                    saturation,
                    bg_color: *bg_color,
                };
                push(elem);
            }
        }

        // If the backdrop is fully covered by opaque background, we can skip it.
        if skip_backdrop {
            return;
        }

        let prev = backdrop.commit();
        if prepare_effect_buffer(ctx.renderer, &mut backdrop, blur) {
            if backdrop.commit() != prev {
                trace!("backdrop damaged");
            }

            let buf_size = backdrop.logical_size();
            let src = geo_in_backdrop.to_buffer(backdrop.scale(), Transform::Normal, &buf_size);

            let mut clip_geo_in_backdrop = Rectangle::new(clip_offset, clip_geo.size).upscale(zoom);
            clip_geo_in_backdrop.loc += geo_in_backdrop.loc;

            let clip_pos_in_backdrop = Vec2::new(
                clip_geo_in_backdrop.loc.x as f32,
                clip_geo_in_backdrop.loc.y as f32,
            );
            let clip_geo_size = Vec2::new(
                clip_geo_in_backdrop.size.w as f32,
                clip_geo_in_backdrop.size.h as f32,
            );

            let buf_size = Vec2::new(buf_size.w as f32, buf_size.h as f32);
            let input_to_clip_geo = Mat3::from_scale(buf_size / clip_geo_size)
                * Mat3::from_translation(-clip_pos_in_backdrop / buf_size);

            let elem = XrayElement {
                buffer: self.backdrop[ctx.target as usize].clone(),
                id: backdrop.id().clone(),
                geometry: params.geometry,
                src,
                subregion: params.subregion.clone(),
                input_to_clip_geo,
                clip_geo_size,
                corner_radius: corner_radius.scaled_by(zoom as f32),
                scale: params.scale as f32,
                blur,
                noise,
                saturation,
                bg_color: self.backdrop_color,
            };
            push(elem);
        }
    }
}

impl XrayElement {
    /// Filter `damage` by the effect subregion (the surface blur-region protocol), identically for
    /// both renderers. Returns the damage to draw, or `None` when the subregion clips it all away
    /// (the caller's `draw` early-returns).
    fn filter_subregion_damage<'a>(
        &self,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &'a [Rectangle<i32, Physical>],
        scratch: &'a mut Vec<Rectangle<i32, Physical>>,
    ) -> Option<&'a [Rectangle<i32, Physical>]> {
        let Some(subregion) = &self.subregion else {
            return Some(damage);
        };

        let src_to_geo = self.geometry.size / self.src.size;

        // Compute crop in geometry coordinates.
        let mut crop = src;
        crop.loc -= self.src.loc;
        crop = crop.upscale(src_to_geo);
        let mut crop = crop.to_logical(1., Transform::Normal, &Size::default());

        // Then convert to subregion coordinates.
        crop.loc += self.geometry.loc;

        subregion.filter_damage(crop, dst, damage, scratch);

        if scratch.is_empty() {
            None
        } else {
            Some(&scratch[..])
        }
    }
}

// Only the Vulkan xray oracle test uses this; gate it so the default test build doesn't flag it as
// dead code.
#[cfg(test)]
impl XrayElement {
    /// Construct an `XrayElement` directly for oracle tests (bypassing `Xray::render`'s scene
    /// math), so a GLES element and a Vulkan element can share identical geometry/src/matrices
    /// and be compared.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_for_test(
        buffer: Rc<RefCell<EffectBuffer>>,
        geometry: Rectangle<f64, Logical>,
        src: Rectangle<f64, Buffer>,
        input_to_clip_geo: Mat3,
        clip_geo_size: Vec2,
        corner_radius: CornerRadius,
        scale: f32,
        blur: bool,
        bg_color: Color32F,
    ) -> Self {
        let id = buffer.borrow().id().clone();
        Self {
            buffer,
            id,
            geometry,
            src,
            subregion: None,
            input_to_clip_geo,
            clip_geo_size,
            corner_radius,
            scale,
            blur,
            noise: 0.,
            saturation: 1.,
            bg_color,
        }
    }
}

impl Element for XrayElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.buffer.borrow().commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        physical_geometry(self.geometry, scale)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // FIXME: if bg_color alpha is 1 then compute opaque regions here taking corners into
        // account
        OpaqueRegions::default()
    }
}

/// The xray's physical rect, rounded **by its extremities**.
///
/// The translucent surface drawn over the xray computes its own far edge as
/// `round((loc + size) * scale)` (smithay's `WaylandSurfaceRenderElement`), so the xray has to
/// use the same formula or the two disagree by a pixel at fractional scale — and the row between
/// them blends the window against whatever is *under* the xray instead of the blurred backdrop.
/// `Rectangle::to_physical_precise_round` rounds the location and the size apart, which is that
/// disagreement (the same defect our smithay fork fixed in `RescaleRenderElement::geometry`).
fn physical_geometry(
    geometry: Rectangle<f64, Logical>,
    scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let scaled = geometry.to_physical(scale);
    Rectangle::from_extremities(
        scaled.loc.to_i32_round::<i32>(),
        (scaled.loc + scaled.size).to_i32_round::<i32>(),
    )
}

impl RenderElement<VulkanRenderer> for XrayElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        let buffer = self.buffer.borrow();

        // Sample the offscreen prepared by `prepare_effect_buffer` (Vulkan arm). `None` means the
        // buffer was never prepared on this renderer (e.g. a zero-size buffer, or a blur that
        // failed to run) — nothing to draw. The clone is frame-scoped only (the frame
        // retains it until `finish`); the element never caches it, so the next prepare
        // keeps the offscreen texture uniquely referenced (else a per-frame recreate +
        // blur-chain rebuild — Venus blob churn).
        let Some(texture) = buffer.texture_vulkan(self.blur) else {
            return Ok(());
        };

        let mut filtered_damage = Vec::new();
        let Some(damage) = self.filter_subregion_damage(src, dst, damage, &mut filtered_damage)
        else {
            return Ok(());
        };

        // Trap: GLES feeds `input_to_geo` the full-buffer UV (its `v_coords` maps the quad to `src`
        // within the texture), while the Vulkan `postprocess.frag` feeds it quad-local `v_uv` plus
        // a separate `src_rect`. Re-base `input_to_clip_geo` onto `v_uv` using the SAME
        // draw-time `src` that `render_postprocess` samples with, normalized by the texture
        // extent exactly as `normalized_src` does, so the rounded clip mask lands on the
        // sampled content.
        let tex_size = texture.size();
        let (tw, th) = (tex_size.w as f32, tex_size.h as f32);
        let s0 = Vec2::new(src.loc.x as f32 / tw, src.loc.y as f32 / th);
        let ss = Vec2::new(src.size.w as f32 / tw, src.size.h as f32 / th);
        let input_to_geo =
            self.input_to_clip_geo * Mat3::from_translation(s0) * Mat3::from_scale(ss);

        let push = PostprocessPush {
            // Placement fields (`origin`/`size`/`proj`/`target`/`src_rect`) are filled by
            // `render_postprocess`.
            origin: [0.0; 2],
            size: [0.0; 2],
            proj: [0.0; 4],
            target: [0.0; 2],
            src_rect: [0.0; 4],
            geo_size: <[f32; 2]>::from(self.clip_geo_size),
            corner_radius: <[f32; 4]>::from(self.corner_radius),
            // Both shaders mix bg premultiplied: `color + bg * (1 - color.a)`, so the value passes
            // through unchanged from the GLES `bg_color` uniform.
            bg_color: self.bg_color.components(),
            input_to_geo: pack_mat3(input_to_geo),
            // The offscreen is Normal-oriented and unflipped, so sampling needs no re-orientation.
            sample_transform: pack_mat3(Mat3::IDENTITY),
            synoik_scale: self.scale,
            synoik_alpha: 1.,
            saturation: self.saturation,
            noise: self.noise,
        };

        frame.render_postprocess(&texture, src, dst, damage, push)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The xray must end exactly where the translucent surface above it ends.
    ///
    /// Reported 2026-07-28: returning from the overview, a kitty window's bottom edge showed
    /// something bleeding through as the animation ended. `Rectangle::to_physical_precise_round`
    /// rounds the location and the size apart, so the xray's far edge was
    /// `round(loc·scale) + round(size·scale)` while the surface drawn over it uses
    /// `round((loc + size)·scale)`. At fractional scale those disagree by a pixel for about half
    /// of all positions, and in that row the translucent window blends against whatever is under
    /// the xray rather than the blurred backdrop.
    #[test]
    fn the_xray_ends_where_the_surface_over_it_ends() {
        let scale = Scale::from(2.25);
        let mut disagreements = 0;

        for step in 0..400 {
            // Sub-pixel positions, as a window travelling home in an animation has.
            let y = 137. + f64::from(step) * 0.017;
            // A height whose physical size is fractional (401 · 2.25 = 902.25) — with an
            // integral one the two roundings agree by luck and prove nothing.
            let geometry = Rectangle::new(Point::from((100., y)), Size::from((640., 401.)));
            let xray = physical_geometry(geometry, scale);

            // How smithay's `WaylandSurfaceRenderElement` computes the same edge.
            let surface_bottom = ((geometry.loc.y + geometry.size.h) * scale.y).round() as i32;
            assert_eq!(
                xray.loc.y + xray.size.h,
                surface_bottom,
                "xray bottom must meet the surface bottom at y {y}"
            );

            // …and that the old rounding really did differ, so this test is not vacuous.
            let old: Rectangle<i32, Physical> = geometry.to_physical_precise_round(scale);
            if old.loc.y + old.size.h != surface_bottom {
                disagreements += 1;
            }
        }

        assert!(
            disagreements > 0,
            "the sweep never hit a position where the two roundings disagree; \
             it would pass with the defect in place"
        );
    }
}
