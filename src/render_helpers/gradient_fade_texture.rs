use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::texture::TextureRenderElement;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::renderer::AsGlesFrame as _;
use crate::render_helpers::shaders::Shaders;

/// Generic over the stored texture `T` (mirrors
/// [`super::rounded_texture::RoundedTextureRenderElement`]): `GlesTexture` (the default — every
/// bare mention resolves to it) fades via the `gradient_fade` shader in `program`; the owned Vulkan
/// renderer fades in its own pipeline and leaves `program` `None`.
#[derive(Debug, Clone)]
pub struct GradientFadeTextureRenderElement<T: Texture = GlesTexture> {
    inner: TextureRenderElement<T>,
    program: Option<GradientFadeShader>,
    cutoff: (f32, f32),
}

#[derive(Debug, Clone)]
pub struct GradientFadeShader(GlesTexProgram);

impl<T: Texture> GradientFadeTextureRenderElement<T> {
    /// The horizontal fade band `(left, right)` in the sampled texture's u coordinate — `(1, 1)`
    /// (no fade) when the texture is shown full-width, else a fade near the clipped right edge.
    /// Depends only on the buffer/src widths, so it is renderer-agnostic.
    fn compute_cutoff(texture: &TextureRenderElement<T>) -> (f32, f32) {
        let logical_w = texture.buffer().logical_size().w;
        let logical_src_w = texture.logical_src().size.w;
        if logical_src_w < logical_w {
            // Texture is clipped, add a fade.
            let cutoff = 1. - f64::min(18. / logical_src_w, 1.);
            let full = logical_src_w / logical_w;
            ((cutoff * full) as f32, full as f32)
        } else {
            // Texture is displayed full-size, no cutoff necessary.
            (1., 1.)
        }
    }
}

impl GradientFadeTextureRenderElement<GlesTexture> {
    pub fn new(texture: TextureRenderElement<GlesTexture>, program: GradientFadeShader) -> Self {
        let cutoff = Self::compute_cutoff(&texture);
        Self {
            inner: texture,
            program: Some(program),
            cutoff,
        }
    }

    pub fn shader(renderer: &mut GlesRenderer) -> Option<GradientFadeShader> {
        let program = Shaders::get(renderer)
            .expect("a GlesRenderer is always GLES-backed")
            .gradient_fade
            .clone();
        program.map(GradientFadeShader)
    }
}

impl<T: Texture> Element for GradientFadeTextureRenderElement<T> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for GradientFadeTextureRenderElement<GlesTexture> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(program) = self.program.as_ref() else {
            return RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        };
        let uniforms = vec![Uniform::new("cutoff", self.cutoff)];
        frame.override_default_tex_program(program.0.clone(), uniforms);
        RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        frame.clear_tex_program_override();
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // If scanout for things other than Wayland buffers is implemented, this will need to take
        // the target GPU into account.
        None
    }
}

impl<'render> RenderElement<TtyRenderer<'render>>
    for GradientFadeTextureRenderElement<GlesTexture>
{
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
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

    fn underlying_storage(
        &self,
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        // If scanout for things other than Wayland buffers is implemented, this will need to take
        // the target GPU into account.
        None
    }
}

// The owned Vulkan renderer fades a `VkTexture` in its own pipeline (M3). Like rounded-texture,
// the `GlesTexture` specialization keeps a degraded no-op `RenderElement<VulkanRenderer>`.
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

impl GradientFadeTextureRenderElement<VkTexture> {
    /// Build a gradient-fade element for the owned Vulkan renderer, which fades in its own
    /// pipeline and needs no GLES shader program.
    pub fn new_vulkan(texture: TextureRenderElement<VkTexture>) -> Self {
        let cutoff = Self::compute_cutoff(&texture);
        Self {
            inner: texture,
            program: None,
            cutoff,
        }
    }
}

impl RenderElement<VulkanRenderer> for GradientFadeTextureRenderElement<VkTexture> {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        let texture = self.inner.buffer().texture();
        let alpha = Element::alpha(&self.inner);
        let src_transform = Element::transform(&self.inner);
        frame.render_gradient_fade(texture, src, dst, damage, src_transform, self.cutoff, alpha)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
