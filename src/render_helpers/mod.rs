use std::ptr;

use anyhow::{ensure, Context as _};
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
use synoik_config::BlockOutFrom;

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
pub mod inset_ring;
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
pub mod window_thumbnail;
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

/// Render into an offscreen texture and copy the result into plain CPU memory.
///
/// This is the shared core of every "the consumer cannot take a dmabuf" path: a Wayland shm pool
/// ([`render_to_shm`]) and a PipeWire MemFd/MemPtr buffer both want exactly this, and the only
/// thing they disagree about is where the destination bytes live. Keep it that way — the second
/// copy of this function is where the two paths start to drift.
///
/// **Every frame is drawn in full, deliberately.** An [`OutputDamageTracker`] must not be used
/// here even though the caller has one: partial damage is only sound against a framebuffer that
/// still holds the previous frame, and this allocates a *fresh* texture each call. Smithay's
/// damage render skips clearing anything an opaque element covers and `continue`s past elements
/// whose damage is empty after occlusion (`damage/mod.rs:876-911`), so on a new allocation those
/// pixels are never written at all and the consumer sees uninitialized GPU memory — which reads
/// as a collage of old windows, popovers and animation frames rather than as obvious garbage.
/// That is a real bug this function used to have; the damage tracker still decides *whether* to
/// produce a frame, never *how much* of one.
///
/// The destination receives `size.h` rows of `size.w * 4` bytes at `dst_stride` byte intervals.
///
/// # Safety
///
/// `dst` must be valid for writes of `dst_stride * size.h` bytes, and `dst_stride` must be at
/// least `size.w * 4`. The caller is responsible for the destination outliving the call.
pub unsafe fn render_and_copy_to_memory<E>(
    renderer: &mut VulkanRenderer,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    dst: *mut u8,
    dst_stride: usize,
    elements: impl Iterator<Item = E>,
) -> anyhow::Result<()>
where
    E: RenderElement<VulkanRenderer>,
{
    let _span = tracy_client::span!();

    let row_bytes = size.w as usize * 4;
    ensure!(
        dst_stride >= row_bytes,
        "destination stride {dst_stride} is narrower than a {row_bytes}-byte row"
    );

    // The destination wants `Xrgb8888` — BGRA byte order. That is also the renderer's own order
    // ([`NATIVE_FOURCC`]) since 2026-07-31, so the readback is a plain copy: no conversion blit.
    let mapping = render_and_download(renderer, size, scale, transform, Fourcc::Xrgb8888, elements)
        .context("error rendering")?;
    let bytes = renderer
        .map_texture(&mapping)
        .context("error mapping texture")?;

    ensure!(
        bytes.len() >= row_bytes * size.h as usize,
        "readback returned {} bytes, need {}",
        bytes.len(),
        row_bytes * size.h as usize
    );

    unsafe {
        let _span = tracy_client::span!("copy_nonoverlapping");
        if dst_stride == row_bytes {
            ptr::copy_nonoverlapping(bytes.as_ptr(), dst.cast(), row_bytes * size.h as usize);
        } else {
            // A consumer is free to ask for a padded stride; copy row by row rather than
            // refusing, since the readback is always tightly packed.
            for y in 0..size.h as usize {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(y * row_bytes),
                    dst.add(y * dst_stride).cast(),
                    row_bytes,
                );
            }
        }
    }

    Ok(())
}

pub fn render_to_shm(
    renderer: &mut VulkanRenderer,
    damage_tracker: &mut OutputDamageTracker,
    buffer: &WlBuffer,
    elements: &[impl RenderElement<VulkanRenderer>],
) -> anyhow::Result<()> {
    let _span = tracy_client::span!();

    let (size, scale, transform) = damage_tracker.mode().try_into().unwrap();

    shm::with_buffer_contents_mut(buffer, |shm_buffer, shm_len, buffer_data| {
        ensure!(
            // The buffer prefers pixels in little endian ...
            buffer_data.format == wl_shm::Format::Xrgb8888
                && buffer_data.width == size.w
                && buffer_data.height == size.h
                && buffer_data.stride == size.w * 4
                && shm_len == buffer_data.stride as usize * buffer_data.height as usize,
            "invalid buffer format or size"
        );

        unsafe {
            render_and_copy_to_memory(
                renderer,
                size,
                scale,
                transform,
                shm_buffer.cast(),
                buffer_data.stride as usize,
                elements.iter().rev(),
            )
        }
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
