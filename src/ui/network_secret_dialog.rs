// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The network secret dialog's interactive surface (`js/ui/components/networkAgent.js:23-156`).
//!
//! What it is *asking* lives in [`crate::network_secret_dialog`]; this is the drawing, the
//! hit-testing and the open/close animation. It is the polkit dialog's sibling and deliberately
//! shares its card, its buttons and its entry primitive — the two are the session's only modal
//! password surfaces and must not read as coming from different programs.
//!
//! # Layout
//!
//! Child order is GNOME's construction sequence (`:37-118`): the `MessageDialogContent` (title,
//! then description when there is one), then one entry per secret in the order
//! [`crate::network_secret`] produced them, then the caps-lock warning, then the WPS line, then
//! Cancel and Connect.
//!
//! Unlike the polkit dialog, **nothing here reserves space it is not using**. Polkit reserves the
//! message and caps rows because PAM adds and removes them *while the dialog is up*, which would
//! walk the buttons out from under the pointer. A secret request is fixed at the moment it
//! arrives: the field list, the message and the WPS line never change while it is showing, so the
//! card is sized to what it actually holds.
//!
//! An entry's label is its **placeholder**, not a row above it (`:50`, `hint_text: secret.label`).
//! That is why a three-field 802.1X dialog is no taller than three entries.
//!
//! # Which parts are their own elements
//!
//! The card — background, title, description, caps warning, WPS line, buttons — is one bake. The
//! entries are not: each re-bakes as it is typed into, and baking them into the card would re-bake
//! the whole card per keystroke.

use std::cell::RefCell;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use crate::network_secret::SecretContent;
use crate::network_secret_dialog::{Focus, NetworkSecretDialog, CANCEL, CONNECT, WPS_HINT};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{
    self, style, BakeCache, Painter, ParagraphSpan, Rgba, ShapedParagraph, ShapedText, TextShaper,
    TextStyle,
};
use crate::utils::{output_size, to_physical_precise_round};

/// `.prompt-dialog { width: 28em }` (`_dialogs.scss:101`) — the polkit dialog's width, because
/// they are the same card.
fn width() -> f64 {
    crate::ui::em(28.)
}
/// `.prompt-dialog-password-entry { width: 20em }` (`:120`).
fn entry_width() -> f64 {
    crate::ui::em(20.)
}

const PADDING: f64 = 24.;
const SPACING: f64 = 18.;
const CONTENT_TOP: f64 = 12.;
const BLOCK_SPACING: f64 = 18.;
/// Between stacked entries, and between the last entry and the notes under it.
const STACK_SPACING: f64 = 8.;
const BUTTON_TOP: f64 = 6.;
const BUTTON_H: f64 = 40.;
const BUTTON_GAP: f64 = 12.;

const TITLE_PT: f64 = 15.;
const BODY_PT: f64 = 11.;
const CAPTION_PT: f64 = 9.;

const BACKDROP_COLOR: Rgba = [0., 0., 0., 0.4];
const TEXT: Rgba = [1., 1., 1., 1.];
const WARNING: Rgba = [0.804, 0.576, 0.035, 1.];

const CAPS_TEXT: &str = "Caps lock is on";

/// How many lines the description is given. Long enough for the SSID sentence at 28em; reserving
/// by line count rather than measuring keeps the card's height a pure function of the request.
const DESC_LINES: f64 = 2.;
/// And the WPS sentence, which is longer.
const WPS_LINES: f64 = 2.;

synoik_render_elements! {
    NetworkSecretDialogRenderElement => {
        Texture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
        Plain = TextureRenderElement<VkTexture>,
        SolidColor = SolidColorRenderElement,
    }
}

enum State {
    Hidden,
    Showing(crate::animation::Animation),
    Visible,
    Hiding(crate::animation::Animation),
}

/// Where every row sits, in dialog-local logical coordinates. One source of truth for the paint
/// and the hit-test: the entries and buttons are both clickable, so a layout the pointer disagreed
/// with would be a dialog whose controls sit next to themselves.
#[derive(Debug, Clone)]
pub struct Layout {
    pub size: Size<f64, Logical>,
    /// One rect per field, parallel to [`SecretContent::fields`] — display rows included, because
    /// they are drawn as (non-reactive) entries too.
    pub entries: Vec<Rectangle<f64, Logical>>,
    /// Only present when something is masked.
    pub caps: Option<Rectangle<f64, Logical>>,
    /// Only present when NM said the router's WPS button is live.
    pub wps: Option<Rectangle<f64, Logical>>,
    pub cancel: Rectangle<f64, Logical>,
    pub connect: Rectangle<f64, Logical>,
}

pub struct NetworkSecretDialogUi {
    state: State,
    cache: RefCell<BakeCache>,
    /// One per entry; they re-bake per keystroke while the card does not.
    entry_caches: RefCell<Vec<BakeCache>>,
    clock: crate::animation::Clock,
    config: std::rc::Rc<RefCell<synoik_config::Config>>,
}

impl NetworkSecretDialogUi {
    pub fn new(
        clock: crate::animation::Clock,
        config: std::rc::Rc<RefCell<synoik_config::Config>>,
    ) -> Self {
        Self {
            state: State::Hidden,
            cache: RefCell::new(BakeCache::new()),
            entry_caches: RefCell::new(Vec::new()),
            clock,
            config,
        }
    }

    fn animation(&self, from: f64, to: f64) -> crate::animation::Animation {
        let c = self.config.borrow();
        crate::animation::Animation::new(
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

    pub fn show(&mut self) {
        if !self.is_open() {
            self.state = State::Showing(self.animation(self.value(), 1.));
        }
    }

    pub fn hide(&mut self) {
        if self.is_open() {
            self.state = State::Hiding(self.animation(self.value(), 0.));
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    /// Interactive only once fully open — during the scale-in the controls are still moving.
    pub fn is_interactive(&self) -> bool {
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

    /// Finish everything in flight — for headless tests, which have no frame clock.
    pub fn settle(&mut self) {
        if matches!(self.state, State::Showing(_)) {
            self.state = State::Visible;
        } else if matches!(self.state, State::Hiding(_)) {
            self.state = State::Hidden;
        }
    }

    /// Top-left of the dialog in output-local logical coordinates.
    pub fn origin(output_size: Size<f64, Logical>, layout: &Layout) -> Point<f64, Logical> {
        let x = ((output_size.w - layout.size.w) / 2.).max(0.);
        let y = ((output_size.h - layout.size.h) / 2.).max(0.);
        Point::from((x, y))
    }

    /// What is under `pos` (output-local logical), or `None` for the card's background.
    pub fn hit(
        &self,
        dialog: &NetworkSecretDialog,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> Option<Focus> {
        if !self.is_interactive() {
            return None;
        }
        let content = dialog.content()?;
        let l = layout(content);
        let local = pos - Self::origin(output_size, &l);
        if l.cancel.contains(local) {
            return Some(Focus::Cancel);
        }
        if l.connect.contains(local) {
            return Some(Focus::Connect);
        }
        // A display row's rect is hit-tested like any other, and refused by `set_focus` — one rule
        // for what is answerable, rather than two that can drift apart.
        l.entries
            .iter()
            .position(|rect| rect.contains(local))
            .map(Focus::Field)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        dialog: &NetworkSecretDialog,
        accent: [u8; 3],
        push: &mut dyn FnMut(NetworkSecretDialogRenderElement),
    ) {
        let (value, clamped) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible => (1., 1.),
        };
        let Some(content) = dialog.content() else {
            return;
        };
        let _span = tracy_client::span!("NetworkSecretDialogUi::render");
        let clamped = clamped.clamp(0., 1.) as f32;

        let scale = output.current_scale().fractional_scale();
        let out_size = output_size(output);
        let accent_rgba: Rgba = widget::style::accent_rgba(accent);

        let l = layout(content);
        let origin = Self::origin(out_size, &l);
        let centre = (origin + l.size.to_point().downscale(2.)).to_physical_precise_round(scale);

        // --- The card. ---
        let revision = widget::Revision::new()
            .of(&content.title)
            .of(content.message.as_deref().unwrap_or_default())
            .of(dialog.caps_warning())
            .of(content.wps_available)
            .of(dialog.can_connect())
            .of(focus_key(dialog.focus()))
            .of(accent)
            .px(l.size.w)
            .px(l.size.h)
            .done();

        let card = widget::bake(
            renderer,
            &mut self.cache.borrow_mut(),
            scale,
            l.size,
            revision,
            |r| prepare_card(r, scale, dialog, content, &l),
            |frame, phys, prepared| paint_card(frame, phys, prepared, &l, scale, accent_rgba),
        );

        // --- The entries, over the card. ---
        {
            let mut caches = self.entry_caches.borrow_mut();
            // The cache list follows the field list; a new request with a different shape must not
            // draw the last one's text through a stale cache entry.
            caches.resize_with(content.fields.len(), BakeCache::new);

            for (index, field) in content.fields.iter().enumerate() {
                let Some(rect) = l.entries.get(index) else {
                    continue;
                };
                let Some(edit) = dialog.entry(index) else {
                    continue;
                };
                let focused = dialog.focus() == Focus::Field(index);
                let entry_rev = widget::Revision::new()
                    .of(dialog.entry_display(index))
                    .of(edit.cursor())
                    .of(&field.label)
                    .of(focused)
                    .px(rect.size.w)
                    .done();
                let content_for_entry = match dialog.entry_mask(index) {
                    Some(mask) => widget::EntryContent::masked(edit, &field.label, focused, mask),
                    None => widget::EntryContent::of(edit, &field.label, focused),
                };
                match widget::Entry::bake(
                    renderer,
                    &mut caches[index],
                    scale,
                    rect.size.w,
                    rect.size.h,
                    content_for_entry,
                    widget::EntryStyle::Dialog,
                    focused,
                    false,
                    accent_rgba,
                    entry_rev,
                ) {
                    Ok(texture) => push(NetworkSecretDialogRenderElement::Plain(plain(
                        texture,
                        origin + rect.loc,
                        clamped,
                    ))),
                    Err(err) => warn!("error drawing a network secret entry: {err:#}"),
                }
            }
        }

        // --- The card itself, under everything that sits on it. ---
        match card {
            Ok(texture) => push(NetworkSecretDialogRenderElement::Texture(scaled(
                texture, origin, clamped, value, centre,
            ))),
            // This dialog holds a modal grab; a failed draw must not leave an invisible trap, so
            // the backdrop below is pushed either way.
            Err(err) => warn!("error rendering the network secret dialog: {err:#}"),
        }

        // --- The backdrop, dimming everything behind. ---
        let backdrop = SolidColorBuffer::new(out_size, BACKDROP_COLOR);
        push(NetworkSecretDialogRenderElement::SolidColor(
            SolidColorRenderElement::from_buffer(
                &backdrop,
                Point::new(0., 0.),
                clamped,
                Kind::Unspecified,
            ),
        ));
    }
}

/// A bake-revision key for focus. `Focus` is not a plain enum any more, so it cannot be cast.
fn focus_key(focus: Focus) -> u64 {
    match focus {
        Focus::Field(index) => index as u64,
        Focus::Cancel => u64::MAX - 1,
        Focus::Connect => u64::MAX,
    }
}

fn plain(
    buffer: TextureBuffer<VkTexture>,
    at: Point<f64, Logical>,
    alpha: f32,
) -> TextureRenderElement<VkTexture> {
    TextureRenderElement::from_texture_buffer(buffer, at, alpha, None, None, Kind::Unspecified)
}

/// The card, scaled about its own centre as it opens — the same 0.8→1.0 the polkit and end-session
/// dialogs use, so every modal surface arrives the same way.
fn scaled(
    buffer: TextureBuffer<VkTexture>,
    at: Point<f64, Logical>,
    alpha: f32,
    value: f64,
    centre: Point<i32, Physical>,
) -> RescaleRenderElement<TextureRenderElement<VkTexture>> {
    RescaleRenderElement::from_element(plain(buffer, at, alpha), centre, value.max(0.) * 0.2 + 0.8)
}

/// Stack the rows and size the card.
///
/// Every variation is fixed when the request arrives — how many entries, whether there is a
/// description, a caps warning or a WPS line — so unlike the polkit dialog nothing is reserved
/// against a change that cannot happen while the card is up.
pub fn layout(content: &SecretContent) -> Layout {
    let w = width();
    let inner = w - PADDING * 2.;
    let centred = |y: f64, h: f64, cw: f64| {
        Rectangle::new(Point::from(((w - cw) / 2., y)), Size::from((cw, h)))
    };

    let title_h = crate::ui::line_height_px(TITLE_PT);
    let desc_h = crate::ui::line_height_px(BODY_PT);
    let caption_h = crate::ui::line_height_px(CAPTION_PT);

    let mut y = PADDING + CONTENT_TOP;
    y += title_h + BLOCK_SPACING;
    if content.message.is_some() {
        y += desc_h * DESC_LINES + BLOCK_SPACING;
    }

    let mut entries = Vec::with_capacity(content.fields.len());
    for (index, _) in content.fields.iter().enumerate() {
        if index > 0 {
            y += STACK_SPACING;
        }
        entries.push(centred(y, widget::Entry::HEIGHT, entry_width()));
        y += widget::Entry::HEIGHT;
    }

    let caps = content.fields.iter().any(|f| f.password).then(|| {
        y += STACK_SPACING;
        let rect = centred(y, caption_h, inner);
        y += caption_h;
        rect
    });

    let wps = content.wps_available.then(|| {
        y += STACK_SPACING;
        let rect = centred(y, caption_h * WPS_LINES, inner);
        y += caption_h * WPS_LINES;
        rect
    });

    y += SPACING + BUTTON_TOP;
    let button_w = (inner - BUTTON_GAP) / 2.;
    let cancel = Rectangle::new(Point::from((PADDING, y)), Size::from((button_w, BUTTON_H)));
    let connect = Rectangle::new(
        Point::from((PADDING + button_w + BUTTON_GAP, y)),
        Size::from((button_w, BUTTON_H)),
    );
    let height = y + BUTTON_H + PADDING;

    Layout {
        size: Size::from((w, height)),
        entries,
        caps,
        wps,
        cancel,
        connect,
    }
}

/// Everything shaped, ready to paint.
struct Prepared {
    title: ShapedText,
    description: Option<(ShapedParagraph, Point<i32, Physical>)>,
    caps: Option<ShapedText>,
    wps: Option<(ShapedParagraph, Point<i32, Physical>)>,
    cancel: ShapedText,
    connect: ShapedText,
    cancel_btn: widget::Button,
    connect_btn: widget::Button,
}

fn prepare_card(
    renderer: &mut VulkanRenderer,
    scale: f64,
    dialog: &NetworkSecretDialog,
    content: &SecretContent,
    l: &Layout,
) -> anyhow::Result<Prepared> {
    let _span = tracy_client::span!("network_secret_dialog::prepare_card");
    let inner = l.size.w - PADDING * 2.;
    let mut shaper = TextShaper::new(renderer, scale);
    let px = |logical: f64| to_physical_precise_round::<i32>(scale, logical);

    let title = shaper.shape(&content.title, TextStyle::new(TITLE_PT).bold())?;

    // Paragraphs are placed by their ink top, as everywhere else in the toolkit.
    let description = content
        .message
        .as_deref()
        .map(|text| -> anyhow::Result<_> {
            let para = shaper.paragraph(&[ParagraphSpan::new(text, BODY_PT)], inner, BODY_PT)?;
            let (_, iy, _, _) = para.ink_bounds();
            let y = PADDING + CONTENT_TOP + crate::ui::line_height_px(TITLE_PT) + BLOCK_SPACING;
            Ok((
                para,
                Point::<i32, Physical>::from((px(PADDING), px(y) - iy)),
            ))
        })
        .transpose()?;

    let caps = dialog
        .caps_warning()
        .then(|| shaper.shape(CAPS_TEXT, TextStyle::new(CAPTION_PT)))
        .transpose()?;

    let wps = l
        .wps
        .map(|rect| -> anyhow::Result<_> {
            let para = shaper.paragraph(
                &[ParagraphSpan::new(WPS_HINT, CAPTION_PT)],
                inner,
                CAPTION_PT,
            )?;
            let (_, iy, _, _) = para.ink_bounds();
            Ok((
                para,
                Point::<i32, Physical>::from((px(PADDING), px(rect.loc.y) - iy)),
            ))
        })
        .transpose()?;

    let cancel = shaper.shape(CANCEL, TextStyle::new(BODY_PT).bold())?;
    let connect = shaper.shape(CONNECT, TextStyle::new(BODY_PT).bold())?;

    let cancel_btn = widget::Button::new(l.cancel, widget::ButtonStyle::Dialog)
        .focused(dialog.focus() == Focus::Cancel);
    // Connect is insensitive until every field is satisfied (`_updateOkButton`, `:122-131`); like
    // the polkit dialog we draw that as an unlit, unfocusable button rather than inventing an
    // insensitive style.
    let connect_btn = widget::Button::new(l.connect, widget::ButtonStyle::Dialog)
        .focused(dialog.focus() == Focus::Connect && dialog.can_connect());

    Ok(Prepared {
        title,
        description,
        caps,
        wps,
        cancel,
        connect,
        cancel_btn,
        connect_btn,
    })
}

fn paint_card(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    p: &Prepared,
    l: &Layout,
    scale: f64,
    accent: Rgba,
) -> anyhow::Result<()> {
    let mut painter = Painter::new(frame, scale, phys);
    let full = Rectangle::from_size(l.size);
    let inner = Rectangle::new(
        Point::from((PADDING, 0.)),
        Size::from((l.size.w - PADDING * 2., l.size.h)),
    );
    let centre_x = l.size.w / 2.;

    // GNOME modal card: transparent clear then a flat rounded fill, so the corners stay
    // transparent (the composited element carries no opaque region).
    painter.clear(style::TRANSPARENT)?;
    painter.fill_rounded(full, style::DIALOG_RADIUS, style::DIALOG_BG)?;

    painter.text_band(
        &p.title,
        centre_x,
        widget::HAlign::Center,
        PADDING + CONTENT_TOP,
        crate::ui::line_height_px(TITLE_PT),
        TEXT,
        inner,
    )?;

    if let Some((para, origin)) = &p.description {
        painter.paragraph(para, *origin, TEXT)?;
    }

    if let (Some(caps), Some(rect)) = (&p.caps, l.caps) {
        painter.text_band(
            caps,
            centre_x,
            widget::HAlign::Center,
            rect.loc.y,
            rect.size.h,
            WARNING,
            inner,
        )?;
    }

    if let Some((para, origin)) = &p.wps {
        painter.paragraph(para, *origin, TEXT)?;
    }

    painter.button(&p.cancel_btn, &p.cancel, accent)?;
    painter.button(&p.connect_btn, &p.connect, accent)?;
    Ok(())
}

pub fn a11y_node(content: &SecretContent) -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label(content.title.clone());
    if let Some(message) = &content.message {
        node.set_description(message.clone());
    }
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::network_secret::{content, ConnectionInfo};

    fn wifi(wps: bool) -> SecretContent {
        let info = ConnectionInfo {
            kind: "802-11-wireless".to_owned(),
            id: "Café".to_owned(),
            ssid: Some("Café".to_owned()),
            key_mgmt: Some("wpa-psk".to_owned()),
            ..Default::default()
        };
        content(&info, "802-11-wireless-security", &[], wps).unwrap()
    }

    fn wired_8021x() -> SecretContent {
        let info = ConnectionInfo {
            kind: "802-3-ethernet".to_owned(),
            id: "Office".to_owned(),
            eap: Some("peap".to_owned()),
            ..Default::default()
        };
        content(&info, "802-1x", &[], false).unwrap()
    }

    #[test]
    fn every_field_gets_a_rect_in_order() {
        let content = wired_8021x();
        let l = layout(&content);
        assert_eq!(l.entries.len(), content.fields.len());
        for pair in l.entries.windows(2) {
            assert!(
                pair[1].loc.y >= pair[0].loc.y + pair[0].size.h,
                "entries must stack without overlapping: {pair:?}"
            );
        }
    }

    #[test]
    fn more_fields_make_a_taller_card() {
        let one = layout(&wifi(false));
        let three = layout(&wired_8021x());
        assert_eq!(one.entries.len(), 1);
        assert_eq!(three.entries.len(), 3);
        assert!(three.size.h > one.size.h);
        assert_eq!(one.size.w, three.size.w, "the width is fixed at 28em");
    }

    #[test]
    fn a_message_and_a_wps_line_each_add_height() {
        // Wired has no message; wireless does.
        let bare = layout(&wired_8021x());
        let with_wps = layout(&wifi(true));
        let without_wps = layout(&wifi(false));
        assert!(with_wps.wps.is_some());
        assert!(without_wps.wps.is_none());
        assert!(with_wps.size.h > without_wps.size.h);
        assert!(bare.size.h > 0.);
    }

    #[test]
    fn the_caps_row_exists_only_where_something_is_masked() {
        assert!(
            layout(&wifi(false)).caps.is_some(),
            "a password field earns the warning row"
        );
        // A dialog with no masked field: a gsm PIN is masked, so build one that is not by hand.
        let mut content = wifi(false);
        content.fields[0].password = false;
        assert!(layout(&content).caps.is_none());
    }

    #[test]
    fn nothing_overlaps_the_buttons() {
        let l = layout(&wifi(true));
        let last = l.entries.last().unwrap();
        assert!(l.cancel.loc.y > last.loc.y + last.size.h);
        assert!(l.wps.unwrap().loc.y < l.cancel.loc.y);
        assert_eq!(l.cancel.loc.y, l.connect.loc.y);
        assert!(l.cancel.loc.x + l.cancel.size.w <= l.connect.loc.x);
        assert!(l.connect.loc.x + l.connect.size.w <= l.size.w);
        assert!(l.cancel.loc.y + l.cancel.size.h < l.size.h);
    }

    /// The card draws for the shapes the live dialog produces, and the two things a blank draw
    /// would hide are checked: the card is opaque where it should be, and there is glyph ink on
    /// it. Skips cleanly with no Vulkan device.
    #[test]
    fn draws_every_variant() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, Texture};
        use smithay::utils::Buffer as BufferCoord;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_every_variant: no Vulkan device ({e})");
                return;
            }
        };

        for (name, content, caps) in [
            ("wpa", wifi(false), false),
            ("wpa+wps+caps", wifi(true), true),
            ("wired 802.1x", wired_8021x(), false),
        ] {
            let mut dialog = NetworkSecretDialog::new();
            dialog.begin(
                crate::dbus::network_agent::SecretRequest {
                    request_id: "/settings/1/x".to_owned(),
                    content: content.clone(),
                    user_requested: true,
                },
                Duration::ZERO,
            );
            dialog.set_caps_warning(caps);

            let l = layout(&content);
            let mut cache = BakeCache::new();
            let tex = widget::bake(
                &mut vk,
                &mut cache,
                1.,
                l.size,
                0,
                |r| prepare_card(r, 1., &dialog, &content, &l),
                |frame, phys, prepared| {
                    paint_card(frame, phys, prepared, &l, 1., [0.2, 0.52, 0.89, 1.])
                },
            )
            .unwrap_or_else(|e| panic!("{name}: dialog texture: {e}"));

            let mut tex = tex.texture().clone();
            let size = tex.size();
            let fb = vk.bind(&mut tex).expect("bind");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
            let at = |x: i32, y: i32| {
                let i = ((y * size.w + x) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };

            let bg = at(size.w / 2, size.h - 3);
            assert_eq!(bg[3], 255, "{name}: the card must be opaque, got {bg:?}");
            assert_eq!(
                at(size.w - 1, size.h - 1)[3],
                0,
                "{name}: the card's corner should be transparent"
            );

            let bright = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200)
                .count();
            assert!(
                bright > 40,
                "{name}: expected visible glyph ink, got {bright}"
            );
        }
    }
}
