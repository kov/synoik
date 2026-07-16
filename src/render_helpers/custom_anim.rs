//! The custom window **open**/**close** animation element, dispatched per concrete renderer like
//! [`ResizeRenderElement`](super::resize::ResizeRenderElement): on GLES/Tty it wraps the
//! runtime-GLSL [`ShaderRenderElement`] (`ProgramType::Open`/`Close`), and on the owned Vulkan
//! renderer it carries the window snapshot `VkTexture` + a prebuilt `CustomAnimPush`, drawn through
//! [`VulkanFrame::render_custom_anim`](super::vulkan::VulkanFrame::render_custom_anim). The
//! open/close animations construct the matching arm for whichever renderer is compositing; the GLES
//! arm is behaviourally identical to using `ShaderRenderElement` directly.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::shader_element::ShaderRenderElement;

#[derive(Debug)]
pub enum CustomAnimRenderElement {
    Gles(ShaderRenderElement),
    Vulkan(vulkan::VulkanCustomAnimRenderElement),
}

impl Element for CustomAnimRenderElement {
    fn id(&self) -> &Id {
        match self {
            CustomAnimRenderElement::Gles(e) => e.id(),
            CustomAnimRenderElement::Vulkan(e) => &e.id,
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            CustomAnimRenderElement::Gles(e) => e.current_commit(),
            CustomAnimRenderElement::Vulkan(e) => e.commit,
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            CustomAnimRenderElement::Gles(e) => e.geometry(scale),
            CustomAnimRenderElement::Vulkan(e) => e.area.to_physical_precise_round(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            CustomAnimRenderElement::Gles(e) => e.transform(),
            CustomAnimRenderElement::Vulkan(_) => Transform::Normal,
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            CustomAnimRenderElement::Gles(e) => e.src(),
            CustomAnimRenderElement::Vulkan(_) => Rectangle::from_size((1., 1.).into()),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            CustomAnimRenderElement::Gles(e) => e.damage_since(scale, commit),
            CustomAnimRenderElement::Vulkan(e) => {
                if commit != Some(e.commit) {
                    DamageSet::from_slice(&[e.area.to_physical_precise_round(scale)])
                } else {
                    DamageSet::default()
                }
            }
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            CustomAnimRenderElement::Gles(e) => e.opaque_regions(scale),
            CustomAnimRenderElement::Vulkan(_) => OpaqueRegions::default(),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            CustomAnimRenderElement::Gles(e) => e.alpha(),
            CustomAnimRenderElement::Vulkan(e) => e.alpha,
        }
    }

    fn kind(&self) -> Kind {
        match self {
            CustomAnimRenderElement::Gles(e) => e.kind(),
            CustomAnimRenderElement::Vulkan(e) => e.kind,
        }
    }
}

impl RenderElement<GlesRenderer> for CustomAnimRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        // The `Vulkan` arm only exists with the `vulkan` feature; without it this is irrefutable.
        #[allow(irrefutable_let_patterns)]
        let CustomAnimRenderElement::Gles(inner) = self
        else {
            return Ok(());
        };
        frame.with_gpu_span(
            gpu_span_location!("CustomAnimRenderElement::draw"),
            |frame| {
                RenderElement::<GlesRenderer>::draw(
                    inner,
                    frame,
                    src,
                    dst,
                    damage,
                    opaque_regions,
                    cache,
                )
            },
        )
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            CustomAnimRenderElement::Gles(e) => e.underlying_storage(renderer),
            CustomAnimRenderElement::Vulkan(_) => None,
        }
    }
}

mod vulkan {
    use smithay::backend::renderer::element::{Id, Kind, RenderElement, UnderlyingStorage};
    use smithay::backend::renderer::utils::CommitCounter;
    use smithay::utils::user_data::UserDataMap;
    use smithay::utils::{Buffer, Logical, Physical, Rectangle};

    use super::CustomAnimRenderElement;
    use crate::render_helpers::vulkan::{
        CustomAnimPush, CustomShaderType, VkTexture, VulkanError, VulkanFrame, VulkanRenderer,
    };

    /// The Vulkan arm of [`CustomAnimRenderElement`]: one window snapshot + a prebuilt
    /// `CustomAnimPush`, drawn via `VulkanFrame::render_custom_anim` for the user's `open`/`close`
    /// shader (`ty`). The material fields of `push` are filled by the animation;
    /// origin/size/target/ proj are filled by `render_custom_anim`.
    #[derive(Debug)]
    pub struct VulkanCustomAnimRenderElement {
        pub(super) id: Id,
        pub(super) commit: CommitCounter,
        pub(super) ty: CustomShaderType,
        pub(super) area: Rectangle<f64, Logical>,
        pub(super) alpha: f32,
        pub(super) kind: Kind,
        pub(super) texture: VkTexture,
        pub(super) push: CustomAnimPush,
    }

    impl CustomAnimRenderElement {
        /// Build the Vulkan arm for a custom `open`/`close` animation over `texture`, covering
        /// `area` (logical). `push` carries the material fields (`input_to_geo`/`geo_to_tex`/
        /// `geo_size`/`progress`/`random_seed`/`alpha`/`scale`); `ty` selects the shader slot.
        pub(crate) fn new_vulkan_anim(
            ty: CustomShaderType,
            texture: VkTexture,
            area: Rectangle<f64, Logical>,
            alpha: f32,
            push: CustomAnimPush,
        ) -> Self {
            Self::Vulkan(VulkanCustomAnimRenderElement {
                id: Id::new(),
                commit: CommitCounter::default(),
                ty,
                area,
                alpha,
                kind: Kind::Unspecified,
                texture,
                push,
            })
        }
    }

    impl RenderElement<VulkanRenderer> for CustomAnimRenderElement {
        fn draw(
            &self,
            frame: &mut VulkanFrame<'_, '_>,
            _src: Rectangle<f64, Buffer>,
            dst: Rectangle<i32, Physical>,
            damage: &[Rectangle<i32, Physical>],
            _opaque_regions: &[Rectangle<i32, Physical>],
            _cache: Option<&UserDataMap>,
        ) -> Result<(), VulkanError> {
            let CustomAnimRenderElement::Vulkan(inner) = self else {
                return Ok(());
            };
            frame.render_custom_anim(inner.ty, &inner.texture, dst, damage, inner.push)
        }

        fn underlying_storage(
            &self,
            _renderer: &mut VulkanRenderer,
        ) -> Option<UnderlyingStorage<'_>> {
            None
        }
    }
}
