//! The GNOME desktop wallpaper.
//!
//! Holds the decoded `org.gnome.desktop.background` picture (resolved in
//! [`crate::gnome::BackgroundSettings`]) and hands out render elements for it:
//! the CPU-side decode runs **on a worker thread** when the setting changes (a
//! 4K JPEG-XL decode is far too slow to run on the main loop — it stalls the
//! whole session for the duration, e.g. when the Dark Style toggle flips to the
//! dark wallpaper variant), and the GPU upload happens lazily on first render.
//! gnome-shell fits the picture with `picture-options`; we implement `zoom`
//! (cover + center crop, the default) and draw every other mode the same way for
//! now.
//!
//! The worker writes its pixels **straight into device-visible memory** when a device is
//! available ([`niri_vk::staging::HostStaging`]). Moving the decode off the main loop had left one
//! multi-megabyte host write behind: the upload's own copy from the decoded `Vec` into a staging
//! buffer, measured at 7–9 ms for a 4K picture — first-touch page faults on a freshly mapped
//! buffer, no GPU work in it at all (`docs/fork/venus-cost.md` §9.2). Staging on the worker leaves
//! the render thread with only the image creation, the copy command and the submit. Without a
//! device (headless tests, or before the renderer exists) it falls back to a plain `Vec` and the
//! old upload path.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use calloop::channel::Sender;
use image::ImageReader;
use niri_vk::gpu::Gpu;
use niri_vk::staging::HostStaging;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer as _};
use smithay::utils::{Buffer, Logical, Point, Rectangle, Scale, Size, Transform};

use crate::gnome::{BackgroundOptions, BackgroundSettings};
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};

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
}

/// Where a decoded picture's bytes live.
enum Pixels {
    /// On the heap. The upload copies them into a staging buffer it makes itself — the write this
    /// module exists to keep off the render thread, kept for when there is no device to stage on.
    Host(Vec<u8>),
    /// Already in mapped device-visible memory, written by the decode worker. The upload only has
    /// to record a copy.
    Staged(HostStaging),
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

    /// Returns the wallpaper covering a `view_size` workspace, corners rounded by `corner_radius`
    /// (logical units; 0 disables). `None` — letting the caller draw the solid backstop — when
    /// there is no usable picture or the upload fails.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        self.render_vulkan(renderer, view_size, corner_radius, scale)
    }

    /// The Vulkan sibling of [`render`](Self::render): uploads the decoded picture to a `VkTexture`
    /// (cached across frames like the GLES texture) and returns an element the owned Vulkan
    /// renderer rounds in its own pipeline.
    pub fn render_vulkan(
        &self,
        renderer: &mut VulkanRenderer,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
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

        let mut texture = self.vk_texture.borrow_mut();
        let buffer = texture
            .get_or_insert_with(|| upload_vulkan(renderer, image))
            .as_ref()?
            .clone();

        // Texture scale is 1, so buffer logical size == pixel size.
        let src = zoom_crop(buffer.logical_size(), view_size);
        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            (0., 0.),
            1.,
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

/// Decode `path` into RGBA8. With a `gpu`, the pixels are written straight into device-visible
/// memory so the render thread never has to copy them; without one (no renderer yet, headless
/// tests) they land on the heap and the upload copies them as before. A staging allocation that
/// fails falls back to the heap rather than failing the decode — running with the old cost beats
/// no wallpaper.
fn decode(path: &Path, gpu: Option<&Arc<Gpu>>) -> Option<Image> {
    let _span = tracy_client::span!("wallpaper::decode");

    // TODO(perf): a stock 4K JPEG-XL takes *seconds* here, which is why this had
    // to move off the main loop at all. Investigate before it becomes a papered-
    // over cost: (1) the gsrs binary is a *debug* build and `jxl-oxide`/`image`
    // are unoptimized — an `opt-level = 3` dev-profile override for the decode
    // crates (like the `insta`/`similar` overrides in Cargo.toml) may alone cut
    // it to well under a second; (2) we decode full-res then downscale to an 8192
    // cap and upload — decoding straight to the output size would do far less
    // work; (3) cache the decoded light/dark variants so toggling color-scheme
    // back is instant instead of re-decoding.

    let decoded = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jxl"))
    {
        // GNOME's stock backgrounds are JPEG XL, which the image crate doesn't
        // cover; jxl-oxide plugs into it as an external decoder.
        File::open(path)
            .map_err(image::ImageError::IoError)
            .and_then(|file| {
                let decoder = jxl_oxide::integration::JxlDecoder::new(BufReader::new(file))
                    .map_err(|err| {
                        image::ImageError::Decoding(image::error::DecodingError::new(
                            image::error::ImageFormatHint::Name("jxl".to_owned()),
                            err,
                        ))
                    })?;
                image::DynamicImage::from_decoder(decoder)
            })
    } else {
        ImageReader::open(path)
            .and_then(|reader| reader.with_guessed_format())
            .map_err(image::ImageError::IoError)
            .and_then(|reader| reader.decode())
    };

    let mut decoded = match decoded {
        Ok(decoded) => decoded,
        Err(err) => {
            warn!("error decoding background picture {path:?}: {err}");
            return None;
        }
    };

    if decoded.width() > MAX_TEXTURE_EDGE || decoded.height() > MAX_TEXTURE_EDGE {
        decoded = decoded.resize(
            MAX_TEXTURE_EDGE,
            MAX_TEXTURE_EDGE,
            image::imageops::FilterType::Triangle,
        );
    }

    let opaque = !decoded.color().has_alpha();
    let size = Size::new(decoded.width() as i32, decoded.height() as i32);
    let data = decoded.into_rgba8().into_raw();

    // This copy is the whole point of the staging path: it is tens of megabytes into a mapping
    // that has never been touched, and here it runs on the worker instead of between two frames.
    let pixels = match gpu.map(|gpu| HostStaging::new(gpu, data.len())) {
        Some(Ok(mut staging)) => {
            staging.as_mut_slice().copy_from_slice(&data);
            Pixels::Staged(staging)
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
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// GNOME's stock backgrounds are JPEG XL; decode one through the
    /// jxl-oxide integration end-to-end. Ignored by default: it needs the
    /// installed backgrounds and a debug-build 4K decode is slow.
    #[test]
    #[ignore = "decodes an installed 4K background; run explicitly"]
    fn decodes_a_stock_jxl_background() {
        let path = Path::new("/usr/share/backgrounds/gnome/adwaita-l.jxl");
        if !path.exists() {
            return;
        }
        let image = decode(path, None).unwrap();
        assert!(image.size.w > 0 && image.size.h > 0);
        assert_eq!(
            image.pixels.len(),
            (image.size.w * image.size.h * 4) as usize
        );
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
            }),
            ..Default::default()
        };

        // Counted as *resources created*, not as upload submits: an import no longer submits at
        // all — its copy rides the next frame's command buffer
        // (`VulkanRenderer::pending_texture_uploads`), so the submit count is zero either way and
        // could no longer tell a cold upload from a cached one. The image is still allocated once
        // per real upload, which is the thing this test is about.
        let uploads = |_: ()| niri_vk::stats::take_creates().0;
        let render = |wp: &Wallpaper, vk: &mut VulkanRenderer| {
            wp.render(vk, Size::from((16., 16.)), 0., Scale::from(1.))
        };

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
                pixels: Pixels::Staged(staging),
                size: Size::from((8, 8)),
                opaque: true,
            }),
            decode_tx: Some(req_tx),
            ..Default::default()
        };
        let render = |wp: &Wallpaper, vk: &mut VulkanRenderer| {
            wp.render(vk, Size::from((16., 16.)), 0., Scale::from(1.))
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
