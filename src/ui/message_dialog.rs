// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The shell's modal message dialog — gnome-shell's `ModalDialog` (`js/ui/modalDialog.js`) with a
//! `Dialog` (`js/ui/dialog.js`) holding a `MessageDialogContent`: a centred title and description,
//! an optional check box under them, and a row of buttons.
//!
//! What a dialog says and which buttons it has is [`DialogContent`], given by its owner (the
//! end-session dialog, the keyboard-accessibility confirmation); this is the surface — layout,
//! focus, keys, pointer, the open/close fade and the drawing.
//!
//! **Layout** follows `_dialogs.scss`: a card padded 24px with 18px between the content and the
//! button row; the content 12px further down, at most 28em wide or a fixed width in em; title and
//! description centred and wrapped, 18px apart; buttons homogeneous across the full row, 12px
//! apart (`Clutter.BoxLayout { homogeneous: true }`). It is measured on the CPU
//! ([`synoik_vk::text::wrap_lines_weighted`], [`crate::ui::line_height_px`]), not from the baked
//! texture, so hit-testing needs no renderer.
//!
//! **Keys:** Enter activates the focused button, Tab and Left/Right move focus, a button's own key
//! (`addButton`'s `key`, e.g. Escape) activates it from anywhere, and Space toggles a focused check
//! box. Focus starts on the default button (`_setInitialKeyFocus`). Hovering a button focuses it.
//!
//! **Divergences:** the title is bold where `%title_2` asks for 800 (our text stack has 400 and
//! 700); keys act on press rather than on the matching release (`Dialog.vfunc_event`); and Space
//! does not activate a button, so it cannot double as Enter on one that powers the machine off.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock, Curve};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::system_compositor::{Style, DEPTH_NEAR};
use crate::ui::widget::{
    self, style, BakeCache, HAlign, Painter, Rgba, ShapedText, TextShaper, TextStyle,
};
use crate::utils::output_size;

/// `.modal-dialog { padding: $base_padding * 4 }`.
const PADDING: f64 = 24.;
/// `.modal-dialog { spacing: $base_padding * 3 }`: content box to button box.
const DIALOG_SPACING: f64 = 18.;
/// `.modal-dialog-content-box { padding-top: $base_padding * 2 }`.
const CONTENT_TOP: f64 = 12.;
/// `.message-dialog-content` and `.modal-dialog-content-box` `spacing: $base_padding * 3`.
const CONTENT_SPACING: f64 = 18.;
/// `.modal-dialog-content-box { max-width: 28em }`.
const MAX_CONTENT_EM: f64 = 28.;
/// `.modal-dialog-button-box { padding-top: $base_padding }`.
const BUTTON_BOX_TOP: f64 = 6.;
/// `Clutter.BoxLayout { spacing: 12 }` between the buttons.
const BUTTON_GAP: f64 = 12.;
/// `%dialog_button { padding: $base_padding * 2 }`.
const BUTTON_PADDING: f64 = 12.;
/// The check box's frame is taller than an 11pt line box, so it sets the row. See
/// [`widget::CheckBox::frame_px`].
const CHECK_ROW_H: f64 = 20.;
/// `%title_2` (15pt) and the body (11pt, inherited), GNOME points.
const TITLE_PT: f64 = 15.;
const BODY_PT: f64 = 11.;
/// `OPEN_AND_CLOSE_TIME` (`modalDialog.js:13`), eased `EASE_OUT_QUAD`.
const OPEN_AND_CLOSE_MS: u64 = 100;
/// More lines than any dialog's text runs to; the wrap needs a bound.
const MAX_LINES: usize = 64;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
const TITLE_COLOR: Rgba = [1., 1., 1., 1.];
const DESC_COLOR: Rgba = [0.8, 0.8, 0.8, 1.];

/// One `addButton` (`dialog.js`): its label, whether it is the default (Enter, and initial focus),
/// and the key that activates it from anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogButton {
    pub label: String,
    pub default: bool,
    pub key: Option<Keysym>,
}

impl DialogButton {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            default: false,
            key: None,
        }
    }

    pub fn default(mut self) -> Self {
        self.default = true;
        self
    }

    pub fn key(mut self, key: Keysym) -> Self {
        self.key = Some(key);
        self
    }
}

/// A check box between the text and the buttons: its label and whether it is ticked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogCheck {
    pub label: String,
    pub checked: bool,
}

/// What a dialog says (`MessageDialogContent`'s title and description) and offers.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogContent {
    pub title: String,
    pub description: String,
    pub check: Option<DialogCheck>,
    pub buttons: Vec<DialogButton>,
    /// A fixed content width in em, as a dialog's own style class sets it
    /// (`.end-session-dialog { width: 24em }`); `None` sizes to the content, up to 28em.
    pub width_em: Option<f64>,
}

impl Hash for DialogContent {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.title.hash(state);
        self.description.hash(state);
        self.check.hash(state);
        self.buttons.hash(state);
        self.width_em.map(f64::to_bits).hash(state);
    }
}

/// The shell's message dialogs, for routing input to the one on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDialogKind {
    EndSession,
    KbdA11y,
}

/// What an input event on the dialog asks its owner to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Consumed; nothing else to do.
    Handled,
    /// Button `n` (in [`DialogContent::buttons`] order) was activated.
    Button(usize),
    /// The check box was toggled.
    ToggleCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Focus {
    Check,
    Button(usize),
}

enum State {
    Hidden,
    Showing(Animation),
    Visible,
    /// Confirmed under the system compositor's motion: it stays on screen, inert, for the greeter
    /// or the shutdown splash to come in over it, rather than moving away just before they move
    /// in. The animation is a bound, 1 to 1: past it, a session that did not end hides it.
    Held(Animation),
    Hiding(Animation),
}

/// The card's geometry, card-local logical px.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    size: Size<f64, Logical>,
    /// Each wrapped line and the top of its line box.
    title: Vec<(String, f64)>,
    desc: Vec<(String, f64)>,
    title_h: f64,
    desc_h: f64,
    check: Option<Rectangle<f64, Logical>>,
    buttons: Vec<Rectangle<f64, Logical>>,
}

fn wrap(text: &str, pt: f64, bold: bool, width: f64) -> Vec<String> {
    let px = crate::ui::pt_to_px(pt) as f32;
    synoik_vk::text::wrap_lines_weighted(text, px, bold, width, MAX_LINES, true)
}

fn line_width(text: &str, pt: f64, bold: bool) -> f64 {
    synoik_vk::text::measure_line_width_weighted(text, crate::ui::pt_to_px(pt) as f32, bold)
}

/// Lay the card out — `Dialog`'s vertical box, height-for-width.
fn layout(content: &DialogContent) -> Layout {
    let em = crate::ui::pt_to_px(BODY_PT);
    let inner_w = match content.width_em {
        Some(w) => w * em,
        None => {
            // The content's natural width, as St asks of a box whose labels wrap: each label's
            // one-line width, and the button row's.
            let widest_label = content
                .buttons
                .iter()
                .map(|b| line_width(&b.label, BODY_PT, true))
                .fold(0., f64::max);
            let n = content.buttons.len() as f64;
            let buttons = n * (widest_label + 2. * BUTTON_PADDING) + (n - 1.).max(0.) * BUTTON_GAP;
            let check = content.check.as_ref().map_or(0., |c| {
                widget::CheckBox::frame_px()
                    + widget::CheckBox::label_gap(BODY_PT)
                    + line_width(&c.label, BODY_PT, false)
            });
            line_width(&content.title, TITLE_PT, true)
                .max(line_width(&content.description, BODY_PT, false))
                .max(check)
                .max(buttons)
                .min(MAX_CONTENT_EM * em)
        }
    }
    .ceil();

    let title_h = crate::ui::line_height_px(TITLE_PT);
    let desc_h = crate::ui::line_height_px(BODY_PT);
    let mut y = PADDING + CONTENT_TOP;
    let place = |lines: Vec<String>, line_h: f64, y: &mut f64| {
        lines
            .into_iter()
            .map(|line| {
                let top = *y;
                *y += line_h;
                (line, top)
            })
            .collect::<Vec<_>>()
    };

    // `_setLabel`: an empty title or description is hidden, taking no space or spacing.
    let title = place(
        wrap(&content.title, TITLE_PT, true, inner_w),
        title_h,
        &mut y,
    );
    if !title.is_empty() && !content.description.is_empty() {
        y += CONTENT_SPACING;
    }
    let desc = place(
        wrap(&content.description, BODY_PT, false, inner_w),
        desc_h,
        &mut y,
    );

    let check = content.check.as_ref().map(|_| {
        y += CONTENT_SPACING;
        let rect = Rectangle::new(
            Point::from((PADDING, y)),
            Size::from((inner_w, CHECK_ROW_H)),
        );
        y += CHECK_ROW_H;
        rect
    });

    y += DIALOG_SPACING + BUTTON_BOX_TOP;
    let button_h = (desc_h + 2. * BUTTON_PADDING).ceil();
    let n = content.buttons.len().max(1) as f64;
    let button_w = (inner_w - (n - 1.) * BUTTON_GAP) / n;
    let buttons = (0..content.buttons.len())
        .map(|i| {
            Rectangle::new(
                Point::from((PADDING + i as f64 * (button_w + BUTTON_GAP), y)),
                Size::from((button_w, button_h)),
            )
        })
        .collect();
    y += button_h + PADDING;

    Layout {
        size: Size::from((inner_w + 2. * PADDING, y.ceil())),
        title,
        desc,
        title_h,
        desc_h,
        check,
        buttons,
    }
}

pub struct MessageDialog {
    state: State,
    focused: Focus,
    /// What is on screen, kept through the close animation so the fade-out still has something to
    /// draw.
    content: Option<DialogContent>,
    layout: Option<Layout>,
    cache: RefCell<BakeCache>,
    clock: Clock,
    /// Nested in the system compositor, how it moves switches: the dialog arrives and leaves the
    /// same way. `None` for gnome-shell's fade.
    motion: Option<Style>,
    /// Stacking order among the open dialogs: the last raised is on top and takes the input.
    raised: u64,
}

/// Hands out [`MessageDialog::raised`] stamps.
static RAISES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

synoik_render_elements! {
    MessageDialogRenderElement => {
        Texture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
        SolidColor = SolidColorRenderElement,
    }
}

struct OutputData {
    backdrop: SolidColorBuffer,
}

impl MessageDialog {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: State::Hidden,
            focused: Focus::Button(0),
            content: None,
            layout: None,
            cache: RefCell::new(BakeCache::new()),
            clock,
            motion: None,
            raised: 0,
        }
    }

    /// Where it stacks: of two open dialogs, the greater was raised last and is on top.
    pub fn raised(&self) -> u64 {
        self.raised
    }

    /// How the system compositor moves switches, if nested in one.
    pub fn set_motion(&mut self, motion: Option<Style>) {
        self.motion = motion;
    }

    fn moving(&self) -> Option<Style> {
        self.motion.filter(|s| *s != Style::Fade)
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        if self.moving().is_some() {
            // As the system compositor's curtain slide (`CURTAIN_SLIDE_TIME`).
            return Animation::ease(self.clock.clone(), from, to, 0., 300, Curve::EaseOutQuad);
        }
        Animation::ease(
            self.clock.clone(),
            from,
            to,
            0.,
            OPEN_AND_CLOSE_MS,
            Curve::EaseOutQuad,
        )
    }

    fn value(&self) -> f64 {
        match &self.state {
            State::Hidden => 0.,
            State::Showing(anim) | State::Hiding(anim) => anim.value(),
            State::Visible | State::Held(_) => 1.,
        }
    }

    /// Open with `content`, focus on its default button (`_setInitialKeyFocus`: the default, or
    /// else the first).
    pub fn show(&mut self, content: DialogContent) {
        self.focused = Focus::Button(content.buttons.iter().position(|b| b.default).unwrap_or(0));
        self.set_content(content);
        if !self.is_open() {
            self.state = State::Showing(self.animation(self.value(), 1.));
            self.raised = RAISES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        }
    }

    /// Change what an open dialog says. Focus stays where it was while that still exists.
    pub fn set_content(&mut self, content: DialogContent) {
        let gone = match self.focused {
            Focus::Check => content.check.is_none(),
            Focus::Button(i) => i >= content.buttons.len(),
        };
        if gone {
            self.focused =
                Focus::Button(content.buttons.iter().position(|b| b.default).unwrap_or(0));
        }
        if self.content.as_ref() != Some(&content) {
            self.layout = Some(layout(&content));
            self.content = Some(content);
        }
    }

    pub fn hide(&mut self) {
        if self.is_open() || matches!(self.state, State::Held(_)) {
            self.state = State::Hiding(self.animation(self.value(), 0.));
        }
    }

    /// The dialog's action is under way: hide it, or under the system compositor's motion hold it
    /// ([`State::Held`]) for what comes next to move in over it.
    pub fn confirmed(&mut self) {
        if self.moving().is_none() || !self.is_open() {
            self.hide();
            return;
        }
        // Long enough for the session to end and the switch away to cover it.
        let bound = Animation::ease(self.clock.clone(), 1., 1., 0., 3000, Curve::Linear);
        self.state = State::Held(bound);
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    /// Interactive only once fully open.
    pub fn is_interactive(&self) -> bool {
        matches!(self.state, State::Visible)
    }

    pub fn advance_animations(&mut self) {
        match &mut self.state {
            State::Hidden | State::Visible => (),
            State::Held(bound) => {
                if bound.is_done() {
                    self.state = State::Hiding(self.animation(1., 0.));
                }
            }
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
        matches!(
            self.state,
            State::Showing(_) | State::Held(_) | State::Hiding(_)
        )
    }

    /// Feed a key. Only presses act; everything is consumed while open.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> DialogOutcome {
        if !pressed || !self.is_interactive() {
            return DialogOutcome::Handled;
        }
        let Some(content) = &self.content else {
            return DialogOutcome::Handled;
        };
        let n = content.buttons.len();
        let has_check = content.check.is_some();

        // A button's own key (`_buttonKeys`), then the default's Enter.
        if let Some(raw) = raw {
            if let Some(i) = content.buttons.iter().position(|b| b.key == Some(raw)) {
                return DialogOutcome::Button(i);
            }
        }

        match raw {
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                self.activate(self.focused)
            }
            // Space is the check box's own key (an St.Button activates on it), and only its.
            Some(Keysym::space | Keysym::KP_Space) if self.focused == Focus::Check => {
                DialogOutcome::ToggleCheck
            }
            Some(Keysym::Tab) => {
                self.focused = match self.focused {
                    Focus::Check => Focus::Button(0),
                    Focus::Button(i) if i + 1 < n => Focus::Button(i + 1),
                    Focus::Button(_) if has_check => Focus::Check,
                    Focus::Button(_) => Focus::Button(0),
                };
                DialogOutcome::Handled
            }
            Some(Keysym::ISO_Left_Tab) => {
                self.focused = match self.focused {
                    Focus::Check => Focus::Button(n.saturating_sub(1)),
                    Focus::Button(0) if has_check => Focus::Check,
                    Focus::Button(0) => Focus::Button(n.saturating_sub(1)),
                    Focus::Button(i) => Focus::Button(i - 1),
                };
                DialogOutcome::Handled
            }
            // Arrows stay on the button row, where a horizontal direction means something.
            Some(Keysym::Left) => {
                self.focused = match self.focused {
                    Focus::Button(i) => Focus::Button(i.saturating_sub(1)),
                    Focus::Check => Focus::Button(0),
                };
                DialogOutcome::Handled
            }
            Some(Keysym::Right) => {
                self.focused = match self.focused {
                    Focus::Button(i) => Focus::Button((i + 1).min(n.saturating_sub(1))),
                    Focus::Check => Focus::Button(0),
                };
                DialogOutcome::Handled
            }
            _ => DialogOutcome::Handled,
        }
    }

    fn activate(&self, target: Focus) -> DialogOutcome {
        match target {
            Focus::Button(i) => DialogOutcome::Button(i),
            Focus::Check => DialogOutcome::ToggleCheck,
        }
    }

    /// Pointer moved to `pos` (output-local logical) on an output of `output_size`. Hovering a
    /// button focuses it; returns whether focus changed.
    pub fn pointer_motion(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> bool {
        if !self.is_interactive() {
            return false;
        }
        match self.focus_at(output_size, pos) {
            Some(target) if target != self.focused => {
                self.focused = target;
                true
            }
            _ => false,
        }
    }

    /// Left click at `pos` (output-local logical). Activates what is under it; a click anywhere
    /// else on the modal is swallowed.
    pub fn pointer_click(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> DialogOutcome {
        if !self.is_interactive() {
            return DialogOutcome::Handled;
        }
        match self.focus_at(output_size, pos) {
            Some(target) => self.activate(target),
            None => DialogOutcome::Handled,
        }
    }

    /// Top-left of the card, output-local: centred, clamped to the origin.
    fn origin(&self, output_size: Size<f64, Logical>) -> Point<f64, Logical> {
        let size = self.layout.as_ref().map_or(Size::default(), |l| l.size);
        Point::from((
            ((output_size.w - size.w) / 2.).max(0.),
            ((output_size.h - size.h) / 2.).max(0.),
        ))
    }

    /// Button `i`, output-local, where a click on it lands.
    pub fn button_rect(
        &self,
        output_size: Size<f64, Logical>,
        i: usize,
    ) -> Option<Rectangle<f64, Logical>> {
        let mut rect = *self.layout.as_ref()?.buttons.get(i)?;
        rect.loc += self.origin(output_size);
        Some(rect)
    }

    fn check_box(&self) -> Option<widget::CheckBox> {
        let rect = self.layout.as_ref()?.check?;
        let checked = self.content.as_ref()?.check.as_ref()?.checked;
        Some(widget::CheckBox::new(rect, checked).focused(self.focused == Focus::Check))
    }

    fn focus_at(&self, output_size: Size<f64, Logical>, pos: Point<f64, Logical>) -> Option<Focus> {
        let layout = self.layout.as_ref()?;
        let local = pos - self.origin(output_size);
        if self.check_box().is_some_and(|c| c.contains(local)) {
            return Some(Focus::Check);
        }
        layout
            .buttons
            .iter()
            .position(|b| b.contains(local))
            .map(Focus::Button)
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        output: &Output,
        accent: [u8; 3],
        push: &mut dyn FnMut(MessageDialogRenderElement),
    ) {
        let (value, clamped_value) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible | State::Held(_) => (1., 1.),
        };
        let (Some(content), Some(layout)) = (&self.content, &self.layout) else {
            return;
        };
        let _span = tracy_client::span!("MessageDialog::render");
        let clamped_value = clamped_value.clamp(0., 1.);

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let accent_rgba: Rgba = widget::style::accent_rgba(accent);

        // Everything painted is in the key: the words, the focus, the tick, the accent.
        let revision = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            content.hash(&mut hasher);
            self.focused.hash(&mut hasher);
            accent.hash(&mut hasher);
            hasher.finish()
        };

        let texture = {
            let mut cache = self.cache.borrow_mut();
            // Fail visible: on error the backdrop still draws, so a modal grab never leaves the
            // seat with nothing on screen.
            match widget::bake(
                renderer,
                &mut cache,
                scale,
                layout.size,
                revision,
                |renderer| prepare(renderer, scale, content, layout, self.focused),
                |frame, phys, prepared| paint(frame, phys, prepared, layout, scale, accent_rgba),
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    warn!("error rendering a message dialog: {err:#}");
                    None
                }
            }
        };

        // How far down from the top the curtain is, how big the dialog is drawn about its centre,
        // and how opaque, for the motion it arrives with; gnome-shell's is a plain fade.
        let (drop, rescale, alpha) = match self.moving() {
            Some(Style::Curtain) => (-(1. - clamped_value) * output_size.h, 1., 1.),
            Some(Style::Depth) => {
                let away = 1. - value.max(0.);
                (
                    0.,
                    1. + (DEPTH_NEAR - 1.) * away,
                    clamped_value * clamped_value,
                )
            }
            _ => (0., 1., clamped_value),
        };

        if let Some(buffer) = texture {
            let size = buffer.logical_size();
            let location = (output_size.to_point() - size.to_point()).downscale(2.);
            let mut location = location.to_physical_precise_round(scale).to_logical(scale);
            location.x = f64::max(0., location.x);
            location.y = f64::max(0., location.y);
            location.y += drop;

            let elem = TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                alpha as f32,
                None,
                None,
                Kind::Unspecified,
            );
            let centre = (location + size.downscale(2.)).to_physical_precise_round(scale);
            let elem = RescaleRenderElement::from_element(elem, centre, rescale);
            push(MessageDialogRenderElement::Texture(elem));

            // The check box's tick. An icon is a texture, not a paint verb, so it rides *over* the
            // baked card — and takes the same fade and rescale about the same centre, or it drifts
            // off its own frame on the way in.
            if let Some(check) = self.check_box().filter(|c| c.checked) {
                if let Some(elem) = widget::icon_element_alpha(
                    renderer,
                    icons,
                    &["check-symbolic"],
                    widget::CheckBox::ICON_PX,
                    scale,
                    style::TEXT,
                    location,
                    check.glyph_centre(),
                    alpha as f32,
                ) {
                    let elem = RescaleRenderElement::from_element(elem, centre, rescale);
                    push(MessageDialogRenderElement::Texture(elem));
                }
            }
        }

        // The backdrop dimming what is behind, faded with the dialog.
        let data = output.user_data().get_or_insert(|| {
            Mutex::new(OutputData {
                backdrop: SolidColorBuffer::new(output_size, BACKDROP_COLOR),
            })
        });
        let mut data = data.lock().unwrap();
        data.backdrop.resize(output_size);
        let elem = SolidColorRenderElement::from_buffer(
            &data.backdrop,
            Point::new(0., drop),
            alpha as f32,
            Kind::Unspecified,
        );
        push(MessageDialogRenderElement::SolidColor(elem));
    }
}

/// The shaped runs [`paint`] draws.
struct Prepared {
    title: Vec<ShapedText>,
    desc: Vec<ShapedText>,
    buttons: Vec<(widget::Button, ShapedText)>,
    check: Option<(widget::CheckBox, ShapedText)>,
}

fn prepare(
    renderer: &mut VulkanRenderer,
    scale: f64,
    content: &DialogContent,
    layout: &Layout,
    focused: Focus,
) -> anyhow::Result<Prepared> {
    let mut shaper = TextShaper::new(renderer, scale);
    let title = layout
        .title
        .iter()
        .map(|(line, _)| shaper.shape(line, TextStyle::new(TITLE_PT).bold()))
        .collect::<anyhow::Result<_>>()?;
    let desc = layout
        .desc
        .iter()
        .map(|(line, _)| shaper.shape(line, TextStyle::new(BODY_PT)))
        .collect::<anyhow::Result<_>>()?;
    let buttons = content
        .buttons
        .iter()
        .zip(&layout.buttons)
        .enumerate()
        .map(|(i, (button, rect))| {
            let widget = widget::Button::new(*rect, widget::ButtonStyle::Dialog)
                .focused(focused == Focus::Button(i));
            Ok((
                widget,
                shaper.shape(&button.label, TextStyle::new(BODY_PT).bold())?,
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    let check = match (&content.check, layout.check) {
        (Some(check), Some(rect)) => Some((
            widget::CheckBox::new(rect, check.checked).focused(focused == Focus::Check),
            shaper.shape(&check.label, TextStyle::new(BODY_PT))?,
        )),
        _ => None,
    };
    Ok(Prepared {
        title,
        desc,
        buttons,
        check,
    })
}

fn paint(
    frame: &mut VulkanFrame,
    phys: Size<i32, smithay::utils::Physical>,
    prepared: &Prepared,
    layout: &Layout,
    scale: f64,
    accent: Rgba,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    let full = Rectangle::from_size(layout.size);

    // A flat rounded card on a transparent clear, so the corners stay see-through.
    p.clear(style::TRANSPARENT)?;
    p.fill_rounded(full, style::DIALOG_RADIUS, style::dialog_bg())?;

    let centre_x = layout.size.w / 2.;
    for (run, (_, top)) in prepared.title.iter().zip(&layout.title) {
        p.text_band(
            run,
            centre_x,
            HAlign::Center,
            *top,
            layout.title_h,
            TITLE_COLOR,
            full,
        )?;
    }
    for (run, (_, top)) in prepared.desc.iter().zip(&layout.desc) {
        p.text_band(
            run,
            centre_x,
            HAlign::Center,
            *top,
            layout.desc_h,
            DESC_COLOR,
            full,
        )?;
    }

    // The check box's frame here; its tick is composited over the texture as an element.
    if let Some((check, label)) = &prepared.check {
        p.check_box(check, accent)?;
        p.text(
            label,
            Point::from((
                check.label_x(BODY_PT),
                check.rect.loc.y + check.rect.size.h / 2.,
            )),
            widget::Align::LEFT_MIDDLE,
            style::TEXT,
        )?;
    }

    for (button, label) in &prepared.buttons {
        p.button(button, label, accent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(buttons: &[&str], check: bool, width_em: Option<f64>) -> DialogContent {
        DialogContent {
            title: "Sticky Keys Turned On".into(),
            description: "You just pressed the Shift key 5 times in a row. This is the shortcut \
                          for the Sticky Keys feature, which affects the way your keyboard works."
                .into(),
            check: check.then(|| DialogCheck {
                label: "Install pending software updates".into(),
                checked: false,
            }),
            buttons: buttons.iter().map(|l| DialogButton::new(*l)).collect(),
            width_em,
        }
    }

    /// The buttons share the row equally and span it, inside the padding; the check row sits
    /// between the text and the buttons; a fixed width is the content width plus padding.
    #[test]
    fn the_layout_follows_the_stylesheet() {
        let em = crate::ui::pt_to_px(BODY_PT);
        for check in [false, true] {
            let l = layout(&content(&["Leave On", "Turn Off"], check, Some(24.)));
            assert_eq!(l.size.w, (24. * em).ceil() + 2. * PADDING);
            let [a, b] = [l.buttons[0], l.buttons[1]];
            assert_eq!(a.size, b.size, "homogeneous");
            assert_eq!(a.loc.x, PADDING);
            assert_eq!(
                b.loc.x + b.size.w,
                l.size.w - PADDING,
                "the row is full width"
            );
            assert_eq!(b.loc.x - (a.loc.x + a.size.w), BUTTON_GAP);
            assert_eq!(a.loc.y + a.size.h + PADDING, l.size.h);
            let text_bottom = l.desc.last().unwrap().1 + l.desc_h;
            if let Some(c) = l.check {
                assert_eq!(c.loc.y, text_bottom + CONTENT_SPACING);
                assert_eq!(
                    a.loc.y,
                    c.loc.y + c.size.h + DIALOG_SPACING + BUTTON_BOX_TOP
                );
            } else {
                assert_eq!(a.loc.y, text_bottom + DIALOG_SPACING + BUTTON_BOX_TOP);
            }
        }
        let free = layout(&content(&["OK"], false, None));
        assert!(
            free.size.w <= MAX_CONTENT_EM * em + 2. * PADDING + 1.,
            "a long description wraps at 28em"
        );
        assert!(free.desc.len() > 1, "and so takes more than one line");
    }

    /// Enter activates the focused button, starting on the default; a button's key activates it
    /// from anywhere; Tab walks the buttons and the check box.
    #[test]
    fn keys_reach_the_buttons() {
        let at = |ms: u64| std::time::Duration::from_millis(ms);
        let mut clock = Clock::with_time(at(0));
        let mut dialog = MessageDialog::new(clock.clone());
        let mut c = content(&["Turn Off", "Leave On"], true, None);
        c.buttons[0].key = Some(Keysym::Escape);
        c.buttons[1].default = true;
        dialog.show(c);
        clock.set_unadjusted(at(1000));
        dialog.advance_animations();

        let key = |d: &mut MessageDialog, k| d.handle_key(Some(k), true);
        assert_eq!(key(&mut dialog, Keysym::Return), DialogOutcome::Button(1));
        assert_eq!(key(&mut dialog, Keysym::Escape), DialogOutcome::Button(0));
        assert_eq!(key(&mut dialog, Keysym::Left), DialogOutcome::Handled);
        assert_eq!(key(&mut dialog, Keysym::Return), DialogOutcome::Button(0));
        key(&mut dialog, Keysym::Tab);
        key(&mut dialog, Keysym::Tab);
        assert_eq!(key(&mut dialog, Keysym::space), DialogOutcome::ToggleCheck);
        assert_eq!(
            dialog.handle_key(Some(Keysym::Return), false),
            DialogOutcome::Handled,
            "releases do nothing"
        );
    }

    /// Under the system compositor's curtain a confirmed dialog stays on screen for the greeter to
    /// come down over, instead of sliding away first; one whose session does not end goes after
    /// the bound. Without a motion it closes on confirm, as gnome-shell's.
    #[test]
    fn a_confirmed_dialog_waits_under_the_curtain() {
        let at = |ms: u64| std::time::Duration::from_millis(ms);
        for (motion, held) in [(Some(Style::Curtain), true), (None, false)] {
            let mut clock = Clock::with_time(at(0));
            let mut dialog = MessageDialog::new(clock.clone());
            dialog.set_motion(motion);
            dialog.show(content(&["Cancel", "Log Out"], false, Some(24.)));
            clock.set_unadjusted(at(1000));
            dialog.advance_animations();
            assert!(dialog.is_interactive());

            dialog.confirmed();
            clock.set_unadjusted(at(2000));
            dialog.advance_animations();
            assert!(!dialog.is_interactive(), "{motion:?}: inert once confirmed");
            assert_eq!(dialog.value() == 1., held, "{motion:?}");

            clock.set_unadjusted(at(4500));
            dialog.advance_animations();
            clock.set_unadjusted(at(5000));
            dialog.advance_animations();
            assert!(
                matches!(dialog.state, State::Hidden),
                "{motion:?}: gone past the bound"
            );
        }
    }

    /// The GPU draws the card at its laid-out size, the focused button carries the accent ring,
    /// and the text is there. Skips cleanly with no Vulkan device.
    #[test]
    fn draws_the_card() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, Texture as _};
        use smithay::utils::Buffer as BufferCoord;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_card: no Vulkan device ({e})");
                return;
            }
        };
        for (check, focused) in [(false, 0), (false, 1), (true, 1)] {
            let c = content(&["Cancel", "Restart & Install"], check, Some(24.));
            let l = layout(&c);
            let focus = Focus::Button(focused);
            let mut cache = BakeCache::new();
            let tex = widget::bake(
                &mut vk,
                &mut cache,
                1.,
                l.size,
                0,
                |r| prepare(r, 1., &c, &l, focus),
                |frame, phys, prepared| paint(frame, phys, prepared, &l, 1., [0.2, 0.52, 0.89, 1.]),
            )
            .expect("dialog texture");
            let mut tex = tex.texture().clone();
            let size = tex.size();
            assert_eq!((f64::from(size.w), f64::from(size.h)), (l.size.w, l.size.h));

            let fb = vk.bind(&mut tex).expect("bind");
            let mapping = vk
                .copy_framebuffer(
                    &fb,
                    Rectangle::<i32, BufferCoord>::from_size(size),
                    Fourcc::Abgr8888,
                )
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

            // The focus ring runs along the inside of the focused button's top edge.
            let rect = l.buttons[focused];
            let (bx, by) = (
                (rect.loc.x + rect.size.w / 2.) as i32,
                rect.loc.y as i32 + 1,
            );
            let i = ((by * size.w + bx) * 4) as usize;
            let p = [pixels[i], pixels[i + 1], pixels[i + 2]];
            assert!(
                p[2] > 150 && p[2] > p[0] + 40,
                "focused button has no accent ring inside its top edge: {p:?}"
            );
            let bright = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200)
                .count();
            assert!(bright > 40, "expected visible glyph ink, got {bright}");
        }
    }

    /// High Contrast (`gnome-shell-high-contrast.css`) turns the card `#1a1a1a` and draws the 1px
    /// `$hc_inset_color` outline inside a button — through the same cache, which has to let go of
    /// the dark bake rather than serve it on.
    #[test]
    fn high_contrast_recolours_the_card() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, Texture as _};
        use smithay::utils::Buffer as BufferCoord;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping high_contrast_recolours_the_card: no Vulkan device ({e})");
                return;
            }
        };
        let c = content(&["Cancel", "Log Out"], false, Some(24.));
        let l = layout(&c);
        let mut cache = BakeCache::new();
        let mut draw = |vk: &mut VulkanRenderer| {
            let tex = widget::bake(
                vk,
                &mut cache,
                1.,
                l.size,
                0,
                |r| prepare(r, 1., &c, &l, Focus::Button(1)),
                |frame, phys, prepared| paint(frame, phys, prepared, &l, 1., [0.2, 0.52, 0.89, 1.]),
            )
            .expect("dialog texture");
            let mut tex = tex.texture().clone();
            let size = tex.size();
            let fb = vk.bind(&mut tex).expect("bind");
            let mapping = vk
                .copy_framebuffer(
                    &fb,
                    Rectangle::<i32, BufferCoord>::from_size(size),
                    Fourcc::Abgr8888,
                )
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
            move |x: f64, y: f64| {
                let i = ((y as i32 * size.w + x as i32) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2]]
            }
        };
        // Left of the buttons, level with them: card. The unfocused Cancel's top edge, mid-way.
        let cancel = l.buttons[0];
        let card_at = (12., cancel.loc.y + cancel.size.h / 2.);
        let edge_at = (cancel.loc.x + cancel.size.w / 2., cancel.loc.y);
        let inside_at = (edge_at.0, cancel.loc.y + 4.);

        let dark = draw(&mut vk);
        assert_eq!(
            dark(card_at.0, card_at.1),
            [0x36, 0x36, 0x3a],
            "the dark card"
        );

        crate::ui::publish_high_contrast(true);
        let hc = draw(&mut vk);
        crate::ui::publish_high_contrast(false);
        assert_eq!(
            hc(card_at.0, card_at.1),
            [0x1a, 0x1a, 0x1a],
            "the High Contrast card"
        );
        let (edge, inside) = (hc(edge_at.0, edge_at.1), hc(inside_at.0, inside_at.1));
        assert!(
            edge[0] > inside[0] + 40,
            "an outline inside the button's edge: edge {edge:?}, inside {inside:?}"
        );
        let (edge, inside) = (dark(edge_at.0, edge_at.1), dark(inside_at.0, inside_at.1));
        assert!(
            edge[0] < inside[0] + 10,
            "and none without: edge {edge:?}, inside {inside:?}"
        );
    }
}
