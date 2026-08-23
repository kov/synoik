// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};
use synoik_config::Config;

use crate::animation::{Animation, Clock};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{self, ContentCache, Painter, ParagraphSpan, ShapedParagraph, TextShaper};
use crate::utils::{output_size, to_physical_precise_round};

const KEY_NAME: &str = "Enter";
const PADDING: i32 = 16;
/// Dialog font size, GNOME message-dialog body, GNOME points. `font_px()` is its
/// logical px, used only for keycap-padding geometry; shaping uses [`ParagraphSpan`].
const FONT_PT: f64 = 11.;
fn font_px() -> f64 {
    crate::ui::pt_to_px(FONT_PT)
}
/// A generous non-wrapping layout width (logical px); the dialog is sized to its content, so this
/// only needs to exceed the widest line.
const WRAP_WIDTH: i32 = 1000;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// Dialog card background — GNOME modal `$bg_color`; rounded, borderless.
const BOX_BG: [f32; 4] = widget::style::DIALOG_BG;
/// The keycap background behind " Enter " (#2C2C2C, matching the old pango `bgcolor`).
const KEYCAP_BG: [f32; 4] = [0.172, 0.172, 0.172, 1.];
/// Dialog text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Index of the keycap span in [`prepare_dialog`]'s span list.
const KEYCAP_SPAN: u32 = 1;

pub struct ExitConfirmDialog {
    state: State,
    /// Cached dialog box texture (content-sized; the text is static so the revision
    /// is always 0). Tied to a renderer context: dropped wholesale when it changes.
    cache: RefCell<ContentCache>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
}

synoik_render_elements! {
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
            cache: RefCell::new(ContentCache::new()),
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

        let texture = {
            let mut cache = self.cache.borrow_mut();
            // Content-sized, static text → revision 0. Fail visible: on error fall
            // through to always draw the backdrop below (this dialog is a modal
            // grab; never leave the seat with no overlay at all).
            match widget::bake_content(
                renderer,
                &mut cache,
                scale,
                0,
                |renderer| prepare_dialog(renderer, scale),
                |frame, phys, layout| paint_dialog(frame, phys, layout, scale),
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    warn!("error rendering the exit confirm dialog: {err:#}");
                    None
                }
            }
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

/// The computed physical layout of the dialog box (content-sized, static text),
/// produced by [`prepare_dialog`] and drawn by [`paint_dialog`].
struct DialogLayout {
    run: ShapedParagraph,
    origin: Point<i32, Physical>,
    keycap: Option<Rectangle<i32, Physical>>,
}

/// Shape the message and compute the content-sized box layout — the prepare phase
/// for [`widget::bake_content`]. Span 1 (mono " Enter ") is [`KEYCAP_SPAN`].
fn prepare_dialog(
    renderer: &mut VulkanRenderer,
    scale: f64,
) -> anyhow::Result<(Size<i32, Physical>, DialogLayout)> {
    let _span = tracy_client::span!("exit_confirm_dialog::prepare_dialog");

    let padding = to_physical_precise_round::<i32>(scale, f64::from(PADDING)).max(0);

    // Span 1 is the keycap (mono " Enter "), matching KEYCAP_SPAN.
    let key = format!(" {KEY_NAME} ");
    let spans = [
        ParagraphSpan::new("Are you sure you want to exit synoik?\n\nPress", FONT_PT),
        ParagraphSpan::new(&key, FONT_PT).mono(),
        ParagraphSpan::new(" to confirm.", FONT_PT),
    ];
    let mut shaper = TextShaper::new(renderer, scale);
    let run = shaper.paragraph(&spans, f64::from(WRAP_WIDTH), FONT_PT)?;

    // The box is content-sized: place the whole ink block at (padding, padding).
    let (ix, iy, iw, ih) = run.ink_bounds();
    let box_w = (iw + padding * 2).max(1);
    let box_h = (ih + padding * 2).max(1);
    let origin = Point::<i32, Physical>::from((padding - ix, padding - iy));

    let size = Size::<i32, Physical>::from((box_w, box_h));

    // The keycap background: the " Enter " span's ink, padded to a keycap, clamped inside the card.
    let (kx, ky, kw, kh) = run.span_ink_bounds(KEYCAP_SPAN);
    let keycap = (kw > 0 && kh > 0)
        .then(|| {
            let pad_x = to_physical_precise_round::<i32>(scale, font_px() * 0.3);
            let pad_y = to_physical_precise_round::<i32>(scale, font_px() * 0.2);
            Rectangle::new(
                Point::from((origin.x + kx - pad_x, origin.y + ky - pad_y)),
                Size::from((kw + pad_x * 2, kh + pad_y * 2)),
            )
            .intersection(Rectangle::from_size(size))
        })
        .flatten();

    Ok((
        size,
        DialogLayout {
            run,
            origin,
            keycap,
        },
    ))
}

/// Draw the red border, the dark box, the keycap patch, and the message — the paint
/// phase for [`widget::bake_content`].
fn paint_dialog(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    layout: &DialogLayout,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    // Rounded borderless card (GNOME modal): transparent clear, flat rounded fill,
    // then the keycap patch + message.
    p.clear(widget::style::TRANSPARENT)?;
    p.fill_rounded_full(widget::style::DIALOG_RADIUS, BOX_BG)?;
    if let Some(keycap) = layout.keycap {
        p.fill_rect_px(keycap, KEYCAP_BG)?;
    }
    p.paragraph(&layout.run, layout.origin, TEXT)?;
    Ok(())
}

/// The dialog message as plain text (for accessibility).
fn text() -> String {
    format!(
        "Are you sure you want to exit synoik?\n\n\
         Press {KEY_NAME} to confirm."
    )
}

pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("Exit synoik");
    node.set_description(text());
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem};
    use smithay::utils::Buffer as BufferCoord;

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

        let mut cache = ContentCache::new();
        let mut tex = widget::bake_content(
            &mut vk,
            &mut cache,
            1.,
            0,
            |r| prepare_dialog(r, 1.),
            |frame, phys, layout| paint_dialog(frame, phys, layout, 1.),
        )
        .expect("dialog texture");
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

        // The top edge center is the flat GNOME modal card bg (#36363a ≈ 54), opaque —
        // borderless now, not the old red alert border.
        let top = px_at(size.w / 2, 1);
        assert_eq!(top[3], 255, "card top edge must be opaque, got {top:?}");
        assert!(
            (48..=64).contains(&top[0])
                && (48..=64).contains(&top[1])
                && (48..=70).contains(&top[2]),
            "top edge not the flat dialog card bg: {top:?}"
        );

        // A pixel inside a corner (away from text) is the card bg.
        let bg = px_at(size.w - 8, size.h - 8);
        assert!(
            (48..=64).contains(&bg[0]) && bg[3] == 255,
            "card interior not the dialog bg: {bg:?}"
        );

        // The extreme corner is transparent — the card is rounded, not a square box.
        let corner = px_at(size.w - 1, size.h - 1);
        assert_eq!(
            corner[3], 0,
            "card corner should be transparent (rounded), got {corner:?}"
        );

        // A grey keycap pixel (~0x2C = 44) exists, distinctly darker than the 54 card bg.
        let keycap = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| (40..=48).contains(&p[0]) && (40..=48).contains(&p[1]));
        assert!(keycap, "expected a grey keycap patch (~0x2C)");

        // Bright glyph ink.
        let bright = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }
}
