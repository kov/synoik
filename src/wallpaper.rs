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

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use calloop::channel::Sender;
use image::ImageReader;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
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
    pending: Option<PathBuf>,
    image: Option<Image>,
    /// Lazily uploaded from `image`; the outer `Option` is "not tried yet", the inner one records
    /// a failed upload so we don't retry every frame.
    vk_texture: RefCell<Option<Option<TextureBuffer<VkTexture>>>>,
    /// Request sink to the decode worker. `None` before [`spawn_worker`] wires it
    /// (e.g. in headless tests), in which case decoding falls back to synchronous.
    ///
    /// [`spawn_worker`]: Wallpaper::spawn_worker
    decode_tx: Option<std::sync::mpsc::Sender<PathBuf>>,
}

struct Image {
    /// RGBA8, tightly packed.
    data: Vec<u8>,
    size: Size<i32, Buffer>,
    opaque: bool,
}

/// A finished decode, delivered from the worker thread back to the main loop.
/// Opaque to the caller (which just routes it to [`Wallpaper::apply_decoded`]);
/// `Send` because [`Image`] is plain data.
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
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PathBuf>();
        self.decode_tx = Some(req_tx);
        if let Err(err) = std::thread::Builder::new()
            .name("wallpaper-decode".to_owned())
            .spawn(move || {
                // Ends when the request sender (held by `Wallpaper`) is dropped.
                for path in req_rx {
                    let image = decode(&path);
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
    fn target(&self) -> Option<&PathBuf> {
        self.pending.as_ref().or(self.picture.as_ref())
    }

    /// Syncs with the current settings, decoding the picture if it changed. The
    /// decode runs on the worker thread ([`spawn_worker`](Self::spawn_worker)); the
    /// previous wallpaper stays up until the new one is ready. Falls back to a
    /// synchronous decode when no worker is wired.
    pub fn update(&mut self, settings: &BackgroundSettings) {
        if settings.picture.as_ref() != self.target() {
            match (&settings.picture, &self.decode_tx) {
                // Async: keep the current image, ask the worker to decode the new one.
                (Some(path), Some(tx)) => {
                    self.pending = Some(path.clone());
                    if tx.send(path.clone()).is_err() {
                        // Worker gone; fall back to a synchronous decode.
                        self.pending = None;
                        self.picture = Some(path.clone());
                        self.image = decode(path);
                        self.vk_texture.replace(None);
                    }
                }
                // No worker (tests): decode inline.
                (Some(path), None) => {
                    self.pending = None;
                    self.picture = Some(path.clone());
                    self.image = decode(path);
                    self.vk_texture.replace(None);
                }
                // Cleared.
                (None, _) => {
                    self.pending = None;
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
        if self.pending.as_deref() != Some(decoded.path.as_path()) {
            return false;
        }
        self.picture = self.pending.take();
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
    TextureBuffer::from_memory(
        renderer,
        &image.data,
        Fourcc::Abgr8888,
        image.size,
        false,
        1.,
        Transform::Normal,
        opaque_regions,
    )
    .map_err(|err| warn!("error uploading wallpaper texture to Vulkan: {err}"))
    .ok()
}

fn decode(path: &Path) -> Option<Image> {
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
    Some(Image {
        data: decoded.into_rgba8().into_raw(),
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
        wp.update(&bg(&a));
        assert_eq!(wp.pending.as_ref(), Some(&a));
        assert!(wp.picture.is_none());

        // A lands → it's shown, nothing pending.
        assert!(wp.apply_decoded(WallpaperDecoded {
            path: a.clone(),
            image: None,
        }));
        assert_eq!(wp.picture.as_ref(), Some(&a));
        assert!(wp.pending.is_none());

        // Switching to B keeps A on screen while B decodes.
        wp.update(&bg(&b));
        assert_eq!(wp.pending.as_ref(), Some(&b));
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
        assert_eq!(wp.pending.as_ref(), Some(&b));
        assert_eq!(wp.picture.as_ref(), Some(&a));

        // B lands → B is shown.
        assert!(wp.apply_decoded(WallpaperDecoded {
            path: b.clone(),
            image: None,
        }));
        assert_eq!(wp.picture.as_ref(), Some(&b));
        assert!(wp.pending.is_none());

        // Re-applying the same setting is a no-op (no re-request, no flicker).
        wp.update(&bg(&b));
        assert!(wp.pending.is_none());
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
        let image = decode(path).unwrap();
        assert!(image.size.w > 0 && image.size.h > 0);
        assert_eq!(image.data.len(), (image.size.w * image.size.h * 4) as usize);
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
