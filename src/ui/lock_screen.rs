// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The lock screen curtain — `UnlockDialog`'s clock page (`js/ui/unlockDialog.js:357-428`).
//!
//! This is what the shield shows before you ask to unlock: the wallpaper, dimmed, with a big clock
//! over it and a hint that fades in once you have been idle a moment. GNOME keeps it in
//! `unlockDialog.js` rather than `screenShield.js` — it is the `_stack`'s clock page, with the
//! password prompt as the *other* page — but it is the shield's whole appearance, so it lives on
//! its own here and the prompt joins it in slice 3 (`docs/fork/lock-screen-port.md`).
//!
//! Child order comes from the JS, not the SCSS (`:381-383`): `_time`, `_date`, `_hint`, each
//! `x_align: CENTER`, the hint starting at `opacity: 0`.
//!
//! Style, from `.unlock-dialog-clock` (`_login-lock.scss:238-259`): a vertical box with `2em`
//! spacing in `$system_fg_color`, holding
//!
//! - `.unlock-dialog-clock-time` — `%numeric` at **72pt**, weight 800.
//! - `.unlock-dialog-clock-date` — `%title_1` (20pt) but overridden back to weight **400**.
//! - `.unlock-dialog-clock-hint` — bold, `margin-top: 2em`, padding `$base_padding
//!   $base_padding*3`. It also carries a `border-radius` but **no `background-color`**, so nothing
//!   is drawn behind it and the radius is inert; see [`HINT_PAD_V`].
//!
//! Our rasterizer tops out at bold (700) where GNOME's `%title_1` asks for 800 — the standing
//! divergence recorded in `gnome-style-reference.md`, not a local decision.

use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::Curve;
use crate::image_source::ImageSource;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, style, HAlign, Painter, Rgba, ShapedText, TextShaper, TextStyle};

/// `.unlock-dialog-clock-time` — `@include fontsize(72pt)`.
const TIME_PT: f64 = 72.;
/// `.unlock-dialog-clock-date` — `%title_1`, whose 20pt survives the weight override.
const DATE_PT: f64 = 20.;
/// The hint inherits the shell's body size; the theme sets only its weight and spacing.
const HINT_PT: f64 = 11.;

/// `.unlock-dialog-clock`'s `spacing: 2em`. The box sets no font size of its own, so `em` here is
/// the shell's base font.
const SPACING_EM: f64 = 2.;

/// `$system_fg_color` — `$_base_color_light`, i.e. white.
const FG: Rgba = [1., 1., 1., 1.];

/// `$base_padding` / `$base_padding * 3` (`_common.scss`).
///
/// `.unlock-dialog-clock-hint` sets padding and a `border-radius` but **no `background-color`**
/// (`_login-lock.scss:253-258`), so nothing is drawn behind the text and the radius has no visible
/// effect. The padding is kept because it is real layout: it is what separates the hint from the
/// date beyond the box's spacing. Do not "restore" a pill here — the shipped theme has none, and a
/// tinted plate under white text over an arbitrary wallpaper is exactly the kind of invented chrome
/// that reads as ours rather than GNOME's.
///
/// (The `build/` directory in the reference checkout holds a *2021* compiled `gnome-shell.css` that
/// disagrees — 16pt date, normal-weight hint. It is stale by four years; the SCSS is the source.)
const HINT_PAD_V: f64 = 6.;
const HINT_PAD_H: f64 = 18.;

/// `HINT_TIMEOUT = 4` seconds (`unlockDialog.js:28`).
pub const HINT_IDLE: Duration = Duration::from_secs(4);
/// `CROSSFADE_TIME` (`:30`). The `ease` names no mode, so it runs at Clutter's actor default,
/// `EASE_OUT_CUBIC` (`:396-401`).
pub const HINT_FADE: Duration = Duration::from_millis(300);

/// `BLUR_BRIGHTNESS` (`:34`) — the wallpaper behind the curtain is multiplied by this.
///
/// It is not decoration: it is what makes white 72pt text legible over an arbitrary picture. The
/// multiply rides inside the blur pass rather than being a separate wash, as GNOME's does.
pub const BLUR_BRIGHTNESS: f64 = 0.65;

/// `BLUR_RADIUS = 90` (`:35`), in **stage pixels** — so it scales with the output, and a caller
/// converts it into whatever resolution the texture it blurs happens to be.
pub const BLUR_RADIUS: f64 = 90.;

/// What the curtain shows. Plain strings, so the caller owns formatting and this is testable
/// without a wall clock.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClockContent {
    /// `WallClock` with `time_only`, trimmed (`_updateClock`, `:409`).
    pub time: String,
    /// `%A %B %-d` — "Friday August 1" (`:411-415`).
    pub date: String,
    /// "Click or press a key to unlock", or the touch wording (`_updateHint`, `:420-425`).
    pub hint: String,
}

impl ClockContent {
    /// Build the curtain's text for `epoch` (local time).
    ///
    /// The time comes from the same interface keys the panel clock uses, with the date and weekday
    /// forced off: GNOME's clock here is a `WallClock` constructed `{time_only: true}` (`:384`),
    /// which is exactly that. `clock-show-seconds` is *not* forced off — a user who asked for
    /// seconds gets them on the lock screen too, as in GNOME.
    pub fn new(epoch: libc::time_t, clock: crate::gnome::ClockFormat, touch_mode: bool) -> Self {
        let time_only = crate::gnome::ClockFormat {
            show_weekday: false,
            show_date: false,
            ..clock
        };
        Self {
            time: super::panel::strftime_local(epoch, super::panel::strftime_format(time_only)),
            // `Shell.util_translate_time_string(N_('%A %B %-d'))` (`:411-415`).
            date: super::panel::strftime_local(epoch, "%A %B %-d"),
            hint: if touch_mode {
                "Swipe up to unlock".to_owned()
            } else {
                "Click or press a key to unlock".to_owned()
            },
        }
    }
}

/// `STANDARD_FADE_TIME` (`screenShield.js:39`) — the idle fade to black, before the shield.
///
/// The shield's lightboxes are built with `fadeFactor: 1` (`:130-137`), so this fades all the way
/// to opaque, not to GNOME's usual 0.4 dimming.
pub const FADE_TIME: Duration = Duration::from_secs(10);

/// `Overview.ANIMATION_TIME` (`js/ui/overview.js:12`) — how long the shield takes to slide.
///
/// The curtain comes down from above and leaves the same way: `_lockDialogGroup.translation_y`
/// eased between `-screen_height` and 0 (`_resetLockScreen` `:452-462`, `_continueDeactivate`
/// `:551-556`), `EASE_OUT_QUAD` both ways.
pub const SLIDE_TIME: Duration = Duration::from_millis(250);

/// `CROSSFADE_TIME` (`unlockDialog.js:30`) — how long the clock↔prompt swap takes.
pub const CROSSFADE_TIME: Duration = Duration::from_millis(300);
/// `FADE_OUT_TRANSLATION` (`:31`) — how far the outgoing page slides, in logical px.
const FADE_OUT_TRANSLATION: f64 = 200.;
/// `FADE_OUT_SCALE` (`:32`) — how small a page is when fully faded out.
const FADE_OUT_SCALE: f64 = 0.3;

/// How one page is drawn part-way through the crossfade (`_setTransitionProgress`, `:815-843`).
///
/// The two pages move in opposite directions: at `progress = 0` the clock is at rest and the prompt
/// is small, transparent and *below* it; at `1` they have swapped, the clock having shrunk and
/// risen. So this is not a plain dissolve — the pair reads as one page giving way to the other.
///
/// Scaling is about the page's **centre**: both actors set `pivot_point(0.5, 0.5)` (`:599`,
/// `:604`), without which a shrinking page would slide toward its own top-left corner instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageTransform {
    pub alpha: f64,
    pub scale: f64,
    pub translation_y: f64,
}

impl PageTransform {
    /// The identity — a page at rest, which is what every non-animating caller wants.
    pub const REST: Self = Self {
        alpha: 1.,
        scale: 1.,
        translation_y: 0.,
    };

    /// The clock at transition progress `p` (0 = clock showing, 1 = prompt showing).
    pub fn clock(p: f64) -> Self {
        Self {
            alpha: 1. - p,
            scale: FADE_OUT_SCALE + (1. - FADE_OUT_SCALE) * (1. - p),
            translation_y: -FADE_OUT_TRANSLATION * p,
        }
    }

    /// The prompt at transition progress `p`.
    pub fn prompt(p: f64) -> Self {
        Self {
            alpha: p,
            scale: FADE_OUT_SCALE + (1. - FADE_OUT_SCALE) * p,
            translation_y: FADE_OUT_TRANSLATION * (1. - p),
        }
    }

    /// Whether this page contributes anything — `visible = progress > 0` (`:816-817`).
    pub fn is_visible(&self) -> bool {
        self.alpha > 0.
    }

    /// Where a point of the page lands, scaled about `centre` and then translated.
    fn place(&self, p: Point<f64, Logical>, centre: Point<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            centre.x + (p.x - centre.x) * self.scale,
            centre.y + (p.y - centre.y) * self.scale + self.translation_y,
        ))
    }

    /// The buffer scale that draws a texture baked at `scale` physical px per logical px at this
    /// transform's size. Changing the *buffer* scale rather than re-baking is what keeps a 300 ms
    /// crossfade from re-rasterizing the page every frame ([[animation-per-frame-bake]]).
    ///
    /// Icons take the same route, through [`widget::icon_element_scaled`]. They used to ask the
    /// cache for a bucketed *size* instead, which bounded the number of keys but made the first
    /// run of the crossfade miss a cold key per bucket — and a cold key draws nothing, so the
    /// avatar blinked its way in. Bucketing turned one cold miss into twelve.
    fn buffer_scale(&self, scale: f64) -> f64 {
        scale / self.scale
    }
}

/// Where each line of the curtain lands, relative to the clock block's own top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockLayout {
    pub time: Rectangle<f64, Logical>,
    pub date: Rectangle<f64, Logical>,
    pub hint: Rectangle<f64, Logical>,
    /// The hint's padding box — its text plus `$base_padding` / `$base_padding * 3`. Nothing is
    /// drawn in it (the theme sets no background); it is the clip and the block's bottom edge.
    pub hint_box: Rectangle<f64, Logical>,
}

/// The height of each of the curtain's three text rows, in logical px.
///
/// These are **line boxes**, not point sizes. A row sized by its point size is shorter than the
/// font's ascent+descent, so descenders are clipped and nothing else looks wrong — the `g` of
/// "August" loses its tail. See [`widget::ShapedText::line_box_height`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockRows {
    pub time: f64,
    pub date: f64,
    pub hint: f64,
}

impl ClockRows {
    /// The stand-in used before anything is shaped, so [`block_height`] can size the bake without
    /// a font (see [`LINE_BOX_ESTIMATE`]). It over-estimates, which costs transparent pixels at the
    /// bottom of the bake and never clips.
    pub fn estimated() -> Self {
        Self {
            time: crate::ui::pt_to_px(TIME_PT) * LINE_BOX_ESTIMATE,
            date: crate::ui::pt_to_px(DATE_PT) * LINE_BOX_ESTIMATE,
            hint: crate::ui::pt_to_px(HINT_PT) * LINE_BOX_ESTIMATE,
        }
    }
}

/// The block's height for a given set of row heights.
pub fn block_height_of(rows: ClockRows, base_px: f64) -> f64 {
    let spacing = SPACING_EM * base_px;
    // The hint carries `margin-top: 2em` *on top of* the box's own `spacing: 2em`
    // (`_login-lock.scss:240,254`), so it sits twice as far below the date as the date does below
    // the time. Folding the two into one gap is the easy misreading and it reads as a cramped hint.
    rows.time + spacing + rows.date + spacing * 2. + rows.hint + HINT_PAD_V * 2.
}

/// The block's height as the bake must be sized — before the text exists, so on estimates.
pub fn block_height(base_px: f64) -> f64 {
    block_height_of(ClockRows::estimated(), base_px)
}

/// Lay the three lines out inside a block of `width`, centred horizontally. `hint_w` is the shaped
/// width of the hint text, which only its padding box needs.
pub fn layout(width: f64, hint_w: f64, base_px: f64, rows: ClockRows) -> ClockLayout {
    let ClockRows {
        time: time_h,
        date: date_h,
        hint: hint_h,
    } = rows;
    let spacing = SPACING_EM * base_px;
    let cx = width / 2.;

    let line =
        |y: f64, h: f64, w: f64| Rectangle::new(Point::from((cx - w / 2., y)), Size::from((w, h)));

    let time = line(0., time_h, width);
    let date = line(time_h + spacing, date_h, width);
    let pill_y = date.loc.y + date_h + spacing * 2.;
    ClockLayout {
        time,
        date,
        hint: line(pill_y + HINT_PAD_V, hint_h, hint_w),
        hint_box: line(pill_y, hint_h + HINT_PAD_V * 2., hint_w + HINT_PAD_H * 2.),
    }
}

/// Where the page stack's top edge sits (`UnlockDialogLayout.vfunc_allocate`, `:478-487`):
/// `min(height / 3, height - stackHeight)`.
///
/// **Not centred.** Both pages share this box — that is what makes the clock↔prompt crossfade
/// happen in place rather than sliding — and the box is anchored a third of the way down, so the
/// clock sits above centre. Our slice 2 centred it; this is the correction, cited.
///
/// The `min` is the degenerate-monitor guard: on a screen too short to hold the block, the top
/// clamps up rather than pushing the entry off the bottom.
pub fn stack_top(monitor: Rectangle<f64, Logical>, block_h: f64) -> f64 {
    monitor.loc.y + (monitor.size.h / 3.).min((monitor.size.h - block_h).max(0.))
}

/// `$_gdm_dialog_width: 25em` on `.login-dialog-prompt-layout` (`_login-lock.scss:16-18`).
const PROMPT_EM: f64 = 25.;
/// That layout's `spacing: $base_padding * 1.5`.
const PROMPT_SPACING: f64 = 9.;
/// `.user-widget.vertical` `spacing: $base_padding * 4` (`:378`).
const USER_SPACING: f64 = 24.;
/// `.user-icon` at `icon-size: $base_icon_size * 10` in the vertical widget (`:388`).
pub const AVATAR_PX: f64 = 160.;
/// Its `StIcon { padding: $base_padding * 5 }` (`:391`) — the inset of the fallback glyph.
const AVATAR_ICON_PAD: f64 = 30.;
/// `.login-dialog-button.switch-user-button` — an `.icon-button` whose `.icon-button` padding is
/// overridden to `to_em(16px)` (`_login-lock.scss:39-45`). `to_em` divides by a 16px reference and
/// multiplies by 1.091, which is exactly `16/11pt`, so it reads as 16px at the default font and
/// scales with the user's.
const SWITCH_PAD_PX: f64 = 16.;
/// `system-users-symbolic` (`unlockDialog.js:626`).
const SWITCH_ICON: &str = "system-users-symbolic";

/// The upload slot the account picture lives in. There is only ever one avatar on this page.
const AVATAR_SLOT: u64 = 0;
/// `.user-icon`'s fill under the lock screen: `transparentize($_gdm_fg, .87)` (`:356`).
const AVATAR_BG: Rgba = [1., 1., 1., 0.13];
/// `.user-widget.vertical .user-widget-label` — 20pt, weight 400 (`:380-385`).
const NAME_PT: f64 = 20.;
/// ...and its `margin-bottom: .75em`, where `em` is the label's own 20pt.
const NAME_MARGIN_BOTTOM_EM: f64 = 0.75;
/// `.login-dialog-message` `min-height: 2.75em` (`:92-95`), `em` at the base font.
const MESSAGE_MIN_EM: f64 = 2.75;
/// The message keeps clear of the column's edges so a wrapped line is not flush against them.
const MESSAGE_PAD: f64 = 9.;
/// `.login-dialog-message` `color: darken($_gdm_fg, 10%)`.
const MESSAGE_FG: Rgba = [0.9, 0.9, 0.9, 1.];
/// A `Problem`/failure reads in the error colour rather than the muted one; GNOME distinguishes
/// them by `MessageType` (`authPrompt.js`).
const MESSAGE_ERROR_FG: Rgba = [1., 0.48, 0.42, 1.];

/// `wiggle` (`js/misc/animationUtils.js:87-124`), with the arguments `authPrompt.js:489` passes:
/// The shake, from the toolkit ([`widget::Wiggle`]). Only a fingerprint `ERROR` gets one
/// (`authPrompt.js:485-490`).
pub const WIGGLE_TIME: Duration = widget::Wiggle::TIME;

/// `CapsLockWarning`'s text (`shellEntry.js:170`).
pub const CAPS_TEXT: &str = "Caps lock is on";
/// It eases in and out over 200 ms (`shellEntry.js:210-217`).
pub const CAPS_FADE: Duration = Duration::from_millis(200);
/// `.caps-lock-warning-label { color: $_gdm_fg }` (`_login-lock.scss:10-13`) — the dialog's own
/// foreground, not a warning red. It is a statement of fact, not an error.
const CAPS_FG: Rgba = FG;

/// What the prompt page shows. The entry text arrives **already masked** — this type never sees a
/// password, which is the point (see [`crate::unlock_dialog::UnlockDialog::entry_display`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContent {
    pub display_name: String,
    /// Masked or not, per the dialog. Never the raw secret.
    pub entry: String,
    /// The caret's byte offset **into `entry`** — already mapped through the mask by
    /// [`TextEdit::masked_positions`](crate::ui::text_edit::TextEdit::masked_positions), so
    /// this stays true to the "the view never sees the password" rule that makes `entry` a
    /// masked `String` in the first place.
    pub cursor: Option<usize>,
    /// The selection, in the same masked coordinates as `cursor`.
    pub selection: Option<std::ops::Range<usize>>,
    /// gdm's prompt, shown as the entry's placeholder when nothing is typed.
    pub question: String,
    pub message: Option<String>,
    pub message_is_error: bool,
    /// Whether gdm is waiting for input — drives the entry's focus ring.
    pub entry_live: bool,
    /// The peek toggle's state, or `None` when there is no toggle to draw —
    /// `org.gnome.desktop.lockdown disable-show-password`, or a non-secret prompt
    /// (`st-password-entry.c:174-184`). `Some(true)` means the password is currently visible, so
    /// the glyph is `view-conceal-symbolic`.
    pub peek: Option<bool>,
    /// The account's picture, if AccountsService gave us one that is still on disk
    /// (`UserAccount::icon_file`). `None` draws the themed `avatar-default-symbolic`, which is
    /// also what shows while the decode is still in flight.
    pub avatar: Option<ImageSource>,
}

/// Where a page is being drawn, and when.
///
/// The two pages take exactly these three between them, and both are drawn from the same place in
/// the same frame — so they travel together rather than as three parallel parameters each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageCtx {
    pub scale: f64,
    pub monitor: Rectangle<f64, Logical>,
    /// Monotonic; the hint fade, the caps ease and the crossfade all read it.
    pub now: Duration,
}

/// Where the prompt page's parts sit, relative to the block's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromptLayout {
    pub avatar: Rectangle<f64, Logical>,
    pub name: Rectangle<f64, Logical>,
    pub entry: Rectangle<f64, Logical>,
    /// The caps-lock warning's row, between the entry and the message
    /// (`authPrompt.js:199-212` — `_initInputRow`, then the placeholder, then the warning, then
    /// `_message`, all in the vertical `_inputWell`).
    pub caps: Rectangle<f64, Logical>,
    pub message: Rectangle<f64, Logical>,
}

/// The caps row's height. Always reserved — see [`prompt_layout`].
fn caps_row_height(base_px: f64) -> f64 {
    base_px * LINE_BOX_ESTIMATE
}

/// The prompt column's width — `25em` at the base font.
pub fn prompt_width(base_px: f64) -> f64 {
    PROMPT_EM * base_px
}

/// A text row must be as tall as its font's **line box**, not its point size, or descenders are
/// clipped — a 20pt row is ~26.7px where the line box is nearer 32, and `g`/`y`/`p` lose their
/// tails while everything else looks right. Callers pass the measured heights from
/// [`widget::ShapedText::line_box_height`]; [`prompt_block_height`] uses this factor to size the
/// bake before anything is shaped, generously, since a too-tall transparent texture costs nothing.
const LINE_BOX_ESTIMATE: f64 = 1.6;

pub fn prompt_layout(base_px: f64, name_h: f64, message_h: f64) -> PromptLayout {
    let width = prompt_width(base_px);
    // GNOME's message reserves `min-height: 2.75em` whether or not there is text in it, so the
    // entry does not jump when an error appears.
    let message_h = message_h.max(MESSAGE_MIN_EM * base_px);

    let mut y = 0.;
    let centred = |y: f64, h: f64, w: f64| {
        Rectangle::new(Point::from(((width - w) / 2., y)), Size::from((w, h)))
    };

    let avatar = centred(y, AVATAR_PX, AVATAR_PX);
    y += AVATAR_PX + USER_SPACING;
    let name = centred(y, name_h, width);
    // The label's own `margin-bottom` sits *inside* the user widget, so it adds to the prompt
    // layout's spacing rather than replacing it.
    y += name_h + NAME_MARGIN_BOTTOM_EM * crate::ui::pt_to_px(NAME_PT) + PROMPT_SPACING;
    let entry = centred(y, widget::Entry::HEIGHT, width);
    y += widget::Entry::HEIGHT + PROMPT_SPACING;
    // **Divergence: the row is always reserved.** GNOME eases the warning's *height* from 0
    // (`shellEntry.js:210-217`), so the dialog grows when caps comes on, and an empty placeholder
    // label holds the line for non-secret questions instead (`authPrompt.js:201-212`). Animating a
    // height here would move the name and message every frame, which means re-rasterising the whole
    // column every frame — the [[animation-per-frame-bake]] shape. Reserving the line costs one
    // blank row under a password entry with caps off, and buys a dialog whose height never moves;
    // it is also exactly the space GNOME's own placeholder reserves on a non-secret prompt.
    let caps = centred(y, caps_row_height(base_px), width);
    y += caps.size.h + PROMPT_SPACING;
    let message = centred(y, message_h, width);

    PromptLayout {
        avatar,
        name,
        entry,
        caps,
        message,
    }
}

/// Hit-test a point on the prompt page against the entry, in output-global coordinates.
///
/// Lives here because the geometry does: the caller knows where the pointer is, not where the
/// entry ended up.
pub fn peek_hit(
    monitor: Rectangle<f64, Logical>,
    pos: Point<f64, Logical>,
) -> Option<widget::EntryHit> {
    let base_px = crate::ui::pt_to_px(crate::ui::base_font_pt());
    let width = prompt_width(base_px);
    let block_h = prompt_block_height(base_px);
    let l = prompt_layout(
        base_px,
        crate::ui::pt_to_px(NAME_PT) * LINE_BOX_ESTIMATE,
        MESSAGE_MIN_EM * base_px * 2.,
    );
    let origin = Point::<f64, Logical>::from((
        monitor.loc.x + (monitor.size.w - width) / 2.,
        stack_top(monitor, block_h),
    ));
    let entry = widget::Entry::layout(
        origin.x + l.entry.loc.x + l.entry.size.w / 2.,
        origin.y + l.entry.loc.y,
        l.entry.size.w,
        l.entry.size.h,
        widget::EntryStyle::Lockscreen,
    );
    widget::Entry::hit(&entry, pos, true)
}

/// The "Log in as another user" button's box, in output-global coordinates.
///
/// `UnlockDialogLayout.allocate` (`unlockDialog.js:491-506`) puts it a **button-width in from the
/// right edge and a button-height up from the bottom** — `x1 = box.x2 - natWidth * 2`,
/// `y1 = box.y2 - natHeight * 2` — so the inset is its own size rather than a padding constant.
/// RTL mirrors it to `box.x1 + natWidth`, which we do not do yet: nothing else in this port is
/// direction-aware, and a half-mirrored lock screen is worse than a consistently LTR one.
pub fn switch_user_rect(monitor: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let size = switch_user_size();
    Rectangle::new(
        Point::from((
            monitor.loc.x + monitor.size.w - size * 2.,
            monitor.loc.y + monitor.size.h - size * 2.,
        )),
        Size::from((size, size)),
    )
}

/// The button's diameter: the glyph plus its padding on both sides.
pub fn switch_user_size() -> f64 {
    widget::IconButton::diameter(
        crate::ui::scaled_px(widget::IconButton::ICON_PX),
        crate::ui::scaled_px(SWITCH_PAD_PX),
    )
}

/// Whether `pos` is on the button — **round**, see [`widget::IconButton::contains`].
pub fn switch_user_hit(monitor: Rectangle<f64, Logical>, pos: Point<f64, Logical>) -> bool {
    let rect = switch_user_rect(monitor);
    widget::IconButton::new(rect, 0., style::TRANSPARENT).contains(pos)
}

/// The prompt block's total height, sized before anything is shaped (see [`LINE_BOX_ESTIMATE`]).
pub fn prompt_block_height(base_px: f64) -> f64 {
    let l = prompt_layout(
        base_px,
        crate::ui::pt_to_px(NAME_PT) * LINE_BOX_ESTIMATE,
        MESSAGE_MIN_EM * base_px * 2.,
    );
    l.message.loc.y + l.message.size.h
}

/// Where the curtain is, and which way it is going.
///
/// The shield keeps drawing after it stops being *active*, which is the whole reason this is a
/// state machine and not a boolean: the model raises the shield the instant it is unlocked, and the
/// curtain still owes 250 ms of sliding away. Rendering only while `active` would make unlocking a
/// hard cut and locking an appearance out of nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Curtain {
    /// Off the top of the screen, drawing nothing.
    #[default]
    Hidden,
    Showing(Duration),
    Covering,
    Hiding(Duration),
}

impl Curtain {
    /// Where a slide in flight ends up. Retiring the state matters because it is what a *later*
    /// `set_shown` reads to decide whether the curtain is already on its way; a `Hiding` that never
    /// becomes `Hidden` leaves the next lock parked off-screen.
    fn settled(self) -> Self {
        match self {
            Curtain::Showing(_) => Curtain::Covering,
            Curtain::Hiding(_) => Curtain::Hidden,
            settled => settled,
        }
    }
}

/// The curtain's on-screen state: when it went down, its slide, and its bakes.
#[derive(Default)]
pub struct LockScreen {
    /// Monotonic instant the shield went down, or of the last input while it was down — the hint's
    /// fade hangs off an **idle** watch on the core idle monitor (`:395`), not off activation.
    idle_since: Option<Duration>,
    curtain: Curtain,
    /// When the long fade to black started, if it is running (`_longLightbox`).
    fade_since: Option<Duration>,
    /// Which page is being eased *to*, and when the ease started. `None` means settled.
    ///
    /// The page a transition ends on comes from the model ([`crate::unlock_dialog::Page`]); this
    /// is only the view's clock for it, so the crossfade stays a drawing concern and the model
    /// keeps no notion of time.
    page_since: Option<(Duration, bool)>,
    /// The page as of the last [`Self::set_page`] — what a new transition eases *from*.
    showing_prompt: bool,
    /// Whether the caps-lock warning should be up, and when that last changed.
    ///
    /// `None` means settled at [`Self::caps_warning`]. Kept here rather than in
    /// [`PromptContent`] because it is a *clock*: the content says what to draw, this says how far
    /// through the 200 ms ease it is.
    caps_since: Option<Duration>,
    caps_warning: bool,
    /// The alpha the running ease started from.
    ///
    /// Not always the opposite extreme: double-tap Caps Lock inside 200 ms and the second ease
    /// begins wherever the first had got to. Deriving the start from the *target* instead makes a
    /// reversal jump to full opacity before fading — a flash of a warning that was never up.
    caps_from: f64,
    /// The message's shake.
    wiggle: widget::Wiggle,
    cache: RefCell<widget::BakeCache>,
    /// The entry has its own cache: it re-bakes per keystroke, the column around it does not.
    entry_cache: RefCell<widget::BakeCache>,
    /// ...and so does the caps warning, whose text never changes at all: keeping it out of the
    /// column's bake is what lets its alpha animate on the *element* instead of in the bake key,
    /// which would re-rasterise the whole column every frame ([[animation-per-frame-bake]]).
    caps_cache: RefCell<widget::BakeCache>,
    /// ...and so does the message, for the same reason one step along: its wiggle is a
    /// **translation**, and a translation folded into the bake key re-rasterises the column on
    /// every one of the 390 ms of frames ([[animation-per-frame-bake]]). Out here it is an element
    /// offset and costs nothing. It also matches GNOME's actor split — `wiggle` moves
    /// `this._message` alone, not the dialog around it.
    message_cache: RefCell<widget::BakeCache>,
    /// The avatar's inset ring — constant content, so one bake for the life of the process, and
    /// only drawn when there is a picture under it (see [`widget::Avatar`]).
    ring_cache: RefCell<widget::BakeCache>,
    /// The uploaded avatar picture. One slot, pruned to the current source on each draw: an avatar
    /// the user has replaced is a texture nothing will ask for again.
    avatar_uploads: RefCell<widget::ImageUploads>,
    /// The switch-user button's circle. Its own cache because its revision moves on *hover*, which
    /// nothing else on the page cares about.
    switch_cache: RefCell<widget::BakeCache>,
}

/// One drawable of the prompt page.
///
/// The page is nearly all baked chrome, but the avatar is a *photo clipped to a circle*, which is
/// the rounded-texture pipeline rather than a bake — so the page's elements are no longer all one
/// type. Both variants already have a home in the output's element enum.
#[derive(Debug)]
pub enum PromptElement {
    Texture(TextureRenderElement<VkTexture>),
    Rounded(crate::render_helpers::rounded_texture::RoundedTextureRenderElement<VkTexture>),
}

impl From<TextureRenderElement<VkTexture>> for PromptElement {
    fn from(e: TextureRenderElement<VkTexture>) -> Self {
        Self::Texture(e)
    }
}

impl LockScreen {
    /// Drop the uploaded account picture.
    ///
    /// For when the *bytes* behind the path changed: everything here is keyed by path, so nothing
    /// else would notice. See [`crate::dbus::accounts_service::UserAccount::icon_stamp`].
    pub fn forget_avatar(&mut self) {
        self.avatar_uploads.borrow_mut().clear();
    }

    /// The shield went down (`true`) or came back up.
    ///
    /// Takes `now` unconditionally rather than folding it into an `Option`, because *both*
    /// directions start an animation — the old shape could say when the curtain started coming
    /// down but not when it started going away.
    pub fn set_shown(&mut self, shown: bool, now: Duration) {
        self.idle_since = shown.then_some(now);
        self.curtain = match (shown, self.curtain) {
            // Already there or on the way: let it finish rather than restarting it.
            (true, Curtain::Showing(_) | Curtain::Covering) => self.curtain,
            (true, _) => Curtain::Showing(now),
            (false, Curtain::Hidden | Curtain::Hiding(_)) => self.curtain,
            (false, _) => Curtain::Hiding(now),
        };
        if !shown {
            self.fade_since = None;
        }
    }

    /// Retire the curtain into its resting state, resetting the page once it is fully gone.
    ///
    /// The page reset has to wait for the *end* of the slide. Doing it when the shield stops being
    /// active — which is where it used to live — flips the curtain back to the clock for the whole
    /// 250 ms it spends sliding away, so unlocking snaps the prompt out from under itself. GNOME
    /// never calls `_showClock` on a successful unlock at all: the group slides off still showing
    /// the prompt you authenticated with. But the reset does have to happen eventually, or the next
    /// lock opens on the tail of the crossfade this one was in the middle of.
    fn retire_curtain(&mut self) {
        self.curtain = self.curtain.settled();
        if self.curtain == Curtain::Hidden {
            self.showing_prompt = false;
            self.settle_page();
        }
    }

    /// Start the fade to black over the desktop (`lightOn(STANDARD_FADE_TIME)`).
    pub fn light_on(&mut self, now: Duration) {
        if self.fade_since.is_none() {
            self.fade_since = Some(now);
        }
    }

    /// Drop it at once — `lightOff()` with no duration is instant (`lightbox.js:223-227`).
    pub fn light_off(&mut self) {
        self.fade_since = None;
    }

    /// How black the desktop is on its way to being covered: 0 to 1, `EASE_OUT_QUAD`.
    pub fn fade_alpha(&self, now: Duration) -> f64 {
        let Some(since) = self.fade_since else {
            return 0.;
        };
        let t = (now.saturating_sub(since).as_secs_f64() / FADE_TIME.as_secs_f64()).clamp(0., 1.);
        Curve::EaseOutQuad.y(t)
    }

    /// Whether the fade still owes frames.
    pub fn is_fading(&self, now: Duration) -> bool {
        self.fade_since
            .is_some_and(|since| now.saturating_sub(since) < FADE_TIME)
    }

    /// How far the curtain is off the top of the screen: `0` fully down, `1` fully away.
    ///
    /// Multiply by the monitor height for GNOME's `translation_y` (negated — the shield leaves
    /// upward).
    pub fn curtain_progress(&self, now: Duration) -> f64 {
        let eased = |since: Duration| {
            let t =
                (now.saturating_sub(since).as_secs_f64() / SLIDE_TIME.as_secs_f64()).clamp(0., 1.);
            Curve::EaseOutQuad.y(t)
        };
        match self.curtain {
            Curtain::Hidden => 1.,
            Curtain::Covering => 0.,
            Curtain::Showing(since) => 1. - eased(since),
            Curtain::Hiding(since) => eased(since),
        }
    }

    /// Whether the curtain is on screen at all — the render path's gate.
    ///
    /// True through the whole slide away, which is *after* the shield stops being active. Gating
    /// the draw on the model's `active` instead is what makes unlocking a hard cut.
    pub fn is_covering(&self, now: Duration) -> bool {
        match self.curtain {
            Curtain::Hidden => false,
            // Read off the *state*, not `curtain_progress`: at the exact instant a descent starts
            // the progress is still 1 (fully off-screen), and a frame landing there would skip the
            // curtain entirely — which is a locked session drawing the desktop for one frame.
            Curtain::Showing(_) | Curtain::Covering => true,
            Curtain::Hiding(since) => now.saturating_sub(since) < SLIDE_TIME,
        }
    }

    /// Whether the slide still owes frames.
    pub fn is_sliding(&self, now: Duration) -> bool {
        match self.curtain {
            Curtain::Hidden | Curtain::Covering => false,
            Curtain::Showing(since) | Curtain::Hiding(since) => {
                now.saturating_sub(since) < SLIDE_TIME
            }
        }
    }

    /// The model moved to a page. Starts the crossfade, or does nothing if we are already going
    /// there.
    pub fn set_page(&mut self, prompt: bool, now: Duration) {
        if self.showing_prompt == prompt {
            return;
        }
        self.showing_prompt = prompt;
        self.page_since = Some((now, prompt));
    }

    /// Raise or drop the caps-lock warning, starting its 200 ms ease.
    ///
    /// GNOME watches the keymap's `state-changed` (`shellEntry.js:175-188`) so the warning tracks
    /// caps lock even with no keystroke in the entry. We have no keymap signal: the state rides in
    /// on the key event that changed it, which is why the shield's key path must call this for
    /// *modifier* keys too — Caps Lock itself being the one that matters most.
    /// Returns whether this changed anything, so the caller knows to ask for a frame.
    pub fn set_caps_warning(&mut self, warn: bool, now: Duration) -> bool {
        if self.caps_warning == warn {
            return false;
        }
        // Sampled *before* the target flips, so a reversal continues from where it is.
        self.caps_from = self.caps_alpha(now);
        self.caps_warning = warn;
        self.caps_since = Some(now);
        true
    }

    /// How opaque the caps-lock warning is: 0 hidden, 1 fully up.
    pub fn caps_alpha(&self, now: Duration) -> f64 {
        let target = if self.caps_warning { 1. } else { 0. };
        let Some(since) = self.caps_since else {
            return target;
        };
        let t = (now.saturating_sub(since).as_secs_f64() / CAPS_FADE.as_secs_f64()).clamp(0., 1.);
        let eased = Curve::EaseOutQuad.y(t);
        self.caps_from + (target - self.caps_from) * eased
    }

    /// Whether the caps warning still owes frames.
    pub fn caps_is_animating(&self, now: Duration) -> bool {
        self.caps_since
            .is_some_and(|since| now.saturating_sub(since) < CAPS_FADE)
    }

    /// Shake the message (`wiggle`, `animationUtils.js:87-124`). Restarts one already running.
    ///
    /// GNOME does this for a fingerprint **error** only (`authPrompt.js:485-490`) — the reader is
    /// not the service the user is looking at, so its bad news has to catch the eye of somebody
    /// whose attention is on the entry. A refused *password* does not wiggle: they are already
    /// looking at the thing that refused them.
    pub fn start_wiggle(&mut self, now: Duration) {
        self.wiggle.start(now);
    }

    /// How far the message is displaced, in logical pixels. 0 at rest.
    pub fn wiggle_offset(&self, now: Duration) -> f64 {
        self.wiggle.offset(now)
    }

    /// Whether the wiggle still owes frames.
    pub fn wiggle_is_animating(&self, now: Duration) -> bool {
        self.wiggle.is_animating(now)
    }

    /// Put the message back where it belongs, immediately.
    pub fn settle_wiggle(&mut self) {
        self.wiggle.settle();
    }

    /// Finish the caps ease now, wherever it had got to.
    pub fn settle_caps(&mut self) {
        self.caps_since = None;
        self.caps_from = if self.caps_warning { 1. } else { 0. };
    }

    /// How far through the clock→prompt crossfade we are: 0 is the clock, 1 the prompt.
    ///
    /// `EASE_OUT_QUAD` over [`CROSSFADE_TIME`] (`_showPrompt` / `_showClock`, `:786-810`).
    pub fn page_progress(&self, now: Duration) -> f64 {
        let Some((since, to_prompt)) = self.page_since else {
            return if self.showing_prompt { 1. } else { 0. };
        };
        let t =
            (now.saturating_sub(since).as_secs_f64() / CROSSFADE_TIME.as_secs_f64()).clamp(0., 1.);
        let eased = Curve::EaseOutQuad.y(t);
        if to_prompt {
            eased
        } else {
            1. - eased
        }
    }

    /// Whether the crossfade still owes frames.
    pub fn page_is_animating(&self, now: Duration) -> bool {
        self.page_since
            .is_some_and(|(since, _)| now.saturating_sub(since) < CROSSFADE_TIME)
    }

    /// Retire a finished slide, so the state stops being a function of the clock.
    ///
    /// Called from the render path once per frame. Without it `Hiding` stays `Hiding` forever and
    /// a later `set_shown(true)` would see "already on its way" and never restart the descent.
    /// Retires **only the curtain**. It used to call [`settle`](Self::settle), which finishes every
    /// running animation — and since this runs once per frame, that erased the crossfade and the
    /// idle fade on the frame after they started. Both then drew a single frame and snapped, which
    /// looks exactly like the animation never having been written.
    pub fn settle_curtain(&mut self, now: Duration) {
        if self.is_sliding(now) {
            return;
        }
        self.retire_curtain();
    }

    /// Finish every running animation at once — the slide *and* the crossfade.
    ///
    /// For callers that must not animate, and for anything that samples the screen right after a
    /// state change. Both animations start from "invisible" — the curtain fully off the top, the
    /// incoming page at alpha zero — so a frame taken immediately shows nothing at all and reads
    /// as "the lock screen did not draw" ([[headless-animation-clock-trap]]).
    pub fn settle(&mut self) {
        self.fade_since = None;
        // Not `settle_curtain`: that one is clock-gated, and this must also finish a slide that has
        // only just started.
        self.retire_curtain();
        self.settle_page();
        self.settle_caps();
        // Without this every existing lock-screen pixel test could catch a wiggle mid-swing and
        // find the message up to 6 px from where it computed it should be.
        self.settle_wiggle();
    }

    /// Finish the crossfade now, wherever it had got to.
    ///
    /// For page changes that must not animate. Note the shape of the bug it exists to avoid: a
    /// half-finished crossfade draws the incoming page at a *partial alpha*, so anything that
    /// samples the screen right after a page change sees a prompt that is nearly invisible and
    /// reads it as "the prompt did not draw" ([[headless-animation-clock-trap]]).
    pub fn settle_page(&mut self) {
        self.page_since = None;
    }

    /// Input arrived while the shield is down: the idle watch restarts, so the hint fades back out
    /// to nothing. (`power-save-mode-changed` slams it to 0 the same way, `:391-393`.)
    pub fn note_activity(&mut self, now: Duration) {
        if self.idle_since.is_some() {
            self.idle_since = Some(now);
        }
    }

    /// 0 until [`HINT_IDLE`] of idle, then eased to 1 over [`HINT_FADE`].
    pub fn hint_alpha(&self, now: Duration) -> f64 {
        let Some(since) = self.idle_since else {
            return 0.;
        };
        let Some(fading) = now.saturating_sub(since).checked_sub(HINT_IDLE) else {
            return 0.;
        };
        let t = (fading.as_secs_f64() / HINT_FADE.as_secs_f64()).clamp(0., 1.);
        Curve::EaseOutCubic.y(t)
    }

    /// Whether the curtain still needs frames — the fade is time-driven, so nothing else would ask
    /// for one and the hint would appear only when some unrelated damage happened to land.
    pub fn is_animating(&self, now: Duration) -> bool {
        let Some(since) = self.idle_since else {
            return false;
        };
        now.saturating_sub(since) < HINT_IDLE + HINT_FADE
    }

    /// The prompt page: avatar, name, entry, message. Front-to-back like every other `render`.
    ///
    /// Two bakes rather than one, plus the avatar: the entry re-bakes on every keystroke and the
    /// rest of the column does not, so folding them together would re-draw a 160px avatar plate per
    /// character typed.
    pub fn render_prompt(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        images: &crate::render_helpers::icon::ImageCache,
        ctx: PageCtx,
        content: &PromptContent,
        t: PageTransform,
    ) -> Vec<PromptElement> {
        let PageCtx {
            scale,
            monitor,
            now,
        } = ctx;
        let base_px = crate::ui::pt_to_px(crate::ui::base_font_pt());
        let width = prompt_width(base_px);
        let block_h = prompt_block_height(base_px);
        // Placement inside the block needs real font metrics, so it happens in `paint` where the
        // runs exist. Only the entry's position is needed out here, and it does not depend on them.
        let l = prompt_layout(
            base_px,
            crate::ui::pt_to_px(NAME_PT) * LINE_BOX_ESTIMATE,
            MESSAGE_MIN_EM * base_px * 2.,
        );

        let origin = Point::<f64, Logical>::from((
            monitor.loc.x + (monitor.size.w - width) / 2.,
            stack_top(monitor, block_h),
        ))
        .to_physical_precise_round(scale)
        .to_logical(scale);

        // The page scales about its own middle (`pivot_point(0.5, 0.5)`, `:599`).
        let centre = Point::<f64, Logical>::from((
            monitor.loc.x + monitor.size.w / 2.,
            origin.y + block_h / 2.,
        ));

        let mut elements: Vec<PromptElement> = Vec::new();

        // --- The entry pill, first (topmost is first). ---
        let entry_rev = widget::Revision::new()
            .of(&content.entry)
            .of(content.cursor)
            .of(content.selection.clone())
            .of(&content.question)
            .of(content.entry_live)
            .px(width)
            .done();
        match widget::Entry::bake(
            renderer,
            &mut self.entry_cache.borrow_mut(),
            scale,
            width,
            widget::Entry::HEIGHT,
            widget::EntryContent {
                text: &content.entry,
                placeholder: &content.question,
                cursor: content.cursor,
                selection: content.selection.clone(),
                mask: None,
            },
            widget::EntryStyle::Lockscreen,
            content.entry_live,
            content.peek.is_some(),
            // Unused: the lock screen's focus ring is white, not the accent — there is a wallpaper
            // behind it, so an accent ring would compete with whatever happens to be there.
            widget::style::TEXT,
            entry_rev,
        ) {
            Ok(texture) => elements.push(
                Self::element(renderer, texture, scale, origin + l.entry.loc, t, centre).into(),
            ),
            Err(err) => tracing::error!("error drawing the unlock entry: {err:#}"),
        }

        // --- The caps-lock warning, under the entry. ---
        //
        // Its alpha rides the *element*, not the bake: the text is a constant, so it is
        // rasterised once for the life of the process and the 200 ms ease costs nothing.
        let caps_alpha = self.caps_alpha(now);
        if caps_alpha > 0. {
            let caps_h = caps_row_height(base_px);
            match widget::bake(
                renderer,
                &mut self.caps_cache.borrow_mut(),
                scale,
                Size::from((width, caps_h)),
                widget::Revision::new().px(width).done(),
                |renderer| {
                    let mut shaper = TextShaper::new(renderer, scale);
                    shaper.shape(CAPS_TEXT, TextStyle::new(HINT_PT))
                },
                |frame, phys, text: &ShapedText| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.text_band(
                        text,
                        width / 2.,
                        HAlign::Center,
                        0.,
                        caps_h,
                        CAPS_FG,
                        Rectangle::from_size(Size::from((width, caps_h))),
                    )?;
                    Ok(())
                },
            ) {
                Ok(texture) => {
                    let mut faded = t;
                    faded.alpha *= caps_alpha;
                    elements.push(
                        Self::element(renderer, texture, scale, origin + l.caps.loc, faded, centre)
                            .into(),
                    );
                }
                Err(err) => tracing::error!("error drawing the caps-lock warning: {err:#}"),
            }
        }

        // --- The peek toggle, at the entry's trailing edge. ---
        if let Some(visible) = content.peek {
            let entry_layout = widget::Entry::layout(
                l.entry.loc.x + l.entry.size.w / 2.,
                l.entry.loc.y,
                l.entry.size.w,
                l.entry.size.h,
                widget::EntryStyle::Lockscreen,
            );
            // `view-reveal-symbolic` while hidden, `view-conceal-symbolic` once shown
            // (`st-password-entry.c:333-346`).
            let icon = if visible {
                "view-conceal-symbolic"
            } else {
                "view-reveal-symbolic"
            };
            if let Some(el) = widget::icon_element_scaled(
                renderer,
                icons,
                &[icon],
                widget::Entry::ICON_PX,
                scale,
                t.scale,
                FG,
                origin,
                t.place(origin + entry_layout.secondary_icon.to_f64(), centre) - origin,
                t.alpha as f32,
            ) {
                elements.push(el.into());
            }
        }

        // --- The avatar: picture + ring, or the themed glyph. ---
        //
        // `Avatar.update()` is one branch or the other (`userWidget.js:78-92`): with a picture it
        // sets the `background-image` and adds `.user-avatar` (hence the ring); without one it
        // drops the style and puts an `avatar-default-symbolic` StIcon inside instead. Drawing both
        // would be a glyph showing through a photograph.
        let avatar_centre = t.place(
            origin
                + Point::from((
                    l.avatar.loc.x + l.avatar.size.w / 2.,
                    l.avatar.loc.y + l.avatar.size.h / 2.,
                )),
            centre,
        ) - origin;
        let picture = content.avatar.as_ref().and_then(|source| {
            let mut uploads = self.avatar_uploads.borrow_mut();
            // One slot, so anything else in it is a picture the user has replaced.
            uploads.retain_sources(|s| s == source);
            widget::Avatar::element(
                renderer,
                &mut uploads,
                images,
                source,
                AVATAR_SLOT,
                AVATAR_PX,
                scale,
                t.scale,
                origin,
                avatar_centre,
                t.alpha as f32,
            )
        });

        if let Some(el) = picture {
            // The ring goes over the picture, so it is pushed first.
            match widget::bake_card_border(
                renderer,
                &mut self.ring_cache.borrow_mut(),
                scale,
                widget::Revision::new().px(AVATAR_PX).done(),
                Size::from((AVATAR_PX, AVATAR_PX)),
                AVATAR_PX / 2.,
                widget::Avatar::RING_COLOR,
            ) {
                Ok(texture) => elements.push(
                    Self::element(renderer, texture, scale, origin + l.avatar.loc, t, centre)
                        .into(),
                ),
                Err(err) => tracing::error!("error drawing the avatar ring: {err:#}"),
            }
            elements.push(PromptElement::Rounded(el));
        } else if let Some(el) = widget::icon_element_scaled(
            renderer,
            icons,
            &["avatar-default-symbolic"],
            AVATAR_PX - AVATAR_ICON_PAD * 2.,
            scale,
            t.scale,
            FG,
            origin,
            avatar_centre,
            t.alpha as f32,
        ) {
            elements.push(el.into());
        }

        // --- The rest of the column: plate, name, message. ---
        let content_owned = content.clone();
        let baked = widget::bake(
            renderer,
            &mut self.cache.borrow_mut(),
            scale,
            Size::from((width, block_h)),
            widget::Revision::new()
                .of(&content_owned.display_name)
                .px(width)
                .done(),
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                shaper.shape(&content_owned.display_name, TextStyle::new(NAME_PT))
            },
            |frame, phys, name: &ShapedText| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;

                // Real metrics now that the runs exist — see `LINE_BOX_ESTIMATE`. The message row
                // is last, so its own height cannot move anything drawn here; the reserved
                // minimum is enough to place everything above it.
                let l = prompt_layout(
                    base_px,
                    name.line_box_height() as f64 / scale,
                    MESSAGE_MIN_EM * base_px,
                );

                // `border-radius: $forced_circular_radius` on a square box is a circle.
                p.fill_rounded(l.avatar, AVATAR_PX / 2., AVATAR_BG)?;

                let cx = width / 2.;
                p.text_band(
                    name,
                    cx,
                    HAlign::Center,
                    l.name.loc.y,
                    l.name.size.h,
                    FG,
                    l.name,
                )?;
                Ok(())
            },
        );
        match baked {
            Ok(texture) => {
                elements.push(Self::element(renderer, texture, scale, origin, t, centre).into())
            }
            Err(err) => tracing::error!("error drawing the unlock prompt: {err:#}"),
        }

        // --- The message, last in the input well and the only thing that wiggles. ---
        //
        // Its own bake so the wiggle can ride the *element*: see `message_cache`. Placed from the
        // same layout as the entry and the caps row above it, which is what keeps the three in
        // step — the column's bake refines the rows it draws with real metrics, but the message
        // row is last and nothing below it depends on the refinement.
        if let Some(text) = content.message.as_deref() {
            let is_error = content.message_is_error;
            let row = l.message.size;
            match widget::bake(
                renderer,
                &mut self.message_cache.borrow_mut(),
                scale,
                row,
                widget::Revision::new()
                    .of(text)
                    .of(is_error)
                    .px(row.w)
                    .done(),
                |renderer| {
                    let mut shaper = TextShaper::new(renderer, scale);
                    // The message **wraps** (`this._message.clutter_text.line_wrap = true`,
                    // `authPrompt.js:220`): PAM's strings are sentences, and a single clipped line
                    // loses both ends of one. Wrapped to the prompt column, minus its padding.
                    shaper.paragraph(
                        &[widget::ParagraphSpan::new(text, HINT_PT)],
                        row.w - MESSAGE_PAD * 2.,
                        HINT_PT,
                    )
                },
                |frame, phys, message: &widget::ShapedParagraph| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    let fg = if is_error {
                        MESSAGE_ERROR_FG
                    } else {
                        MESSAGE_FG
                    };
                    // `text-align: center` (`_login-lock.scss:90-92`) centres the *block*; the
                    // paragraph shaper does not centre individual wrapped lines, so a two-line
                    // message is a centred left-aligned block. Close enough to be invisible at one
                    // line, which is every message PAM actually sends.
                    // Centre the *ink*, not the layout frame: the paragraph's frame is the wrap
                    // width and its ink starts somewhere inside, so ignoring `ix` pushes the text
                    // off-centre by that much.
                    let (msg_ix, _, msg_w, _) = message.ink_bounds();
                    let at = Point::<f64, Logical>::from((
                        row.w / 2. - (msg_ix as f64 + msg_w as f64 / 2.) / scale,
                        0.,
                    ))
                    .to_physical_precise_round(scale);
                    p.paragraph(message, at, fg)?;
                    Ok(())
                },
            ) {
                Ok(texture) => {
                    let mut at = l.message.loc;
                    at.x += self.wiggle_offset(now);
                    elements.push(
                        Self::element(renderer, texture, scale, origin + at, t, centre).into(),
                    );
                }
                Err(err) => tracing::error!("error drawing the prompt message: {err:#}"),
            }
        }

        elements
    }

    /// The "Log in as another user" button, bottom-right.
    ///
    /// A sibling of the page stack rather than part of it (`mainBox.add_child`,
    /// `unlockDialog.js:659`), so it is drawn separately — but it rides the same crossfade the
    /// prompt does, at the same alpha and scale (`:817-821`, `:838-842`). Two differences from a
    /// page, both from that code: **no `translation_y`**, and it scales about **its own** centre
    /// (`set_pivot_point(0.5, 0.5)`, `:627`) rather than the stack's, which is why `t` arrives here
    /// with the page's alpha and scale but this passes the button's own centre as the pivot.
    pub fn render_switch_user(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        ctx: PageCtx,
        hovered: bool,
        accent: [u8; 3],
        t: PageTransform,
    ) -> Vec<PromptElement> {
        let PageCtx { scale, monitor, .. } = ctx;
        let rect = switch_user_rect(monitor);
        let size = rect.size.w;
        let icon_px = crate::ui::scaled_px(widget::IconButton::ICON_PX);
        let mut elements: Vec<PromptElement> = Vec::new();

        // It scales about its own middle, so that is the pivot for both layers.
        let centre = Point::from((rect.loc.x + size / 2., rect.loc.y + size / 2.));

        // The glyph, over its circle.
        if let Some(el) = widget::icon_element_scaled(
            renderer,
            icons,
            &[SWITCH_ICON],
            icon_px,
            scale,
            t.scale,
            FG,
            rect.loc,
            t.place(centre, centre) - rect.loc,
            t.alpha as f32,
        ) {
            elements.push(el.into());
        }

        // The circle itself. Baked in its own local space (origin at the rect's top-left) so the
        // texture is position-independent and the cache key is just its size.
        let button = widget::IconButton::new(
            Rectangle::from_size(Size::from((size, size))),
            icon_px,
            style::SYSTEM_BUTTON_BG,
        )
        .hovered(hovered);
        let accent_rgb = accent;
        let accent: Rgba = [
            f32::from(accent[0]) / 255.,
            f32::from(accent[1]) / 255.,
            f32::from(accent[2]) / 255.,
            1.,
        ];
        match widget::bake(
            renderer,
            &mut self.switch_cache.borrow_mut(),
            scale,
            Size::from((size, size)),
            widget::Revision::new()
                .of(hovered)
                .of(accent_rgb)
                .px(size)
                .done(),
            |_| Ok(()),
            |frame, phys, ()| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.icon_button(&button, accent)?;
                Ok(())
            },
        ) {
            Ok(texture) => {
                elements.push(Self::element(renderer, texture, scale, rect.loc, t, centre).into())
            }
            Err(err) => tracing::error!("error drawing the switch-user button: {err:#}"),
        }

        elements
    }

    fn element(
        renderer: &mut VulkanRenderer,
        texture: VkTexture,
        scale: f64,
        loc: Point<f64, Logical>,
        t: PageTransform,
        centre: Point<f64, Logical>,
    ) -> TextureRenderElement<VkTexture> {
        let buffer = TextureBuffer::from_texture(
            renderer,
            texture,
            t.buffer_scale(scale),
            Transform::Normal,
            Vec::new(),
        );
        TextureRenderElement::from_texture_buffer(
            buffer,
            t.place(loc, centre),
            t.alpha as f32,
            None,
            None,
            Kind::Unspecified,
        )
    }

    /// The clock block, front-to-back like every other UI `render`. The caller draws the dimmed
    /// wallpaper behind it.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        ctx: PageCtx,
        content: &ClockContent,
        t: PageTransform,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let PageCtx {
            scale,
            monitor,
            now,
        } = ctx;
        let base_px = crate::ui::pt_to_px(crate::ui::base_font_pt());
        let size = Size::from((monitor.size.w, block_height(base_px)));
        let hint_alpha = self.hint_alpha(now);

        // Bucketed so a 300 ms fade re-bakes ~16 times rather than once a frame. The alpha is
        // *inside* the bake because it belongs to the hint alone; an element-level alpha would
        // fade the clock along with it.
        let hint_bucket = (hint_alpha * 16.).round() as i64;
        let revision = widget::Revision::new()
            .of(&content.time)
            .of(&content.date)
            .of(&content.hint)
            .of(hint_bucket)
            .px(size.w)
            .done();

        let content = content.clone();
        let baked = widget::bake(
            renderer,
            &mut self.cache.borrow_mut(),
            scale,
            size,
            revision,
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                Ok((
                    // `%numeric` is `font-feature-settings: "tnum"`, and it is not decoration: a
                    // proportional `1` is narrower than a `0`, so a centred clock would shuffle
                    // sideways every minute.
                    shaper.shape(&content.time, TextStyle::new(TIME_PT).bold())?,
                    shaper.shape(&content.date, TextStyle::new(DATE_PT))?,
                    shaper.shape(&content.hint, TextStyle::new(HINT_PT).bold())?,
                ))
            },
            |frame, phys, shaped: &(ShapedText, ShapedText, ShapedText)| {
                let (time, date, hint) = shaped;
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;

                let (_, _, hint_iw, _) = hint.ink_bounds();
                // Real metrics now that the runs exist — see `ClockRows`.
                let rows = ClockRows {
                    time: time.line_box_height() as f64 / scale,
                    date: date.line_box_height() as f64 / scale,
                    hint: hint.line_box_height() as f64 / scale,
                };
                let l = layout(size.w, hint_iw as f64 / scale, base_px, rows);
                let fade = |c: Rgba| [c[0], c[1], c[2], c[3] * hint_alpha as f32];

                let cx = size.w / 2.;
                p.text_band(
                    time,
                    cx,
                    HAlign::Center,
                    l.time.loc.y,
                    l.time.size.h,
                    FG,
                    l.time,
                )?;
                p.text_band(
                    date,
                    cx,
                    HAlign::Center,
                    l.date.loc.y,
                    l.date.size.h,
                    FG,
                    l.date,
                )?;

                if hint_alpha > 0. {
                    p.text_band(
                        hint,
                        cx,
                        HAlign::Center,
                        l.hint.loc.y,
                        l.hint.size.h,
                        fade(FG),
                        l.hint_box,
                    )?;
                }
                Ok(())
            },
        );

        let texture = match baked {
            Ok(texture) => texture,
            Err(err) => {
                // The caller still dims and covers the screen — a failed clock must not leave the
                // desktop plainly readable behind a shield that is nominally down.
                tracing::error!("error drawing the lock screen clock: {err:#}");
                return Vec::new();
            }
        };

        let loc = Point::from((monitor.loc.x, stack_top(monitor, size.h)))
            .to_physical_precise_round(scale)
            .to_logical(scale);
        // Scaled about the block's middle (`pivot_point(0.5, 0.5)`, `:604`).
        let centre = Point::from((monitor.loc.x + monitor.size.w / 2., loc.y + size.h / 2.));

        vec![Self::element(renderer, texture, scale, loc, t, centre)]
    }
}

#[cfg(test)]
mod tests {
    /// The message is displaced, and nothing else is.
    ///
    /// GNOME wiggles `this._message` alone (`authPrompt.js:489`). Shaking the avatar and the name
    /// with it would read as the whole dialog rejecting the user, which is a much louder statement
    /// than "the reader could not do it".
    #[test]
    fn the_wiggle_moves_the_message_and_not_the_column() {
        // The message's own bake is what makes that possible, and it is only separate because of
        // the wiggle — so if someone folds it back into the column, this is the test that says why
        // they cannot.
        let l = super::prompt_layout(16., 20., 44.);
        assert!(
            l.message.loc.y > l.caps.loc.y,
            "the message is last in the input well (`authPrompt.js`'s `_inputWell` order)"
        );
    }

    use super::*;

    const T0: Duration = Duration::from_secs(1_000);

    /// The three lines stack in the JS's order, centred, without overlapping.
    #[test]
    fn the_clock_stacks_time_then_date_then_hint_centred() {
        let l = layout(1920., 300., 16., ClockRows::estimated());

        assert!(l.time.loc.y < l.date.loc.y, "time above date");
        assert!(
            l.date.loc.y + l.date.size.h <= l.hint_box.loc.y,
            "date must not overlap the hint pill"
        );

        for r in [l.time, l.date, l.hint, l.hint_box] {
            assert_eq!(r.loc.x + r.size.w / 2., 960., "every line is centred");
        }
    }

    /// The hint sits *twice* as far below the date as the date does below the time.
    ///
    /// `.unlock-dialog-clock-hint` adds `margin-top: 2em` on top of the box's own `spacing: 2em`
    /// (`_login-lock.scss:240,254`). Folding those into one gap is the easy misreading, and it
    /// reads on screen as a hint crowding the date.
    #[test]
    fn the_hint_carries_its_own_margin_on_top_of_the_box_spacing() {
        let base = 16.;
        let l = layout(1920., 300., base, ClockRows::estimated());

        let time_to_date = l.date.loc.y - (l.time.loc.y + l.time.size.h);
        let date_to_hint = l.hint_box.loc.y - (l.date.loc.y + l.date.size.h);

        // Row heights now come from font metrics, so the sums carry float drift; the assertion
        // is about which gap is which, not about the last ULP.
        assert!((time_to_date - SPACING_EM * base).abs() < 1e-9);
        assert!((date_to_hint - SPACING_EM * base * 2.).abs() < 1e-9);
    }

    /// The pill pads its text on all four sides, and the block is exactly tall enough for it.
    #[test]
    fn the_hint_box_pads_its_text_and_fits_the_block() {
        let l = layout(1920., 300., 16., ClockRows::estimated());
        assert_eq!(l.hint_box.size.w, 300. + HINT_PAD_H * 2.);
        assert_eq!(l.hint_box.size.h, l.hint.size.h + HINT_PAD_V * 2.);
        assert_eq!(l.hint.loc.y - l.hint_box.loc.y, HINT_PAD_V);

        // `block_height` is what sizes the bake, so a mismatch would clip the pill away.
        assert!((l.hint_box.loc.y + l.hint_box.size.h - block_height(16.)).abs() < 1e-9);
    }

    /// Every row is as tall as its font's line box, not its point size.
    ///
    /// Sizing a row by the point size clips descenders and nothing else changes — the date's
    /// `g` in "August" loses its tail while the layout still passes every spacing assertion above.
    /// So pin the two things that make it wrong: the row must be taller than the point size, and
    /// `layout` must use the height it is handed rather than deriving one.
    #[test]
    fn the_rows_are_line_boxes_not_point_sizes() {
        let rows = ClockRows::estimated();
        assert!(rows.time > crate::ui::pt_to_px(TIME_PT));
        assert!(rows.date > crate::ui::pt_to_px(DATE_PT));
        assert!(rows.hint > crate::ui::pt_to_px(HINT_PT));

        let measured = ClockRows {
            time: 100.,
            date: 50.,
            hint: 25.,
        };
        let l = layout(1920., 300., 16., measured);
        assert_eq!(l.time.size.h, measured.time);
        assert_eq!(l.date.size.h, measured.date);
        assert_eq!(l.hint.size.h, measured.hint);
        assert_eq!(
            l.hint_box.loc.y + l.hint_box.size.h,
            block_height_of(measured, 16.)
        );
    }

    /// The curtain comes down, and keeps drawing while it goes away again.
    ///
    /// The second half is the point. `is_covering` must stay true after the shield stops being
    /// active, or the render path drops the curtain the instant it is unlocked and the slide out
    /// is a hard cut with a 250 ms animation nobody ever sees.
    #[test]
    fn the_curtain_slides_in_and_keeps_drawing_on_the_way_out() {
        let mut shield = LockScreen::default();
        assert!(!shield.is_covering(T0), "nothing before the shield");
        assert_eq!(shield.curtain_progress(T0), 1., "parked off the top");

        shield.set_shown(true, T0);
        assert!(shield.is_covering(T0), "on screen from the first frame");
        let mid = shield.curtain_progress(T0 + SLIDE_TIME / 2);
        assert!(mid > 0. && mid < 1., "part-way down: {mid}");
        assert_eq!(shield.curtain_progress(T0 + SLIDE_TIME), 0., "fully down");
        assert!(!shield.is_sliding(T0 + SLIDE_TIME));

        let settled = T0 + SLIDE_TIME;
        shield.settle_curtain(settled);
        shield.set_shown(false, settled);
        assert!(
            shield.is_covering(settled),
            "still drawing: the shield is up but the curtain has not left"
        );
        assert!(
            shield.is_sliding(settled),
            "and it owes the frames to do it"
        );
        assert_eq!(shield.curtain_progress(settled + SLIDE_TIME), 1.);
        assert!(!shield.is_covering(settled + SLIDE_TIME), "gone");
    }

    /// A finished slide is retired, so the next lock actually descends.
    ///
    /// Without the retirement `Hiding` never becomes `Hidden`, and the next `set_shown(true)` is
    /// told the curtain is "already on its way" — leaving it parked off-screen with a shield that
    /// believes it is covering the display. That is a locked session showing the desktop.
    #[test]
    fn a_settled_slide_lets_the_next_lock_descend() {
        let mut shield = LockScreen::default();
        shield.set_shown(true, T0);
        shield.settle_curtain(T0 + SLIDE_TIME);
        shield.set_shown(false, T0 + SLIDE_TIME);

        let done = T0 + SLIDE_TIME * 2;
        shield.settle_curtain(done);
        assert!(!shield.is_covering(done));

        shield.set_shown(true, done);
        assert!(shield.is_sliding(done), "the next lock slides in afresh");
        assert_eq!(shield.curtain_progress(done + SLIDE_TIME), 0.);
    }

    /// Reversing the caps fade mid-flight continues from where it is, rather than snapping.
    ///
    /// Double-tap Caps Lock inside 200 ms. Deriving the ease's start from the *target* means the
    /// second one begins at the opposite extreme — so a fade-in caught half way jumps to fully
    /// opaque and then fades out, a flash of a warning that was never up. Invisible to any test
    /// that settles before it looks.
    #[test]
    fn reversing_the_caps_fade_does_not_snap() {
        let mut shield = LockScreen::default();
        assert!(shield.set_caps_warning(true, T0));

        let half = T0 + CAPS_FADE / 2;
        let mid = shield.caps_alpha(half);
        assert!(mid > 0. && mid < 1., "part-way in: {mid}");

        // Reverse. The very next instant must still be ~`mid`, not 1.
        assert!(shield.set_caps_warning(false, half));
        let after = shield.caps_alpha(half);
        assert!(
            (after - mid).abs() < 1e-9,
            "the reversal jumped from {mid} to {after}"
        );
        assert!(
            shield.caps_alpha(half + CAPS_FADE / 4) < mid,
            "and then keeps going down"
        );
        assert_eq!(shield.caps_alpha(half + CAPS_FADE), 0., "all the way out");

        // A no-op set reports no change, so the caller does not ask for frames it does not need.
        assert!(!shield.set_caps_warning(false, half + CAPS_FADE));
    }

    /// The per-frame curtain retirement must leave the *other* animations alone.
    ///
    /// `settle_curtain` runs once per frame from the render path, and it used to delegate to
    /// `settle`, which finishes everything. So a crossfade or an idle fade starting while the
    /// curtain was at rest — which is every crossfade, since you press a key at a shield that has
    /// already landed — was erased on the very next frame. Both drew one frame and snapped, which
    /// looks exactly like the animation never having been written.
    #[test]
    fn the_per_frame_retirement_does_not_settle_the_other_animations() {
        let mut shield = LockScreen::default();
        shield.set_shown(true, T0);
        let landed = T0 + SLIDE_TIME;
        shield.settle_curtain(landed);

        shield.set_page(true, landed);
        shield.light_on(landed);

        // The next frame, with the curtain long since parked.
        let next = landed + Duration::from_millis(16);
        shield.settle_curtain(next);

        assert!(
            shield.page_is_animating(next),
            "the crossfade still owes frames"
        );
        let mid = shield.page_progress(next);
        assert!(mid > 0. && mid < 1., "and is part-way through it: {mid}");
        assert!(shield.is_fading(next), "so does the fade");
        let fade = shield.fade_alpha(next);
        assert!(fade > 0. && fade < 1., "part-way to black: {fade}");
    }

    /// The two pages move in opposite directions and swap cleanly at the ends.
    ///
    /// The discriminating property is the *sign* of the translation: both pages shrinking and
    /// fading is a dissolve, and would look right in a still frame while reading as mush in
    /// motion. GNOME sends the clock up and the prompt down (`:826-836`), which is what makes it
    /// read as one page giving way to the other.
    #[test]
    fn the_pages_cross_in_opposite_directions() {
        let (clock0, prompt0) = (PageTransform::clock(0.), PageTransform::prompt(0.));
        assert_eq!(clock0, PageTransform::REST, "the clock is at rest at 0");
        assert_eq!(prompt0.alpha, 0.);
        assert_eq!(prompt0.scale, FADE_OUT_SCALE);

        let (clock1, prompt1) = (PageTransform::clock(1.), PageTransform::prompt(1.));
        assert_eq!(prompt1, PageTransform::REST, "and the prompt at 1");
        assert_eq!(clock1.alpha, 0.);
        assert_eq!(clock1.scale, FADE_OUT_SCALE);

        let (clock, prompt) = (PageTransform::clock(0.5), PageTransform::prompt(0.5));
        assert!(clock.translation_y < 0., "the clock leaves upward");
        assert!(prompt.translation_y > 0., "the prompt arrives from below");
        assert_eq!(clock.scale, prompt.scale, "and they pass at the same size");
    }

    /// Scaling happens about the page's centre, not its corner.
    ///
    /// `pivot_point(0.5, 0.5)` (`:599`, `:604`). Without it a shrinking page also drifts toward
    /// its own top-left, which reads as the text sliding off-centre rather than receding.
    #[test]
    fn a_page_scales_about_its_middle() {
        let centre = Point::<f64, Logical>::from((960., 400.));
        let t = PageTransform {
            alpha: 1.,
            scale: 0.5,
            translation_y: 0.,
        };
        assert_eq!(
            t.place(centre, centre),
            centre,
            "the centre is a fixed point"
        );
        // A point 100px left of centre ends up 50px left of it.
        assert_eq!(t.place(Point::from((860., 400.)), centre).x, 910.);
    }

    /// The crossfade eases to its end and then stops asking for frames.
    #[test]
    fn the_crossfade_runs_once_and_settles() {
        let mut shield = LockScreen::default();
        shield.set_shown(true, T0);
        assert_eq!(shield.page_progress(T0), 0.);

        shield.set_page(true, T0);
        assert!(shield.page_is_animating(T0));
        let mid = shield.page_progress(T0 + CROSSFADE_TIME / 2);
        assert!(mid > 0. && mid < 1., "part-way: {mid}");
        assert_eq!(shield.page_progress(T0 + CROSSFADE_TIME), 1.);
        assert!(!shield.page_is_animating(T0 + CROSSFADE_TIME));

        // Asking for the page we are already on does not restart it.
        shield.set_page(true, T0 + CROSSFADE_TIME);
        assert_eq!(shield.page_progress(T0 + CROSSFADE_TIME), 1.);

        // And going back runs the other way.
        shield.set_page(false, T0 + CROSSFADE_TIME);
        assert_eq!(shield.page_progress(T0 + CROSSFADE_TIME), 1.);
        assert_eq!(shield.page_progress(T0 + CROSSFADE_TIME * 2), 0.);
    }

    /// The hint is invisible until four seconds of idle, then fades in over 300 ms.
    ///
    /// The watch is on the **idle** monitor (`:395`), not on a timer started at activation — so
    /// input while the shield is down puts the hint back to nothing rather than leaving it up.
    #[test]
    fn the_hint_waits_for_idle_and_restarts_on_input() {
        let mut shield = LockScreen::default();
        assert_eq!(shield.hint_alpha(T0), 0., "nothing while the shield is up");

        shield.set_shown(true, T0);
        assert_eq!(shield.hint_alpha(T0 + Duration::from_secs(3)), 0.);
        assert_eq!(
            shield.hint_alpha(T0 + HINT_IDLE),
            0.,
            "the fade starts at the timeout, it has not run yet"
        );
        assert!(shield.hint_alpha(T0 + HINT_IDLE + Duration::from_millis(150)) > 0.);
        assert_eq!(shield.hint_alpha(T0 + HINT_IDLE + HINT_FADE), 1.);
        assert!(!shield.is_animating(T0 + HINT_IDLE + HINT_FADE));

        // Input restarts the watch.
        let t = T0 + Duration::from_secs(10);
        shield.note_activity(t);
        assert_eq!(shield.hint_alpha(t), 0., "back to nothing");
        assert!(shield.is_animating(t));
        assert_eq!(shield.hint_alpha(t + HINT_IDLE + HINT_FADE), 1.);

        // And raising the shield stops it entirely — `note_activity` must not resurrect it.
        shield.set_shown(false, T0);
        shield.note_activity(t + Duration::from_secs(60));
        assert_eq!(shield.hint_alpha(t + Duration::from_secs(120)), 0.);
        assert!(!shield.is_animating(t + Duration::from_secs(120)));
    }
}
