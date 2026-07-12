use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Context as _;
use glam::{Mat3, Vec2};
use niri_config::BlockOutFrom;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{Blocker, BlockerState};

use crate::animation::Animation;
use crate::niri_render_elements;
use crate::render_helpers::custom_anim::CustomAnimRenderElement;
use crate::render_helpers::dual_texture::DualTextureRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::shader_element::ShaderRenderElement;
use crate::render_helpers::shaders::{mat3_uniform, ProgramType, Shaders};
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{render_to_encompassing_texture, RenderCtx, RenderTarget};
use crate::utils::transaction::TransactionBlocker;

#[derive(Debug)]
pub struct ClosingWindow {
    /// Contents of the window.
    buffer: TextureBuffer<GlesTexture>,

    /// Contents that are not blocked out, but the background is blocked out.
    ///
    /// If `None` then the background doesn't have any blocked-out surfaces, and normal `buffer`
    /// can be used instead.
    buffer_with_blocked_out_bg: Option<TextureBuffer<GlesTexture>>,

    /// Blocked-out contents of the window.
    blocked_out_buffer: TextureBuffer<GlesTexture>,

    /// Where the window should be blocked out from.
    block_out_from: Option<BlockOutFrom>,

    /// Size of the window geometry.
    geo_size: Size<f64, Logical>,

    /// Position in the workspace.
    pos: Point<f64, Logical>,

    /// How much the texture should be offset.
    buffer_offset: Point<f64, Logical>,

    /// How much the texture with blocked-out bg should be offset.
    buffer_with_blocked_out_bg_offset: Point<f64, Logical>,

    /// How much the blocked-out texture should be offset.
    blocked_out_buffer_offset: Point<f64, Logical>,

    /// The closing animation.
    anim_state: AnimationState,

    /// Random seed for the shader.
    random_seed: f32,

    /// The `buffer` contents read back to a renderer-neutral `MemoryBuffer` at capture time, so
    /// the owned Vulkan renderer can render the closing animation on the on-screen output.
    /// Only captured when compositing through Vulkan (see `bridge_vulkan` in `new`); the
    /// block-out variants are never needed on the Output target, so only `buffer` is bridged.
    #[cfg(feature = "vulkan")]
    buffer_mem: Option<crate::render_helpers::memory::MemoryBuffer>,

    /// The lazily-uploaded `VkTexture` for [`Self::buffer_mem`], cached across frames (uploaded
    /// once on the first Vulkan render, like the resize animation's `prev_vk`).
    #[cfg(feature = "vulkan")]
    buffer_vk: std::cell::RefCell<Option<TextureBuffer<crate::render_helpers::vulkan::VkTexture>>>,
}

niri_render_elements! {
    ClosingWindowRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<DualTextureRenderElement>>,
        Shader = CustomAnimRenderElement,
    }
}

#[derive(Debug)]
enum AnimationState {
    Waiting {
        /// Blocker for a transaction before starting the animation.
        blocker: TransactionBlocker,
        anim: Animation,
    },
    Animating(Animation),
}

impl AnimationState {
    pub fn new(blocker: TransactionBlocker, anim: Animation) -> Self {
        if blocker.state() == BlockerState::Pending {
            Self::Waiting { blocker, anim }
        } else {
            // This actually doesn't normally happen because the window is removed only after the
            // closing animation is created. Though, it does happen with disable-transactions debug
            // flag.
            Self::Animating(anim)
        }
    }
}

impl ClosingWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        bridge_vulkan: bool,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        geo_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
        anim: Animation,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("ClosingWindow::new");
        #[cfg(not(feature = "vulkan"))]
        let _ = bridge_vulkan;

        // The main contents: on a Vulkan session, read the freshly-rendered GLES texture back to a
        // MemoryBuffer here (we have the raw texture in hand) so the owned renderer can re-upload
        // it. Only `buffer` is bridged — the block-out variants are `target != Output`
        // only, and the Vulkan path only runs for the Output target. Rendered before
        // `render_to_texture` is defined so its `renderer` borrow doesn't clash with the
        // closure's.
        #[cfg(feature = "vulkan")]
        let mut buffer_mem = None;
        let (buffer, buffer_offset) = {
            // `mut` is only needed for the Vulkan readback bind below.
            #[cfg_attr(not(feature = "vulkan"), allow(unused_mut))]
            let (mut texture, _sync_point, geo) = render_to_encompassing_texture(
                renderer,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &snapshot.contents,
            )
            .context("error rendering contents")?;
            #[cfg(feature = "vulkan")]
            if bridge_vulkan {
                // Prefer the neutral captured through the Vulkan renderer at snapshot time
                // (self-hosting, no GLES readback); fall back to reading the freshly-rendered GLES
                // texture back when the Vulkan capture was unavailable.
                buffer_mem = snapshot
                    .neutral
                    .get()
                    .and_then(|n| n.as_ref())
                    .map(|(buf, _)| buf.clone());
                if buffer_mem.is_none() {
                    buffer_mem = readback_snapshot(renderer, &mut texture, scale)
                        .map_err(|err| {
                            warn!("error reading back closing snapshot for Vulkan: {err:?}")
                        })
                        .ok();
                }
            }
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );
            (buffer, geo.loc.to_f64().to_logical(scale))
        };

        let mut render_to_texture = |elements: Vec<E>| -> anyhow::Result<_> {
            let (texture, _sync_point, geo) = render_to_encompassing_texture(
                renderer,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                &elements,
            )
            .context("error rendering to texture")?;

            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );

            let offset = geo.loc.to_f64().to_logical(scale);

            Ok((buffer, offset))
        };

        let (buffer_with_blocked_out_bg, buffer_with_blocked_out_bg_offset) =
            if let Some(contents) = snapshot.contents_with_blocked_out_bg {
                let (buffer, offset) = render_to_texture(contents)
                    .context("error rendering contents with blocked-out bg")?;
                (Some(buffer), offset)
            } else {
                (None, Point::default())
            };
        let (blocked_out_buffer, blocked_out_buffer_offset) =
            render_to_texture(snapshot.blocked_out_contents)
                .context("error rendering blocked-out contents")?;

        Ok(Self {
            buffer,
            buffer_with_blocked_out_bg,
            blocked_out_buffer,
            block_out_from: snapshot.block_out_from,
            geo_size,
            pos,
            buffer_offset,
            buffer_with_blocked_out_bg_offset,
            blocked_out_buffer_offset,
            anim_state: AnimationState::new(blocker, anim),
            random_seed: fastrand::f32(),
            #[cfg(feature = "vulkan")]
            buffer_mem,
            #[cfg(feature = "vulkan")]
            buffer_vk: std::cell::RefCell::new(None),
        })
    }

    pub fn advance_animations(&mut self) {
        match &mut self.anim_state {
            AnimationState::Waiting { blocker, anim } => {
                if blocker.state() != BlockerState::Pending {
                    let anim = anim.restarted(0., 1., 0.);
                    self.anim_state = AnimationState::Animating(anim);
                }
            }
            AnimationState::Animating(_anim) => (),
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        match &self.anim_state {
            AnimationState::Waiting { .. } => true,
            AnimationState::Animating(anim) => !anim.is_done(),
        }
    }

    pub fn render(
        &self,
        ctx: RenderCtx<GlesRenderer>,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
    ) -> ClosingWindowRenderElement {
        let (buffer, offset) = if ctx.target.should_block_out(self.block_out_from) {
            (&self.blocked_out_buffer, self.blocked_out_buffer_offset)
        } else if ctx.target != RenderTarget::Output && self.buffer_with_blocked_out_bg.is_some() {
            (
                self.buffer_with_blocked_out_bg.as_ref().unwrap(),
                self.buffer_with_blocked_out_bg_offset,
            )
        } else {
            (&self.buffer, self.buffer_offset)
        };

        let anim = match &self.anim_state {
            AnimationState::Waiting { .. } => {
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer.clone(),
                    Point::from((0., 0.)),
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                );

                let elem = PrimaryGpuTextureRenderElement(elem);
                let elem = DualTextureRenderElement::Gles(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), 1.);

                let mut location = self.pos + offset;
                location.x -= view_rect.loc.x;
                let elem = RelocateRenderElement::from_element(
                    elem,
                    location.to_physical_precise_round(scale),
                    Relocate::Relative,
                );

                return elem.into();
            }
            AnimationState::Animating(anim) => anim,
        };

        let progress = anim.value();
        let clamped_progress = anim.clamped_value().clamp(0., 1.);

        if Shaders::get(ctx.renderer).is_some_and(|s| s.program(ProgramType::Close).is_some()) {
            let area_loc = Vec2::new(view_rect.loc.x as f32, view_rect.loc.y as f32);
            let area_size = Vec2::new(view_rect.size.w as f32, view_rect.size.h as f32);

            // Round to physical pixels relative to the view position. This is similar to what
            // happens when rendering normal windows.
            let relative = self.pos - view_rect.loc;
            let pos = view_rect.loc + relative.to_physical_precise_round(scale).to_logical(scale);

            let geo_loc = Vec2::new(pos.x as f32, pos.y as f32);
            let geo_size = Vec2::new(self.geo_size.w as f32, self.geo_size.h as f32);

            let input_to_geo = Mat3::from_scale(area_size / geo_size)
                * Mat3::from_translation((area_loc - geo_loc) / area_size);

            let tex_scale = self.buffer.texture_scale();
            let tex_scale = Vec2::new(tex_scale.x as f32, tex_scale.y as f32);
            let tex_loc = Vec2::new(offset.x as f32, offset.y as f32);
            let tex_size = self.buffer.texture().size();
            let tex_size = Vec2::new(tex_size.w as f32, tex_size.h as f32) / tex_scale;

            let geo_to_tex =
                Mat3::from_translation(-tex_loc / tex_size) * Mat3::from_scale(geo_size / tex_size);

            let elem = ShaderRenderElement::new(
                ProgramType::Close,
                view_rect.size,
                None,
                scale.x as f32,
                1.,
                Rc::new([
                    mat3_uniform("niri_input_to_geo", input_to_geo),
                    Uniform::new("niri_geo_size", geo_size.to_array()),
                    mat3_uniform("niri_geo_to_tex", geo_to_tex),
                    Uniform::new("niri_progress", progress as f32),
                    Uniform::new("niri_clamped_progress", clamped_progress as f32),
                    Uniform::new("niri_random_seed", self.random_seed),
                ]),
                HashMap::from([(String::from("niri_tex"), buffer.texture().clone())]),
                Kind::Unspecified,
            )
            .with_location(Point::from((0., 0.)));
            return CustomAnimRenderElement::Gles(elem).into();
        }

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            1. - clamped_progress as f32,
            None,
            None,
            Kind::Unspecified,
        );

        let elem = PrimaryGpuTextureRenderElement(elem);
        let elem = DualTextureRenderElement::Gles(elem);

        let center = self.geo_size.to_point().downscale(2.);
        let elem = RescaleRenderElement::from_element(
            elem,
            (center - offset).to_physical_precise_round(scale),
            ((1. - clamped_progress) / 5. + 0.8).max(0.),
        );

        let mut location = self.pos + offset;
        location.x -= view_rect.loc.x;
        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        elem.into()
    }

    /// The Vulkan sibling of [`render`](Self::render): draws the closing animation on the owned
    /// renderer, using the `buffer` contents read back at capture time (see `new`'s
    /// `bridge_vulkan`) and lazily re-uploaded to a cached `VkTexture`. Applies the user's
    /// custom `close` shader if one is installed, else the built-in scale + fade.
    ///
    /// Only valid for [`RenderTarget::Output`]: the block-out / blocked-out-bg variants are never
    /// captured for Vulkan (see `new`), so a non-Output target — where GLES may need them — returns
    /// `None`. Returns `None` too if there was nothing to read back or the upload failed.
    #[cfg(feature = "vulkan")]
    pub fn render_vulkan(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        target: RenderTarget,
    ) -> Option<ClosingWindowRenderElement> {
        use crate::render_helpers::vulkan::{pack_affine, CustomAnimPush, CustomShaderType};

        // The privacy safeguard: only `buffer` is bridged to Vulkan (never the blocked-out
        // variants), so we must not run for a target that would select one of those.
        if target != RenderTarget::Output {
            return None;
        }

        let offset = self.buffer_offset;

        // Lazily upload the captured snapshot to a cached VkTexture (like the resize `prev_vk`).
        let mem = self.buffer_mem.as_ref()?;
        if self.buffer_vk.borrow().is_none() {
            match TextureBuffer::from_memory_buffer(renderer, mem) {
                Ok(tb) => *self.buffer_vk.borrow_mut() = Some(tb),
                Err(err) => {
                    warn!("error uploading closing snapshot to Vulkan: {err:?}");
                    return None;
                }
            }
        }
        let buffer = self.buffer_vk.borrow().clone()?;

        let anim = match &self.anim_state {
            AnimationState::Waiting { .. } => {
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer.clone(),
                    Point::from((0., 0.)),
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                );

                let elem = DualTextureRenderElement::Vulkan(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), 1.);

                let mut location = self.pos + offset;
                location.x -= view_rect.loc.x;
                let elem = RelocateRenderElement::from_element(
                    elem,
                    location.to_physical_precise_round(scale),
                    Relocate::Relative,
                );

                return Some(elem.into());
            }
            AnimationState::Animating(anim) => anim,
        };

        let progress = anim.value();
        let clamped_progress = anim.clamped_value().clamp(0., 1.);

        if renderer.has_custom_shader(CustomShaderType::Close) {
            // Mirror the GLES custom-close branch: same absolute-coordinate affine geometry, packed
            // into a CustomAnimPush and drawn via render_custom_anim over the whole viewport.
            let area_loc = Vec2::new(view_rect.loc.x as f32, view_rect.loc.y as f32);
            let area_size = Vec2::new(view_rect.size.w as f32, view_rect.size.h as f32);

            let relative = self.pos - view_rect.loc;
            let pos = view_rect.loc + relative.to_physical_precise_round(scale).to_logical(scale);

            let geo_loc = Vec2::new(pos.x as f32, pos.y as f32);
            let geo_size = Vec2::new(self.geo_size.w as f32, self.geo_size.h as f32);

            let input_to_geo = Mat3::from_scale(area_size / geo_size)
                * Mat3::from_translation((area_loc - geo_loc) / area_size);

            let tex_scale = buffer.texture_scale();
            let tex_scale = Vec2::new(tex_scale.x as f32, tex_scale.y as f32);
            let tex_loc = Vec2::new(offset.x as f32, offset.y as f32);
            let tex_size = buffer.texture().size();
            let tex_size = Vec2::new(tex_size.w as f32, tex_size.h as f32) / tex_scale;

            let geo_to_tex =
                Mat3::from_translation(-tex_loc / tex_size) * Mat3::from_scale(geo_size / tex_size);

            let push = CustomAnimPush {
                geo_size: geo_size.to_array(),
                input_to_geo: pack_affine(input_to_geo),
                geo_to_tex: pack_affine(geo_to_tex),
                progress: progress as f32,
                clamped_progress: clamped_progress as f32,
                random_seed: self.random_seed,
                alpha: 1.,
                scale: scale.x as f32,
                ..Default::default()
            };

            // The element covers the whole viewport at the origin, exactly like the GLES shader
            // element (which is located at (0, 0) with size = view_rect.size and carries the world
            // positions in its matrices).
            let area = Rectangle::new(Point::from((0., 0.)), view_rect.size);
            let elem = CustomAnimRenderElement::new_vulkan_anim(
                CustomShaderType::Close,
                buffer.texture().clone(),
                area,
                1.,
                push,
            );
            return Some(elem.into());
        }

        let elem = TextureRenderElement::from_texture_buffer(
            buffer.clone(),
            Point::from((0., 0.)),
            1. - clamped_progress as f32,
            None,
            None,
            Kind::Unspecified,
        );

        let elem = DualTextureRenderElement::Vulkan(elem);

        let center = self.geo_size.to_point().downscale(2.);
        let elem = RescaleRenderElement::from_element(
            elem,
            (center - offset).to_physical_precise_round(scale),
            ((1. - clamped_progress) / 5. + 0.8).max(0.),
        );

        let mut location = self.pos + offset;
        location.x -= view_rect.loc.x;
        let elem = RelocateRenderElement::from_element(
            elem,
            location.to_physical_precise_round(scale),
            Relocate::Relative,
        );

        Some(elem.into())
    }
}

/// Read a freshly-rendered GLES snapshot texture back into a renderer-neutral [`MemoryBuffer`], so
/// the owned Vulkan renderer can re-upload it at render time. Called once at capture (see
/// [`ClosingWindow::new`]); mirrors
/// [`RenderSnapshot::capture_neutral`](crate::render_helpers::snapshot).
#[cfg(feature = "vulkan")]
fn readback_snapshot(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    scale: Scale<f64>,
) -> anyhow::Result<crate::render_helpers::memory::MemoryBuffer> {
    use smithay::backend::renderer::{Bind as _, ExportMem as _};

    use crate::render_helpers::copy_framebuffer;
    use crate::render_helpers::memory::MemoryBuffer;

    let fourcc = Fourcc::Abgr8888;
    let buffer_size = texture.size();
    let mapping = {
        let target = renderer
            .bind(texture)
            .context("error binding closing snapshot for readback")?;
        copy_framebuffer(renderer, &target, fourcc).context("error copying framebuffer")?
    };
    let bytes = renderer
        .map_texture(&mapping)
        .context("error mapping closing snapshot")?;

    Ok(MemoryBuffer::new(
        bytes.to_vec(),
        fourcc,
        buffer_size,
        scale,
        Transform::Normal,
    ))
}
