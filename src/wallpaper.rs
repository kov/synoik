// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The GNOME desktop wallpaper.
//!
//! Holds the decoded `org.gnome.desktop.background` picture (resolved in
//! [`crate::gnome::BackgroundSettings`]) and hands out render elements for it:
//! the decode runs **on a worker thread** when the setting changes (a 4K
//! picture takes ~100 ms end to end, far too long for the main loop — it would
//! stall the whole session for the duration, e.g. when the Dark Style toggle
//! flips to the dark wallpaper variant), and the GPU upload happens lazily on
//! first render. gnome-shell fits the picture with `picture-options`; we
//! implement `zoom` (cover + center crop, the default) and draw every other mode
//! the same way for now.
//!
//! The pixels come from a **sandboxed glycin loader process** — see [`decode`] for what that
//! costs us and buys us.
//!
//! The worker writes its pixels **straight into device-visible memory** when a device is
//! available ([`synoik_vk::staging::HostStaging`]). Moving the decode off the main loop had left
//! one multi-megabyte host write behind: the upload's own copy from the decoded `Vec` into a
//! staging buffer, measured at 7–9 ms for a 4K picture — first-touch page faults on a freshly
//! mapped buffer, no GPU work in it at all (`docs/fork/foundation.md` §5). Staging on the worker
//! leaves the render thread with only the image creation and a copy queued for the next frame's
//! command buffer — no submit of its own, which on a live frame was `first upload 18.62ms` for 48
//! MiB. Without a device (headless tests, or before the renderer exists) it falls back to a plain
//! `Vec` and the ordinary import, which stages into the renderer's pool and queues its copy just
//! the same.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use calloop::channel::Sender;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer as _, Texture as _};
use smithay::utils::{Buffer, Logical, Point, Rectangle, Scale, Size, Transform};
use synoik_vk::gpu::Gpu;
use synoik_vk::staging::HostStaging;

use crate::gnome::{BackgroundOptions, BackgroundSettings};
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{GaussianBackdrop, VkTexture, VulkanRenderer};

/// Larger pictures are downscaled to this bound before upload; it comfortably
/// fits common GL max-texture-size limits.
const MAX_TEXTURE_EDGE: u32 = 8192;

#[derive(Default)]
pub struct Wallpaper {
    /// The path of the currently displayed picture.
    picture: Option<PathBuf>,
    /// The path currently being decoded on the worker, if any. While a decode is
    /// in flight the old `image` keeps showing, so the screen never blanks or
    /// freezes mid-change.
    ///
    /// A `RefCell` because [`render_vulkan`](Self::render_vulkan) — which only has `&self` — is
    /// where a device change is noticed, and staged pixels cannot outlive their device, so it has
    /// to be able to re-request the decode.
    pending: RefCell<Option<PathBuf>>,
    image: Option<Image>,
    /// Lazily uploaded from `image`; the outer `Option` is "not tried yet", the inner one records
    /// a failed upload so we don't retry every frame.
    vk_texture: RefCell<Option<Option<TextureBuffer<VkTexture>>>>,
    /// Identifies the renderer `vk_texture` was uploaded to. A mismatch drops it: the
    /// image belongs to a device that is gone, and handing it out would sample freed
    /// memory. Every other texture cache in the tree carries this guard; this one was
    /// the exception, reachable only through a renderer recreation (device loss).
    context: RefCell<Option<ContextId<VkTexture>>>,
    /// Request sink to the decode worker. `None` before [`spawn_worker`] wires it
    /// (e.g. in headless tests), in which case decoding falls back to synchronous.
    ///
    /// [`spawn_worker`]: Wallpaper::spawn_worker
    decode_tx: Option<std::sync::mpsc::Sender<DecodeRequest>>,
    /// The blurred copy [`render_blurred`](Self::render_blurred) hands out, and the identity of
    /// the texture it was built over.
    ///
    /// Keyed by image handle rather than rebuilt per frame because the chain binds its source's
    /// view at construction: a cache kept across a wallpaper change would sample a dangling
    /// descriptor, and one rebuilt every frame would be the per-frame `VkTexture` churn that takes
    /// Venus down.
    blur: RefCell<Option<(u64, GaussianBackdrop)>>,
    /// Bumped on every upload; see [`ensure_texture`](Self::ensure_texture).
    texture_generation: std::cell::Cell<u64>,
}

/// What the main loop asks the worker for: a picture, and the device to stage it on if there is
/// one. The device rides with the *request* rather than being wired in at
/// [`spawn_worker`](Wallpaper::spawn_worker), because the worker outlives any one renderer — a
/// device that has been replaced must not be reused for the next decode.
struct DecodeRequest {
    path: PathBuf,
    gpu: Option<Arc<Gpu>>,
}

struct Image {
    /// RGBA8, tightly packed, either on the heap or already in device-visible memory.
    pixels: Pixels,
    size: Size<i32, Buffer>,
    opaque: bool,
    /// The CPU-side copy the legibility rule reads; see [`Thumb`].
    thumb: Thumb,
    /// Memoized [`Wallpaper::legible_blur_brightness`]. Lives on the `Image` rather than on the
    /// `Wallpaper` so it cannot outlive the picture it describes — a wallpaper change replaces the
    /// whole `Image`, and with it this.
    legible: RefCell<Option<(LegibilityQuery, f64)>>,
}

/// A small aspect-preserving RGB copy of the picture, kept so the legibility rule can be answered
/// on the CPU without a readback.
///
/// It is built on the **decode worker**, which already holds the full picture off the main loop;
/// 256px on the long edge is enough because everything the rule looks at has been blurred by tens
/// of pixels first. Measured 2026-08-17 against the renderer's own output on three radii: the
/// predicted p99 luminance tracked the GPU within ±3.3%.
struct Thumb {
    w: u32,
    h: u32,
    /// RGB8, tightly packed.
    rgb: Vec<u8>,
}

/// The long edge of a [`Thumb`].
const THUMB_EDGE: u32 = 256;

impl Thumb {
    fn from_rgba(data: &[u8], w: u32, h: u32) -> Self {
        let full = (w > 0 && h > 0)
            .then(|| image::RgbaImage::from_raw(w, h, data.to_vec()))
            .flatten();
        let Some(full) = full else {
            return Self {
                w: 0,
                h: 0,
                rgb: Vec::new(),
            };
        };
        let small = image::DynamicImage::ImageRgba8(full)
            .resize(
                THUMB_EDGE,
                THUMB_EDGE,
                image::imageops::FilterType::Triangle,
            )
            .to_rgb8();
        Self {
            w: small.width(),
            h: small.height(),
            rgb: small.into_raw(),
        }
    }
}

/// What a memoized legibility answer was computed for. All of it can change without the picture
/// changing (an output resize moves the crop *and* the band), so all of it is in the key.
#[derive(Clone, Copy, PartialEq)]
struct LegibilityQuery {
    view: Size<f64, Logical>,
    scale: f64,
    radius: f64,
    band: Rectangle<f64, Logical>,
    ceiling: f64,
    target: f64,
}

/// Where a decoded picture's bytes live.
enum Pixels {
    /// On the heap. The upload copies them into a staging buffer it makes itself — the write this
    /// module exists to keep off the render thread, kept for when there is no device to stage on.
    Host(Vec<u8>),
    /// Already in mapped device-visible memory, written by the decode worker. The upload only has
    /// to queue a copy — and the `Arc` is what keeps the bytes alive until a frame records it,
    /// since the wallpaper can change the moment after.
    Staged(Arc<HostStaging>),
}

impl Pixels {
    /// Only the decode test looks at this — the uploads check the length themselves, against the
    /// extent they are about to create, which is the check that matters.
    #[cfg_attr(not(test), allow(dead_code))]
    fn len(&self) -> usize {
        match self {
            Pixels::Host(data) => data.len(),
            Pixels::Staged(staging) => staging.len(),
        }
    }
}

/// A finished decode, delivered from the worker thread back to the main loop.
/// Opaque to the caller (which just routes it to [`Wallpaper::apply_decoded`]);
/// `Send` because [`Image`] is plain data or a `Send` staging buffer.
pub struct WallpaperDecoded {
    path: PathBuf,
    image: Option<Image>,
}

impl Wallpaper {
    /// Start the background decode worker and give it the sink that delivers
    /// finished decodes back to the main loop (register `result_tx`'s receiver as
    /// a calloop source that calls [`apply_decoded`](Self::apply_decoded)). Until
    /// this is called, [`update`](Self::update) decodes synchronously.
    pub fn spawn_worker(&mut self, result_tx: Sender<WallpaperDecoded>) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DecodeRequest>();
        self.decode_tx = Some(req_tx);
        if let Err(err) = std::thread::Builder::new()
            .name("wallpaper-decode".to_owned())
            .spawn(move || {
                // Ends when the request sender (held by `Wallpaper`) is dropped.
                for DecodeRequest { path, gpu } in req_rx {
                    let image = decode(&path, gpu.as_ref());
                    if result_tx.send(WallpaperDecoded { path, image }).is_err() {
                        break;
                    }
                }
            })
        {
            warn!("could not spawn the wallpaper decode thread: {err}; decoding synchronously");
            self.decode_tx = None;
        }
    }

    /// The picture we're heading toward: the in-flight decode if any, else the one
    /// on screen. `update` compares against this so a repeated setting is a no-op.
    fn target(&self) -> Option<PathBuf> {
        self.pending
            .borrow()
            .clone()
            .or_else(|| self.picture.clone())
    }

    /// Syncs with the current settings, decoding the picture if it changed. The
    /// decode runs on the worker thread ([`spawn_worker`](Self::spawn_worker)); the
    /// previous wallpaper stays up until the new one is ready. Falls back to a
    /// synchronous decode when no worker is wired.
    pub fn update(&mut self, settings: &BackgroundSettings, gpu: Option<&Arc<Gpu>>) {
        if settings.picture != self.target() {
            match (&settings.picture, &self.decode_tx) {
                // Async: keep the current image, ask the worker to decode the new one.
                (Some(path), Some(tx)) => {
                    *self.pending.borrow_mut() = Some(path.clone());
                    let request = DecodeRequest {
                        path: path.clone(),
                        gpu: gpu.cloned(),
                    };
                    if tx.send(request).is_err() {
                        // Worker gone; fall back to a synchronous decode.
                        *self.pending.borrow_mut() = None;
                        self.picture = Some(path.clone());
                        self.image = decode(path, gpu);
                        self.vk_texture.replace(None);
                    }
                }
                // No worker (tests): decode inline.
                (Some(path), None) => {
                    *self.pending.borrow_mut() = None;
                    self.picture = Some(path.clone());
                    self.image = decode(path, gpu);
                    self.vk_texture.replace(None);
                }
                // Cleared.
                (None, _) => {
                    *self.pending.borrow_mut() = None;
                    self.picture = None;
                    self.image = None;
                    self.vk_texture.replace(None);
                }
            }
        }

        if self.target().is_some()
            && !matches!(
                settings.options,
                BackgroundOptions::Zoom | BackgroundOptions::None
            )
        {
            debug!(
                "background picture-options {:?} not implemented; drawing as zoom",
                settings.options
            );
        }
    }

    /// Adopt a finished decode from the worker. Ignores a stale result (a later
    /// change superseded it). Returns whether the displayed wallpaper changed, so
    /// the caller can queue a redraw.
    pub fn apply_decoded(&mut self, decoded: WallpaperDecoded) -> bool {
        if self.pending.borrow().as_deref() != Some(decoded.path.as_path()) {
            return false;
        }
        self.picture = self.pending.borrow_mut().take();
        self.image = decoded.image;
        self.vk_texture.replace(None);
        true
    }

    /// The brightness a blurred draw of this wallpaper needs so white text over `band` clears
    /// `target` WCAG contrast — never brighter than `ceiling`, which is the constant the caller
    /// would otherwise have used.
    ///
    /// **Brightness is the lever, not radius.** A wider blur removes *variance*; it barely moves
    /// the level, and on a wallpaper of one flat colour it does not move it at all. Measured
    /// 2026-08-17 over the lock screen's clock band: `lcd-rainbow-l` reads 2.93:1 at radius 30, 50
    /// and 90 alike — below even WCAG AA — while `lcd-rainbow-d` reads 11.2/12.0/14.0 across the
    /// same three. So the rule dims, and [`crate::ui::lock_screen::BLUR_RADIUS`] stays a constant
    /// chosen for how it looks.
    ///
    /// The answer is a pure function of the picture, the view and the band, so it is memoized and
    /// never re-derived per frame; it also never watches its own output, which is what keeps it
    /// from oscillating the way a rule fed by the rendered result would.
    ///
    /// `radius` is in output physical px, as [`render_blurred`](Self::render_blurred) takes it.
    pub fn legible_blur_brightness(
        &self,
        view_size: Size<f64, Logical>,
        scale: Scale<f64>,
        radius: f64,
        band: Rectangle<f64, Logical>,
        ceiling: f64,
        target: f64,
    ) -> f64 {
        let Some(image) = self.image.as_ref() else {
            return ceiling;
        };
        image.legible_blur_brightness(LegibilityQuery {
            view: view_size,
            scale: scale.x,
            radius,
            band,
            ceiling,
            target,
        })
    }

    /// Returns the wallpaper covering a `view_size` workspace, corners rounded by `corner_radius`
    /// (logical units; 0 disables). `None` — letting the caller draw the solid backstop — when
    /// there is no usable picture or the upload fails.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        origin: Point<f64, Logical>,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        self.render_at_alpha(renderer, origin, view_size, corner_radius, scale, 1.)
    }

    /// The wallpaper drawn translucent — for a workspace that is fading in or out and so
    /// is not fully there yet (the overview row's phantom slot).
    pub fn render_at_alpha(
        &self,
        renderer: &mut VulkanRenderer,
        origin: Point<f64, Logical>,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        self.render_vulkan(renderer, origin, view_size, corner_radius, scale, alpha)
    }

    /// The uploaded picture, uploading it if this is the first ask since it changed.
    ///
    /// Bumps [`texture_generation`](Self::texture_generation) on every *new* upload, which is what
    /// the blur cache keys on: the blur chain binds its source's image view at construction, so
    /// "same wallpaper, re-uploaded" has to invalidate it just as much as a different picture does.
    /// The path is not a usable key for that — a device loss re-uploads the same file.
    fn ensure_texture(&self, renderer: &mut VulkanRenderer) -> Option<TextureBuffer<VkTexture>> {
        let image = self.image.as_ref()?;
        let mut texture = self.vk_texture.borrow_mut();
        if texture.is_none() {
            *texture = Some(upload_vulkan(renderer, image));
            self.texture_generation
                .set(self.texture_generation.get().wrapping_add(1));
        }
        texture.as_ref()?.clone()
    }

    /// As [`render`](Self::render), but through GNOME's gaussian — the lock screen's blurred
    /// backdrop (`unlockDialog.js:706-713`).
    ///
    /// `radius` and `brightness` are `BLUR_RADIUS` and `BLUR_BRIGHTNESS` (`:34-35`), with `radius`
    /// given in **output physical pixels**, which is what GNOME's stage-space radius means.
    ///
    /// The conversion matters and is easy to skip: the wallpaper is stored at the picture's own
    /// resolution and scaled to the screen when drawn, so blurring it with a radius meant for the
    /// screen makes a 4K picture on a 1080p output come out half as blurred as GNOME's. The radius
    /// is scaled by the same factor the draw will magnify by.
    ///
    /// Returns `None` if there is no wallpaper *or* if the blur could not be built — the caller
    /// must fall back to the unblurred picture and its own dimming, since a lock screen with no
    /// backdrop at all would show the desktop.
    pub fn render_blurred(
        &self,
        renderer: &mut VulkanRenderer,
        origin: Point<f64, Logical>,
        view_size: Size<f64, Logical>,
        scale: Scale<f64>,
        radius: f64,
        brightness: f32,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        // The upload (and its device-loss handling) is `render_vulkan`'s; go through it so a
        // blurred draw cannot diverge from a plain one about which texture is current.
        self.render_vulkan(renderer, origin, view_size, 0., scale, 1.)?;
        let buffer = self.ensure_texture(renderer)?;
        let texture = buffer.texture().clone();

        // How much the draw will magnify the sampled region by, in each axis. `zoom_crop` keeps the
        // aspect ratio, so the two agree; take the width.
        let src = zoom_crop(buffer.logical_size(), view_size);
        if src.size.w <= 0. {
            return None;
        }
        let magnification = view_size.w * scale.x / src.size.w;
        let texture_radius = radius / magnification.max(f64::EPSILON);

        let key = self.texture_generation.get();
        let mut cache = self.blur.borrow_mut();
        if cache.as_ref().is_none_or(|(cached, _)| *cached != key) {
            match GaussianBackdrop::new(renderer, &texture, texture_radius) {
                Ok(backdrop) => *cache = Some((key, backdrop)),
                Err(err) => {
                    warn!("error building the wallpaper blur: {err}");
                    return None;
                }
            }
        }
        let (_, backdrop) = cache.as_mut()?;
        if !backdrop.is_current(texture_radius, brightness) {
            backdrop.queue(renderer, &texture, texture_radius, brightness);
        }

        // Same geometry as the unblurred element, sampling the blurred copy instead — including
        // the opacity, which this used to drop on the floor.
        //
        // A blur of an opaque picture is opaque: the chain samples only that picture and the
        // brightness multiply does not touch alpha. Declaring nothing meant the overview's
        // full-screen blurred backdrop occluded nothing, so the full-output `SolidColor` backdrop
        // underneath it (`Synoik::render_output`'s `push(backdrop)`) drew in full behind a layer
        // hiding every pixel of it, on every overview frame.
        //
        // Keyed on the *source* being opaque rather than on the blur output's format, so a
        // wallpaper with real transparency stays honest.
        let output = backdrop.output().clone();
        let opaque_regions = if self.image.as_ref().is_some_and(|image| image.opaque) {
            vec![Rectangle::from_size(output.size())]
        } else {
            Vec::new()
        };
        let blurred =
            TextureBuffer::from_texture(renderer, output, 1., Transform::Normal, opaque_regions);
        let elem = TextureRenderElement::from_texture_buffer(
            blurred,
            origin,
            1.,
            Some(src),
            Some(view_size),
            Kind::Unspecified,
        );
        Some(RoundedTextureRenderElement::new(elem, 0., scale))
    }

    /// The Vulkan sibling of [`render`](Self::render): uploads the decoded picture to a `VkTexture`
    /// (cached across frames like the GLES texture) and returns an element the owned Vulkan
    /// renderer rounds in its own pipeline.
    pub fn render_vulkan(
        &self,
        renderer: &mut VulkanRenderer,
        origin: Point<f64, Logical>,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        let image = self.image.as_ref()?;

        // A recreated renderer invalidates the upload before anything else looks at it.
        let context = renderer.context_id();
        if self.context.borrow().as_ref() != Some(&context) {
            self.vk_texture.replace(None);
            *self.context.borrow_mut() = Some(context);
        }

        // Heap pixels survive a device change and simply re-upload; staged ones do not — they live
        // in memory belonging to the device that is gone. Re-request the decode rather than draw
        // nothing forever, and guard against asking once a frame while it runs.
        if let Pixels::Staged(staging) = &image.pixels {
            if !staging.belongs_to(renderer.gpu()) {
                let path = self.picture.clone();
                if path.is_some() && *self.pending.borrow() != path {
                    warn!("wallpaper was staged on a device that is gone; decoding it again");
                    if let (Some(tx), Some(path)) = (self.decode_tx.as_ref(), path) {
                        *self.pending.borrow_mut() = Some(path.clone());
                        let request = DecodeRequest {
                            path,
                            gpu: Some(renderer.gpu().clone()),
                        };
                        if tx.send(request).is_err() {
                            *self.pending.borrow_mut() = None;
                        }
                    }
                }
                return None;
            }
        }

        let buffer = self.ensure_texture(renderer)?;

        // Texture scale is 1, so buffer logical size == pixel size.
        let src = zoom_crop(buffer.logical_size(), view_size);
        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            origin,
            alpha,
            Some(src),
            Some(view_size),
            Kind::Unspecified,
        );
        Some(RoundedTextureRenderElement::new(elem, corner_radius, scale))
    }
}

fn upload_vulkan(renderer: &mut VulkanRenderer, image: &Image) -> Option<TextureBuffer<VkTexture>> {
    let opaque_regions = if image.opaque {
        vec![Rectangle::from_size(image.size)]
    } else {
        Vec::new()
    };
    match &image.pixels {
        Pixels::Host(data) => TextureBuffer::from_memory(
            renderer,
            data,
            Fourcc::Abgr8888,
            image.size,
            false,
            1.,
            Transform::Normal,
            opaque_regions,
        )
        .map_err(|err| warn!("error uploading wallpaper texture to Vulkan: {err}"))
        .ok(),
        Pixels::Staged(staging) => {
            match renderer.import_host_staging(staging, Fourcc::Abgr8888, image.size) {
                Ok(texture) => Some(TextureBuffer::from_texture(
                    renderer,
                    texture,
                    1.,
                    Transform::Normal,
                    opaque_regions,
                )),
                Err(err) => {
                    warn!("error uploading staged wallpaper texture to Vulkan: {err}");
                    None
                }
            }
        }
    }
}

/// One decoded picture as the renderer wants it: tightly packed straight-alpha RGBA8 in sRGB.
struct Decoded {
    data: Vec<u8>,
    size: Size<i32, Buffer>,
    opaque: bool,
}

/// Why [`load`] had nothing to hand back.
enum LoadError {
    /// The loader process refused the file, or could not be started at all — a missing
    /// `glycin-loaders` package and an unreadable path both land here.
    Loader(Box<glycin::ErrorCtx>),
    /// A frame with no pixels in it. Nothing downstream copes with a zero-sized picture, and the
    /// row loop in `load` would chunk by its width.
    Empty,
}

impl From<glycin::ErrorCtx> for LoadError {
    fn from(err: glycin::ErrorCtx) -> Self {
        Self::Loader(Box::new(err))
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loader(err) => write!(f, "{err:?}"),
            Self::Empty => write!(f, "the loader returned an empty frame"),
        }
    }
}

/// Run `path` through a sandboxed glycin loader and normalise the frame into [`Decoded`].
///
/// The scale request is only sent for a picture that would otherwise need the CPU downscale in
/// [`decode`]: it is advisory, and a loader that can honour it (SVG rasterises at the asked-for
/// size; JPEG XL ignores it) saves both the decode and the resample. Asking unconditionally would
/// mean asking a small picture to be *enlarged*.
async fn load(path: &Path) -> Result<Decoded, LoadError> {
    let mut loader = glycin::Loader::new(gio::File::for_path(path));
    // Both, so the format we get back is the answer to "does this picture have alpha"; see the
    // note on `decode`. Neither is premultiplied, which is what the texture upload expects.
    loader.accepted_memory_formats(
        glycin::MemoryFormatSelection::R8g8b8a8 | glycin::MemoryFormatSelection::R8g8b8,
    );
    let image = loader.load().await?;

    let details = image.details();
    let frame = if details.width() > MAX_TEXTURE_EDGE || details.height() > MAX_TEXTURE_EDGE {
        let request = glycin::FrameRequest::new().scale(MAX_TEXTURE_EDGE, MAX_TEXTURE_EDGE);
        image.specific_frame(request).await?
    } else {
        image.next_frame().await?
    };

    let opaque = !frame.memory_format().has_alpha();
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    if w == 0 || h == 0 {
        return Err(LoadError::Empty);
    }
    let stride = frame.stride() as usize;
    let src = frame.buf_slice();

    // The buffer is a mapping we do not own and its rows may be padded, so a copy is owed either
    // way; widening the opaque case to RGBA here costs nothing on top of it.
    let mut data = vec![0u8; w * h * 4];
    for (row, dst_row) in data.chunks_exact_mut(w * 4).enumerate() {
        if opaque {
            let (dst_pixels, _) = dst_row.as_chunks_mut::<4>();
            let src_row = &src[row * stride..row * stride + w * 3];
            for (src_px, dst_px) in src_row.as_chunks::<3>().0.iter().zip(dst_pixels) {
                dst_px[..3].copy_from_slice(src_px);
                dst_px[3] = 255;
            }
        } else {
            dst_row.copy_from_slice(&src[row * stride..row * stride + w * 4]);
        }
    }

    if let glycin::ColorState::Cicp(cicp) = frame.color_state() {
        to_srgb(cicp, &mut data);
    }

    Ok(Decoded {
        data,
        size: Size::new(w as i32, h as i32),
        opaque,
    })
}

/// Convert `data` (straight-alpha RGBA8 in `cicp`'s colour space) to sRGB in place.
///
/// A picture we cannot build a transform for is left as it decoded: wrong primaries are a tint,
/// a missing wallpaper is a black screen.
fn to_srgb(cicp: &glycin::Cicp, data: &mut [u8]) {
    let _span = tracy_client::span!("wallpaper::to_srgb");

    // Both sides speak H.273 code points, so the bytes are the conversion.
    let [primaries, transfer, matrix, range] = cicp.to_bytes();
    let (Ok(color_primaries), Ok(transfer_characteristics), Ok(matrix_coefficients)) = (
        moxcms::CicpColorPrimaries::try_from(primaries),
        moxcms::TransferCharacteristics::try_from(transfer),
        moxcms::MatrixCoefficients::try_from(matrix),
    ) else {
        warn!("background picture has colour points we cannot convert ({cicp:?}); leaving it as decoded");
        return;
    };

    let source = moxcms::ColorProfile::new_from_cicp(moxcms::CicpProfile {
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range: range == 1,
    });
    // In place: the picture is tens of megabytes, and a second copy of it is the one allocation
    // on this path big enough to be worth avoiding.
    let converted = source
        .create_in_place_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::TransformOptions::default(),
        )
        .and_then(|transform| transform.transform(data));
    if let Err(err) = converted {
        warn!("could not convert the background picture to sRGB ({err}); leaving it as decoded");
    }
}

/// Decode `path` into RGBA8. With a `gpu`, the pixels are written straight into device-visible
/// memory so the render thread never has to copy them; without one (no renderer yet, headless
/// tests) they land on the heap and the upload copies them as before. A staging allocation that
/// fails falls back to the heap rather than failing the decode — running with the old cost beats
/// no wallpaper.
///
/// The decode itself happens in a **sandboxed loader process**, through glycin — the same
/// mechanism mutter 50.3 loads backgrounds with (`src/compositor/meta-background-image.c`). That
/// is what makes the format list GNOME's rather than ours: JPEG XL, HEIF and SVG come from the
/// installed `glycin-loaders` package, not from a crate we picked. `bwrap` and those loaders are
/// hard runtime dependencies; without them there is no wallpaper.
///
/// Two consequences worth knowing before touching this:
///
/// 1. **The frame is not sRGB.** GNOME's stock backgrounds are Display P3, and glycin reports that
///    as [`glycin::ColorState::Cicp`] rather than converting — mutter carries the CICP into a
///    `ClutterColorState` and converts while compositing. We composite sRGB only, so the conversion
///    is [`to_srgb`], here on the worker.
/// 2. **Ask for both RGB and RGBA.** The `alpha_channel` detail is `None` for JPEG XL and SVG, so
///    the accepted-format set is what tells us whether the picture is opaque: a loader hands back
///    `R8g8b8` exactly when there is no alpha to carry.
fn decode(path: &Path, gpu: Option<&Arc<Gpu>>) -> Option<Image> {
    let _span = tracy_client::span!("wallpaper::decode");

    let decoded = match async_io::block_on(load(path)) {
        Ok(decoded) => decoded,
        Err(err) => {
            warn!("error decoding background picture {path:?}: {err}");
            return None;
        }
    };

    let Decoded {
        mut data,
        mut size,
        opaque,
    } = decoded;

    if size.w > MAX_TEXTURE_EDGE as i32 || size.h > MAX_TEXTURE_EDGE as i32 {
        // Only reachable for a loader that ignored the scale we asked for in `load`.
        let full = image::RgbaImage::from_raw(size.w as u32, size.h as u32, data)?;
        let scaled = image::DynamicImage::ImageRgba8(full)
            .resize(
                MAX_TEXTURE_EDGE,
                MAX_TEXTURE_EDGE,
                image::imageops::FilterType::Triangle,
            )
            .into_rgba8();
        size = Size::new(scaled.width() as i32, scaled.height() as i32);
        data = scaled.into_raw();
    }

    // On the worker, next to the decode that already owns these bytes: the main loop never sees
    // this downscale, and after it the legibility rule never touches the full picture again.
    let thumb = Thumb::from_rgba(&data, size.w as u32, size.h as u32);

    // This copy is the whole point of the staging path: it is tens of megabytes into a mapping
    // that has never been touched, and here it runs on the worker instead of between two frames.
    let pixels = match gpu.map(|gpu| HostStaging::new(gpu, data.len())) {
        Some(Ok(mut staging)) => {
            staging.as_mut_slice().copy_from_slice(&data);
            Pixels::Staged(Arc::new(staging))
        }
        Some(Err(err)) => {
            warn!("could not stage the wallpaper on the GPU ({err:#}); uploading from the heap");
            Pixels::Host(data)
        }
        None => Pixels::Host(data),
    };
    Some(Image {
        pixels,
        size,
        opaque,
        thumb,
        legible: RefCell::new(None),
    })
}

impl Image {
    /// The blur brightness that keeps white text over `band` at `target` contrast, capped at
    /// `ceiling`. See [`Wallpaper::legible_blur_brightness`].
    fn legible_blur_brightness(&self, q: LegibilityQuery) -> f64 {
        if let Some((key, answer)) = *self.legible.borrow() {
            if key == q {
                return answer;
            }
        }
        let answer = self.compute_legible_blur_brightness(q);
        *self.legible.borrow_mut() = Some((q, answer));
        answer
    }

    fn compute_legible_blur_brightness(&self, q: LegibilityQuery) -> f64 {
        if self.thumb.w == 0 || self.thumb.h == 0 || self.size.w <= 0 || self.size.h <= 0 {
            return q.ceiling;
        }
        let picture = Size::new(self.size.w as f64, self.size.h as f64);
        let src = zoom_crop(picture, q.view);
        if src.size.w <= 0. || src.size.h <= 0. {
            return q.ceiling;
        }

        // Everything below happens in *thumbnail* pixels. Two conversions get us there: the
        // picture-to-thumb ratio, and the magnification the draw applies to the sampled crop.
        let to_thumb = self.thumb.w as f64 / picture.w;
        let crop_w_thumb = src.size.w * to_thumb;
        let view_physical_w = q.view.w * q.scale;
        if crop_w_thumb <= 0. || view_physical_w <= 0. {
            return q.ceiling;
        }
        // `radius` arrives in output physical px, and `radius = 2σ` (`GaussianBackdrop`).
        let sigma = (q.radius / 2.) * (crop_w_thumb / view_physical_w);

        let Some(full) =
            image::RgbImage::from_raw(self.thumb.w, self.thumb.h, self.thumb.rgb.clone())
        else {
            return q.ceiling;
        };
        // Blurring the whole thumbnail rather than the band keeps the band's own edges from
        // clamping against a wall of themselves, which reads as extra contrast that isn't there.
        let blurred = if sigma > 0.01 {
            image::imageops::blur(&full, sigma as f32)
        } else {
            full
        };

        // The band, logical view coords -> fraction of the view -> thumbnail crop.
        let px = |v: f64, span: f64, crop_loc: f64, crop_span: f64, limit: u32| -> f64 {
            (crop_loc + (v / span) * crop_span).clamp(0., limit as f64)
        };
        let x0 = px(
            q.band.loc.x,
            q.view.w,
            src.loc.x * to_thumb,
            crop_w_thumb,
            self.thumb.w,
        );
        let x1 = px(
            q.band.loc.x + q.band.size.w,
            q.view.w,
            src.loc.x * to_thumb,
            crop_w_thumb,
            self.thumb.w,
        );
        let crop_h_thumb = src.size.h * to_thumb;
        let y0 = px(
            q.band.loc.y,
            q.view.h,
            src.loc.y * to_thumb,
            crop_h_thumb,
            self.thumb.h,
        );
        let y1 = px(
            q.band.loc.y + q.band.size.h,
            q.view.h,
            src.loc.y * to_thumb,
            crop_h_thumb,
            self.thumb.h,
        );
        let (x0, x1) = (x0.floor() as u32, (x1.ceil() as u32).min(self.thumb.w));
        let (y0, y1) = (y0.floor() as u32, (y1.ceil() as u32).min(self.thumb.h));
        if x1 <= x0 || y1 <= y0 {
            return q.ceiling;
        }

        let mut band: Vec<[f64; 3]> = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = blurred.get_pixel(x, y).0;
                band.push([p[0] as f64, p[1] as f64, p[2] as f64]);
            }
        }

        // `brightness` multiplies the *encoded* sample in the shader, so it goes in before the
        // sRGB transfer function, not after. Getting that backwards put an early version of this
        // predictor 130% off.
        let contrast_at = |b: f64| -> f64 {
            let mut lums: Vec<f64> = band
                .iter()
                .map(|c| {
                    let l = |v: f64| {
                        let v = (v * b) / 255.;
                        if v <= 0.04045 {
                            v / 12.92
                        } else {
                            ((v + 0.055) / 1.055).powf(2.4)
                        }
                    };
                    0.2126 * l(c[0]) + 0.7152 * l(c[1]) + 0.0722 * l(c[2])
                })
                .collect();
            // p99, not the mean: one bright frond behind a stroke is what makes a glyph vanish,
            // and a mean over a band this wide would never see it.
            let idx = (((lums.len() - 1) as f64) * 0.99).round() as usize;
            lums.select_nth_unstable_by(idx, f64::total_cmp);
            1.05 / (lums[idx] + 0.05)
        };

        if contrast_at(q.ceiling) >= q.target {
            return q.ceiling;
        }
        // Monotone in `b`, so a bisection is exact to the tolerance. ~20 rounds over a band of a
        // few thousand thumbnail pixels, once per wallpaper — not a per-frame cost.
        let (mut lo, mut hi) = (0., q.ceiling);
        for _ in 0..20 {
            let mid = (lo + hi) / 2.;
            if contrast_at(mid) >= q.target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// GNOME's `zoom` fit (gnome-desktop's `gnome_bg` scaling): scale the picture
/// to cover the view keeping its aspect ratio, cropping the overflow equally
/// on both sides. Returns the source rectangle to sample, in picture pixels.
fn zoom_crop(picture: Size<f64, Logical>, view: Size<f64, Logical>) -> Rectangle<f64, Logical> {
    let scale = f64::max(view.w / picture.w, view.h / picture.h);
    let src_size = Size::new(view.w / scale, view.h / scale);
    let loc = Point::new((picture.w - src_size.w) / 2., (picture.h - src_size.h) / 2.);
    Rectangle::new(loc, src_size)
}

/// A `Wallpaper` holding one flat colour, sized like a stock background.
///
/// A picture is what makes a thumbnail's background *visible*: without one a workspace falls back
/// to a solid fill, and a draw that goes missing lands on a colour close enough to hide it.
#[cfg(test)]
pub(crate) fn flat(rgb: [u8; 3]) -> Wallpaper {
    const N: i32 = 512;
    let mut data = Vec::with_capacity((N * N * 4) as usize);
    for _ in 0..N * N {
        data.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0xff]);
    }
    Wallpaper {
        image: Some(Image {
            thumb: Thumb::from_rgba(&data, N as u32, N as u32),
            pixels: Pixels::Host(data),
            size: Size::from((N, N)),
            opaque: true,
            legible: RefCell::new(None),
        }),
        ..Default::default()
    }
}

/// A `Wallpaper` holding a checkerboard, so a draw that samples the wrong part of the picture —
/// or does not cover what it was given — shows up as pattern, not as one flat colour.
#[cfg(test)]
pub(crate) fn checker(w: i32, h: i32, cell: i32) -> Wallpaper {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let dark = ((x / cell) + (y / cell)) % 2 == 0;
            let ramp = ((x * 255) / w) as u8;
            if dark {
                data.extend_from_slice(&[0xff, ramp, 0x00, 0xff]);
            } else {
                data.extend_from_slice(&[ramp, 0xff, 0x00, 0xff]);
            }
        }
    }
    Wallpaper {
        image: Some(Image {
            thumb: Thumb::from_rgba(&data, w as u32, h as u32),
            pixels: Pixels::Host(data),
            size: Size::from((w, h)),
            opaque: true,
            legible: RefCell::new(None),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3072x1728 view at scale 1.25 — kov's seat — and the band the curtain's text lands in.
    fn seat_query(wp: &Wallpaper, ceiling: f64, target: f64) -> f64 {
        let view = Size::<f64, Logical>::from((3072., 1728.));
        wp.legible_blur_brightness(
            view,
            Scale::from(1.25),
            crate::ui::lock_screen::BLUR_RADIUS * 1.25,
            crate::ui::lock_screen::text_band(Rectangle::from_size(view)),
            ceiling,
            target,
        )
    }

    /// The rule only engages on a picture GNOME's constant cannot carry, and it engages *hard* on
    /// the one that motivated it: a near-white wallpaper leaves white 72pt text at 2.93:1 —
    /// below WCAG AA — at every radius we might pick, because the radius is not the lever.
    #[test]
    fn a_bright_wallpaper_dims_the_lock_blur_and_a_dark_one_does_not() {
        use crate::ui::lock_screen::{BLUR_BRIGHTNESS, BLUR_CONTRAST_TARGET};

        let dark = seat_query(
            &flat([0x10, 0x10, 0x10]),
            BLUR_BRIGHTNESS,
            BLUR_CONTRAST_TARGET,
        );
        assert_eq!(
            dark, BLUR_BRIGHTNESS,
            "a dark wallpaper must keep GNOME's constant untouched"
        );

        let white = seat_query(
            &flat([0xff, 0xff, 0xff]),
            BLUR_BRIGHTNESS,
            BLUR_CONTRAST_TARGET,
        );
        assert!(
            (0.44..0.48).contains(&white),
            "pure white should land near the 0.46 worst case, got {white}"
        );

        // Monotone in how bright the picture is: nothing about the rule should be able to make a
        // brighter wallpaper come out brighter.
        let mid = seat_query(
            &flat([0xa0, 0xa0, 0xa0]),
            BLUR_BRIGHTNESS,
            BLUR_CONTRAST_TARGET,
        );
        assert!(
            white <= mid && mid <= dark,
            "brightness must fall as the wallpaper rises: {white} / {mid} / {dark}"
        );
    }

    /// The answer it lands on is the answer it was asked for — a fixed point, which is what makes
    /// the rule safe to run open-loop.
    #[test]
    fn the_chosen_brightness_meets_the_target_it_was_given() {
        use crate::ui::lock_screen::BLUR_CONTRAST_TARGET;

        let wp = flat([0xff, 0xff, 0xff]);
        let b = seat_query(&wp, 0.65, BLUR_CONTRAST_TARGET);
        // Re-asking with the answer as the ceiling must not move it further.
        assert_eq!(b, seat_query(&wp, b, BLUR_CONTRAST_TARGET));
    }

    /// No picture, no opinion: the caller's constant survives.
    #[test]
    fn an_empty_wallpaper_keeps_the_callers_brightness() {
        let wp = Wallpaper::default();
        assert_eq!(seat_query(&wp, 0.65, 4.5), 0.65);
    }

    /// A blur of an opaque picture is opaque, and has to say so: the overview draws the blurred
    /// wallpaper full-screen over the full-output `SolidColor` backdrop, and a layer that declares
    /// no opaque region occludes nothing, so that backdrop is drawn in full behind something
    /// hiding every pixel of it. Live, the blurred layer read `opaque=0.00x` against a
    /// `SolidColor 1.00x` beneath it.
    ///
    /// Keyed on the source picture, so a wallpaper with real transparency keeps declaring nothing.
    #[test]
    fn a_blur_of_an_opaque_wallpaper_is_itself_opaque() {
        use smithay::backend::renderer::element::Element as _;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping a_blur_of_an_opaque_wallpaper_is_itself_opaque: no Vulkan device ({e})");
                return;
            }
        };

        const N: i32 = 64;
        let view = Size::<f64, Logical>::from((N as f64, N as f64));
        let scale = Scale::from(1.);

        for opaque in [true, false] {
            let wp = Wallpaper {
                image: Some(Image {
                    pixels: Pixels::Host(vec![0xffu8; (N * N * 4) as usize]),
                    size: Size::from((N, N)),
                    opaque,
                    thumb: Thumb::from_rgba(
                        &vec![0xffu8; (N * N * 4) as usize],
                        N as u32,
                        N as u32,
                    ),
                    legible: RefCell::new(None),
                }),
                ..Default::default()
            };

            let Some(elem) = wp.render_blurred(&mut vk, Default::default(), view, scale, 4., 1.)
            else {
                panic!("the blurred wallpaper must build (opaque = {opaque})");
            };

            let geo = elem.geometry(scale);
            let regions = elem.opaque_regions(scale).to_vec();
            if opaque {
                assert_eq!(
                    regions,
                    vec![geo],
                    "a blur of an opaque picture covers its whole geometry {geo:?}",
                );
            } else {
                assert!(
                    regions.is_empty(),
                    "a blur of a picture with transparency must claim nothing, got {regions:?}",
                );
            }
        }
    }

    /// A picture change decodes off-thread: the current wallpaper keeps showing
    /// until the new decode lands (no freeze/blank), and a stale result is ignored.
    #[test]
    fn async_decode_keeps_the_old_wallpaper_until_the_new_one_lands() {
        let a = PathBuf::from("/a.png");
        let b = PathBuf::from("/b.png");
        let bg = |p: &PathBuf| BackgroundSettings {
            picture: Some(p.clone()),
            ..Default::default()
        };

        let mut wp = Wallpaper::default();
        // Inject a request sink without a real worker thread (we drive the results
        // by hand); the requests just buffer in `_req_rx`.
        let (req_tx, _req_rx) = std::sync::mpsc::channel();
        wp.decode_tx = Some(req_tx);

        // Requesting A leaves the display empty (nothing decoded yet), A pending.
        wp.update(&bg(&a), None);
        assert_eq!(wp.pending.borrow().as_ref(), Some(&a));
        assert!(wp.picture.is_none());

        // A lands → it's shown, nothing pending.
        assert!(wp.apply_decoded(WallpaperDecoded {
            path: a.clone(),
            image: None,
        }));
        assert_eq!(wp.picture.as_ref(), Some(&a));
        assert!(wp.pending.borrow().is_none());

        // Switching to B keeps A on screen while B decodes.
        wp.update(&bg(&b), None);
        assert_eq!(wp.pending.borrow().as_ref(), Some(&b));
        assert_eq!(
            wp.picture.as_ref(),
            Some(&a),
            "A must stay up while B decodes — no mid-change freeze/blank"
        );

        // A late/stale result for A is ignored (B is what we're waiting for).
        assert!(!wp.apply_decoded(WallpaperDecoded {
            path: a.clone(),
            image: None,
        }));
        assert_eq!(wp.pending.borrow().as_ref(), Some(&b));
        assert_eq!(wp.picture.as_ref(), Some(&a));

        // B lands → B is shown.
        assert!(wp.apply_decoded(WallpaperDecoded {
            path: b.clone(),
            image: None,
        }));
        assert_eq!(wp.picture.as_ref(), Some(&b));
        assert!(wp.pending.borrow().is_none());

        // Re-applying the same setting is a no-op (no re-request, no flicker).
        wp.update(&bg(&b), None);
        assert!(wp.pending.borrow().is_none());
    }

    #[test]
    fn zoom_crops_a_wide_picture_on_the_sides() {
        let src = zoom_crop(Size::new(3840., 1080.), Size::new(1920., 1080.));
        assert_eq!(
            src,
            Rectangle::new(Point::new(960., 0.), Size::new(1920., 1080.))
        );
    }

    #[test]
    fn zoom_crops_a_tall_picture_top_and_bottom() {
        let src = zoom_crop(Size::new(1920., 2160.), Size::new(1920., 1080.));
        assert_eq!(
            src,
            Rectangle::new(Point::new(0., 540.), Size::new(1920., 1080.))
        );
    }

    #[test]
    fn zoom_takes_the_whole_picture_at_matching_aspect() {
        let src = zoom_crop(Size::new(3840., 2160.), Size::new(1920., 1080.));
        assert_eq!(
            src,
            Rectangle::new(Point::new(0., 0.), Size::new(3840., 2160.))
        );
    }

    /// Whether a glycin loader can actually run here: `bwrap` has to be installed *and* able to
    /// create a user namespace, which it cannot inside an unprivileged container. Probing the
    /// capability rather than the binary is the difference between a test that skips on a CI
    /// runner and one that fails there.
    fn sandbox_available() -> bool {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            std::process::Command::new("bwrap")
                .args(["--ro-bind", "/", "/", "--dev", "/dev", "true"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    /// A stock background to decode, or `None` with the reason printed: these tests are the only
    /// coverage the loader path has, so a skip has to say why it skipped.
    fn stock_background(name: &str) -> Option<PathBuf> {
        let path = PathBuf::from(format!("/usr/share/backgrounds/gnome/{name}"));
        if !path.exists() {
            eprintln!("skipping: {name} is not installed (gnome-backgrounds)");
            return None;
        }
        if !sandbox_available() {
            eprintln!("skipping: bwrap cannot sandbox a glycin loader here");
            return None;
        }
        Some(path)
    }

    /// The picture that motivated the legibility rule, end to end through the real decode:
    /// `lcd-rainbow-l` is a near-white background, and GNOME's 0.65 leaves the curtain's white text
    /// at 2.93:1 over it — under WCAG AA — while its dark sibling is comfortably clear at the same
    /// constant. Skips itself when the backgrounds are not installed.
    #[test]
    fn the_stock_light_rainbow_is_the_wallpaper_that_needs_dimming() {
        use crate::ui::lock_screen::{BLUR_BRIGHTNESS, BLUR_CONTRAST_TARGET};

        let load = |name: &str| {
            stock_background(name).map(|path| Wallpaper {
                image: decode(&path, None),
                ..Default::default()
            })
        };
        let (Some(light), Some(dark)) = (load("lcd-rainbow-l.jxl"), load("lcd-rainbow-d.jxl"))
        else {
            return;
        };

        assert!(
            seat_query(&light, BLUR_BRIGHTNESS, BLUR_CONTRAST_TARGET) < 0.55,
            "the light rainbow must be dimmed well past GNOME's constant"
        );
        assert_eq!(
            seat_query(&dark, BLUR_BRIGHTNESS, BLUR_CONTRAST_TARGET),
            BLUR_BRIGHTNESS,
            "the dark rainbow must be left alone"
        );
    }

    /// GNOME's stock backgrounds are JPEG XL, a format we have no decoder for: it comes from the
    /// installed glycin loaders, through a sandboxed process. Skips itself when the backgrounds
    /// are not installed.
    #[test]
    fn decodes_a_stock_jxl_background() {
        let Some(path) = stock_background("adwaita-l.jxl") else {
            return;
        };
        let image = decode(&path, None).unwrap();
        assert!(image.size.w > 0 && image.size.h > 0);
        assert_eq!(
            image.pixels.len(),
            (image.size.w * image.size.h * 4) as usize
        );
        assert!(image.opaque, "a stock background has no alpha to carry");
    }

    /// SVG is a stock background format too (`blobs-l.svg`), and one nothing in our own dependency
    /// tree decodes into a picture — the loader rasterises it, at the size we ask for.
    #[test]
    fn decodes_a_stock_svg_background() {
        let Some(path) = stock_background("blobs-l.svg") else {
            return;
        };
        let image = decode(&path, None).unwrap();
        assert!(image.size.w > 0 && image.size.h > 0);
        assert_eq!(
            image.pixels.len(),
            (image.size.w * image.size.h * 4) as usize
        );
    }

    /// The stock backgrounds are **Display P3**, and the loader hands the pixels over in that
    /// space rather than converting (mutter carries the CICP into a `ClutterColorState` instead).
    /// We composite sRGB only, so `decode` must convert: `adwaita-l`'s top-left blue is
    /// (0, 46, 133) as it decodes and (0, 47, 138) in sRGB. Drawing the P3 numbers unconverted is
    /// a picture that is quietly too saturated, which no other assertion here would notice.
    #[test]
    fn a_display_p3_background_is_converted_to_srgb() {
        let Some(path) = stock_background("adwaita-l.jxl") else {
            return;
        };
        let image = decode(&path, None).unwrap();
        let Pixels::Host(data) = &image.pixels else {
            unreachable!("no gpu was passed, so the pixels are on the heap");
        };

        let expected = [0, 47, 138];
        for (channel, (got, want)) in data.iter().zip(expected.iter()).enumerate() {
            assert!(
                got.abs_diff(*want) <= 1,
                "channel {channel} of the converted top-left pixel is {got}, expected ~{want} \
                 (the unconverted Display P3 value is {:?})",
                [0, 46, 133],
            );
        }
        assert_eq!(data[3], 255);
    }

    /// The upload is cached across frames, but it belongs to the renderer that made
    /// it. A recreated renderer must re-upload — handing the old texture out would
    /// sample an image destroyed with its device. Counted by submits, because the
    /// failure mode (a stale handle) is invisible until it is drawn.
    #[test]
    fn a_recreated_renderer_re_uploads_the_wallpaper() {
        let (mut vk_a, mut vk_b) = match (VulkanRenderer::new(), VulkanRenderer::new()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                eprintln!(
                    "skipping a_recreated_renderer_re_uploads_the_wallpaper: no Vulkan device"
                );
                return;
            }
        };

        let wp = Wallpaper {
            image: Some(Image {
                pixels: Pixels::Host(vec![0xffu8; 8 * 8 * 4]),
                size: Size::from((8, 8)),
                opaque: true,
                thumb: Thumb::from_rgba(&[0xffu8; 8 * 8 * 4], 8, 8),
                legible: RefCell::new(None),
            }),
            ..Default::default()
        };

        // Counted as *resources created*, not as upload submits: an import no longer submits at
        // all — its copy rides the next frame's command buffer
        // (`VulkanRenderer::pending_texture_uploads`), so the submit count is zero either way and
        // could no longer tell a cold upload from a cached one. The image is still allocated once
        // per real upload, which is the thing this test is about.
        let uploads = |_: ()| synoik_vk::stats::take_creates().0;
        let render = |wp: &Wallpaper, vk: &mut VulkanRenderer| {
            wp.render(
                vk,
                Default::default(),
                Size::from((16., 16.)),
                0.,
                Scale::from(1.),
            )
        };

        // The shared staging chunk is created once per renderer and counts as a resource; warm
        // both so the counts below are the wallpaper's own.
        vk_a.warm_staging_pool();
        vk_b.warm_staging_pool();
        let _ = uploads(());
        assert!(render(&wp, &mut vk_a).is_some());
        assert_eq!(uploads(()), 1, "the first frame must upload");
        assert!(render(&wp, &mut vk_a).is_some());
        assert_eq!(uploads(()), 0, "the same renderer must reuse the upload");

        assert!(render(&wp, &mut vk_b).is_some());
        assert_eq!(
            uploads(()),
            1,
            "a different renderer got a texture built by the old one — that image was \
             destroyed with its device"
        );
        assert!(render(&wp, &mut vk_b).is_some());
        assert_eq!(uploads(()), 0, "the new renderer must then cache too");
    }

    /// Staged pixels live in memory belonging to one device, so unlike heap pixels they cannot
    /// survive a renderer recreation — and there is no host copy left to re-upload from. Rather
    /// than leave the desktop on its solid backstop for the rest of the session, the render that
    /// notices has to ask for the picture again. It must also ask **once**, not once per frame,
    /// or a device change turns into a decode storm on a worker that is already the slow part.
    #[test]
    fn a_wallpaper_staged_on_a_dead_device_is_decoded_again_exactly_once() {
        let (mut vk_a, mut vk_b) = match (VulkanRenderer::new(), VulkanRenderer::new()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                eprintln!("skipping a_wallpaper_staged_on_a_dead_device...: no Vulkan device");
                return;
            }
        };

        let path = PathBuf::from("/wall.png");
        let mut staging = HostStaging::new(vk_a.gpu(), 8 * 8 * 4).expect("staging");
        staging.as_mut_slice().fill(0xff);

        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let wp = Wallpaper {
            picture: Some(path.clone()),
            image: Some(Image {
                pixels: Pixels::Staged(Arc::new(staging)),
                size: Size::from((8, 8)),
                opaque: true,
                thumb: Thumb::from_rgba(&[0xffu8; 8 * 8 * 4], 8, 8),
                legible: RefCell::new(None),
            }),
            decode_tx: Some(req_tx),
            ..Default::default()
        };
        let render = |wp: &Wallpaper, vk: &mut VulkanRenderer| {
            wp.render(
                vk,
                Default::default(),
                Size::from((16., 16.)),
                0.,
                Scale::from(1.),
            )
        };

        assert!(
            render(&wp, &mut vk_a).is_some(),
            "the device that staged the pixels could not draw them"
        );
        assert!(req_rx.try_recv().is_err(), "nothing should be re-requested");

        assert!(
            render(&wp, &mut vk_b).is_none(),
            "a renderer drew a wallpaper staged on a device it does not own"
        );
        assert_eq!(
            req_rx.try_recv().map(|r| r.path).ok(),
            Some(path.clone()),
            "the wallpaper was dropped without being re-requested — it never comes back"
        );
        assert_eq!(wp.pending.borrow().as_ref(), Some(&path));

        // The decode is in flight now; further frames must not pile more requests onto it.
        assert!(render(&wp, &mut vk_b).is_none());
        assert!(
            req_rx.try_recv().is_err(),
            "every frame re-requested the decode while one was already running"
        );
    }

    #[test]
    fn zoom_upscales_a_small_picture_rather_than_tiling() {
        // A 800x600 picture on a 1920x1080 view: width is the tighter fit,
        // so the crop trims height.
        let src = zoom_crop(Size::new(800., 600.), Size::new(1920., 1080.));
        assert_eq!(src.size.w, 800.);
        assert_eq!(src.size.h, 450.);
        assert_eq!(src.loc, Point::new(0., 75.));
    }
}
