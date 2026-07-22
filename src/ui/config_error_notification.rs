use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use niri_config::Config;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::widget::{self, ContentCache, Painter, ParagraphSpan, ShapedParagraph, TextShaper};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
/// Notification font size (body), GNOME points. `FONT_PX` is its logical px, used
/// only for keycap-padding geometry; shaping goes through [`ParagraphSpan`] at pt.
const FONT_PT: f64 = 11.;
const FONT_PX: f64 = crate::ui::pt_to_px(FONT_PT);
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
/// Index of the keycap span in [`prepare_dialog`]'s span list.
const KEYCAP_SPAN: u32 = 1;

pub struct ConfigErrorNotification {
    state: State,
    /// Content-sized box texture cache (keyed by scale + `revision`).
    cache: RefCell<ContentCache>,
    /// Bumped whenever the content (error vs. created-path, or the path itself)
    /// changes, invalidating the cached bake.
    revision: u64,

    // If set, this is a "Created config at {path}" notification. If unset, this is a config error
    // notification.
    created_path: Option<PathBuf>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
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
            cache: RefCell::new(ContentCache::new()),
            revision: 0,
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
            self.revision += 1;
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
            self.revision += 1;
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

        let texture = {
            let mut cache = self.cache.borrow_mut();
            match widget::bake_content(
                renderer,
                &mut cache,
                scale,
                self.revision,
                |renderer| prepare_dialog(renderer, scale, path),
                |frame, phys, layout| paint_dialog(frame, phys, layout, scale),
            ) {
                Ok(texture) => texture,
                Err(err) => {
                    warn!("error rendering the config error notification: {err:#}");
                    return None;
                }
            }
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

/// The computed physical layout of the notification box, produced by
/// [`prepare_dialog`] and drawn by [`paint_dialog`] (the two [`widget::bake_content`]
/// phases). Content-sized, so everything is in physical px.
struct DialogLayout {
    run: ShapedParagraph,
    /// Where the paragraph ink block is placed (top-left of the ink at `(pad, pad)`).
    origin: Point<i32, Physical>,
    /// The dark box interior (inside the coloured border).
    inner: Rectangle<i32, Physical>,
    /// The black keycap patch behind the mono command/path span, if present.
    keycap: Option<Rectangle<i32, Physical>>,
    /// Border tint: red for a parse error, green for a created config.
    border_color: [f32; 4],
}

/// Shape the message and compute the content-sized box layout — the prepare phase
/// for [`widget::bake_content`]. Span 1 (mono command / path) is [`KEYCAP_SPAN`].
fn prepare_dialog(
    renderer: &mut VulkanRenderer,
    scale: f64,
    created_path: Option<&Path>,
) -> anyhow::Result<(Size<i32, Physical>, DialogLayout)> {
    let _span = tracy_client::span!("config_error_notification::prepare_dialog");

    let padding = to_physical_precise_round::<i32>(scale, f64::from(PADDING)).max(0);
    let border = ((f64::from(BORDER) / 2. * scale).round() as i32).max(1);

    let mut shaper = TextShaper::new(renderer, scale);
    let path_text;
    let (spans, border_color): (Vec<ParagraphSpan>, [f32; 4]) = if let Some(path) = created_path {
        path_text = format!("{path:?}");
        (
            vec![
                ParagraphSpan::new("Created a default config file at ", FONT_PT),
                ParagraphSpan::new(&path_text, FONT_PT).mono(),
            ],
            BORDER_CREATED,
        )
    } else {
        (
            vec![
                ParagraphSpan::new("Failed to parse the config file. Please run ", FONT_PT),
                ParagraphSpan::new("niri validate", FONT_PT).mono(),
                ParagraphSpan::new(" to see the errors.", FONT_PT),
            ],
            BORDER_ERROR,
        )
    };

    let run = shaper.paragraph(&spans, f64::from(WRAP_WIDTH), FONT_PT)?;

    // Content-sized box: place the whole ink block at (padding, padding).
    let (ix, iy, iw, ih) = run.ink_bounds();
    let box_w = (iw + padding * 2).max(1);
    let box_h = (ih + padding * 2).max(1);
    let origin = Point::<i32, Physical>::from((padding - ix, padding - iy));

    let size = Size::<i32, Physical>::from((box_w, box_h));
    let inner = Rectangle::new(
        Point::from((border, border)),
        Size::from(((box_w - border * 2).max(0), (box_h - border * 2).max(0))),
    );

    // The keycap background: the command/path span's ink, padded and clamped inside the box.
    let (kx, ky, kw, kh) = run.span_ink_bounds(KEYCAP_SPAN);
    let keycap = (kw > 0 && kh > 0)
        .then(|| {
            let pad_x = to_physical_precise_round::<i32>(scale, FONT_PX * 0.25);
            let pad_y = to_physical_precise_round::<i32>(scale, FONT_PX * 0.15);
            Rectangle::new(
                Point::from((origin.x + kx - pad_x, origin.y + ky - pad_y)),
                Size::from((kw + pad_x * 2, kh + pad_y * 2)),
            )
            .intersection(inner)
        })
        .flatten();

    Ok((
        size,
        DialogLayout {
            run,
            origin,
            inner,
            keycap,
            border_color,
        },
    ))
}

/// Draw the coloured border, the dark box, the keycap patch, and the message — the
/// paint phase for [`widget::bake_content`].
fn paint_dialog(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    layout: &DialogLayout,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    p.clear(layout.border_color)?;
    p.fill_rect_px(layout.inner, BOX_BG)?;
    if let Some(keycap) = layout.keycap {
        p.fill_rect_px(keycap, KEYCAP_BG)?;
    }
    p.paragraph(&layout.run, layout.origin, TEXT)?;
    Ok(())
}

/// The parse-error message as plain text (for accessibility).
pub fn error_text() -> String {
    "Failed to parse the config file. Please run niri validate to see the errors.".to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem};
    use smithay::utils::Buffer as BufferCoord;

    use super::*;
    use crate::ui::widget::ContentCache;

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
            let mut cache = ContentCache::new();
            let mut tex = widget::bake_content(
                &mut vk,
                &mut cache,
                1.,
                0,
                |r| prepare_dialog(r, 1., path),
                |frame, phys, layout| paint_dialog(frame, phys, layout, 1.),
            )
            .expect("notification texture");
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
