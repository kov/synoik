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
use crate::end_session::EndSessionType;
use crate::niri_render_elements;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

// Logical layout. Fixed so the pointer hit-test and the rendered geometry agree without threading a
// measured layout between them.
const WIDTH: i32 = 400;
const HEIGHT: i32 = 190;
const PADDING: i32 = 24;
const BORDER: i32 = 2;
const BUTTON_W: i32 = 120;
const BUTTON_H: i32 = 40;
const BUTTON_GAP: i32 = 12;
/// Title font size (bold, GNOME `%title_3` = 15pt) and body/label font size (11pt),
/// logical px-per-em.
const TITLE_PX: f64 = crate::ui::pt_to_px(15.);
const BODY_PX: f64 = crate::ui::pt_to_px(11.);
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// Box background, grey border, and the two button fills (accent when focused), straight RGBA.
const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const BORDER_COLOR: [f32; 4] = [0.3, 0.3, 0.3, 1.];
const BUTTON_BG: [f32; 4] = [0.22, 0.22, 0.22, 1.];
const ACCENT: [f32; 4] = [0.20, 0.52, 0.89, 1.];
/// Title (white), description (grey), and button-label (white) text colours.
const TITLE_COLOR: [f32; 4] = [1., 1., 1., 1.];
const DESC_COLOR: [f32; 4] = [0.8, 0.8, 0.8, 1.];
const LABEL_COLOR: [f32; 4] = [1., 1., 1., 1.];

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

/// The rendered-content signature: re-render only when one of these changes (per output scale).
type Sig = (u8, Option<u64>, Button);

/// Per-scale texture cache: the signature it was rendered at, and the box texture. Tied to a
/// renderer context (dropped when it changes).
struct DialogCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (Sig, VkTexture)>,
}

impl DialogCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
        }
    }
}

pub struct EndSessionDialog {
    state: State,
    focused: Button,
    /// What the dialog is showing (type, seconds left). Fed from the [`crate::end_session`] state
    /// machine — the single source of truth — via [`Self::show`]/[`Self::set_content`], and kept
    /// through the close animation (when the state machine has already cleared) so the fade-out
    /// still has something to draw.
    content: Option<(EndSessionType, Option<u64>)>,
    /// One cached texture per output scale, tagged with the content signature it was rendered at.
    cache: RefCell<DialogCache>,

    clock: Clock,
    config: Rc<RefCell<Config>>,
}

niri_render_elements! {
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
            cache: RefCell::new(DialogCache::new()),
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
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return;
        };
        let sig: Sig = (kind as u8, seconds_left, self.focused);

        let texture = {
            let mut cache = self.cache.borrow_mut();

            // The cached textures belong to one renderer context; drop them all if it changed.
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.textures.clear();
                cache.context = Some(context);
            }

            let fresh = matches!(cache.textures.get(&scale_key), Some((s, _)) if *s == sig);
            if !fresh {
                match draw_dialog_texture(renderer, scale, kind, seconds_left, self.focused) {
                    Ok(texture) => {
                        cache.textures.insert(scale_key, (sig, texture));
                    }
                    Err(err) => {
                        // Fail visible: fall through to always draw the backdrop (modal grab).
                        warn!("error rendering the end-session dialog: {err:#}");
                    }
                }
            }
            cache.textures.get(&scale_key).map(|(_, t)| t.clone())
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

/// Draw the fixed-size dialog into an offscreen [`VkTexture`] on the GPU: the dark box with a grey
/// border, a centered white title, a centered grey (counting-down) description, and the two buttons
/// (Cancel left, action right) — filled accent-blue when focused, else dark grey, with a centered
/// white label. No cairo/pango. (The old pango path left-aligned the title/description and rounded
/// the button corners; this centers them — matching gnome-shell — and squares the buttons.)
fn draw_dialog_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    kind: EndSessionType,
    seconds_left: Option<u64>,
    focused: Button,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("end_session_dialog::draw_dialog_texture");
    let (title, action_label, description) = content(kind, seconds_left);

    let px = |logical: i32| to_physical_precise_round::<i32>(scale, logical);
    let width = px(WIDTH).max(1);
    let height = px(HEIGHT).max(1);
    let padding = px(PADDING).max(0);
    let border = px(BORDER).max(1);
    let inner_wrap = (width - padding * 2).max(1);
    let title_px = (TITLE_PX * scale) as f32;
    let body_px = (BODY_PX * scale) as f32;

    fn sans(text: &str, bold: bool, px: f32) -> TextSpan<'_> {
        TextSpan {
            text,
            family: SpanFamily::Sans,
            bold,
            px,
        }
    }

    // One centered run per text element; render_glyphs is one colour per call, and the title
    // (white) and description (grey) differ, so they are separate runs.
    let title_run = renderer.build_glyph_paragraph(
        &[sans(title, true, title_px)],
        inner_wrap as f32,
        title_px,
    )?;
    let desc_run = renderer.build_glyph_paragraph(
        &[sans(&description, false, body_px)],
        inner_wrap as f32,
        body_px,
    )?;
    let button_wrap = px(BUTTON_W).max(1) as f32;
    let cancel_run =
        renderer.build_glyph_paragraph(&[sans("Cancel", false, body_px)], button_wrap, body_px)?;
    let action_run = renderer.build_glyph_paragraph(
        &[sans(action_label, false, body_px)],
        button_wrap,
        body_px,
    )?;

    let size = Size::<i32, Physical>::from((width, height));
    let full = Rectangle::from_size(size);
    let inner = Rectangle::new(
        Point::from((border, border)),
        Size::from(((width - border * 2).max(0), (height - border * 2).max(0))),
    );

    // Stack the title then the description under it, each centered within the inner width.
    let (_, tiy, _, tih) = title_run.ink_bounds();
    let title_origin = Point::<i32, Physical>::from((padding, padding - tiy));
    let (_, diy, _, _) = desc_run.ink_bounds();
    let desc_origin = Point::<i32, Physical>::from((padding, padding + tih + px(12) - diy));

    // Physical button rects (from the shared logical geometry) + centered label origins.
    let button_phys = |b: Button| -> Rectangle<i32, Physical> {
        let r = EndSessionDialog::button_rect(b);
        Rectangle::new(
            Point::from((px(r.loc.x as i32), px(r.loc.y as i32))),
            Size::from((px(r.size.w as i32), px(r.size.h as i32))),
        )
    };
    let cancel_rect = button_phys(Button::Cancel);
    let action_rect = button_phys(Button::Action);

    let (_, ciy, _, cih) = cancel_run.ink_bounds();
    let cancel_origin = Point::<i32, Physical>::from((
        cancel_rect.loc.x,
        cancel_rect.loc.y + (cancel_rect.size.h - cih) / 2 - ciy,
    ));
    let (_, aiy, _, aih) = action_run.ink_bounds();
    let action_origin = Point::<i32, Physical>::from((
        action_rect.loc.x,
        action_rect.loc.y + (action_rect.size.h - aih) / 2 - aiy,
    ));

    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((width, height)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        // Grey border = whole box grey, then the inner rect dark.
        frame.clear(Color32F::from(BORDER_COLOR), &[full])?;
        frame.clear(Color32F::from(BOX_BG), &[inner])?;

        // Button backgrounds (accent when focused).
        let cancel_bg = if focused == Button::Cancel {
            ACCENT
        } else {
            BUTTON_BG
        };
        let action_bg = if focused == Button::Action {
            ACCENT
        } else {
            BUTTON_BG
        };
        frame.clear(Color32F::from(cancel_bg), &[cancel_rect])?;
        frame.clear(Color32F::from(action_bg), &[action_rect])?;

        // Text.
        frame.render_glyphs(&title_run, title_origin, TITLE_COLOR, full, &[full])?;
        frame.render_glyphs(&desc_run, desc_origin, DESC_COLOR, full, &[full])?;
        frame.render_glyphs(&cancel_run, cancel_origin, LABEL_COLOR, full, &[full])?;
        frame.render_glyphs(&action_run, action_origin, LABEL_COLOR, full, &[full])?;
        let _sync = frame.finish()?;
    }

    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
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
    use smithay::backend::renderer::ExportMem;

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
                    let mut tex = draw_dialog_texture(&mut vk, 1., kind, seconds, focused)
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

                    // The focused button's top strip (above its vertically-centered label) is the
                    // accent-blue fill (B clearly dominant).
                    let rect = EndSessionDialog::button_rect(focused);
                    let bx = (rect.loc.x + rect.size.w / 2.) as i32;
                    let by = rect.loc.y as i32 + 4;
                    let i = ((by * size.w + bx) * 4) as usize;
                    let p = [pixels[i], pixels[i + 1], pixels[i + 2]];
                    assert!(
                        p[2] > 150 && p[2] > p[0] + 40,
                        "focused button not accent-blue at its top: {p:?}"
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
