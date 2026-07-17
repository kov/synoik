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
use ordered_float::NotNan;
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{Alignment, FontDescription};
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::end_session::EndSessionType;
use crate::niri_render_elements;
use crate::render_helpers::memory::MemoryBuffer;
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
const FONT: &str = "sans 12px";
const TITLE_FONT: &str = "sans bold 15px";
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
const ACCENT: (f64, f64, f64) = (0.20, 0.52, 0.89);

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

/// Per-scale rendered-buffer cache: the signature it was rendered at, and the buffer.
type Buffers = HashMap<NotNan<f64>, (Option<Sig>, Option<MemoryBuffer>)>;

pub struct EndSessionDialog {
    state: State,
    focused: Button,
    /// What the dialog is showing (type, seconds left). Fed from the [`crate::end_session`] state
    /// machine — the single source of truth — via [`Self::show`]/[`Self::set_content`], and kept
    /// through the close animation (when the state machine has already cleared) so the fade-out
    /// still has something to draw.
    content: Option<(EndSessionType, Option<u64>)>,
    /// One cached buffer per output scale, tagged with the content signature it was rendered at.
    buffers: RefCell<Buffers>,

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
            buffers: RefCell::new(HashMap::new()),
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

        let sig: Sig = (kind as u8, seconds_left, self.focused);
        let mut buffers = self.buffers.borrow_mut();
        let (cached_sig, buffer) = buffers
            .entry(NotNan::new(scale).unwrap())
            .or_insert((None, None));
        if *cached_sig != Some(sig) {
            *buffer = render(scale, kind, seconds_left, self.focused)
                .map_err(|err| warn!("error rendering the end-session dialog: {err:?}"))
                .ok();
            *cached_sig = Some(sig);
        }
        let Some(buffer) = buffer else {
            return;
        };

        let size = buffer.logical_size();
        let Ok(buffer) = TextureBuffer::from_memory_buffer(renderer, buffer) else {
            return;
        };

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

fn render(
    scale: f64,
    kind: EndSessionType,
    seconds_left: Option<u64>,
    focused: Button,
) -> anyhow::Result<MemoryBuffer> {
    let _span = tracy_client::span!("end_session_dialog::render");

    let (title, action_label, description) = content(kind, seconds_left);

    let px = |logical: i32| to_physical_precise_round::<i32>(scale, logical);
    let width = px(WIDTH);
    let height = px(HEIGHT);
    let padding = f64::from(px(PADDING));

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;

    // Dialog background + border.
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;
    cr.rectangle(0., 0., width.into(), height.into());
    cr.set_source_rgb(0.3, 0.3, 0.3);
    cr.set_line_width((f64::from(px(BORDER))).max(1.));
    cr.stroke()?;

    // Title.
    let mut title_font = FontDescription::from_string(TITLE_FONT);
    title_font.set_absolute_size(to_physical_precise_round(scale, title_font.size()));
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&title_font));
    layout.set_alignment(Alignment::Left);
    layout.set_width((width - 2 * px(PADDING)) * pangocairo::pango::SCALE);
    layout.set_text(title);
    cr.move_to(padding, padding);
    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &layout);
    let (_, title_h) = layout.pixel_size();

    // Description (with the countdown).
    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Left);
    layout.set_width((width - 2 * px(PADDING)) * pangocairo::pango::SCALE);
    layout.set_text(&description);
    cr.move_to(padding, padding + f64::from(title_h) + f64::from(px(12)));
    cr.set_source_rgb(0.8, 0.8, 0.8);
    pangocairo::functions::show_layout(&cr, &layout);

    // Buttons.
    draw_button(
        &cr,
        scale,
        EndSessionDialog::button_rect(Button::Cancel),
        "Cancel",
        focused == Button::Cancel,
        &font,
    )?;
    draw_button(
        &cr,
        scale,
        EndSessionDialog::button_rect(Button::Action),
        action_label,
        focused == Button::Action,
        &font,
    )?;

    drop(cr);
    let data = surface.take_data().unwrap();
    Ok(MemoryBuffer::new(
        data.to_vec(),
        Fourcc::Argb8888,
        (width, height),
        scale,
        Transform::Normal,
    ))
}

fn draw_button(
    cr: &cairo::Context,
    scale: f64,
    rect: Rectangle<f64, Logical>,
    label: &str,
    focused: bool,
    font: &FontDescription,
) -> anyhow::Result<()> {
    let x = rect.loc.x * scale;
    let y = rect.loc.y * scale;
    let w = rect.size.w * scale;
    let h = rect.size.h * scale;

    rounded_rect(cr, x, y, w, h, 6. * scale);
    if focused {
        cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
    } else {
        cr.set_source_rgb(0.22, 0.22, 0.22);
    }
    cr.fill()?;

    let layout = pangocairo::functions::create_layout(cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(font));
    layout.set_alignment(Alignment::Center);
    layout.set_width((w.round() as i32) * pangocairo::pango::SCALE);
    layout.set_text(label);
    let (_, label_h) = layout.pixel_size();
    cr.move_to(x, y + (h - f64::from(label_h)) / 2.);
    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(cr, &layout);
    Ok(())
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -0.5 * PI, 0.);
    cr.arc(x + w - r, y + h - r, r, 0., 0.5 * PI);
    cr.arc(x + r, y + h - r, r, 0.5 * PI, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
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
    use super::*;

    /// The CPU render (pango + cairo) produces a sized buffer for every type, with and without the
    /// countdown, and for either focused button — the paths the live dialog cycles through.
    #[test]
    fn render_produces_a_sized_buffer_for_every_variant() {
        for kind in [
            EndSessionType::Logout,
            EndSessionType::Shutdown,
            EndSessionType::Restart,
        ] {
            for seconds in [Some(59), Some(0), None] {
                for focused in [Button::Cancel, Button::Action] {
                    let buffer = render(1., kind, seconds, focused).unwrap();
                    let size = buffer.logical_size();
                    assert!(size.w > 0. && size.h > 0.);
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
