//! `RenderElement<VulkanRenderer>` impls for niri's custom effect elements.
//!
//! For the Stage 2 seam these are **degraded no-ops**: the element draws nothing on Vulkan (a
//! visible gap), so `OutputRenderElements<VulkanRenderer>` composes and renders while the effects
//! are ported one by one in the material tail (M3). The plain Smithay elements
//! (`SolidColorRenderElement`, `WaylandSurfaceRenderElement`, `TextureRenderElement`, …) already
//! implement `RenderElement<R>` generically, so they render correctly through Vulkan today.

use smithay::backend::renderer::element::{RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle};

use super::{VulkanError, VulkanFrame, VulkanRenderer};
use crate::render_helpers::clipped_surface::ClippedSurfaceRenderElement;
use crate::render_helpers::framebuffer_effect::FramebufferEffectElement;
use crate::render_helpers::gradient_fade_texture::GradientFadeTextureRenderElement;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::shader_element::ShaderRenderElement;
use crate::render_helpers::xray::XrayElement;

/// Emit a degraded (no-op) `RenderElement<VulkanRenderer>` impl for a niri effect element.
macro_rules! degraded_vulkan_element {
    ($ty:ty) => {
        impl RenderElement<VulkanRenderer> for $ty {
            fn draw(
                &self,
                _frame: &mut VulkanFrame<'_, '_>,
                _src: Rectangle<f64, Buffer>,
                _dst: Rectangle<i32, Physical>,
                _damage: &[Rectangle<i32, Physical>],
                _opaque_regions: &[Rectangle<i32, Physical>],
                _cache: Option<&UserDataMap>,
            ) -> Result<(), VulkanError> {
                // Not ported to Vulkan yet (M3): render nothing rather than a wrong picture.
                Ok(())
            }

            fn underlying_storage(
                &self,
                _renderer: &mut VulkanRenderer,
            ) -> Option<UnderlyingStorage<'_>> {
                None
            }
        }
    };
}

// BorderRenderElement and ShadowRenderElement have real (procedural) Vulkan draws in their modules.
// The `VkTexture` specialization has a real (SDF-rounding) impl in `rounded_texture.rs`; the
// `GlesTexture` one (carried by `OutputRenderElements<VulkanRenderer>`) stays a no-op.
degraded_vulkan_element!(RoundedTextureRenderElement<GlesTexture>);
degraded_vulkan_element!(ClippedSurfaceRenderElement<VulkanRenderer>);
degraded_vulkan_element!(GradientFadeTextureRenderElement<GlesTexture>);
// ResizeRenderElement has a real Vulkan draw (render_resize) in render_helpers/resize.rs.
degraded_vulkan_element!(XrayElement);
degraded_vulkan_element!(ShaderRenderElement);
degraded_vulkan_element!(FramebufferEffectElement);
degraded_vulkan_element!(OffscreenRenderElement<GlesTexture>);
degraded_vulkan_element!(PrimaryGpuTextureRenderElement);
