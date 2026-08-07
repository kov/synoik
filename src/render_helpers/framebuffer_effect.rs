// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::renderer::element::{Element, Id};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform};
use synoik_config::CornerRadius;

use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blur::{BlurOptions, Finish};
use crate::utils::region::TransformedRegion;

#[derive(Debug)]
pub struct FramebufferEffect {
    id: Id,
    commit: CommitCounter,
}

#[derive(Debug)]
pub struct FramebufferEffectElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    clip_geo: Rectangle<f64, Logical>,
    corner_radius: CornerRadius,
    /// The client-requested blur region (`ext-background-effect-v1` `set_blur_region`). The draw
    /// intersects the damage with it, so the blur lands only inside it.
    subregion: Option<TransformedRegion>,
    scale: f32,
    blur_options: Option<BlurOptions>,
    finish: Finish,
}

impl FramebufferEffect {
    /// The GPU state (capture texture, blur chain) lives wherever the render path keeps
    /// per-element state, **not** here: an [`OutputDamageTracker`] keeps a `UserDataMap` per
    /// element `Id`, so the same effect drawn on two outputs at two sizes gets one capture each,
    /// and — the part that matters — one render *target* cannot draw from a capture another
    /// target filled. Block-out rules key off the target, so a shared capture is how a screencast
    /// would come to composite the un-blocked scene.
    ///
    /// [`OutputDamageTracker`]: smithay::backend::renderer::damage::OutputDamageTracker
    pub fn new() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    pub fn damage(&mut self) {
        self.commit.increment();
    }

    pub fn render(
        &self,
        ns: Option<usize>,
        params: RenderParams,
        blur_options: Option<BlurOptions>,
        finish: Finish,
    ) -> FramebufferEffectElement {
        let (clip_geo, corner_radius) = params
            .clip
            .unwrap_or((params.geometry, CornerRadius::default()));

        let mut id = self.id.clone();
        if let Some(ns) = ns {
            id = id.namespaced(ns);
        }

        FramebufferEffectElement {
            id,
            commit: self.commit,
            geometry: params.geometry,
            clip_geo,
            corner_radius,
            subregion: params.subregion,
            scale: params.scale as f32,
            blur_options,
            finish,
        }
    }
}

impl Element for FramebufferEffectElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        // We don't use src for drawing but we can use it to figure out how we were cropped.
        let size = self.geometry.size.to_buffer(1., Transform::Normal);
        Rectangle::from_size(size)
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

/// The renderer's backdrop-blur path. `capture_framebuffer` grabs the scene-behind into a cached
/// [`BackdropBlur`] (stored in the element's `UserDataMap`) and blurs it; `draw` composites the
/// blurred result with the postprocess-and-clip material.
///
/// Rotated outputs **are** handled: the capture takes the target sub-region and size through the
/// output transform, and `sample_transform` maps the logical `v_uv` back onto the physically-
/// oriented capture. `vulkan_backdrop_effect_roundtrips_under_rotation` pins this under `_90` and
/// `Flipped90` (the transposing-flip case a plain rotation would miss) for a no-op effect, a real
/// blur, and a blur restricted to a subregion.
mod vulkan_impl {
    use std::cell::RefCell;

    use glam::{Mat3, Vec2, Vec3};
    use smithay::backend::renderer::element::{RenderElement, UnderlyingStorage};
    use smithay::utils::user_data::UserDataMap;
    use smithay::utils::{Buffer, Logical, Physical, Rectangle, Transform};
    use synoik_vk::render::PostprocessPush;

    use super::FramebufferEffectElement;
    use crate::render_helpers::vulkan::{BackdropBlur, VulkanError, VulkanFrame, VulkanRenderer};

    /// A `glam::Mat3` packed as `PostprocessPush::input_to_geo`'s 3 column-vectors (`.xyz`, `w =
    /// 0`).
    fn pack_mat3(m: Mat3) -> [[f32; 4]; 3] {
        let col = |v: Vec3| [v.x, v.y, v.z, 0.0];
        [col(m.x_axis), col(m.y_axis), col(m.z_axis)]
    }

    impl FramebufferEffectElement {
        /// Build the postprocess material's push constants for `crop` (the source rect adjusted for
        /// clamping), packed into a [`PostprocessPush`]. `render_postprocess` fills
        /// `origin`/`size`/`target`/`src_rect`.
        fn postprocess_push(
            &self,
            crop: Rectangle<f64, Logical>,
            transform: Transform,
        ) -> PostprocessPush {
            let offset = crop.loc - (self.clip_geo.loc - self.geometry.loc);
            let offset = Vec2::new(offset.x as f32, offset.y as f32);
            let crop_size = Vec2::new(crop.size.w as f32, crop.size.h as f32);
            let clip_size = Vec2::new(self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

            // v_uv is [0,1] in logical space. The geometry clip works there directly (map into
            // clip_geo). Sampling is different: the mid-frame capture holds the backdrop in
            // *physical* orientation, so v_uv must be mapped through the output transform to index
            // it — GLES bakes that into its tex_matrix and reverts it inside input_to_geo; here the
            // two are separate push fields, so input_to_geo carries no transform. Identity for
            // Normal, so both reduce to the previous behavior.
            let input_to_geo = Mat3::from_scale(crop_size / clip_size)
                * Mat3::from_translation(offset / crop_size);
            // Inverse of the output transform (about the UV centre): v_uv is a logical coordinate,
            // and the capture holds the backdrop physically-oriented, so map logical → physical UV
            // with the inverse (GLES's tex_matrix uses `frame.transformation().invert()`).
            let sample_transform = Mat3::from_translation(Vec2::new(0.5, 0.5))
                * Mat3::from_cols_array(transform.invert().matrix().as_ref())
                * Mat3::from_translation(Vec2::new(-0.5, -0.5));

            PostprocessPush {
                geo_size: [self.clip_geo.size.w as f32, self.clip_geo.size.h as f32],
                corner_radius: <[f32; 4]>::from(self.corner_radius),
                bg_color: [0.0; 4],
                input_to_geo: pack_mat3(input_to_geo),
                sample_transform: pack_mat3(sample_transform),
                synoik_scale: self.scale,
                synoik_alpha: 1.0,
                saturation: self.finish.saturation,
                noise: self.finish.noise,
                tint: self.finish.tint_premultiplied(),
                contrast: self.finish.contrast,
                // origin/size/target/src_rect are filled by render_postprocess.
                ..Default::default()
            }
        }
    }

    impl RenderElement<VulkanRenderer> for FramebufferEffectElement {
        fn capture_framebuffer(
            &self,
            frame: &mut VulkanFrame<'_, '_>,
            src: Rectangle<f64, Buffer>,
            dst: Rectangle<i32, Physical>,
            cache: &UserDataMap,
        ) -> Result<(), VulkanError> {
            let output_rect = Rectangle::from_size(frame.output_size());
            let transform = frame.transform();

            // Clamp to the framebuffer (an effect near an edge spills off-screen).
            let clamped_dst = match dst.intersection(output_rect) {
                Some(clamped) => clamped,
                None => return Ok(()),
            };
            let clamp_scale = clamped_dst.size.to_f64() / dst.size.to_f64();

            // Intermediate size from geometry + scale (not dst.size) — matches the GLES capture, so
            // a zoom-out doesn't reallocate every frame and the blur tracks the on-screen size.
            let size = src
                .size
                .to_logical(1., Transform::Normal)
                .upscale(clamp_scale)
                .to_physical_precise_round(self.scale);
            let size = transform.transform_size(size);
            let size = size.to_logical(1).to_buffer(1, Transform::Normal);

            // The target sub-region to grab (Normal: == clamped_dst).
            let src_region = transform.transform_rect_in(clamped_dst, &output_rect.size);

            let passes = self.blur_options.map(|o| (o.passes as usize).clamp(1, 31));
            let offset = self.blur_options.map_or(0.0, |o| o.offset) as f32;

            #[cfg(test)]
            crate::render_helpers::background_effect::trace::record_capture(
                crate::render_helpers::background_effect::trace::CaptureSample {
                    dst,
                    src: src_region,
                },
            );

            let inner =
                cache.get_or_insert::<RefCell<Option<BackdropBlur>>, _>(|| RefCell::new(None));
            let mut slot = inner.borrow_mut();
            frame.capture_backdrop(&mut slot, src_region, size, passes, offset)
        }

        fn draw(
            &self,
            frame: &mut VulkanFrame<'_, '_>,
            src: Rectangle<f64, Buffer>,
            dst: Rectangle<i32, Physical>,
            damage: &[Rectangle<i32, Physical>],
            _opaque_regions: &[Rectangle<i32, Physical>],
            cache: Option<&UserDataMap>,
        ) -> Result<(), VulkanError> {
            let Some(cache) = cache else {
                return Ok(());
            };
            let Some(inner) = cache.get::<RefCell<Option<BackdropBlur>>>() else {
                return Ok(());
            };
            let slot = inner.borrow();
            let Some(blur) = slot.as_ref() else {
                return Ok(());
            };

            // Clamp exactly as capture_framebuffer did.
            let output_rect = Rectangle::from_size(frame.output_size());
            let clamped_dst = match dst.intersection(output_rect) {
                Some(clamped) => clamped,
                None => return Ok(()),
            };
            let clamp_offset = clamped_dst.loc - dst.loc;

            // The source crop, adjusted proportionally for the dst clamping (mirrors GLES draw).
            let src_loc = src.loc.to_logical(1., Transform::Normal, &src.size);
            let dst_to_src = src.size / dst.size.to_f64();
            let crop = Rectangle::new(
                src_loc + clamp_offset.to_f64().upscale(dst_to_src).to_logical(1.),
                clamped_dst.size.to_f64().upscale(dst_to_src).to_logical(1.),
            );

            // Restrict the painted area to the client's blur region (`ext-background-effect-v1`
            // `set_blur_region`), which reaches us as `subregion`. The damage scissors the draw, so
            // intersecting it with the region is what confines the blur; without this the whole
            // quad blurs and the client's region is silently ignored.
            //
            // `filter_damage` wants the subregion's own coordinate space — geometry-relative, and
            // derived from the *unclamped* `src`/`dst`, so this runs before the clamp re-base
            // below, in the same order as the GLES draw this replaced.
            let mut filtered: Vec<Rectangle<i32, Physical>> = Vec::new();
            if let Some(subregion) = &self.subregion {
                let mut sub_crop = src.to_logical(1., Transform::Normal, &src.size);
                sub_crop.loc += self.geometry.loc;
                subregion.filter_damage(sub_crop, dst, damage, &mut filtered);
            } else {
                filtered.extend(damage.iter().copied());
            }

            // The damage is element-local to `dst`; the backdrop is placed at `clamped_dst`, so
            // re-base the rects by the clamp offset, dropping what falls outside, before they
            // scissor the draw.
            if clamped_dst != dst {
                let keep = Rectangle::new(clamp_offset, clamped_dst.size);
                filtered.retain_mut(|d| match d.intersection(keep) {
                    Some(mut c) => {
                        c.loc -= clamp_offset;
                        *d = c;
                        true
                    }
                    None => false,
                });
            }

            if filtered.is_empty() {
                return Ok(());
            }

            let push = self.postprocess_push(crop, frame.transform());
            frame.draw_backdrop(blur, clamped_dst, &filtered, push)
        }

        fn underlying_storage(
            &self,
            _renderer: &mut VulkanRenderer,
        ) -> Option<UnderlyingStorage<'_>> {
            None
        }
    }
}
