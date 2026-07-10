use std::cell::RefCell;

use glam::{Mat3, Vec2};
use niri_config::CornerRadius;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Frame as _, FrameContext, Offscreen, Texture as _};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform};

use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blur::{Blur, BlurOptions};
use crate::render_helpers::renderer::AsGlesFrame as _;
use crate::render_helpers::shaders::{mat3_uniform, Shaders};
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
    subregion: Option<TransformedRegion>,
    scale: f32,
    blur_options: Option<BlurOptions>,
    noise: f32,
    saturation: f32,
}

#[derive(Debug)]
struct Inner {
    framebuffer: Option<GlesTexture>,
    blur: Option<Blur>,
    intermediate: Option<GlesTexture>,
    /// Reusable storage for subregion-filtered damage rects.
    subregion_damage: Vec<Rectangle<i32, Physical>>,
}

impl FramebufferEffect {
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
        noise: f32,
        saturation: f32,
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
            noise,
            saturation,
        }
    }
}

impl FramebufferEffectElement {
    fn compute_uniforms(
        &self,
        crop: Rectangle<f64, Logical>,
        transform: Transform,
    ) -> [Uniform<'static>; 7] {
        let offset = crop.loc - (self.clip_geo.loc - self.geometry.loc);
        let offset = Vec2::new(offset.x as f32, offset.y as f32);
        let crop_size = Vec2::new(crop.size.w as f32, crop.size.h as f32);
        let clip_size = Vec2::new(self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        // Our v_coords are [0, 1] inside crop. We want them to be [0, 1] inside clip_geo.
        let input_to_clip_geo =
            Mat3::from_scale(crop_size / clip_size) * Mat3::from_translation(offset / crop_size);

        // Revert the effect of the texture transform.
        let transform_mat = Mat3::from_translation(Vec2::new(0.5, 0.5))
            * Mat3::from_cols_array(transform.matrix().as_ref())
            * Mat3::from_translation(Vec2::new(-0.5, -0.5));
        let input_to_clip_geo = input_to_clip_geo * transform_mat;

        let clip_geo_size = (self.clip_geo.size.w as f32, self.clip_geo.size.h as f32);

        [
            Uniform::new("niri_scale", self.scale),
            Uniform::new("geo_size", clip_geo_size),
            Uniform::new("corner_radius", <[f32; 4]>::from(self.corner_radius)),
            mat3_uniform("input_to_geo", input_to_clip_geo),
            Uniform::new("noise", self.noise),
            Uniform::new("saturation", self.saturation),
            Uniform::new("bg_color", [0f32, 0., 0., 0.]),
        ]
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

impl RenderElement<GlesRenderer> for FramebufferEffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("FramebufferEffectElement::capture_framebuffer");
        let location = gpu_span_location!("FramebufferEffectElement::capture_framebuffer");
        frame.with_gpu_span(location, |frame| {
            let output_rect = Rectangle::from_size(frame.output_size());
            let transform = frame.transformation();

            let mut guard = frame.renderer();

            let inner = cache
                .get_or_insert::<RefCell<Inner>, _>(|| RefCell::new(Inner::new(guard.as_mut())));
            let mut inner = inner.borrow_mut();
            let inner = &mut *inner;

            inner.intermediate = None;

            // We want clamp-to-edge behavior for out-of-bounds pixels. However, glBlitFramebuffer
            // seems to skip out-of-bounds pixels, even though my reading of the docs suggests
            // otherwise (we use GL_LINEAR filter). So, clamp dst to the framebuffer bounds
            // ourselves.
            let clamped_dst = match dst.intersection(output_rect) {
                Some(clamped) => clamped,
                None => return Ok(()),
            };
            let clamp_scale = clamped_dst.size.to_f64() / dst.size.to_f64();

            let dst = transform.transform_rect_in(clamped_dst, &output_rect.size);

            // Compute size from our geometry and scale.
            //
            // The "correct" size is always dst.size since that's the pixel region we're actually
            // blitting. However, using dst.size causes two undesirable things when zooming out for
            // the overview:
            // 1. dst.size shrinks every frame, causing a texture realloaction for every fb effect
            //    element every frame.
            // 2. The underlying blur visually expands. This is technically correct, since the
            //    underlying contents shrink, but it's not what you visually expect: you expect the
            //    blur to also shrink as the windows zoom out, to give the zooming out effect.
            //
            // Using size computed from geometry and scale solves both of those problems (even
            // though there's a bit of a cost in that zoomed-out elements still blur the entire
            // unzoomed texture size, and even though the blur ends up slightly wrong as there's two
            // layers of texture resampling, up and back down).
            //
            // Here we use src.size rather than geometry directly because src takes into account
            // cropping.
            let size = src
                .size
                .to_logical(1., Transform::Normal)
                .upscale(clamp_scale)
                .to_physical_precise_round(self.scale);
            let size = transform.transform_size(size);

            let size = size.to_logical(1).to_buffer(1, Transform::Normal);

            // Recreate framebuffer if needed.
            if inner
                .framebuffer
                .as_ref()
                .is_some_and(|fb| fb.size() != size)
            {
                inner.framebuffer = None;
            }
            let framebuffer = if let Some(fb) = &inner.framebuffer {
                fb
            } else {
                trace!("creating framebuffer texture sized {} × {}", size.w, size.h);
                let renderer = guard.as_mut();
                let texture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
                inner.framebuffer.insert(texture)
            };

            // Prepare blur textures.
            let mut blur = Option::zip(inner.blur.as_mut(), self.blur_options);
            if let Some((b, options)) = &mut blur {
                let renderer = guard.as_mut();
                if let Err(err) = b.prepare_textures(
                    |fourcc, size| renderer.create_buffer(fourcc, size),
                    framebuffer,
                    *options,
                ) {
                    warn!("error preparing blur textures: {err:?}");
                    blur = None;
                }
            }

            // We can't use renderer.with_context() as that will reset the GlesFrame binding that we
            // want to blit from.
            drop(guard);

            // Blit the framebuffer contents.
            frame.with_context(|gl| unsafe {
                while gl.GetError() != ffi::NO_ERROR {}

                let mut current_fbo = 0i32;
                gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut current_fbo as *mut _);

                // BlitFramebuffer is affected by the scissor test, we don't want that.
                gl.Disable(ffi::SCISSOR_TEST);

                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo as *mut _);
                gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, fbo);

                gl.FramebufferTexture2D(
                    ffi::DRAW_FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    framebuffer.tex_id(),
                    0,
                );

                gl.BlitFramebuffer(
                    dst.loc.x,
                    dst.loc.y,
                    dst.loc.x + dst.size.w,
                    dst.loc.y + dst.size.h,
                    0,
                    0,
                    size.w,
                    size.h,
                    ffi::COLOR_BUFFER_BIT,
                    ffi::LINEAR,
                );

                // Restore state set by GlesFrame that we just modified.
                gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, current_fbo as u32);
                gl.Enable(ffi::SCISSOR_TEST);

                gl.DeleteFramebuffers(1, &mut fbo as *mut _);

                if gl.GetError() != ffi::NO_ERROR {
                    Err(GlesError::BlitError)
                } else {
                    Ok(())
                }
            })??;

            // If blur is off, use the unblurred texture.
            if self.blur_options.is_none() {
                inner.intermediate = Some(framebuffer.clone());
                return Ok(());
            }

            if let Some((blur, options)) = blur {
                let mut guard = frame.renderer();
                let renderer = guard.as_mut();
                match blur.render(renderer, framebuffer, options) {
                    Ok(blurred) => inner.intermediate = Some(blurred),
                    Err(err) => {
                        warn!("error rendering blur: {err:?}");
                    }
                }
            }

            Ok(())
        })
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(cache) = cache else {
            return Ok(());
        };
        let Some(inner) = cache.get::<RefCell<Inner>>() else {
            return Ok(());
        };
        let mut inner = inner.borrow_mut();
        let inner = &mut *inner;

        let Some(texture) = &inner.intermediate else {
            return Ok(());
        };

        // Clamp the same way as in capture_framebuffer().
        let output_rect = Rectangle::from_size(frame.output_size());
        let clamped_dst = match dst.intersection(output_rect) {
            Some(clamped) => clamped,
            None => return Ok(()),
        };
        let clamp_offset = clamped_dst.loc - dst.loc;

        // Filter damage by subregion, reusing the stored Vec to avoid allocation.
        let filtered = &mut inner.subregion_damage;
        filtered.clear();

        if let Some(subregion) = &self.subregion {
            // Convert to subregion coordinates.
            let mut crop = src.to_logical(1., Transform::Normal, &src.size);
            crop.loc += self.geometry.loc;
            subregion.filter_damage(crop, dst, damage, filtered);
        } else {
            filtered.extend(damage.iter());
        };

        // Adjust for clamped dst.
        if clamped_dst != dst {
            let r = Rectangle::new(clamp_offset, clamped_dst.size);
            filtered.retain_mut(|d| {
                if let Some(mut crop) = d.intersection(r) {
                    crop.loc -= clamp_offset;
                    *d = crop;
                    true
                } else {
                    false
                }
            });
        }

        if filtered.is_empty() {
            return Ok(());
        }
        let damage = &filtered[..];

        // Adjust src proportionally to the dst clamping.
        let src_loc = src.loc.to_logical(1., Transform::Normal, &src.size);
        let dst_to_src = src.size / dst.size.to_f64();
        let crop = Rectangle::new(
            src_loc + clamp_offset.to_f64().upscale(dst_to_src).to_logical(1.),
            clamped_dst.size.to_f64().upscale(dst_to_src).to_logical(1.),
        );

        let program = Shaders::get_from_frame(frame).postprocess_and_clip.clone();
        let uniforms = program
            .is_some()
            .then(|| self.compute_uniforms(crop, frame.transformation()));
        let uniforms = uniforms.as_ref().map_or(&[][..], |x| &x[..]);

        frame.render_texture_from_to(
            texture,
            Rectangle::from_size(texture.size().to_f64()),
            clamped_dst,
            damage,
            &[],
            // The intermediate texture has the same transform as the frame.
            frame.transformation().invert(),
            1.,
            program.as_ref(),
            uniforms,
        )
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for FramebufferEffectElement {
    fn capture_framebuffer(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::capture_framebuffer(&self, gles_frame, src, dst, cache)?;
        Ok(())
    }

    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            &self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }
}

/// The owned Vulkan renderer's backdrop-blur path: the real (non-degraded)
/// `RenderElement<VulkanRenderer>`. `capture_framebuffer` grabs the scene-behind into a cached
/// [`BackdropBlur`] (stored in the element's `UserDataMap`, like the GLES `Inner`) and blurs it;
/// `draw` composites the blurred result with the postprocess-and-clip material — the same two-phase
/// shape as the GLES impl above, just driven through `VulkanFrame::capture_backdrop` /
/// `draw_backdrop`. Rotated outputs are not yet handled (Normal-transform only), matching the rest
/// of the Vulkan port.
#[cfg(feature = "vulkan")]
mod vulkan_impl {
    use std::cell::RefCell;

    use glam::{Mat3, Vec2, Vec3};
    use niri_vk::render::PostprocessPush;
    use smithay::backend::renderer::element::{RenderElement, UnderlyingStorage};
    use smithay::utils::user_data::UserDataMap;
    use smithay::utils::{Buffer, Logical, Physical, Rectangle, Transform};

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
        /// clamping) — the Vulkan sibling of [`Self::compute_uniforms`], packed into a
        /// [`PostprocessPush`]. `render_postprocess` fills `origin`/`size`/`target`/`src_rect`.
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
                niri_scale: self.scale,
                niri_alpha: 1.0,
                saturation: self.saturation,
                noise: self.noise,
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

            // KNOWN DIVERGENCE from the GLES draw: `self.subregion` is ignored. GLES restricts the
            // painted area to `subregion ∩ damage` (a client-requested blur region); here the whole
            // `clamped_dst` is blurred. Full-geometry backdrop blur (the common GNOME case) is
            // correct; a client using an explicit blur-region protocol gets blur across its whole
            // geometry. Per-subregion scissored draws are a follow-up.
            // The damage is element-local to `dst`; the backdrop is placed at `clamped_dst`, so
            // re-base the rects by the same clamp offset before they scissor the draw.
            let damage: Vec<Rectangle<i32, Physical>> = damage
                .iter()
                .map(|d| {
                    let mut d = *d;
                    d.loc -= clamp_offset;
                    d
                })
                .collect();
            let push = self.postprocess_push(crop, frame.transform());
            frame.draw_backdrop(blur, clamped_dst, &damage, push)
        }

        fn underlying_storage(
            &self,
            _renderer: &mut VulkanRenderer,
        ) -> Option<UnderlyingStorage<'_>> {
            None
        }
    }
}

impl Inner {
    fn new(renderer: &mut GlesRenderer) -> Self {
        Inner {
            framebuffer: None,
            blur: Blur::new(renderer),
            intermediate: None,
            subregion_damage: Vec::new(),
        }
    }
}
