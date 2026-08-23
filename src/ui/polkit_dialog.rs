// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The "Authentication Required" dialog's interactive surface (`js/ui/components/polkitAgent.js`).
//!
//! What it is *doing* lives in [`crate::polkit_dialog`]; this is the drawing, the hit-testing and
//! the open/close animation.
//!
//! # Layout
//!
//! Child order is GNOME's construction sequence, not a guess at what looks right
//! (`polkitAgent.js:46-161`): title and action description, then the avatar with its label, then
//! the password entry, then the caps-lock warning and the message, then the two buttons.
//!
//! The **message row is always reserved**, even when there is nothing in it. GNOME does this with
//! an invisible `.prompt-dialog-null-label` and says why in a comment (`:131-134`): without it the
//! dialog changes height the moment PAM says anything, and the buttons move out from under the
//! pointer.
//!
//! # Which parts are their own elements
//!
//! The card — background, text, buttons, caps warning, message — is one bake. Three things are
//! not, and each for a reason:
//!
//! - the **entry**, because it shakes ([`widget::Wiggle`]) and re-bakes per keystroke;
//! - the **avatar picture**, because it comes from the image cache rather than the painter;
//! - the **default avatar glyph**, because it comes from the icon cache.

use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use crate::image_source::ImageSource;
use crate::polkit_dialog::{Focus, Message, PolkitDialog, TITLE};
use crate::render_helpers::icon::{IconCache, ImageCache};
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{
    self, style, BakeCache, Painter, ParagraphSpan, Rgba, ShapedParagraph, ShapedText, TextShaper,
    TextStyle,
};
use crate::utils::{output_size, to_physical_precise_round};

/// `.prompt-dialog { width: 28em }` (`_dialogs.scss:101`).
fn width() -> f64 {
    crate::ui::em(28.)
}
/// `.prompt-dialog-password-entry { width: 20em }` (`:120`).
fn entry_width() -> f64 {
    crate::ui::em(20.)
}

/// `.modal-dialog { padding: $base_padding * 4 }` (`_dialogs.scss:7`).
const PADDING: f64 = 24.;
/// `.modal-dialog { spacing: $base_padding * 3 }` (`:8`) — between the content box and the buttons.
const SPACING: f64 = 18.;
/// `.modal-dialog-content-box { padding-top: $base_padding * 2 }` (`:12`).
const CONTENT_TOP: f64 = 12.;
/// `.message-dialog-content { spacing: $base_padding * 3 }` (`:65`).
const BLOCK_SPACING: f64 = 18.;
/// `.polkit-dialog-user-layout { spacing: $base_margin * 2; margin-bottom: $base_padding }`
/// (`:148-149`), and `.prompt-dialog-password-layout { spacing: $base_margin * 2 }` (`:116`).
const STACK_SPACING: f64 = 8.;
const USER_MARGIN_BOTTOM: f64 = 6.;
/// `.modal-dialog-button-box { padding-top: $base_padding }` (`:18`).
const BUTTON_TOP: f64 = 6.;

/// `DIALOG_ICON_SIZE` (`polkitAgent.js:24`).
const AVATAR_PX: f64 = 96.;
/// `.user-icon StIcon { padding: $base_padding * 2 }` (`_misc.scss:17`) — the themed
/// `avatar-default-symbolic` sits inside the circle rather than filling it.
const AVATAR_ICON_PAD: f64 = 12.;
/// `.user-icon { background-color: transparentize($fg_color, 0.95) }` (`_misc.scss:13`) — the plate
/// behind the picture, and the whole of the circle when there is none.
const AVATAR_PLATE: Rgba = [1., 1., 1., 0.05];
/// Lines reserved for polkit's action description. See [`layout`].
const DESC_LINES: f64 = 2.;
/// One image slot, so a picture left over from a previous request is evicted rather than shown.
const AVATAR_SLOT: u64 = 0;

const BUTTON_H: f64 = 40.;
const BUTTON_GAP: f64 = 12.;

/// `%title_2` (`_common.scss:251-254`) for the title, the stage's 11pt body for the description.
const TITLE_PT: f64 = 15.;
const BODY_PT: f64 = 11.;
/// `%title_4` (`:261-264`) — the user label.
const USER_PT: f64 = 13.;
/// `%caption` (`:276-279`) — the caps warning and the message chip.
const CAPTION_PT: f64 = 9.;

const BACKDROP_COLOR: Rgba = [0., 0., 0., 0.4];
const TEXT: Rgba = [1., 1., 1., 1.];
/// `$warning_color: #cd9309` (`_default-colors.scss:20-22`) — the root label, the caps warning and
/// the error chip.
const WARNING: Rgba = [0.804, 0.576, 0.035, 1.];
/// `background-color: transparentize($warning_color, 0.9)` (`_dialogs.scss:136`).
const WARNING_BG: Rgba = [0.804, 0.576, 0.035, 0.1];
/// `.prompt-dialog-info-label` — `$fg_color` on `transparentize($fg_color, 0.9)` (`:140-141`).
const INFO_BG: Rgba = [1., 1., 1., 0.1];
/// `padding: $base_padding * 1.5; border-radius: $base_border_radius; margin: $base_margin 0`
/// (`:129-131`).
const MESSAGE_PAD: f64 = 9.;
const MESSAGE_RADIUS: f64 = 8.;
const MESSAGE_MARGIN: f64 = 4.;

/// `CapsLockWarning`'s text (`shellEntry.js:170`).
const CAPS_TEXT: &str = "Caps lock is on";

synoik_render_elements! {
    PolkitDialogRenderElement => {
        Texture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
        Plain = TextureRenderElement<VkTexture>,
        Rounded = RoundedTextureRenderElement<VkTexture>,
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
/// and the hit-test — the buttons are clickable, so a layout the pointer disagreed with would be a
/// dialog whose buttons are next to themselves.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub size: Size<f64, Logical>,
    pub avatar: Rectangle<f64, Logical>,
    pub entry: Rectangle<f64, Logical>,
    /// Always reserved, warning showing or not.
    pub caps: Rectangle<f64, Logical>,
    /// ...and so is this — GNOME's `.prompt-dialog-null-label`.
    pub message: Rectangle<f64, Logical>,
    pub cancel: Rectangle<f64, Logical>,
    pub authenticate: Rectangle<f64, Logical>,
}

pub struct PolkitDialogUi {
    state: State,
    /// The entry's shake — GNOME wiggles the entry, not the message (`polkitAgent.js:268`).
    wiggle: widget::Wiggle,
    cache: RefCell<BakeCache>,
    /// The entry re-bakes per keystroke; the card around it does not.
    entry_cache: RefCell<BakeCache>,
    ring_cache: RefCell<BakeCache>,
    avatar_uploads: RefCell<widget::ImageUploads>,

    clock: crate::animation::Clock,
    config: std::rc::Rc<RefCell<synoik_config::Config>>,
}

impl PolkitDialogUi {
    pub fn new(
        clock: crate::animation::Clock,
        config: std::rc::Rc<RefCell<synoik_config::Config>>,
    ) -> Self {
        Self {
            state: State::Hidden,
            wiggle: widget::Wiggle::default(),
            cache: RefCell::new(BakeCache::new()),
            entry_cache: RefCell::new(BakeCache::new()),
            ring_cache: RefCell::new(BakeCache::new()),
            avatar_uploads: RefCell::new(widget::ImageUploads::default()),
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
        self.wiggle.settle();
        if self.is_open() {
            self.state = State::Hiding(self.animation(self.value(), 0.));
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Visible)
    }

    /// Interactive only once fully open — during the scale-in the controls are still moving, so we
    /// do not hit-test against them.
    pub fn is_interactive(&self) -> bool {
        matches!(self.state, State::Visible)
    }

    pub fn start_wiggle(&mut self, now: Duration) {
        self.wiggle.start(now);
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

    pub fn are_animations_ongoing(&self, now: Duration) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_)) || self.wiggle.is_animating(now)
    }

    /// Finish everything in flight, wherever it had got to — for headless tests, which have no
    /// frame clock to walk an animation forward.
    pub fn settle(&mut self) {
        self.wiggle.settle();
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
        dialog: &PolkitDialog,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> Option<Focus> {
        if !self.is_interactive() {
            return None;
        }
        let layout = layout(dialog);
        let local = pos - Self::origin(output_size, &layout);
        if layout.cancel.contains(local) {
            return Some(Focus::Cancel);
        }
        if layout.authenticate.contains(local) {
            return Some(Focus::Authenticate);
        }
        if dialog.shows_entry() && layout.entry.contains(local) {
            return Some(Focus::Entry);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        icons: &IconCache,
        images: &ImageCache,
        dialog: &PolkitDialog,
        accent: [u8; 3],
        now: Duration,
        push: &mut dyn FnMut(PolkitDialogRenderElement),
    ) {
        let (value, clamped) = match &self.state {
            State::Hidden => return,
            State::Showing(anim) | State::Hiding(anim) => (anim.value(), anim.clamped_value()),
            State::Visible => (1., 1.),
        };
        let _span = tracy_client::span!("PolkitDialogUi::render");
        let clamped = clamped.clamp(0., 1.) as f32;

        let scale = output.current_scale().fractional_scale();
        let out_size = output_size(output);
        let accent_rgba: Rgba = widget::style::accent_rgba(accent);

        let l = layout(dialog);
        let origin = Self::origin(out_size, &l);
        let centre = (origin + l.size.to_point().downscale(2.)).to_physical_precise_round(scale);

        // --- The card. ---
        let revision = widget::Revision::new()
            .of(dialog.action_message())
            .of(dialog.user_label())
            .of(dialog.message().map(Message::text).unwrap_or_default())
            .of(dialog.message().is_some_and(Message::is_error))
            .of(dialog.caps_warning())
            .of(dialog.shows_entry())
            .of(dialog.can_authenticate())
            .of(dialog.focus() as u64)
            .of(dialog.is_root())
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
            |r| prepare_card(r, scale, dialog, &l),
            |frame, phys, prepared| paint_card(frame, phys, prepared, &l, scale, accent_rgba),
        );
        // Baked now so its cache entry is warm with the rest, but pushed *last* of the card's own
        // elements: the first element pushed is the topmost one, and the card is opaque over its
        // whole rect, so anything queued after it would be drawn behind it and never seen.

        // --- The avatar, over the card's plate. ---
        let avatar_centre = l.avatar.loc + l.avatar.size.to_point().downscale(2.);
        let picture = dialog_avatar(dialog).and_then(|source| {
            let mut uploads = self.avatar_uploads.borrow_mut();
            uploads.retain_sources(|s| *s == source);
            widget::Avatar::element(
                renderer,
                &mut uploads,
                images,
                &source,
                AVATAR_SLOT,
                AVATAR_PX,
                scale,
                1.,
                origin,
                avatar_centre,
                clamped,
            )
        });
        if let Some(el) = picture {
            // The inset ring goes over the picture, so it is pushed first.
            match widget::bake_card_border(
                renderer,
                &mut self.ring_cache.borrow_mut(),
                scale,
                widget::Revision::new().px(AVATAR_PX).done(),
                Size::from((AVATAR_PX, AVATAR_PX)),
                AVATAR_PX / 2.,
                widget::Avatar::RING_COLOR,
            ) {
                Ok(texture) => push(PolkitDialogRenderElement::Plain(plain(
                    texture,
                    origin + l.avatar.loc,
                    clamped,
                ))),
                Err(err) => warn!("error drawing the polkit avatar ring: {err:#}"),
            }
            push(PolkitDialogRenderElement::Rounded(el));
        } else if let Some(el) = widget::icon_element_scaled(
            renderer,
            icons,
            &["avatar-default-symbolic"],
            AVATAR_PX - AVATAR_ICON_PAD * 2.,
            scale,
            1.,
            TEXT,
            origin,
            avatar_centre,
            clamped,
        ) {
            push(PolkitDialogRenderElement::Plain(el));
        }

        // --- The entry: its own element, because it shakes. ---
        if dialog.shows_entry() {
            let text = dialog.entry_display();
            let entry_rev = widget::Revision::new()
                .of(&text)
                .of(dialog.entry().cursor())
                .of(dialog.question().unwrap_or_default())
                .of(dialog.focus() == Focus::Entry)
                .px(l.entry.size.w)
                .done();
            match widget::Entry::bake(
                renderer,
                &mut self.entry_cache.borrow_mut(),
                scale,
                l.entry.size.w,
                l.entry.size.h,
                match dialog.entry_mask() {
                    Some(mask) => widget::EntryContent::masked(
                        dialog.entry(),
                        dialog.question().unwrap_or_default(),
                        dialog.focus() == Focus::Entry,
                        mask,
                    ),
                    None => widget::EntryContent::of(
                        dialog.entry(),
                        dialog.question().unwrap_or_default(),
                        dialog.focus() == Focus::Entry,
                    ),
                },
                widget::EntryStyle::PromptDialog,
                dialog.focus() == Focus::Entry,
                false,
                accent_rgba,
                entry_rev,
            ) {
                Ok(texture) => {
                    let mut at = origin + l.entry.loc;
                    at.x += self.wiggle.offset(now);
                    push(PolkitDialogRenderElement::Plain(plain(
                        texture, at, clamped,
                    )));
                }
                Err(err) => warn!("error drawing the polkit entry: {err:#}"),
            }
        }

        // --- The card itself, under everything that sits on it. ---
        match card {
            Ok(texture) => push(PolkitDialogRenderElement::Texture(scaled(
                texture, origin, clamped, value, centre,
            ))),
            // This dialog holds a modal grab; a failed draw must not leave an invisible trap, so
            // the backdrop below is pushed either way.
            Err(err) => warn!("error rendering the polkit dialog: {err:#}"),
        }

        // --- The backdrop, dimming everything behind. ---
        let backdrop = SolidColorBuffer::new(out_size, BACKDROP_COLOR);
        push(PolkitDialogRenderElement::SolidColor(
            SolidColorRenderElement::from_buffer(
                &backdrop,
                Point::new(0., 0.),
                clamped,
                Kind::Unspecified,
            ),
        ));
    }
}

/// The picture to draw, if AccountsService gave the account one that is on disk.
fn dialog_avatar(dialog: &PolkitDialog) -> Option<ImageSource> {
    dialog.avatar().map(|path| ImageSource::File(path.clone()))
}

fn plain(
    buffer: TextureBuffer<VkTexture>,
    at: Point<f64, Logical>,
    alpha: f32,
) -> TextureRenderElement<VkTexture> {
    TextureRenderElement::from_texture_buffer(buffer, at, alpha, None, None, Kind::Unspecified)
}

/// The card, scaled about its own centre as it opens — the same 0.8→1.0 the end-session dialog
/// uses, so the two modal surfaces arrive the same way.
#[allow(clippy::too_many_arguments)]
fn scaled(
    buffer: TextureBuffer<VkTexture>,
    at: Point<f64, Logical>,
    alpha: f32,
    value: f64,
    centre: Point<i32, Physical>,
) -> RescaleRenderElement<TextureRenderElement<VkTexture>> {
    let elem = plain(buffer, at, alpha);
    RescaleRenderElement::from_element(elem, centre, value.max(0.) * 0.2 + 0.8)
}

/// Stack the rows and size the card.
///
/// The height varies in exactly one way — whether there is an entry — because the caps and message
/// rows are always reserved (see the module docs). That single variation is why this is computed
/// rather than a table of constants, and why the buttons cannot be hit-tested against a guess.
pub fn layout(dialog: &PolkitDialog) -> Layout {
    let w = width();
    let inner = w - PADDING * 2.;
    let centred = |y: f64, h: f64, cw: f64| {
        Rectangle::new(Point::from(((w - cw) / 2., y)), Size::from((cw, h)))
    };

    let title_h = crate::ui::line_height_px(TITLE_PT);
    let desc_h = crate::ui::line_height_px(BODY_PT);
    let user_h = crate::ui::line_height_px(USER_PT);
    let caps_h = crate::ui::line_height_px(CAPTION_PT);
    let message_h = crate::ui::line_height_px(CAPTION_PT) + MESSAGE_PAD * 2. + MESSAGE_MARGIN * 2.;

    let mut y = PADDING + CONTENT_TOP;
    y += title_h + BLOCK_SPACING;
    // The description gets two lines. polkit's action messages are one sentence and 28em is wide;
    // reserving the room unconditionally is what keeps the buttons from moving under the pointer
    // between a short message and a long one.
    y += desc_h * DESC_LINES + BLOCK_SPACING;

    let avatar = centred(y, AVATAR_PX, AVATAR_PX);
    y += AVATAR_PX + STACK_SPACING;
    y += user_h + USER_MARGIN_BOTTOM + BLOCK_SPACING;

    let entry = centred(y, widget::Entry::HEIGHT, entry_width());
    if dialog.shows_entry() {
        y += widget::Entry::HEIGHT + STACK_SPACING;
    }
    let caps = centred(y, caps_h, inner);
    y += caps_h;
    let message = centred(y + MESSAGE_MARGIN, message_h - MESSAGE_MARGIN * 2., inner);
    y += message_h;

    y += SPACING + BUTTON_TOP;
    let button_w = (inner - BUTTON_GAP) / 2.;
    let cancel = Rectangle::new(Point::from((PADDING, y)), Size::from((button_w, BUTTON_H)));
    let authenticate = Rectangle::new(
        Point::from((PADDING + button_w + BUTTON_GAP, y)),
        Size::from((button_w, BUTTON_H)),
    );
    let height = y + BUTTON_H + PADDING;

    Layout {
        size: Size::from((w, height)),
        avatar,
        entry,
        caps,
        message,
        cancel,
        authenticate,
    }
}

/// Everything shaped, ready to paint.
struct Prepared {
    title: ShapedText,
    description: ShapedParagraph,
    description_origin: Point<i32, Physical>,
    user: ShapedText,
    caps: Option<ShapedText>,
    /// The message and whether it is an error, which picks its two colours.
    message: Option<(ShapedText, bool)>,
    cancel: ShapedText,
    authenticate: ShapedText,
    cancel_btn: widget::Button,
    authenticate_btn: widget::Button,
    /// Which colour the user label takes — the account, or the authority.
    user_is_root: bool,
}

fn prepare_card(
    renderer: &mut VulkanRenderer,
    scale: f64,
    dialog: &PolkitDialog,
    l: &Layout,
) -> anyhow::Result<Prepared> {
    let _span = tracy_client::span!("polkit_dialog::prepare_card");
    let inner = l.size.w - PADDING * 2.;
    let mut shaper = TextShaper::new(renderer, scale);

    let title = shaper.shape(TITLE, TextStyle::new(TITLE_PT).bold())?;
    let description = shaper.paragraph(
        &[ParagraphSpan::new(dialog.action_message(), BODY_PT)],
        inner,
        BODY_PT,
    )?;
    let user = shaper.shape(dialog.user_label(), TextStyle::new(USER_PT).bold())?;
    let caps = dialog
        .caps_warning()
        .then(|| shaper.shape(CAPS_TEXT, TextStyle::new(CAPTION_PT)))
        .transpose()?;
    let message = dialog
        .message()
        .map(|m| {
            shaper
                .shape(m.text(), TextStyle::new(CAPTION_PT))
                .map(|shaped| (shaped, m.is_error()))
        })
        .transpose()?;
    let cancel = shaper.shape("Cancel", TextStyle::new(BODY_PT).bold())?;
    let authenticate = shaper.shape("Authenticate", TextStyle::new(BODY_PT).bold())?;

    // The description block is placed by its ink top, like every other paragraph here.
    let px = |logical: f64| to_physical_precise_round::<i32>(scale, logical);
    let (_, diy, _, _) = description.ink_bounds();
    let desc_y = PADDING + CONTENT_TOP + crate::ui::line_height_px(TITLE_PT) + BLOCK_SPACING;
    let description_origin = Point::<i32, Physical>::from((px(PADDING), px(desc_y) - diy));

    let cancel_btn = widget::Button::new(l.cancel, widget::ButtonStyle::Dialog)
        .focused(dialog.focus() == Focus::Cancel);
    // GNOME leaves the OK button insensitive until there is something to send (`:157-159`); we draw
    // that as an unfocusable, unlit button rather than inventing an insensitive style.
    let authenticate_btn = widget::Button::new(l.authenticate, widget::ButtonStyle::Dialog)
        .focused(dialog.focus() == Focus::Authenticate && dialog.can_authenticate());

    Ok(Prepared {
        title,
        description,
        description_origin,
        user,
        caps,
        message,
        cancel,
        authenticate,
        cancel_btn,
        authenticate_btn,
        user_is_root: dialog.is_root(),
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

    // Title and the action's description.
    painter.text_band(
        &p.title,
        centre_x,
        widget::HAlign::Center,
        PADDING + CONTENT_TOP,
        crate::ui::line_height_px(TITLE_PT),
        TEXT,
        inner,
    )?;
    painter.paragraph(&p.description, p.description_origin, TEXT)?;

    // `.user-icon`'s plate — `transparentize($fg_color, 0.95)` (`_misc.scss:13`). It shows through
    // only where the picture does not cover it, which is the whole circle when there is none.
    painter.fill_rounded(l.avatar, AVATAR_PX / 2., AVATAR_PLATE)?;

    // The label under it: the account's name, or `Administrator` in the warning colour for root.
    let user_h = crate::ui::line_height_px(USER_PT);
    let user_y = l.avatar.loc.y + AVATAR_PX + STACK_SPACING;
    let user_color = if p.user_is_root { WARNING } else { TEXT };
    painter.text_band(
        &p.user,
        centre_x,
        widget::HAlign::Center,
        user_y,
        user_h,
        user_color,
        inner,
    )?;

    if let Some(caps) = &p.caps {
        painter.text_band(
            caps,
            centre_x,
            widget::HAlign::Center,
            l.caps.loc.y,
            l.caps.size.h,
            WARNING,
            inner,
        )?;
    }

    if let Some((message, is_error)) = &p.message {
        let (fg, bg) = if *is_error {
            (WARNING, WARNING_BG)
        } else {
            (TEXT, INFO_BG)
        };
        painter.fill_rounded(l.message, MESSAGE_RADIUS, bg)?;
        painter.text_band(
            message,
            centre_x,
            widget::HAlign::Center,
            l.message.loc.y,
            l.message.size.h,
            fg,
            inner,
        )?;
    }

    painter.button(&p.cancel_btn, &p.cancel, accent)?;
    painter.button(&p.authenticate_btn, &p.authenticate, accent)?;
    Ok(())
}

pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label(TITLE);
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem, Texture};
    use smithay::utils::Buffer as BufferCoord;

    use super::*;
    use crate::dbus::polkit_agent::{BeginRequest, PolkitToSynoik};

    fn dialog(user: &str, entry: bool, message: Option<Message>, caps: bool) -> PolkitDialog {
        let mut d = PolkitDialog::new();
        d.begin(BeginRequest {
            action_id: "org.freedesktop.test".to_owned(),
            message: "Authentication is required to install software".to_owned(),
            user_name: user.to_owned(),
            passwordless: false,
            avatar: None,
        });
        if entry {
            d.on_agent_event(PolkitToSynoik::Request {
                prompt: "Password:".to_owned(),
                echo_on: false,
            });
        }
        match message {
            Some(Message::Error(text)) => {
                d.on_agent_event(PolkitToSynoik::ShowError(text));
            }
            Some(Message::Info(text)) => {
                d.on_agent_event(PolkitToSynoik::ShowInfo(text));
            }
            None => (),
        }
        d.set_caps_warning(caps);
        d
    }

    /// The card draws for every combination the live dialog cycles through, and the two things a
    /// blank draw would hide are checked: the card is opaque where it should be, and there is glyph
    /// ink on it. Skips cleanly with no Vulkan device.
    #[test]
    fn draws_every_variant() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_every_variant: no Vulkan device ({e})");
                return;
            }
        };

        let cases = [
            ("root", false, None, false),
            ("root", true, None, false),
            (
                "root",
                true,
                Some(Message::Error("Sorry, that didn’t work.".to_owned())),
                false,
            ),
            (
                "root",
                true,
                Some(Message::Info("Place your finger on the reader".to_owned())),
                true,
            ),
        ];

        for (user, entry, message, caps) in cases {
            let d = dialog(user, entry, message, caps);
            let l = layout(&d);
            let mut cache = BakeCache::new();
            let tex = widget::bake(
                &mut vk,
                &mut cache,
                1.,
                l.size,
                0,
                |r| prepare_card(r, 1., &d, &l),
                |frame, phys, prepared| {
                    paint_card(frame, phys, prepared, &l, 1., [0.2, 0.52, 0.89, 1.])
                },
            )
            .expect("dialog texture");

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

            // The card is opaque away from its rounded corners...
            let bg = at(size.w / 2, size.h - 3);
            assert_eq!(bg[3], 255, "the card must be opaque, got {bg:?}");
            // ...and the extreme corner is not, because it is rounded.
            assert_eq!(
                at(size.w - 1, size.h - 1)[3],
                0,
                "the card's corner should be transparent"
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

    /// The entry appears and disappears without moving the buttons out from under the pointer.
    ///
    /// GNOME hides the entry between conversations (`resetDialog`, `:339`), so this happens on
    /// every retry. It is the whole reason the caps and message rows are reserved rather than
    /// sized to their contents — without that, a message landing would move the buttons too,
    /// and the same test would pass while the dialog still jumped.
    #[test]
    fn the_message_never_moves_the_buttons() {
        let quiet = dialog("root", true, None, false);
        let noisy = dialog(
            "root",
            true,
            Some(Message::Error("Sorry, that didn’t work.".to_owned())),
            true,
        );
        assert_eq!(
            layout(&quiet).cancel,
            layout(&noisy).cancel,
            "a message must not move the buttons"
        );
        assert_eq!(layout(&quiet).size, layout(&noisy).size);
    }

    /// ...but losing the entry *does* shrink the dialog, which is what GNOME does too: the row is
    /// gone, not merely empty. Pinned so the reserved rows above are not quietly extended to it.
    #[test]
    fn losing_the_entry_shrinks_the_dialog() {
        let with = layout(&dialog("root", true, None, false));
        let without = layout(&dialog("root", false, None, false));
        assert!(
            without.size.h < with.size.h,
            "the entry's row should not be reserved: {} vs {}",
            without.size.h,
            with.size.h,
        );
    }

    /// Every control the hit-test can return is inside the card, and the two buttons do not
    /// overlap. A button drawn outside its own dialog is unclickable; two that overlap make the
    /// wrong one fire.
    #[test]
    fn the_controls_fit_and_do_not_overlap() {
        for entry in [false, true] {
            let l = layout(&dialog("root", entry, None, false));
            let card = Rectangle::from_size(l.size);
            for (name, rect) in [
                ("avatar", l.avatar),
                ("entry", l.entry),
                ("cancel", l.cancel),
                ("authenticate", l.authenticate),
                ("message", l.message),
            ] {
                assert!(
                    card.contains_rect(rect),
                    "{name} escapes the card: {rect:?} in {card:?}"
                );
            }
            assert!(
                l.cancel.loc.x + l.cancel.size.w <= l.authenticate.loc.x,
                "the buttons overlap"
            );
        }
    }

    /// The entry and the avatar are drawn *over* the card, not under it.
    ///
    /// The card is one opaque texture covering the whole dialog, and the first element pushed is
    /// the topmost one — so pushing the card first hides everything that sits on it. That is
    /// exactly what shipped: live, the dialog came up with its title, its buttons and a blank gap
    /// where the password entry should be, and typing produced no bullets, because the entry was
    /// being drawn behind the card every frame.
    ///
    /// This drives the real `render` rather than baking the card by hand, which is why the
    /// bake-level test above stayed green through all of it.
    #[test]
    fn the_entry_is_drawn_over_the_card() {
        use std::rc::Rc;

        use smithay::backend::renderer::element::Element;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping the_entry_is_drawn_over_the_card: no Vulkan device ({e})");
                return;
            }
        };

        let output = Output::new(
            "polkit-test".to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "synoik".to_owned(),
                model: "test".to_owned(),
                serial_number: "0".to_owned(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: Size::from((1920, 1080)),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );

        let d = dialog("root", true, None, false);
        let l = layout(&d);
        let origin = PolkitDialogUi::origin(output_size(&output), &l);

        let mut ui = PolkitDialogUi::new(
            crate::animation::Clock::with_time(Duration::ZERO),
            Rc::new(RefCell::new(synoik_config::Config::default())),
        );
        ui.show();
        ui.settle();

        let icons = IconCache::new("Adwaita");
        let images = ImageCache::new();
        let mut elements = Vec::new();
        ui.render(
            &mut vk,
            &output,
            &icons,
            &images,
            &d,
            [53, 132, 228],
            Duration::ZERO,
            &mut |el| elements.push(el),
        );

        let entry_at = (origin + l.entry.loc).to_physical_precise_round(1.);
        let card_at = origin.to_physical_precise_round(1.);
        let index = |at: Point<i32, Physical>| {
            elements
                .iter()
                .position(|el| el.geometry(1.0.into()).loc == at)
                .unwrap_or_else(|| panic!("no element drawn at {at:?}"))
        };

        assert!(
            index(entry_at) < index(card_at),
            "the entry must be pushed before the card, or it is drawn behind it"
        );
    }
}
