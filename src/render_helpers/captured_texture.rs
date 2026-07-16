//! A concrete render element carrying a `VkTexture` that an overlay captured and uploaded.
//!
//! Overlays that freeze the screen (the screen transition, the screenshot UI, the closing window)
//! cache the upload in a non-generic owning struct and need to hand the resulting *concrete*
//! element back into the generic `<R>` render tree. It cannot ride the shared
//! `UiTexture = TextureRenderElement<R::TextureId>` arm as a bare
//! `TextureRenderElement<VkTexture>`: the macro would emit a `From` impl that overlaps
//! `UiTexture`'s at `R = VulkanRenderer`, which does not compile. Hence the newtype, on its own
//! arm.
//!
//! Once the render tree is de-genericised and `R::TextureId` is concretely `VkTexture`, the two
//! arms become the same type and this can merge into `UiTexture` and go.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

#[derive(Debug)]
pub struct CapturedTextureRenderElement(pub TextureRenderElement<VkTexture>);

impl Element for CapturedTextureRenderElement {
    fn id(&self) -> &Id {
        self.0.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.0.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.0.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.0.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.0.alpha()
    }

    fn kind(&self) -> Kind {
        self.0.kind()
    }
}

impl RenderElement<VulkanRenderer> for CapturedTextureRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        RenderElement::<VulkanRenderer>::draw(
            &self.0,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
