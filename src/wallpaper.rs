//! The GNOME desktop wallpaper.
//!
//! Holds the decoded `org.gnome.desktop.background` picture (resolved in
//! [`crate::gnome::BackgroundSettings`]) and hands out render elements for it:
//! the CPU-side decode happens when the setting changes, the GPU upload
//! lazily on first render. gnome-shell fits the picture with
//! `picture-options`; we implement `zoom` (cover + center crop, the default)
//! and draw every other mode the same way for now.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use image::ImageReader;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::utils::{Buffer, Logical, Point, Rectangle, Scale, Size, Transform};

use crate::gnome::{BackgroundOptions, BackgroundSettings};
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};

/// Larger pictures are downscaled to this bound before upload; it comfortably
/// fits common GL max-texture-size limits.
const MAX_TEXTURE_EDGE: u32 = 8192;

#[derive(Default)]
pub struct Wallpaper {
    picture: Option<PathBuf>,
    image: Option<Image>,
    /// Lazily uploaded from `image`; the outer `Option` is "not tried yet", the inner one records
    /// a failed upload so we don't retry every frame.
    vk_texture: RefCell<Option<Option<TextureBuffer<VkTexture>>>>,
}

struct Image {
    /// RGBA8, tightly packed.
    data: Vec<u8>,
    size: Size<i32, Buffer>,
    opaque: bool,
}

impl Wallpaper {
    /// Syncs with the current settings, decoding the picture if it changed.
    pub fn update(&mut self, settings: &BackgroundSettings) {
        if settings.picture != self.picture {
            self.picture = settings.picture.clone();
            self.image = self.picture.as_deref().and_then(decode);
            self.vk_texture.replace(None);
        }

        if self.picture.is_some()
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

    /// Returns the wallpaper covering a `view_size` workspace, corners rounded by `corner_radius`
    /// (logical units; 0 disables). `None` — letting the caller draw the solid backstop — when
    /// there is no usable picture or the upload fails.
    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        view_size: Size<f64, Logical>,
        corner_radius: f64,
        scale: Scale<f64>,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        let vk = renderer.try_as_vulkan_renderer()?;
        self.render_vulkan(vk, view_size, corner_radius, scale)
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
