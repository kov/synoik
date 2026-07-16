//! `RenderElement<VulkanRenderer>` impls for niri's custom effect elements.
//!
//! What remains here are **degraded no-ops** for the GLES-texture specializations of elements whose
//! Vulkan variants live elsewhere: they draw nothing, and exist only so those types satisfy
//! `RenderElement<VulkanRenderer>` where a generic bound demands it. They go with the GLES
//! specializations themselves. The plain Smithay elements (`SolidColorRenderElement`,
//! `WaylandSurfaceRenderElement`, `TextureRenderElement`, …) implement `RenderElement<R>`
//! generically, so they render through Vulkan directly.

use smithay::backend::renderer::element::{RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle};

use super::{VulkanError, VulkanFrame, VulkanRenderer};
use crate::render_helpers::gradient_fade_texture::GradientFadeTextureRenderElement;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::shader_element::ShaderRenderElement;

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

            // A no-op override of Smithay's `unimplemented!()` default. Only reached for a
            // framebuffer effect (`FramebufferEffectElement`, degraded here) — without this the
            // enum's forwarded `capture_framebuffer` would panic on a Vulkan session as soon as any
            // blur/postprocess element is on screen. Degrading means the effect draws nothing.
            fn capture_framebuffer(
                &self,
                _frame: &mut VulkanFrame<'_, '_>,
                _src: Rectangle<f64, Buffer>,
                _dst: Rectangle<i32, Physical>,
                _cache: &UserDataMap,
            ) -> Result<(), VulkanError> {
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
// ClippedSurfaceRenderElement<VulkanRenderer> has a real Vulkan draw (clip via the clipped_texture
// pipeline) in clipped_surface.rs.
degraded_vulkan_element!(GradientFadeTextureRenderElement<GlesTexture>);
// ResizeRenderElement has a real Vulkan draw (render_resize) in render_helpers/resize.rs.
// XrayElement has a real Vulkan draw (offscreen sample + blur + postprocess-and-clip) in xray.rs.
degraded_vulkan_element!(ShaderRenderElement);
// FramebufferEffectElement has a real Vulkan draw (backdrop capture + blur + postprocess) in
// framebuffer_effect.rs.
degraded_vulkan_element!(OffscreenRenderElement<GlesTexture>);
