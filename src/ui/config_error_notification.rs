use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use niri_config::Config;
use niri_vk::text::{SpanFamily, TextSpan};
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
/// Notification font size (body 11pt), logical px-per-em.
const FONT_PX: f64 = crate::ui::pt_to_px(11.);
/// A generous non-wrapping layout width (logical px); the notification is content-sized, so this
/// only needs to exceed the natural line width (a very long config path wraps rather than
/// producing an ultra-wide banner).
const WRAP_WIDTH: i32 = 1200;
const BORDER: i32 = 4;
/// Notification box background (opaque dark grey), straight RGBA.
const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
/// Border for the parse-error variant (red) and the created-config variant (green).
const BORDER_ERROR: [f32; 4] = [1., 0.3, 0.3, 1.];
const BORDER_CREATED: [f32; 4] = [0.5, 1., 0.5, 1.];
/// Keycap background behind the inline command/path (#000000, matching the old pango `bgcolor`).
const KEYCAP_BG: [f32; 4] = [0., 0., 0., 1.];
/// Text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Index of the keycap span in [`draw_dialog_texture`]'s span list.
const KEYCAP_SPAN: u32 = 1;

pub struct ConfigErrorNotification {
    state: State,
    /// Cached notification box textures per output scale. Tied to a renderer context (dropped when
    /// it changes) and cleared whenever the content (error vs. created-path) changes.
    cache: RefCell<DialogCache>,

    // If set, this is a "Created config at {path}" notification. If unset, this is a config error
    // notification.
    created_path: Option<PathBuf>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
}

struct DialogCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, VkTexture>,
}

impl DialogCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
        }
    }
}

enum State {
    Hidden,
    Showing(Animation),
    Shown(Duration),
    Hiding(Animation),
}

impl ConfigErrorNotification {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            cache: RefCell::new(DialogCache::new()),
            created_path: None,
            clock,
            config,
        }
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.config_notification_open_close.0,
        )
    }

    pub fn show_created(&mut self, created_path: &Path) {
        if self.created_path.as_deref() != Some(created_path) {
            self.created_path = Some(created_path.to_owned());
            self.cache.borrow_mut().textures.clear();
        }

        self.state = State::Showing(self.animation(0., 1.));
    }

    pub fn show(&mut self) {
        let c = self.config.borrow();
        if c.config_notification.disable_failed {
            return;
        }

        if self.created_path.is_some() {
            self.created_path = None;
            self.cache.borrow_mut().textures.clear();
        }

        // Show from scratch even if already showing to bring attention.
        self.state = State::Showing(self.animation(0., 1.));
    }

    pub fn hide(&mut self) {
        if matches!(self.state, State::Hidden) {
            return;
        }

        self.state = State::Hiding(self.animation(1., 0.));
    }

    pub fn advance_animations(&mut self) {
        match &mut self.state {
            State::Hidden => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    let duration = if self.created_path.is_some() {
                        // Make this quite a bit longer because it comes with a monitor modeset
                        // (can take a while) and an important hotkeys popup diverting the
                        // attention.
                        Duration::from_secs(8)
                    } else {
                        Duration::from_secs(4)
                    };
                    self.state = State::Shown(self.clock.now_unadjusted() + duration);
                }
            }
            State::Shown(deadline) => {
                if self.clock.now_unadjusted() >= *deadline {
                    self.hide();
                }
            }
            State::Hiding(anim) => {
                if anim.is_clamped_done() {
                    self.state = State::Hidden;
                }
            }
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        !matches!(self.state, State::Hidden)
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
    ) -> Option<TextureRenderElement<VkTexture>> {
        if matches!(self.state, State::Hidden) {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let path = self.created_path.as_deref();
        let scale_key = NotNan::new(scale).ok()?;

        let texture = {
            let mut cache = self.cache.borrow_mut();

            // The cached textures belong to one renderer context; drop them all if it changed.
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.textures.clear();
                cache.context = Some(context);
            }

            // Not the `entry` API: the build is fallible and borrows `renderer`.
            #[allow(clippy::map_entry)]
            if !cache.textures.contains_key(&scale_key) {
                match draw_dialog_texture(renderer, scale, path) {
                    Ok(texture) => {
                        cache.textures.insert(scale_key, texture);
                    }
                    Err(err) => {
                        warn!("error rendering the config error notification: {err:#}");
                    }
                }
            }
            cache.textures.get(&scale_key).cloned()?
        };

        let tex_size = texture.size();
        let size = Size::<f64, Logical>::from((
            f64::from(tex_size.w) / scale,
            f64::from(tex_size.h) / scale,
        ));
        let y_range = size.h + f64::from(PADDING) * 2.;

        let x = (output_size.w - size.w).max(0.) / 2.;
        let y = match &self.state {
            State::Hidden => unreachable!(),
            State::Showing(anim) | State::Hiding(anim) => -size.h + anim.value() * y_range,
            State::Shown(_) => f64::from(PADDING) * 2.,
        };

        let location = Point::from((x, y));
        let location = location.to_physical_precise_round(scale).to_logical(scale);

        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            1.,
            None,
            None,
            Kind::Unspecified,
        );
        Some(elem)
    }
}

/// Draw the notification box into an offscreen [`VkTexture`] on the GPU: the opaque dark box, a
/// coloured border (red for a parse error, green for a created config), a black keycap patch behind
/// the inline `niri validate` command / config path, and the message. Content-sized. No cairo.
fn draw_dialog_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    created_path: Option<&Path>,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("config_error_notification::draw_dialog_texture");

    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let padding = padding.max(0);
    let wrap_px: i32 = to_physical_precise_round(scale, WRAP_WIDTH);
    let wrap_px = wrap_px.max(1);
    let border: i32 = ((f64::from(BORDER) / 2. * scale).round() as i32).max(1);
    let px = (FONT_PX * scale) as f32;

    // Span 1 is the keycap (mono command / path), matching KEYCAP_SPAN.
    let path_text;
    let (spans, border_color): (Vec<TextSpan>, [f32; 4]) = if let Some(path) = created_path {
        path_text = format!("{path:?}");
        (
            vec![
                TextSpan {
                    text: "Created a default config file at ",
                    family: SpanFamily::Sans,
                    bold: false,
                    px,
                },
                TextSpan {
                    text: &path_text,
                    family: SpanFamily::Mono,
                    bold: false,
                    px,
                },
            ],
            BORDER_CREATED,
        )
    } else {
        (
            vec![
                TextSpan {
                    text: "Failed to parse the config file. Please run ",
                    family: SpanFamily::Sans,
                    bold: false,
                    px,
                },
                TextSpan {
                    text: "niri validate",
                    family: SpanFamily::Mono,
                    bold: false,
                    px,
                },
                TextSpan {
                    text: " to see the errors.",
                    family: SpanFamily::Sans,
                    bold: false,
                    px,
                },
            ],
            BORDER_ERROR,
        )
    };

    let run = renderer.build_glyph_paragraph(&spans, wrap_px as f32, px)?;

    // Content-sized box: place the whole ink block at (padding, padding).
    let (ix, iy, iw, ih) = run.ink_bounds();
    let box_w = (iw + padding * 2).max(1);
    let box_h = (ih + padding * 2).max(1);
    let origin = Point::<i32, Physical>::from((padding - ix, padding - iy));

    let size = Size::<i32, Physical>::from((box_w, box_h));
    let full = Rectangle::from_size(size);
    let inner = Rectangle::new(
        Point::from((border, border)),
        Size::from(((box_w - border * 2).max(0), (box_h - border * 2).max(0))),
    );

    // The keycap background: the command/path span's ink, padded and clamped inside the box.
    let (kx, ky, kw, kh) = run.span_ink_bounds(KEYCAP_SPAN);
    let keycap = (kw > 0 && kh > 0).then(|| {
        let pad_x = (px * 0.25).round() as i32;
        let pad_y = (px * 0.15).round() as i32;
        Rectangle::new(
            Point::from((origin.x + kx - pad_x, origin.y + ky - pad_y)),
            Size::from((kw + pad_x * 2, kh + pad_y * 2)),
        )
        .intersection(inner)
    });

    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((box_w, box_h)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        frame.clear(Color32F::from(border_color), &[full])?;
        frame.clear(Color32F::from(BOX_BG), &[inner])?;
        if let Some(Some(keycap)) = keycap {
            frame.clear(Color32F::from(KEYCAP_BG), &[keycap])?;
        }
        frame.render_glyphs(&run, origin, TEXT, full, &[full])?;
        let _sync = frame.finish()?;
    }

    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}

/// The parse-error message as plain text (for accessibility).
pub fn error_text() -> String {
    "Failed to parse the config file. Please run niri validate to see the errors.".to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use smithay::backend::renderer::ExportMem;

    use super::*;

    /// Both variants draw: the parse-error box has a red border, the created-config box a green
    /// one; both are opaque-dark with a black keycap patch and bright glyph ink. Skips with no GPU.
    #[test]
    fn draws_both_variants() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_both_variants: no Vulkan device ({e})");
                return;
            }
        };

        let created = Path::new("/home/user/.config/niri/config.kdl");
        for (path, border_is_red) in [(None, true), (Some(created), false)] {
            let mut tex = draw_dialog_texture(&mut vk, 1., path).expect("notification texture");
            let size = tex.size();
            assert!(size.w > 0 && size.h > 0);

            let fb = vk.bind(&mut tex).expect("bind for readback");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
            let px_at = |x: i32, y: i32| -> [u8; 4] {
                let i = ((y * size.w + x) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };

            // The top edge is the coloured border, opaque.
            let border = px_at(size.w / 2, 1);
            assert_eq!(border[3], 255, "border must be opaque, got {border:?}");
            if border_is_red {
                assert!(
                    border[0] > 180 && border[1] < 120 && border[2] < 120,
                    "error border not red: {border:?}"
                );
            } else {
                assert!(
                    border[1] > 180 && border[0] < 160 && border[2] < 160,
                    "created border not green: {border:?}"
                );
            }

            // A black keycap pixel exists (~0x00, darker than the 0x1A box).
            let keycap = pixels
                .chunks_exact(4)
                .any(|p| p[0] < 12 && p[1] < 12 && p[2] < 12 && p[3] == 255);
            assert!(keycap, "expected a black keycap patch");

            // Bright glyph ink.
            let bright = pixels
                .chunks_exact(4)
                .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
                .count();
            assert!(bright > 40, "expected visible glyph ink, got {bright}");
        }
    }
}
