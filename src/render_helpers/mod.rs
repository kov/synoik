use std::ptr;

use anyhow::{ensure, Context as _};
use niri_config::BlockOutFrom;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Fourcc};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{Element, RenderElement, RenderElementStates};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, Offscreen, Renderer, Texture as _,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shm;

use self::vulkan::VulkanRenderer;
pub use self::vulkan::NATIVE_FOURCC;
use crate::render_helpers::xray::Xray;

pub mod background_effect;
pub mod blur;
pub mod border;
pub mod captured_texture;
pub mod clipped_surface;
pub mod custom_anim;
pub mod damage;
pub mod debug;
pub mod effect_buffer;
pub mod framebuffer_effect;
pub mod gradient_fade_texture;
pub mod icon;
pub mod memory;
pub mod offscreen;
pub mod render_elements;
pub mod renderer;
pub mod resize;
pub mod rounded_solid;
pub mod rounded_texture;
pub mod shadow;
pub mod snapshot;
pub mod solid_color;
pub mod surface;
pub mod texture;
pub mod vulkan;
pub mod xray;

/// A rendering context.
///
/// Bundles together things needed by most rendering code.
pub struct RenderCtx<'a> {
    pub renderer: &'a mut VulkanRenderer,
    pub target: RenderTarget,
    pub xray: Option<&'a Xray>,
}

impl<'a> RenderCtx<'a> {
    /// Reborrows this context with a smaller lifetime.
    #[inline]
    pub fn r<'b>(&'b mut self) -> RenderCtx<'b> {
        RenderCtx {
            renderer: self.renderer,
            target: self.target,
            xray: self.xray,
        }
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
    // Render in the renderer's own order (the only one it has a render pass for) and let the
    // readback convert if `fourcc` is the other one.
    render_and_download_as(
        renderer,
        size,
        scale,
        transform,
        NATIVE_FOURCC,
        fourcc,
        elements,
    )
}

/// [`render_and_download`], but reading the frame back in a byte order that need not be the one it
/// was rendered in — the renderer converts on the way out.
///
/// The two are separate because a renderer can only render the orders it has a render pass for (the
/// owned Vulkan renderer: [`NATIVE_FOURCC`]'s order only), while a consumer wants whatever order
/// *it* declared. Converting on the way out is one GPU blit, versus a CPU pass over every pixel.
pub fn render_and_download_as<R, T>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    render_fourcc: Fourcc,
    read_fourcc: Fourcc,
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
        render_to_texture(renderer, size, scale, transform, render_fourcc, elements)
            .context("error rendering")?;
    let target = renderer
        .bind(&mut texture)
        .context("error binding texture for readback")?;

    copy_framebuffer(renderer, &target, read_fourcc).context("error copying framebuffer")
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

pub fn render_to_dmabuf(
    renderer: &mut VulkanRenderer,
    damage_tracker: &mut OutputDamageTracker,
    mut dmabuf: Dmabuf,
    elements: &[impl RenderElement<VulkanRenderer>],
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

pub fn render_to_shm(
    renderer: &mut VulkanRenderer,
    damage_tracker: &mut OutputDamageTracker,
    buffer: &WlBuffer,
    elements: &[impl RenderElement<VulkanRenderer>],
    states: RenderElementStates,
) -> anyhow::Result<()> {
    let _span = tracy_client::span!();

    // The shm pool wants `Xrgb8888` — BGRA byte order, which is what we read back below, straight
    // into the pool. That is also the renderer's own order ([`NATIVE_FOURCC`]) since 2026-07-31, so
    // the readback is a plain copy: no staging image, no conversion blit.
    let render_fourcc = NATIVE_FOURCC;

    shm::with_buffer_contents_mut(buffer, |shm_buffer, shm_len, buffer_data| {
        let (size, _scale, _transform) = damage_tracker.mode().try_into().unwrap();

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
            create_texture(renderer, size, render_fourcc).context("error creating texture")?;
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

        // Read back in the pool's own order on both renderers, so this is a straight copy.
        let mapping = copy_framebuffer(renderer, &target, Fourcc::Xrgb8888)
            .context("error copying framebuffer")?;
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

pub fn clear_dmabuf(
    renderer: &mut VulkanRenderer,
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
