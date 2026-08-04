//! The logout / power-off / restart confirmation dialog.
//!
//! gnome-session raises this by calling `org.gnome.SessionManager.EndSessionDialog.Open` on the
//! shell (see `dbus::gnome_session`); this is the compositor-side of gnome-shell's
//! `js/ui/endSessionDialog.js`. The dialog lifecycle and the countdown auto-confirm live in the
//! pure [`crate::end_session::EndSession`] state machine; this widget is only the interactive
//! surface — the open/close animation, which button has focus, and the CPU-rendered texture.
//!
//! Each dialog type has exactly one action button (plus Cancel), so the layout is fixed: `Cancel`
//! on the left, the action (`Log Out` / `Power Off` / `Restart`) on the right, with a title and a
//! counting-down description above. Left/Right/Tab move focus, Enter activates the focused button,
//! Esc cancels; the pointer hovers to focus and clicks to activate. Content (which type, seconds
//! left) is passed in at render time so there is a single source of truth in the state machine.

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
use crate::end_session::EndSessionType;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{
    self, style, BakeCache, Painter, ParagraphSpan, Rgba, ShapedParagraph, ShapedText, TextShaper,
    TextStyle,
};
use crate::utils::{output_size, to_physical_precise_round};

// Logical layout. Fixed so the pointer hit-test and the rendered geometry agree without threading a
// measured layout between them.
const WIDTH: i32 = 400;
const HEIGHT: i32 = 190;
const PADDING: i32 = 24;
const BUTTON_W: i32 = 120;
const BUTTON_H: i32 = 40;
const BUTTON_GAP: i32 = 12;
/// Title font size (bold, GNOME `%title_3` = 15pt) and body/label font size (11pt),
/// GNOME points (shaping routes them through [`crate::ui::pt_to_px`]).
const TITLE_PT: f64 = 15.;
const BODY_PT: f64 = 11.;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// GNOME modal-dialog card: flat `$bg_color`, borderless, rounded to `$alert_radius`.
const BOX_BG: [f32; 4] = style::DIALOG_BG;
/// Title (white) and description (grey) text colours; button labels use `style::TEXT`.
const TITLE_COLOR: [f32; 4] = [1., 1., 1., 1.];
const DESC_COLOR: [f32; 4] = [0.8, 0.8, 0.8, 1.];

/// Which button currently has focus (keyboard focus and pointer hover are unified — hovering a
/// button focuses it, and Enter activates the focused one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Cancel,
    Action,
}

impl Button {
    fn toggled(self) -> Self {
        match self {
            Button::Cancel => Button::Action,
            Button::Action => Button::Cancel,
        }
    }
}

/// What an input event on the dialog asks the compositor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Consumed; nothing else to do (swallow the event).
    Handled,
    /// The user activated the action button (or pressed Enter on it): confirm the session action.
    Confirm,
    /// The user pressed Cancel / Esc: abort.
    Cancel,
}

enum State {
    Hidden,
    Showing(Animation),
    Visible,
    Hiding(Animation),
}

/// Pack the content signature (dialog type, countdown, focused button, accent) into a
/// [`widget::bake`] revision — the cache re-renders only when one of these changes (per output
/// scale, which the bake key handles). Bit 0-1 kind, bit 2 focused-is-action, bits 3-39 the
/// countdown (0 = none), bits 40-63 the accent RGB (so a live accent change re-bakes).
fn revision_for(
    kind: EndSessionType,
    seconds_left: Option<u64>,
    focused: Button,
    accent: [u8; 3],
) -> u64 {
    let secs = seconds_left.map_or(0, |s| s + 1);
    let accent = (accent[0] as u64) << 16 | (accent[1] as u64) << 8 | (accent[2] as u64);
    (kind as u64) | (((focused == Button::Action) as u64) << 2) | (secs << 3) | (accent << 40)
}

pub struct EndSessionDialog {
    state: State,
    focused: Button,
    /// What the dialog is showing (type, seconds left). Fed from the [`crate::end_session`] state
    /// machine — the single source of truth — via [`Self::show`]/[`Self::set_content`], and kept
    /// through the close animation (when the state machine has already cleared) so the fade-out
    /// still has something to draw.
    content: Option<(EndSessionType, Option<u64>)>,
    /// One cached texture per output scale, keyed by the content revision it was baked at.
    cache: RefCell<BakeCache>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
}

synoik_render_elements! {
    EndSessionDialogRenderElement => {
        Texture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
        SolidColor = SolidColorRenderElement,
    }
}

struct OutputData {
    backdrop: SolidColorBuffer,
}

impl EndSessionDialog {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            focused: Button::Action,
            content: None,
            cache: RefCell::new(BakeCache::new()),
            clock,
            config,
        }
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        // Reuse the exit-confirmation open/close curve; it's the same kind of modal dialog.
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

    /// Raise the dialog for `kind` (default focus on the action button, so Enter confirms).
    pub fn show(&mut self, kind: EndSessionType) {
        self.focused = Button::Action;
        self.content = Some((kind, None));
        if !self.is_open() {
            self.state = State::Showing(self.animation(self.value(), 1.));
        }
    }

    /// Update the displayed type and countdown (the state machine drives this each second).
    pub fn set_content(&mut self, kind: EndSessionType, seconds_left: Option<u64>) {
        self.content = Some((kind, seconds_left));
    }

    pub fn hide(&mut self) {
        if self.is_open() {
            self.state = State::Hiding(self.animation(self.value(), 0.));
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    /// Interactive only once fully open — during the scale-in/out the buttons are moving, so we
    /// don't hit-test against them.
    fn is_interactive(&self) -> bool {
        matches!(self.state, State::Visible)
    }

    pub fn advance_animations(&mut self) {
        match &mut self.state {
            State::Hidden | State::Visible => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    self.state = State::Visible;
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
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    /// Feed a key. Only presses act; Esc cancels, Enter activates the focused button,
    /// Left/Right/Tab move focus. Returns [`DialogOutcome::Handled`] to swallow everything else
    /// while open.
    pub fn handle_key(
        &mut self,
        raw: Option<smithay::input::keyboard::Keysym>,
        pressed: bool,
    ) -> DialogOutcome {
        use smithay::input::keyboard::Keysym;

        if !pressed || !self.is_interactive() {
            return DialogOutcome::Handled;
        }

        match raw {
            Some(Keysym::Escape) => DialogOutcome::Cancel,
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                self.activate(self.focused)
            }
            Some(Keysym::Left | Keysym::Right | Keysym::Tab | Keysym::ISO_Left_Tab) => {
                self.focused = self.focused.toggled();
                DialogOutcome::Handled
            }
            _ => DialogOutcome::Handled,
        }
    }

    /// Pointer moved to `pos` (output-local logical); `output_size` is that output's logical size.
    /// Hovering a button focuses it. Returns whether focus changed (so the caller can queue a
    /// redraw).
    pub fn pointer_motion(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> bool {
        if !self.is_interactive() {
            return false;
        }
        if let Some(button) = self.button_at(output_size, pos) {
            if self.focused != button {
                self.focused = button;
                return true;
            }
        }
        false
    }

    /// Left click at `pos` (output-local logical). Activates a button if one is under the cursor;
    /// clicks elsewhere on the modal are swallowed.
    pub fn pointer_click(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> DialogOutcome {
        if !self.is_interactive() {
            return DialogOutcome::Handled;
        }
        match self.button_at(output_size, pos) {
            Some(button) => self.activate(button),
            None => DialogOutcome::Handled,
        }
    }

    fn activate(&self, button: Button) -> DialogOutcome {
        match button {
            Button::Cancel => DialogOutcome::Cancel,
            Button::Action => DialogOutcome::Confirm,
        }
    }

    /// Top-left of the dialog in output-local logical coordinates (centered, clamped to the
    /// origin).
    fn origin(output_size: Size<f64, Logical>) -> Point<f64, Logical> {
        let x = ((output_size.w - f64::from(WIDTH)) / 2.).max(0.);
        let y = ((output_size.h - f64::from(HEIGHT)) / 2.).max(0.);
        Point::from((x, y))
    }

    /// Button rectangle in dialog-local logical coordinates.
    fn button_rect(button: Button) -> Rectangle<f64, Logical> {
        let y = HEIGHT - PADDING - BUTTON_H;
        let action_x = WIDTH - PADDING - BUTTON_W;
        let x = match button {
            Button::Action => action_x,
            Button::Cancel => action_x - BUTTON_GAP - BUTTON_W,
        };
        Rectangle::new(
            Point::from((f64::from(x), f64::from(y))),
            Size::from((f64::from(BUTTON_W), f64::from(BUTTON_H))),
        )
    }

    fn button_at(
        &self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> Option<Button> {
        let local = pos - Self::origin(output_size);
        [Button::Cancel, Button::Action]
            .into_iter()
            .find(|&b| Self::button_rect(b).contains(local))
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        accent: [u8; 3],
        push: &mut dyn FnMut(EndSessionDialogRenderElement),
    ) {
        let (value, clamped_value) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible => (1., 1.),
        };
        let Some((kind, seconds_left)) = self.content else {
            return;
        };
        let _span = tracy_client::span!("EndSessionDialog::render");
        let clamped_value = clamped_value.clamp(0., 1.);

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let accent_rgba: Rgba = widget::style::accent_rgba(accent);
        let revision = revision_for(kind, seconds_left, self.focused, accent);

        let texture = {
            let mut cache = self.cache.borrow_mut();
            // Fixed-size box → logical `WIDTH × HEIGHT`; the bake key handles scale + revision.
            // Fail visible: on error fall through to always draw the backdrop (this dialog is a
            // modal grab; never leave the seat with no overlay at all).
            match widget::bake(
                renderer,
                &mut cache,
                scale,
                Size::from((f64::from(WIDTH), f64::from(HEIGHT))),
                revision,
                |renderer| prepare_dialog(renderer, scale, kind, seconds_left, self.focused),
                |frame, phys, layout| paint_dialog(frame, phys, layout, scale, accent_rgba),
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    warn!("error rendering the end-session dialog: {err:#}");
                    None
                }
            }
        };

        if let Some(texture) = texture {
            let tex_size = texture.size();
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
            push(EndSessionDialogRenderElement::Texture(elem));
        }

        // Backdrop dimming the windows behind, faded in with the dialog.
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
        push(EndSessionDialogRenderElement::SolidColor(elem));
    }
}

/// Title, action-button label, and counting-down description for a dialog type — mirrors the
/// per-type content in gnome-shell's `endSessionDialog.js`.
fn content(
    kind: EndSessionType,
    seconds_left: Option<u64>,
) -> (&'static str, &'static str, String) {
    let (title, action, verb) = match kind {
        EndSessionType::Logout => ("Log Out", "Log Out", "logged out"),
        EndSessionType::Shutdown => ("Power Off", "Power Off", "powered off"),
        EndSessionType::Restart => ("Restart", "Restart", "restarted"),
    };
    let description = match seconds_left {
        Some(secs) => format!("The system will be {verb} automatically in {secs} seconds."),
        None => format!("Do you want to {}?", action.to_lowercase()),
    };
    (title, action, description)
}

/// The computed physical layout of the fixed-size dialog box, produced by [`prepare_dialog`] and
/// drawn by [`paint_dialog`].
struct DialogLayout {
    title: ShapedParagraph,
    desc: ShapedParagraph,
    /// Single-line button labels, centered by [`Painter::button`].
    cancel: ShapedText,
    action: ShapedText,
    title_origin: Point<i32, Physical>,
    desc_origin: Point<i32, Physical>,
    /// The two neutral dialog buttons (logical geometry + focus flag); the focused
    /// one draws the accent ring. Both `ButtonStyle::Dialog` — GNOME's end-session
    /// dialog uses no accent-fill / destructive button.
    cancel_btn: widget::Button,
    action_btn: widget::Button,
}

/// Shape the four text runs and compute the fixed-size box layout — the prepare phase for
/// [`widget::bake`]. One centered run per text element; `render_glyphs` is one colour per call, and
/// the title (white) and description (grey) differ, so they are separate runs. No cairo/pango.
/// (The old pango path left-aligned the title/description and rounded the button corners; this
/// centers them — matching gnome-shell — and squares the buttons.)
fn prepare_dialog(
    renderer: &mut VulkanRenderer,
    scale: f64,
    kind: EndSessionType,
    seconds_left: Option<u64>,
    focused: Button,
) -> anyhow::Result<DialogLayout> {
    let _span = tracy_client::span!("end_session_dialog::prepare_dialog");
    let (title, action_label, description) = content(kind, seconds_left);

    let px = |logical: i32| to_physical_precise_round::<i32>(scale, logical);
    let padding = px(PADDING).max(0);
    let inner_wrap = (WIDTH - PADDING * 2).max(1);

    let mut shaper = TextShaper::new(renderer, scale);
    let title_run = shaper.paragraph(
        &[ParagraphSpan::new(title, TITLE_PT).bold()],
        f64::from(inner_wrap),
        TITLE_PT,
    )?;
    let desc_run = shaper.paragraph(
        &[ParagraphSpan::new(&description, BODY_PT)],
        f64::from(inner_wrap),
        BODY_PT,
    )?;
    // Button labels are single-line, centered by `Painter::button` — plain runs, not
    // wrapped paragraphs.
    let cancel_run = shaper.shape("Cancel", TextStyle::new(BODY_PT).bold())?;
    let action_run = shaper.shape(action_label, TextStyle::new(BODY_PT).bold())?;

    // Stack the title then the description under it, each within the inner width.
    let (_, tiy, _, tih) = title_run.ink_bounds();
    let title_origin = Point::<i32, Physical>::from((padding, padding - tiy));
    let (_, diy, _, _) = desc_run.ink_bounds();
    let desc_origin = Point::<i32, Physical>::from((padding, padding + tih + px(12) - diy));

    // The two neutral dialog buttons (logical geometry); the focused one draws the ring.
    let cancel_btn = widget::Button::new(
        EndSessionDialog::button_rect(Button::Cancel),
        widget::ButtonStyle::Dialog,
    )
    .focused(focused == Button::Cancel);
    let action_btn = widget::Button::new(
        EndSessionDialog::button_rect(Button::Action),
        widget::ButtonStyle::Dialog,
    )
    .focused(focused == Button::Action);

    Ok(DialogLayout {
        title: title_run,
        desc: desc_run,
        cancel: cancel_run,
        action: action_run,
        title_origin,
        desc_origin,
        cancel_btn,
        action_btn,
    })
}

/// Draw the grey border, the dark box, the two button fills, and the four text runs — the paint
/// phase for [`widget::bake`].
fn paint_dialog(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    layout: &DialogLayout,
    scale: f64,
    accent: Rgba,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);

    // GNOME modal card: transparent clear, then a flat rounded fill so the corners
    // stay transparent (the composited element carries no opaque region). No border.
    p.clear(style::TRANSPARENT)?;
    p.fill_rounded(
        Rectangle::from_size(Size::from((f64::from(WIDTH), f64::from(HEIGHT)))),
        style::DIALOG_RADIUS,
        BOX_BG,
    )?;

    // Title + description.
    p.paragraph(&layout.title, layout.title_origin, TITLE_COLOR)?;
    p.paragraph(&layout.desc, layout.desc_origin, DESC_COLOR)?;

    // The two neutral dialog buttons (accent focus ring on the focused one).
    p.button(&layout.cancel_btn, &layout.cancel, accent)?;
    p.button(&layout.action_btn, &layout.action, accent)?;
    Ok(())
}

#[cfg(feature = "dbus")]
pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("End Session");
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem};
    use smithay::utils::Buffer as BufferCoord;

    use super::*;

    /// The GPU render produces a fixed-size box for every type, countdown state, and focused button
    /// — the paths the live dialog cycles through — and the focused button's accent-blue fill is
    /// visible. Skips cleanly with no Vulkan device.
    #[test]
    fn draws_every_variant() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_every_variant: no Vulkan device ({e})");
                return;
            }
        };

        for kind in [
            EndSessionType::Logout,
            EndSessionType::Shutdown,
            EndSessionType::Restart,
        ] {
            for seconds in [Some(59), Some(0), None] {
                for focused in [Button::Cancel, Button::Action] {
                    let mut cache = BakeCache::new();
                    let mut tex = widget::bake(
                        &mut vk,
                        &mut cache,
                        1.,
                        Size::from((f64::from(WIDTH), f64::from(HEIGHT))),
                        revision_for(kind, seconds, focused, [0x35, 0x84, 0xe4]),
                        |r| prepare_dialog(r, 1., kind, seconds, focused),
                        |frame, phys, layout| {
                            paint_dialog(frame, phys, layout, 1., [0.2, 0.52, 0.89, 1.])
                        },
                    )
                    .expect("dialog texture");
                    let size = tex.size();
                    assert_eq!(
                        (size.w, size.h),
                        (WIDTH, HEIGHT),
                        "fixed dialog size at scale 1"
                    );

                    let fb = vk.bind(&mut tex).expect("bind");
                    let region = Rectangle::<i32, BufferCoord>::from_size(size);
                    let mapping = vk
                        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                        .expect("copy_framebuffer");
                    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

                    // The focused button carries a 2px accent ring inset along the inside of its
                    // rect (the fill is the neutral translucent dialog-button color, matching GNOME
                    // 50.1). Sample 1px below the top edge → the accent band (B clearly dominant).
                    let rect = EndSessionDialog::button_rect(focused);
                    let bx = (rect.loc.x + rect.size.w / 2.) as i32;
                    let by = rect.loc.y as i32 + 1;
                    let i = ((by * size.w + bx) * 4) as usize;
                    let p = [pixels[i], pixels[i + 1], pixels[i + 2]];
                    assert!(
                        p[2] > 150 && p[2] > p[0] + 40,
                        "focused button has no accent ring inside its top edge: {p:?}"
                    );

                    // Bright glyph ink (title / labels).
                    let bright = pixels
                        .chunks_exact(4)
                        .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200)
                        .count();
                    assert!(bright > 40, "expected visible glyph ink, got {bright}");
                }
            }
        }
    }

    #[test]
    fn buttons_do_not_overlap_and_fit_within_the_dialog() {
        let cancel = EndSessionDialog::button_rect(Button::Cancel);
        let action = EndSessionDialog::button_rect(Button::Action);
        assert!(
            cancel.loc.x + cancel.size.w <= action.loc.x,
            "cancel must sit fully left of the action button",
        );
        assert!(
            action.loc.x + action.size.w <= f64::from(WIDTH),
            "the action button must fit within the dialog width",
        );
    }
}
