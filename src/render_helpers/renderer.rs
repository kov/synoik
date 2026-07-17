use smithay::backend::renderer::Renderer;

/// A renderer that can produce **re-sampleable** offscreen snapshots (see
/// [`crate::render_helpers::offscreen::OffscreenBuffer`]): it knows how to make a freshly-rendered
/// offscreen texture readable by a later sampling draw, and whether a cached offscreen texture can
/// be reused. The owned Vulkan renderer inserts a layout barrier and checks its own `Arc`
/// uniqueness.
pub trait OffscreenRenderer: Renderer {
    /// Prepare a just-rendered offscreen `texture` to be sampled: the Vulkan renderer transitions
    /// the image layout.
    fn make_offscreen_sampleable(&self, _texture: &Self::TextureId) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether a cached offscreen `texture` can be reused, i.e. no other live reference holds its
    /// GPU resources (a still-displayed snapshot from a previous frame must not be drawn over).
    fn offscreen_is_reusable(&self, texture: &mut Self::TextureId) -> bool;
}
