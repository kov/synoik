use std::fmt;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use ash::vk;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Texture, TextureMapping};
use synoik_vk::gpu::Gpu;
use synoik_vk::texture::Texture as SynoikTexture;

/// The one Vulkan color format this renderer renders/imports/exports. `B8G8R8A8_UNORM` is
/// `[B,G,R,A]` in memory, i.e. DRM `ARGB8888` (and `XRGB8888` ignoring alpha).
///
/// **This was `R8G8B8A8_UNORM` until 2026-07-30, and flipping it deleted 89% of a heavy frame's
/// GPU time.** BGRA is what everything around us actually speaks: KMS primary planes advertise
/// `Argb8888`/`Xrgb8888` (this VM's advertises nothing else), and it is the byte order of the two
/// `wl_shm` formats every client is guaranteed to have. With an RGBA-order render target the
/// scanout buffer could never be rendered into directly, so every frame drew into a shadow and
/// then paid a full-damage `vkCmdBlitImage` to reorder the channels on the way out — measured at
/// **8.42 ms p50 on heavy frames against 0.89 ms for the entire render pass**. Rendering in the
/// display's own order makes the scanout buffer a legal render target and both disappear.
///
/// The cost lands on `Abgr8888`/`Xbgr8888` instead, which now take the shadow path. That is the
/// right way round: it is the rarer order here, and nothing on this stack scans it out.
///
/// Note this only ever described *byte order in memory*. Shaders address components (`.r`, `.g`),
/// never bytes, so not one line of shader code cares which way this points.
pub(super) const IMAGE_VK_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Whether `f` is one of the DRM fourccs whose byte order matches [`IMAGE_VK_FORMAT`], i.e. can be
/// rendered into directly rather than through a channel-reordering shadow.
pub(super) fn matches_render_order(f: Fourcc) -> bool {
    matches!(f, Fourcc::Argb8888 | Fourcc::Xrgb8888)
}

/// The fourcc naming [`IMAGE_VK_FORMAT`]'s byte order — what an offscreen this renderer draws into
/// actually contains, and **the only order [`super::VulkanRenderer`] can create an offscreen in**
/// ([`Offscreen::create_buffer`](smithay::backend::renderer::Offscreen::create_buffer) rejects the
/// rest). Spell this rather than a literal fourcc at any render-target call site, so the next flip
/// of the render order does not have to find them all again.
///
/// A caller that wants pixels in the *other* order still gets them: readback converts on the way
/// out (`render_and_download_as`), which is a GPU blit rather than a CPU pass over the pixels.
pub const NATIVE_FOURCC: Fourcc = Fourcc::Argb8888;

/// Map a 32-bpp DRM fourcc to the VkFormat that reproduces its byte order when sampled, plus
/// whether the fourth byte is `X` (undefined) and alpha must be forced to 1.0. Vulkan non-PACK
/// formats name channels in memory-byte order, so the client's bytes upload verbatim (no CPU
/// swizzle) and a sampler returns correct RGBA regardless of texture format. Both VkFormats are
/// mandatory SAMPLED_IMAGE formats, so no runtime feature query is needed. This is broader than
/// [`matches_render_order`] (which gates the render targets / readback) because clients most
/// commonly send ARGB/XRGB (BGRA byte order).
pub(super) fn import_format(f: Fourcc) -> Option<(vk::Format, bool)> {
    match f {
        Fourcc::Argb8888 => Some((vk::Format::B8G8R8A8_UNORM, false)),
        Fourcc::Xrgb8888 => Some((vk::Format::B8G8R8A8_UNORM, true)),
        Fourcc::Abgr8888 => Some((vk::Format::R8G8B8A8_UNORM, false)),
        Fourcc::Xbgr8888 => Some((vk::Format::R8G8B8A8_UNORM, true)),
        _ => None,
    }
}

// --- VkTexture: a sampled texture id (Smithay `TextureId`), optionally an offscreen target -------

struct VkTextureInner {
    gpu: Arc<Gpu>,
    tex: SynoikTexture,
    /// A one-set pool owned by this texture, so freeing the set can't outlive a shared pool.
    desc_pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    /// Present iff this texture was created as an offscreen render target
    /// (`Offscreen::create_buffer`) — a render-pass framebuffer over `tex`'s image, so a frame
    /// can draw into it and it can then be sampled through `set`. Imported textures
    /// (`ImportMem`) leave this `None`.
    framebuffer: Option<vk::Framebuffer>,
    /// The image's current layout, tracked across the synchronous (fence-per-submit) lifecycle so
    /// [`super::VulkanRenderer::make_sampleable`] and readback can insert the right barrier.
    /// Stored as `vk::ImageLayout::as_raw()`; interior-mutable so it can be updated through a
    /// shared `&VkTexture` (sampling borrows the source immutably).
    layout: AtomicI32,
    width: u32,
    height: u32,
    format: Fourcc,
    flipped: bool,
}

impl Drop for VkTextureInner {
    fn drop(&mut self) {
        unsafe {
            let d = &self.gpu.device;
            // Destroying the pool frees `set`; then the render-pass framebuffer (offscreen only);
            // then the sampled image/view/sampler (via `tex.destroy`).
            d.destroy_descriptor_pool(self.desc_pool, None);
            if let Some(fb) = self.framebuffer {
                d.destroy_framebuffer(fb, None);
            }
        }
        self.tex.destroy(&self.gpu);
    }
}

/// A Vulkan texture: always sampleable (owns a combined image-sampler descriptor set), and — when
/// created via `Offscreen::create_buffer` — also a render target (owns a render-pass framebuffer).
/// Cheap to clone (ref-counted); the last clone frees the GPU resources.
#[derive(Clone)]
pub struct VkTexture(Arc<VkTextureInner>);

impl VkTexture {
    /// An imported (sampled-only) texture: no framebuffer, already in `SHADER_READ_ONLY_OPTIMAL`
    /// (its upload transitioned it there).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        gpu: Arc<Gpu>,
        tex: SynoikTexture,
        desc_pool: vk::DescriptorPool,
        set: vk::DescriptorSet,
        width: u32,
        height: u32,
        format: Fourcc,
        flipped: bool,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool,
            set,
            framebuffer: None,
            layout: AtomicI32::new(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw()),
            width,
            height,
            format,
            flipped,
        }))
    }

    /// A freshly imported client dmabuf, **before** its acquire barrier: no framebuffer, and
    /// `UNDEFINED` layout because nothing has transitioned it yet.
    ///
    /// [`synoik_vk::texture::Texture::import_dmabuf_sampled`] deliberately does not acquire — that
    /// cost a command buffer, a submit and a fence wait for one pipeline barrier. The caller
    /// queues this texture on
    /// [`VulkanRenderer::pending_dmabuf_acquires`](super::renderer::VulkanRenderer) so the next
    /// frame records it, exactly like a recommit's re-acquire. Tracking the layout honestly is
    /// load-bearing: [`Self::record_reacquire_dmabuf`] passes the tracked layout as the barrier's
    /// `old_layout`, so claiming `SHADER_READ_ONLY_OPTIMAL` here would emit a barrier out of an
    /// layout the image was never in.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_unacquired_dmabuf(
        gpu: Arc<Gpu>,
        tex: SynoikTexture,
        desc_pool: vk::DescriptorPool,
        set: vk::DescriptorSet,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool,
            set,
            framebuffer: None,
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// An offscreen render target that is also sampleable: carries a render-pass `framebuffer` and
    /// starts in `UNDEFINED` layout (the render pass performs the first transition).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_offscreen(
        gpu: Arc<Gpu>,
        tex: SynoikTexture,
        desc_pool: vk::DescriptorPool,
        set: vk::DescriptorSet,
        framebuffer: vk::Framebuffer,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool,
            set,
            framebuffer: Some(framebuffer),
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// A dmabuf-backed scanout render target: like [`Self::new_offscreen`] but carries **no**
    /// descriptor set (a scanout buffer is rendered into and handed to KMS, never sampled), so its
    /// `SynoikTexture` need not — and, being `COLOR_ATTACHMENT`-only, must not — advertise
    /// `SAMPLED` usage. The null `desc_pool`/`set` are valid no-ops to destroy. Starts
    /// `UNDEFINED`; the render pass performs the first transition.
    pub(super) fn new_dmabuf_target(
        gpu: Arc<Gpu>,
        tex: SynoikTexture,
        framebuffer: vk::Framebuffer,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            framebuffer: Some(framebuffer),
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// A dmabuf-backed **present-blit** target: the scanout buffer whose byte order (`Argb8888`/
    /// `Xrgb8888` → `B8G8R8A8`) differs from the renderer's render pass, so it is never
    /// rendered into directly — [`super::VulkanFrame::finish`] blits the shadow into it
    /// (reordering bytes). Carries no framebuffer (not a render target) and no descriptor set
    /// (never sampled). Starts `UNDEFINED`; the blit barriers transition it.
    pub(super) fn new_present_target(
        gpu: Arc<Gpu>,
        tex: SynoikTexture,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            framebuffer: None,
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// The combined image-sampler descriptor set that binds this texture at set 0, binding 0.
    pub(super) fn descriptor_set(&self) -> vk::DescriptorSet {
        self.0.set
    }

    pub(super) fn flipped(&self) -> bool {
        self.0.flipped
    }

    /// How many handles to the underlying GPU resources are live, this one included. The renderer
    /// uses it to answer the reuse question while discounting the references *it* holds — a
    /// keep-alive queue is not a foreign owner.
    pub(super) fn reference_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Whether this is the only handle to the underlying GPU resources (no other clone is live) —
    /// used by `OffscreenBuffer` to decide whether a cached offscreen texture can be reused.
    pub fn is_unique_reference(&mut self) -> bool {
        Arc::get_mut(&mut self.0).is_some()
    }

    /// The underlying color image (sampled source and, for offscreens, the render-pass attachment).
    /// The wrapped `synoik-vk` texture, for operations that live on it (the glyph atlas's
    /// region upload).
    pub(super) fn inner(&self) -> &SynoikTexture {
        &self.0.tex
    }

    pub(super) fn image(&self) -> vk::Image {
        self.0.tex.image
    }

    /// The underlying synoik-vk texture (image + view + sampler), for low-level pipelines that
    /// sample it directly — e.g. the blur chain, which needs its `view`/`sampler`.
    pub(super) fn synoik_texture(&self) -> &SynoikTexture {
        &self.0.tex
    }

    /// The render-pass framebuffer, iff this is an offscreen target. `None` for imported textures.
    pub(super) fn framebuffer(&self) -> Option<vk::Framebuffer> {
        self.0.framebuffer
    }

    pub(super) fn extent(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    /// The image's tracked current layout.
    pub(super) fn layout(&self) -> vk::ImageLayout {
        vk::ImageLayout::from_raw(self.0.layout.load(Ordering::Acquire))
    }

    /// Record the image's new layout after a barrier / render pass transition.
    pub(super) fn set_layout(&self, layout: vk::ImageLayout) {
        self.0.layout.store(layout.as_raw(), Ordering::Release);
    }

    /// Stage a full frame of tightly-packed pixels for this texture's existing image — the shm
    /// cache hit, refreshing a cached texture in place with no image allocation.
    ///
    /// **Stages only.** The returned [`StagedTexture`] carries the copy; the caller queues it so it
    /// rides a frame's command buffer
    /// ([`VulkanRenderer::pending_texture_uploads`](super::renderer::VulkanRenderer)) instead of
    /// costing a submit and a fence wait per commit. The tracked layout is advanced to
    /// `SHADER_READ_ONLY_OPTIMAL` here, on the same recorded-is-as-good-as-submitted basis as
    /// everything else in that queue: the copy is ordered before any later sample.
    pub(super) fn stage_reupload_shm(
        &self,
        pool: &mut synoik_vk::staging::StagingPool,
        data: &[u8],
    ) -> anyhow::Result<synoik_vk::texture::StagedTexture> {
        let staged = synoik_vk::texture::StagedTexture::reupload_32bpp(
            &self.0.gpu,
            pool,
            self.image(),
            self.0.width,
            self.0.height,
            data,
        )?;
        self.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(staged)
    }

    /// Whether two handles refer to the *same* underlying texture (ref-counted inner). Used by the
    /// dmabuf-import-cache test to prove a cache hit returns the reused image, not a fresh import.
    #[cfg(test)]
    pub(super) fn same_image(&self, other: &VkTexture) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }

    /// Record the deferred re-acquire barrier for this cached dmabuf-import into `cbuf` (a frame's
    /// command buffer), then advance the tracked layout. The client wrote new content into the same
    /// shared dmabuf, so before sampling again we re-take ownership from `FOREIGN` and invalidate
    /// the sampler caches. The barrier is *recorded, not submitted*: it rides the frame's single
    /// submit (see [`super::renderer::VulkanRenderer::record_pending_dmabuf_acquires`]). No
    /// allocation — reuses the imported image, view and descriptor set. Reads the current tracked
    /// layout at record time. The image ends in `SHADER_READ_ONLY_OPTIMAL`. See
    /// [`synoik_vk::texture::Texture::record_reacquire_dmabuf_sampled`].
    pub(super) fn record_reacquire_dmabuf(&self, cbuf: vk::CommandBuffer) {
        self.0
            .tex
            .record_reacquire_dmabuf_sampled(&self.0.gpu, cbuf, self.layout());
        self.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
}

// The shm texture cache stores `VkTexture` in the surface's `data_map`, which requires the value be
// `Send + Sync`. (All the Vulkan handles it holds are `u64`/atomics; `Gpu`'s dispatch tables are
// `Send + Sync`.) Pin it so a future field can't silently break the cache.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VkTexture>();
};

impl fmt::Debug for VkTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkTexture")
            .field("width", &self.0.width)
            .field("height", &self.0.height)
            .field("format", &self.0.format)
            .field("flipped", &self.0.flipped)
            .field("offscreen", &self.0.framebuffer.is_some())
            .finish()
    }
}

impl Texture for VkTexture {
    fn width(&self) -> u32 {
        self.0.width
    }
    fn height(&self) -> u32 {
        self.0.height
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.0.format)
    }
}

// --- GlyphRun: a shaped, rasterized string ready to draw ----------------------------------------

/// A shaped run of text: the shared R8 coverage atlas (as a sampleable [`VkTexture`]) plus the
/// per-glyph placements (run-local top-left pixel offsets + atlas sub-rects). Built by
/// [`super::VulkanRenderer::build_glyph_run`] and drawn by [`super::VulkanFrame::render_glyphs`].
/// Cheap to clone (the atlas is ref-counted); a UI element caches one and re-places it each frame.
#[derive(Clone)]
pub(crate) struct GlyphRun {
    atlas: VkTexture,
    glyphs: std::sync::Arc<[synoik_vk::text::PlacedGlyph]>,
    /// Source-span index of each glyph, parallel to `glyphs` (see
    /// [`synoik_vk::text::GlyphAtlas`]).
    spans: std::sync::Arc<[u32]>,
    side: u32,
    /// First-line line-box metrics (run-local px): baseline y, and the font's ascent/descent about
    /// it. See [`Self::line_box`].
    baseline: i32,
    ascent: f32,
    descent: f32,
}

impl GlyphRun {
    pub(crate) fn new(
        atlas: VkTexture,
        glyphs: Vec<synoik_vk::text::PlacedGlyph>,
        spans: Vec<u32>,
        side: u32,
        line_box: (i32, f32, f32),
    ) -> Self {
        let (baseline, ascent, descent) = line_box;
        GlyphRun {
            atlas,
            glyphs: glyphs.into(),
            spans: spans.into(),
            side,
            baseline,
            ascent,
            descent,
        }
    }

    /// Line-box metrics of the run's first line, run-local px: `(baseline_y, ascent, descent)`.
    /// `baseline_y` is measured from the same run-local origin as the glyph placements. Centering a
    /// single-line label on `[baseline_y - ascent, baseline_y + descent]` reproduces GNOME/Pango's
    /// vertical centering (which reserves descent space) instead of ink centering. Zeros for a
    /// glyph-less run.
    pub(crate) fn line_box(&self) -> (i32, f32, f32) {
        (self.baseline, self.ascent, self.descent)
    }

    pub(crate) fn atlas(&self) -> &VkTexture {
        &self.atlas
    }

    pub(crate) fn glyphs(&self) -> &[synoik_vk::text::PlacedGlyph] {
        &self.glyphs
    }

    /// Source-span index of each glyph, parallel to [`Self::glyphs`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn spans(&self) -> &[u32] {
        &self.spans
    }

    pub(crate) fn side(&self) -> u32 {
        self.side
    }

    /// The tight bounding box of the run's ink in run-local pixels: `(min_x, min_y, width,
    /// height)`. Empty (all zeros) when the run has no drawable glyphs (e.g. all whitespace).
    // Used by the panel draw-layer (increment 3) to size/place the run.
    #[allow(dead_code)]
    pub(crate) fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        Self::bounds_of(self.glyphs.iter())
    }

    /// The ink bounding box of just the glyphs from source span `span` (a paragraph span index),
    /// in run-local pixels — for painting an inline background (e.g. a keycap) behind that span.
    /// Empty (all zeros) when the span has no drawable glyphs. Only meaningful for runs built by
    /// [`super::VulkanRenderer::build_glyph_paragraph`]; single-run builds tag everything span 0.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn span_ink_bounds(&self, span: u32) -> (i32, i32, i32, i32) {
        Self::bounds_of(
            self.glyphs
                .iter()
                .zip(self.spans.iter())
                .filter(move |(_, s)| **s == span)
                .map(|(g, _)| g),
        )
    }

    /// Tight ink bbox over an iterator of glyphs: `(min_x, min_y, width, height)`, zeros if empty.
    fn bounds_of<'a>(
        glyphs: impl Iterator<Item = &'a synoik_vk::text::PlacedGlyph>,
    ) -> (i32, i32, i32, i32) {
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for g in glyphs {
            let (gx0, gy0, gx1, gy1) = (g.x, g.y, g.x + g.w as i32, g.y + g.h as i32);
            bounds = Some(match bounds {
                None => (gx0, gy0, gx1, gy1),
                Some((x0, y0, x1, y1)) => (x0.min(gx0), y0.min(gy0), x1.max(gx1), y1.max(gy1)),
            });
        }
        match bounds {
            Some((x0, y0, x1, y1)) => (x0, y0, x1 - x0, y1 - y0),
            None => (0, 0, 0, 0),
        }
    }
}

// --- VkFramebuffer: a bound target (Smithay `Framebuffer<'buffer>`) -----------------------------

/// An in-use render target — Smithay's `Framebuffer`. Holds the target [`VkTexture`] as a cheap
/// ref-counted clone rather than a borrow: a `Framebuffer` must be able to outlive the `bind` call,
/// and for a dmabuf scanout target the caller owns only the `Dmabuf` (not a `VkTexture`), so there
/// is nothing to borrow — the renderer imports one and the framebuffer keeps it alive (cf. Pixman's
/// refcounted `Framebuffer` image). The lifetime is a marker so this still fits Smithay's
/// `Framebuffer<'buffer>` GAT. Cloning is an `Arc` bump; the underlying GPU image is shared, so
/// rendering through the clone writes the same image the caller (offscreen) or dmabuf reads back.
pub struct VkFramebuffer<'a> {
    pub(super) buffer: VkTexture,
    /// Whether `buffer` is an offscreen texture (bound via `Bind<VkTexture>`) rather
    /// than a scanout dmabuf. An offscreen exists to be sampled afterwards, so a
    /// frame targeting one finishes it in `SHADER_READ_ONLY_OPTIMAL`; a scanout
    /// target must stay in `TRANSFER_SRC_OPTIMAL` for the present blit.
    pub(super) offscreen: bool,
    /// Set on the **present-blit** scanout path (planes whose byte order differs from the
    /// renderer's render pass — since 2026-07-31 that is `Abgr8888`/`Xbgr8888`, not the KMS-common
    /// `Argb8888`/`Xrgb8888`): `buffer` is then a [`NATIVE_FOURCC`]-order shadow rendered into as
    /// usual, and this is the imported dmabuf that [`super::VulkanFrame::finish`] blits the shadow
    /// into, the blit reordering the channels.
    /// `None` for direct render targets (offscreen textures, or RGBA-order dmabufs rendered
    /// into in place).
    pub(super) present: Option<VkTexture>,
    pub(super) _marker: std::marker::PhantomData<&'a mut ()>,
}

impl VkFramebuffer<'_> {
    /// An offscreen framebuffer: `buffer` is a renderable [`VkTexture`] that exists
    /// to be sampled once rendered. See [`Self::offscreen`].
    pub(super) fn new_offscreen(buffer: VkTexture) -> Self {
        VkFramebuffer {
            buffer,
            offscreen: true,
            present: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// A scanout framebuffer rendered into directly (an RGBA-order dmabuf).
    pub(super) fn new(buffer: VkTexture) -> Self {
        VkFramebuffer {
            buffer,
            offscreen: false,
            present: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// A present-blit framebuffer: render into `buffer` (a shadow), then blit into
    /// `present` (the imported scanout dmabuf) on `finish`. See [`Self::present`].
    pub(super) fn new_with_present(buffer: VkTexture, present: VkTexture) -> Self {
        VkFramebuffer {
            buffer,
            offscreen: false,
            present: Some(present),
            _marker: std::marker::PhantomData,
        }
    }
}

impl fmt::Debug for VkFramebuffer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkFramebuffer")
            .field("buffer", &self.buffer)
            .finish()
    }
}

impl Texture for VkFramebuffer<'_> {
    fn width(&self) -> u32 {
        self.buffer.width()
    }
    fn height(&self) -> u32 {
        self.buffer.height()
    }
    fn format(&self) -> Option<Fourcc> {
        // On the present-blit path the true target is the scanout dmabuf (`Argb8888`/`Xrgb8888`),
        // not the shadow rendered into — report the format callers can actually scan out.
        self.present
            .as_ref()
            .map_or_else(|| self.buffer.format(), |p| p.format())
    }
}

// --- VkMapping: a downloaded pixel buffer (Smithay `TextureMapping`) ----------------------------

/// A CPU-readable copy of a framebuffer/texture region — Smithay's `TextureMapping`. Owns the
/// bytes; `ExportMem::map_texture` hands out a borrowed slice of them.
pub struct VkMapping {
    pub(super) data: Vec<u8>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) format: Fourcc,
}

impl fmt::Debug for VkMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkMapping")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("len", &self.data.len())
            .finish()
    }
}

impl Texture for VkMapping {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.format)
    }
}

impl TextureMapping for VkMapping {
    fn flipped(&self) -> bool {
        false
    }
}
