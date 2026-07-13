use std::ptr;

use anyhow::{ensure, Context as _};
use niri_config::BlockOutFrom;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Fourcc};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, Kind, RenderElement, RenderElementStates};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ExportMem, Frame, Offscreen, Renderer, Texture as _};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shm;
use solid_color::{SolidColorBuffer, SolidColorRenderElement};

use self::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use self::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::renderer::{AsGlesRenderer, NiriRenderer};
use crate::render_helpers::xray::Xray;

pub mod background_effect;
pub mod blur;
pub mod border;
pub mod clipped_surface;
pub mod custom_anim;
pub mod damage;
pub mod debug;
pub mod dual_texture;
pub mod effect_buffer;
pub mod framebuffer_effect;
pub mod gradient_fade_texture;
pub mod memory;
pub mod offscreen;
pub mod primary_gpu_texture;
pub mod render_elements;
pub mod renderer;
pub mod resize;
pub mod resources;
pub mod rounded_texture;
pub mod shader_element;
pub mod shaders;
pub mod shadow;
pub mod snapshot;
pub mod solid_color;
pub mod surface;
pub mod texture;
#[cfg(feature = "vulkan")]
pub mod vulkan;
pub mod xray;

/// A rendering context.
///
/// Bundles together things needed by most rendering code.
pub struct RenderCtx<'a, R> {
    pub renderer: &'a mut R,
    pub target: RenderTarget,
    pub xray: Option<&'a Xray>,
}

impl<'a, R> RenderCtx<'a, R> {
    /// Reborrows this context with a smaller lifetime.
    #[inline]
    pub fn r<'b>(&'b mut self) -> RenderCtx<'b, R> {
        RenderCtx {
            renderer: self.renderer,
            target: self.target,
            xray: self.xray,
        }
    }
}

impl<'a, R: AsGlesRenderer> RenderCtx<'a, R> {
    /// Reborrows this context as a concrete `GlesRenderer` context, if the renderer is GLES-backed.
    /// Returns `None` for a non-GLES renderer, so GLES-only render paths degrade gracefully.
    pub fn try_as_gles<'b>(&'b mut self) -> Option<RenderCtx<'b, GlesRenderer>> {
        Some(RenderCtx {
            renderer: self.renderer.try_as_gles_renderer()?,
            target: self.target,
            xray: self.xray,
        })
    }

    /// Infallible [`RenderCtx::try_as_gles`], for GLES-only render paths. **Panics** on a non-GLES
    /// renderer; generic paths that must support Vulkan should use `try_as_gles` and degrade.
    pub fn as_gles<'b>(&'b mut self) -> RenderCtx<'b, GlesRenderer> {
        self.try_as_gles()
            .expect("this render path requires a GLES-backed renderer")
    }
}

#[cfg(feature = "vulkan")]
impl<'a, R: crate::render_helpers::renderer::AsVulkanRenderer> RenderCtx<'a, R> {
    /// Reborrows this context as a concrete `VulkanRenderer` context, if the renderer is the owned
    /// Vulkan renderer. Returns `None` otherwise, so Vulkan-only render paths (e.g. the resize
    /// crossfade) can specialize and fall back elsewhere.
    pub fn try_as_vulkan<'b>(
        &'b mut self,
    ) -> Option<RenderCtx<'b, crate::render_helpers::vulkan::VulkanRenderer>> {
        Some(RenderCtx {
            renderer: self.renderer.try_as_vulkan_renderer()?,
            target: self.target,
            xray: self.xray,
        })
    }
}

/// What we're rendering for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTarget {
    /// Rendering to display on screen.
    Output = 0,
    /// Rendering for a screencast.
    Screencast,
    /// Rendering for any other screen capture.
    ScreenCapture,
}

/// Buffer with location, src and dst.
#[derive(Debug)]
pub struct BakedBuffer<B> {
    pub buffer: B,
    pub location: Point<f64, Logical>,
    pub src: Option<Rectangle<f64, Logical>>,
    pub dst: Option<Size<i32, Logical>>,
}

pub trait ToRenderElement {
    type RenderElement;

    fn to_render_element(
        &self,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        kind: Kind,
    ) -> Self::RenderElement;
}

impl RenderTarget {
    pub const COUNT: usize = 3;

    pub fn should_block_out(self, block_out_from: Option<BlockOutFrom>) -> bool {
        match block_out_from {
            None => false,
            Some(BlockOutFrom::Screencast) => self == RenderTarget::Screencast,
            Some(BlockOutFrom::ScreenCapture) => self != RenderTarget::Output,
        }
    }
}

impl ToRenderElement for BakedBuffer<TextureBuffer<GlesTexture>> {
    type RenderElement = PrimaryGpuTextureRenderElement;

    fn to_render_element(
        &self,
        location: Point<f64, Logical>,
        _scale: Scale<f64>,
        alpha: f32,
        kind: Kind,
    ) -> Self::RenderElement {
        let elem = TextureRenderElement::from_texture_buffer(
            self.buffer.clone(),
            location + self.location,
            alpha,
            self.src,
            self.dst.map(|dst| dst.to_f64()),
            kind,
        );
        PrimaryGpuTextureRenderElement(elem)
    }
}

impl ToRenderElement for BakedBuffer<SolidColorBuffer> {
    type RenderElement = SolidColorRenderElement;

    fn to_render_element(
        &self,
        location: Point<f64, Logical>,
        _scale: Scale<f64>,
        alpha: f32,
        kind: Kind,
    ) -> Self::RenderElement {
        SolidColorRenderElement::from_buffer(&self.buffer, location + self.location, alpha, kind)
    }
}

pub fn encompassing_geo(
    scale: Scale<f64>,
    elements: impl Iterator<Item = impl Element>,
) -> Rectangle<i32, Physical> {
    elements
        .map(|ele| ele.geometry(scale))
        .reduce(|a, b| a.merge(b))
        .unwrap_or_default()
}

pub fn create_texture<R, T>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    fourcc: Fourcc,
) -> Result<T, R::Error>
where
    R: Renderer<TextureId = T> + Offscreen<T>,
{
    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
    renderer.create_buffer(fourcc, buffer_size)
}

pub fn copy_framebuffer<R: ExportMem>(
    renderer: &mut R,
    target: &R::Framebuffer<'_>,
    fourcc: Fourcc,
) -> Result<R::TextureMapping, R::Error> {
    renderer.copy_framebuffer(target, Rectangle::from_size(target.size()), fourcc)
}

pub fn render_to_encompassing_texture(
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    transform: Transform,
    fourcc: Fourcc,
    elements: &[impl RenderElement<GlesRenderer>],
) -> anyhow::Result<(GlesTexture, SyncPoint, Rectangle<i32, Physical>)> {
    let geo = encompassing_geo(scale, elements.iter());
    let elements = elements.iter().rev().map(|ele| {
        RelocateRenderElement::from_element(ele, geo.loc.upscale(-1), Relocate::Relative)
    });

    let (texture, sync_point) =
        render_to_texture(renderer, geo.size, scale, transform, fourcc, elements)?;

    Ok((texture, sync_point, geo))
}

pub fn render_to_texture<R, T>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    fourcc: Fourcc,
    elements: impl Iterator<Item = impl RenderElement<R>>,
) -> anyhow::Result<(T, SyncPoint)>
where
    R: Renderer<TextureId = T> + Offscreen<T>,
    R::Error: Send + Sync + 'static,
{
    let _span = tracy_client::span!();

    let mut texture = create_texture(renderer, size, fourcc).context("error creating texture")?;

    let sync_point = {
        let mut target = renderer
            .bind(&mut texture)
            .context("error binding texture")?;

        render_elements(renderer, &mut target, size, scale, transform, elements)?
    };

    Ok((texture, sync_point))
}

pub fn render_and_download<R, T>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    fourcc: Fourcc,
    elements: impl Iterator<Item = impl RenderElement<R>>,
) -> anyhow::Result<R::TextureMapping>
where
    R: Renderer<TextureId = T> + Offscreen<T> + ExportMem,
    R::Error: Send + Sync + 'static,
{
    let _span = tracy_client::span!();

    // Render into a fresh offscreen, then bind a new framebuffer over it for readback. Factored
    // through `render_to_texture` (rather than one bind reused for both render and copy) so the two
    // phases don't share a live frame — the readback bind starts clean after the render frame has
    // finished and its resources released. Both GLES and the owned Vulkan renderer read back
    // correctly this way.
    let (mut texture, _sync) =
        render_to_texture(renderer, size, scale, transform, fourcc, elements)
            .context("error rendering")?;
    let target = renderer
        .bind(&mut texture)
        .context("error binding texture for readback")?;

    copy_framebuffer(renderer, &target, fourcc).context("error copying framebuffer")
}

pub fn render_to_vec<R, T>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    fourcc: Fourcc,
    elements: impl Iterator<Item = impl RenderElement<R>>,
) -> anyhow::Result<Vec<u8>>
where
    R: Renderer<TextureId = T> + Offscreen<T> + ExportMem,
    R::Error: Send + Sync + 'static,
{
    let _span = tracy_client::span!();

    let mapping = render_and_download(renderer, size, scale, transform, fourcc, elements)
        .context("error rendering")?;
    let copy = renderer
        .map_texture(&mapping)
        .context("error mapping texture")?;
    Ok(copy.to_vec())
}

pub fn render_to_dmabuf<R: NiriRenderer>(
    renderer: &mut R,
    damage_tracker: &mut OutputDamageTracker,
    mut dmabuf: Dmabuf,
    elements: &[impl RenderElement<R>],
    states: RenderElementStates,
) -> anyhow::Result<SyncPoint> {
    let _span = tracy_client::span!();
    let (size, _scale, _transform) = damage_tracker.mode().try_into().unwrap();
    ensure!(
        dmabuf.width() == size.w as u32 && dmabuf.height() == size.h as u32,
        "invalid buffer size"
    );

    let mut target = renderer.bind(&mut dmabuf).context("error binding dmabuf")?;
    let res = damage_tracker
        .render_output_with_states(
            renderer,
            &mut target,
            0,
            elements,
            Color32F::TRANSPARENT,
            states,
        )
        .context("error rendering to dmabuf")?;
    Ok(res.sync)
}

pub fn render_to_shm<R: NiriRenderer + Offscreen<R::NiriTextureId>>(
    renderer: &mut R,
    damage_tracker: &mut OutputDamageTracker,
    buffer: &WlBuffer,
    elements: &[impl RenderElement<R>],
    states: RenderElementStates,
) -> anyhow::Result<()> {
    let _span = tracy_client::span!();
    shm::with_buffer_contents_mut(buffer, |shm_buffer, shm_len, buffer_data| {
        let (size, _scale, _transform) = damage_tracker.mode().try_into().unwrap();
        let fourcc = Fourcc::Xrgb8888;

        ensure!(
            // The buffer prefers pixels in little endian ...
            buffer_data.format == wl_shm::Format::Xrgb8888
                && buffer_data.width == size.w
                && buffer_data.height == size.h
                && buffer_data.stride == size.w * 4
                && shm_len == buffer_data.stride as usize * buffer_data.height as usize,
            "invalid buffer format or size"
        );

        let mut texture =
            create_texture(renderer, size, fourcc).context("error creating texture")?;
        let mut target = renderer
            .bind(&mut texture)
            .context("error binding texture")?;

        let _res = damage_tracker
            .render_output_with_states(
                renderer,
                &mut target,
                0,
                elements,
                Color32F::TRANSPARENT,
                states,
            )
            .context("error rendering")?;

        let mapping =
            copy_framebuffer(renderer, &target, fourcc).context("error copying framebuffer")?;
        let bytes = renderer
            .map_texture(&mapping)
            .context("error mapping texture")?;

        unsafe {
            let _span = tracy_client::span!("copy_nonoverlapping");
            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buffer.cast(), shm_len);
        }

        Ok(())
    })
    .context("expected shm buffer, but didn't get one")?
}

pub fn clear_dmabuf<R: NiriRenderer>(
    renderer: &mut R,
    mut dmabuf: Dmabuf,
) -> anyhow::Result<SyncPoint> {
    let size = dmabuf.size();
    let size = size.to_logical(1, Transform::Normal).to_physical(1);
    let mut target = renderer.bind(&mut dmabuf).context("error binding dmabuf")?;
    let mut frame = renderer
        .render(&mut target, size, Transform::Normal)
        .context("error starting frame")?;
    frame
        .clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])
        .context("error clearing")?;
    frame.finish().context("error finishing frame")
}

fn render_elements<R: Renderer>(
    renderer: &mut R,
    target: &mut R::Framebuffer<'_>,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: impl Iterator<Item = impl RenderElement<R>>,
) -> anyhow::Result<SyncPoint>
where
    R::Error: Send + Sync + 'static,
{
    let transform = transform.invert();
    let output_rect = Rectangle::from_size(transform.transform_size(size));

    let mut frame = renderer
        .render(target, size, transform)
        .context("error starting frame")?;

    frame
        .clear(Color32F::TRANSPARENT, &[output_rect])
        .context("error clearing")?;

    for element in elements {
        let src = element.src();
        let dst = element.geometry(scale);

        if let Some(mut damage) = output_rect.intersection(dst) {
            damage.loc -= dst.loc;

            let cache = UserDataMap::new();
            if element.is_framebuffer_effect() {
                element
                    .capture_framebuffer(&mut frame, src, dst, &cache)
                    .context("error in capture_framebuffer()")?;
            }
            element
                .draw(&mut frame, src, dst, &[damage], &[], Some(&cache))
                .context("error drawing element")?;
        }
    }

    frame.finish().context("error finishing frame")
}
