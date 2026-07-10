//! A concrete render element that carries either a GLES-sampled texture (the live path) or a
//! `VkTexture` sampled through the owned Vulkan renderer.
//!
//! Overlays that capture content through GLES (which the Vulkan renderer can't sample) upload it to
//! a `VkTexture` and need to hand the resulting *concrete* element back into the generic `<R>`
//! render tree. A generic `TextureRenderElement<R::TextureId>` can't be produced from a `VkTexture`
//! cached in a non-generic owning struct (a type-unification gap), so — like
//! [`ResizeRenderElement`](super::resize::ResizeRenderElement) — this is a concrete enum whose
//! `RenderElement<R>` impls dispatch per renderer. It sits as a single `OutputRenderElements<R>`
//! arm shared by every capture-based overlay (screen transition, screenshot UI, …).

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use super::rounded_texture::RoundedTextureRenderElement;
#[cfg(feature = "vulkan")]
use super::texture::TextureRenderElement;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
#[cfg(feature = "vulkan")]
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

/// See the module docs. `Gles` wraps the existing GLES-locked element (a degraded no-op on the
/// Vulkan renderer); `Vulkan` wraps a texture uploaded to and sampled by the owned Vulkan renderer.
#[derive(Debug)]
pub enum DualTextureRenderElement {
    Gles(PrimaryGpuTextureRenderElement),
    #[cfg(feature = "vulkan")]
    Vulkan(TextureRenderElement<VkTexture>),
}

impl Element for DualTextureRenderElement {
    fn id(&self) -> &Id {
        match self {
            DualTextureRenderElement::Gles(e) => e.id(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            DualTextureRenderElement::Gles(e) => e.current_commit(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.current_commit(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            DualTextureRenderElement::Gles(e) => e.geometry(scale),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.geometry(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            DualTextureRenderElement::Gles(e) => e.transform(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.transform(),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            DualTextureRenderElement::Gles(e) => e.src(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.src(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            DualTextureRenderElement::Gles(e) => e.damage_since(scale, commit),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            DualTextureRenderElement::Gles(e) => e.opaque_regions(scale),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            DualTextureRenderElement::Gles(e) => e.alpha(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            DualTextureRenderElement::Gles(e) => e.kind(),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(e) => e.kind(),
        }
    }
}

impl RenderElement<GlesRenderer> for DualTextureRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        match self {
            DualTextureRenderElement::Gles(e) => RenderElement::<GlesRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            // The Vulkan arm is only ever constructed on a Vulkan session, never here.
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(_) => {
                debug_assert!(false, "Vulkan DualTextureRenderElement drawn through GLES");
                Ok(())
            }
        }
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            DualTextureRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(_) => None,
        }
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for DualTextureRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        match self {
            DualTextureRenderElement::Gles(e) => RenderElement::<TtyRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(_) => {
                debug_assert!(
                    false,
                    "Vulkan DualTextureRenderElement drawn through Tty GLES"
                );
                Ok(())
            }
        }
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        match self {
            DualTextureRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            DualTextureRenderElement::Vulkan(_) => None,
        }
    }
}

#[cfg(feature = "vulkan")]
impl RenderElement<VulkanRenderer> for DualTextureRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        match self {
            // A legitimate degraded fallback: reached only if the Vulkan upload failed and we kept
            // the GLES element (PrimaryGpuTextureRenderElement draws nothing on Vulkan).
            DualTextureRenderElement::Gles(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            DualTextureRenderElement::Vulkan(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
        }
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// The rounded sibling of [`DualTextureRenderElement`], for the GNOME wallpaper: `Gles` wraps the
/// GLES-sampled, shader-rounded element (the live path, and the degraded no-op on Vulkan); `Vulkan`
/// wraps a `VkTexture` rounded by the owned Vulkan renderer's own SDF pipeline. Same
/// type-unification reason as its sibling — `RoundedTextureRenderElement<VkTexture>` has no
/// `RenderElement<GlesRenderer>` impl, so it can't ride a generic `<R>` arm directly; this concrete
/// enum dispatches per renderer.
#[derive(Debug)]
pub enum DualRoundedTextureRenderElement {
    Gles(RoundedTextureRenderElement<GlesTexture>),
    #[cfg(feature = "vulkan")]
    Vulkan(RoundedTextureRenderElement<VkTexture>),
}

impl Element for DualRoundedTextureRenderElement {
    fn id(&self) -> &Id {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.id(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.current_commit(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.current_commit(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.geometry(scale),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.geometry(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.transform(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.transform(),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.src(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.src(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.damage_since(scale, commit),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.opaque_regions(scale),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.alpha(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.kind(),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(e) => e.kind(),
        }
    }
}

impl RenderElement<GlesRenderer> for DualRoundedTextureRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => RenderElement::<GlesRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            // The Vulkan arm is only ever constructed on a Vulkan session, never here.
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(_) => {
                debug_assert!(
                    false,
                    "Vulkan DualRoundedTextureRenderElement drawn through GLES"
                );
                Ok(())
            }
        }
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(_) => None,
        }
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for DualRoundedTextureRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => RenderElement::<TtyRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(_) => {
                debug_assert!(
                    false,
                    "Vulkan DualRoundedTextureRenderElement drawn through Tty GLES"
                );
                Ok(())
            }
        }
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        match self {
            DualRoundedTextureRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            DualRoundedTextureRenderElement::Vulkan(_) => None,
        }
    }
}

#[cfg(feature = "vulkan")]
impl RenderElement<VulkanRenderer> for DualRoundedTextureRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        match self {
            // A legitimate degraded fallback: reached only if the Vulkan upload failed and we kept
            // the GLES element (its `RenderElement<VulkanRenderer>` is a no-op).
            DualRoundedTextureRenderElement::Gles(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            DualRoundedTextureRenderElement::Vulkan(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
        }
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
