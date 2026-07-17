use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use niri_config::Config;
use niri_vk::text::{SpanFamily, TextSpan};
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

const KEY_NAME: &str = "Enter";
const PADDING: i32 = 16;
/// Dialog font size, logical px-per-em.
const FONT_PX: f64 = 14.;
/// A generous non-wrapping layout width (logical px); the dialog is sized to its content, so this
/// only needs to exceed the widest line.
const WRAP_WIDTH: i32 = 1000;
const BORDER: i32 = 8;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// Dialog box background (opaque dark grey), straight RGBA.
const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
/// The red alert border.
const BORDER_COLOR: [f32; 4] = [1., 0.3, 0.3, 1.];
/// The keycap background behind " Enter " (#2C2C2C, matching the old pango `bgcolor`).
const KEYCAP_BG: [f32; 4] = [0.172, 0.172, 0.172, 1.];
/// Dialog text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Index of the keycap span in [`draw_dialog_texture`]'s span list.
const KEYCAP_SPAN: u32 = 1;

pub struct ExitConfirmDialog {
    state: State,
    /// Cached dialog box textures per output scale (the text is static). Tied to a renderer
    /// context: dropped wholesale when the renderer changes.
    cache: RefCell<DialogCache>,

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

niri_render_elements! {
    ExitConfirmDialogRenderElement => {
        Texture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
        SolidColor = SolidColorRenderElement,
    }
}

struct OutputData {
    backdrop: SolidColorBuffer,
}

enum State {
    Hidden,
    Showing(Animation),
    Visible,
    Hiding(Animation),
}

impl ExitConfirmDialog {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            cache: RefCell::new(DialogCache::new()),
            clock,
            config,
        }
    }

    pub fn can_show(&self) -> bool {
        // The dialog is drawn lazily on the GPU at render time (no renderer here to test), and that
        // path fails visible — it always draws the backdrop and the box degrades to a dark overlay
        // that most keys still dismiss. So the dialog is always showable; unlike the old cairo
        // path, there is no up-front render that can fail and force an immediate quit.
        true
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.exit_confirmation_open_close.0,
        )
    }

    fn value(&self) -> f64 {
        match &self.state {
            State::Hidden => 0.,
            State::Showing(anim) | State::Hiding(anim) => anim.value(),
            State::Visible => 1.,
        }
    }

    /// Returns true if the dialog will be shown (even if it is already shown).
    pub fn show(&mut self) -> bool {
        if !self.can_show() {
            return false;
        }

        if self.is_open() {
            return true;
        }

        self.state = State::Showing(self.animation(self.value(), 1.));
        true
    }

    /// Returns true if started the hide animation.
    pub fn hide(&mut self) -> bool {
        if !self.is_open() {
            return false;
        }

        self.state = State::Hiding(self.animation(self.value(), 0.));
        true
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    pub fn advance_animations(&mut self) {
        match &mut self.state {
            State::Hidden => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    self.state = State::Visible;
                }
            }
            State::Visible => (),
            State::Hiding(anim) => {
                if anim.is_clamped_done() {
                    self.state = State::Hidden;
                }
            }
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        push: &mut dyn FnMut(ExitConfirmDialogRenderElement),
    ) {
        let (value, clamped_value) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible => (1., 1.),
        };
        let _span = tracy_client::span!("ExitConfirmDialog::render");

        // Can be out of range when starting from past 0. or 1. from a spring bounce.
        let clamped_value = clamped_value.clamp(0., 1.);

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return;
        };

        let texture = {
            let mut cache = self.cache.borrow_mut();

            // The cached textures belong to one renderer context; drop them all if it changed.
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.textures.clear();
                cache.context = Some(context);
            }

            // Not the `entry` API: the build is fallible and borrows `renderer`, which the
            // closure-based `or_insert_with` can't express.
            #[allow(clippy::map_entry)]
            if !cache.textures.contains_key(&scale_key) {
                match draw_dialog_texture(renderer, scale) {
                    Ok(texture) => {
                        cache.textures.insert(scale_key, texture);
                    }
                    Err(err) => {
                        // Fail visible: fall through to always draw the backdrop below (this dialog
                        // is a modal grab; never leave the seat with no overlay at all).
                        warn!("error rendering the exit confirm dialog: {err:#}");
                    }
                }
            }
            cache.textures.get(&scale_key).cloned()
        };

        if let Some(texture) = texture {
            let tex_size = texture.size();
            // Composited with a fade + rescale, so no opaque-region hint (the box is not opaque as
            // drawn once the open animation scales alpha below 1).
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );

            let size = Size::<f64, Logical>::from((
                f64::from(tex_size.w) / scale,
                f64::from(tex_size.h) / scale,
            ));
            let location = (output_size.to_point() - size.to_point()).downscale(2.);
            let mut location = location.to_physical_precise_round(scale).to_logical(scale);
            location.x = f64::max(0., location.x);
            location.y = f64::max(0., location.y);

            let elem = TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                clamped_value as f32,
                None,
                None,
                Kind::Unspecified,
            );
            let elem = RescaleRenderElement::from_element(
                elem,
                (location + size.downscale(2.)).to_physical_precise_round(scale),
                value.max(0.) * 0.2 + 0.8,
            );
            push(ExitConfirmDialogRenderElement::Texture(elem));
        }

        // Backdrop.
        let data = output.user_data().get_or_insert(|| {
            Mutex::new(OutputData {
                backdrop: SolidColorBuffer::new(output_size, BACKDROP_COLOR),
            })
        });
        let mut data = data.lock().unwrap();
        data.backdrop.resize(output_size);

        let elem = SolidColorRenderElement::from_buffer(
            &data.backdrop,
            Point::new(0., 0.),
            clamped_value as f32,
            Kind::Unspecified,
        );
        push(ExitConfirmDialogRenderElement::SolidColor(elem));
    }
}

/// Draw the dialog box into an offscreen [`VkTexture`] on the GPU: the opaque dark box, a red
/// alert border, a grey keycap background behind " Enter ", and the centered two-line message.
/// The box is content-sized (the text is static). No cairo/pango raster.
fn draw_dialog_texture(renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("exit_confirm_dialog::draw_dialog_texture");

    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let padding = padding.max(0);
    let wrap_px: i32 = to_physical_precise_round(scale, WRAP_WIDTH);
    let wrap_px = wrap_px.max(1);
    // Even border thickness to avoid blurry edges, as the old cairo stroke did (~BORDER/2 visible).
    let border: i32 = ((f64::from(BORDER) / 2. * scale).round() as i32).max(1);
    let px = (FONT_PX * scale) as f32;

    // Span 1 is the keycap (mono " Enter "), matching KEYCAP_SPAN.
    let key = format!(" {KEY_NAME} ");
    let spans = [
        TextSpan {
            text: "Are you sure you want to exit niri?\n\nPress",
            family: SpanFamily::Sans,
            bold: false,
            px,
        },
        TextSpan {
            text: &key,
            family: SpanFamily::Mono,
            bold: false,
            px,
        },
        TextSpan {
            text: " to confirm.",
            family: SpanFamily::Sans,
            bold: false,
            px,
        },
    ];
    let run = renderer.build_glyph_paragraph(&spans, wrap_px as f32, px)?;

    // The box is content-sized: place the whole ink block at (padding, padding).
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

    // The keycap background: the " Enter " span's ink, padded to a keycap, clamped inside the box.
    let (kx, ky, kw, kh) = run.span_ink_bounds(KEYCAP_SPAN);
    let keycap = (kw > 0 && kh > 0).then(|| {
        let pad_x = (px * 0.3).round() as i32;
        let pad_y = (px * 0.2).round() as i32;
        let rect = Rectangle::new(
            Point::from((origin.x + kx - pad_x, origin.y + ky - pad_y)),
            Size::from((kw + pad_x * 2, kh + pad_y * 2)),
        );
        rect.intersection(inner)
    });

    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((box_w, box_h)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        // Red border = whole box cleared red, then the inner rect cleared to the dark bg.
        frame.clear(Color32F::from(BORDER_COLOR), &[full])?;
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

/// The dialog message as plain text (for accessibility).
fn text() -> String {
    format!(
        "Are you sure you want to exit niri?\n\n\
         Press {KEY_NAME} to confirm."
    )
}

#[cfg(feature = "dbus")]
pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("Exit niri");
    node.set_description(text());
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::ExportMem;

    use super::*;

    /// Drive the GPU dialog box into an offscreen and read it back: opaque dark box, a red border
    /// on the edge, the grey keycap patch behind "Enter", and bright glyph ink. Skips with no GPU.
    #[test]
    fn draws_the_dialog_with_border_and_keycap() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skipping draws_the_dialog_with_border_and_keycap: no Vulkan device ({e})"
                );
                return;
            }
        };

        let mut tex = draw_dialog_texture(&mut vk, 1.).expect("dialog texture");
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

        // The top edge is the red alert border (R high, G/B low), opaque.
        let border = px_at(size.w / 2, 1);
        assert_eq!(border[3], 255, "border must be opaque, got {border:?}");
        assert!(
            border[0] > 180 && border[1] < 120 && border[2] < 120,
            "top edge not red: {border:?}"
        );

        // A pixel just inside a corner (past the border, away from text) is the dark box.
        let bg = px_at(size.w - 8, size.h - 8);
        assert!(
            bg[0] < 45 && bg[1] < 45 && bg[2] < 45 && bg[3] == 255,
            "inner box not dark/opaque: {bg:?}"
        );

        // Somewhere a grey keycap pixel (~0x2C) exists that is neither the 0x1A box nor white text.
        let keycap = pixels.chunks_exact(4).any(|p| {
            (38..=60).contains(&p[0]) && (38..=60).contains(&p[1]) && (38..=60).contains(&p[2])
        });
        assert!(keycap, "expected a grey keycap patch (~0x2C)");

        // Bright glyph ink.
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }
}
