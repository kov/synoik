use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::{Blocker, BlockerState};

use crate::animation::Animation;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::custom_anim::CustomAnimRenderElement;
use crate::render_helpers::snapshot::NeutralSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::RenderTarget;
use crate::synoik_render_elements;
use crate::utils::transaction::TransactionBlocker;

/// The three block-out variants of a neutral snapshot, uploaded to `VkTexture`s on first use.
type VariantCache =
    std::cell::RefCell<[Option<TextureBuffer<crate::render_helpers::vulkan::VkTexture>>; 3]>;

/// The window contents a [`ClosingWindow`] samples, per block-out variant.
#[derive(Debug)]
struct ClosingBuffers {
    snapshot: NeutralSnapshot,

    /// Each variant uploaded to a `VkTexture` on first use, cached across the animation's frames —
    /// re-uploading every frame would churn virtio-gpu blobs. Keyed strictly by variant index (see
    /// [`NeutralSnapshot::variant`]); a failed upload never falls back to another variant's
    /// texture.
    vk: VariantCache,
}

#[derive(Debug)]
pub struct ClosingWindow {
    /// The window contents to animate.
    buffers: ClosingBuffers,

    /// Size of the window geometry.
    geo_size: Size<f64, Logical>,

    /// Position in the workspace.
    pos: Point<f64, Logical>,

    /// The closing animation.
    anim_state: AnimationState,

    /// Random seed for the shader.
    random_seed: f32,
}

synoik_render_elements! {
    ClosingWindowRenderElement => {
        Texture = RelocateRenderElement<RescaleRenderElement<CapturedTextureRenderElement>>,
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
    /// The snapshot is already renderer-neutral — captured as CPU buffers when the window unmapped
    /// — so there is nothing to bake here; the variants upload to `VkTexture`s on first draw.
    pub fn new(
        snapshot: NeutralSnapshot,
        geo_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
        anim: Animation,
    ) -> Self {
        let _span = tracy_client::span!("ClosingWindow::new");

        Self {
            geo_size,
            pos,
            buffers: ClosingBuffers {
                snapshot,
                vk: std::cell::RefCell::new(Default::default()),
            },
            anim_state: AnimationState::new(blocker, anim),
            random_seed: fastrand::f32(),
        }
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

    /// Draw the closing animation from the neutral buffers captured at snapshot time, uploaded once
    /// each to a cached `VkTexture`. Applies the user's custom `close` shader if one is installed,
    /// else the built-in scale + fade.
    ///
    /// `None` when the target has no block-out variant safe to draw, or when the upload failed —
    /// never a substitute, which for a blocked-out target would mean drawing the real window into a
    /// screencast.
    pub fn render_vulkan(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
        view_rect: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        target: RenderTarget,
    ) -> Option<ClosingWindowRenderElement> {
        use crate::render_helpers::vulkan::{pack_affine, CustomAnimPush, CustomShaderType};

        let ClosingBuffers { snapshot, vk } = &self.buffers;

        // Select the variant *before* the animation state below: the Waiting branch draws too, and
        // must be just as target-correct as the animating one.
        let (idx, (mem, geo)) = snapshot.variant(target)?;
        let offset = geo.loc.to_f64().to_logical(scale);

        // Lazily upload this variant to its own cached VkTexture (like the resize `prev_vk`).
        if vk.borrow()[idx].is_none() {
            match TextureBuffer::from_memory_buffer(renderer, mem) {
                Ok(tb) => vk.borrow_mut()[idx] = Some(tb),
                Err(err) => {
                    warn!("error uploading closing snapshot to Vulkan: {err:?}");
                    return None;
                }
            }
        }
        let buffer = vk.borrow()[idx].clone()?;

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

                let elem = CapturedTextureRenderElement(elem);
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

        let elem = CapturedTextureRenderElement(elem);

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
