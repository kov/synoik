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
    fn buffer_scale(&self, scale: f64) -> f64 {
        scale / self.scale
    }

    /// The scale to rasterize an *icon* at, bucketed.
    ///
    /// Icons have no buffer-scale knob — `icon_element` rasterizes at the size it is asked for —
    /// so an unbucketed scale would re-raster the 160 px avatar on every frame of the fade.
    /// Sixteen steps are indistinguishable in motion and populate the cache once.
    fn icon_scale(&self) -> f64 {
        (self.scale * 16.).round() / 16.
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
const AVATAR_PX: f64 = 160.;
/// Its `StIcon { padding: $base_padding * 5 }` (`:391`) — the inset of the fallback glyph.
const AVATAR_ICON_PAD: f64 = 30.;
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

/// What the prompt page shows. The entry text arrives **already masked** — this type never sees a
/// password, which is the point (see [`crate::unlock_dialog::UnlockDialog::entry_display`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContent {
    pub display_name: String,
    /// Masked or not, per the dialog. Never the raw secret.
    pub entry: String,
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
}

/// Where the prompt page's parts sit, relative to the block's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromptLayout {
    pub avatar: Rectangle<f64, Logical>,
    pub name: Rectangle<f64, Logical>,
    pub entry: Rectangle<f64, Logical>,
    pub message: Rectangle<f64, Logical>,
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
    let message = centred(y, message_h, width);

    PromptLayout {
        avatar,
        name,
        entry,
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
        widget::EntryStyle::Lockscreen,
    );
    widget::Entry::hit(&entry, pos, true)
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

/// The curtain's on-screen state: when it went down, and its bake.
#[derive(Default)]
pub struct LockScreen {
    /// Monotonic instant the shield went down, or of the last input while it was down — the hint's
    /// fade hangs off an **idle** watch on the core idle monitor (`:395`), not off activation.
    idle_since: Option<Duration>,
    /// Which page is being eased *to*, and when the ease started. `None` means settled.
    ///
    /// The page a transition ends on comes from the model ([`crate::unlock_dialog::Page`]); this
    /// is only the view's clock for it, so the crossfade stays a drawing concern and the model
    /// keeps no notion of time.
    page_since: Option<(Duration, bool)>,
    /// The page as of the last [`Self::set_page`] — what a new transition eases *from*.
    showing_prompt: bool,
    cache: RefCell<widget::BakeCache>,
    /// The entry has its own cache: it re-bakes per keystroke, the column around it does not.
    entry_cache: RefCell<widget::BakeCache>,
}

impl LockScreen {
    /// The shield went down (or came back up, with `None`).
    pub fn set_shown(&mut self, now: Option<Duration>) {
        self.idle_since = now;
        // A raised shield settles instantly: the next lock must not open on the tail of the
        // crossfade the last one was in the middle of.
        if now.is_none() {
            self.showing_prompt = false;
            self.settle_page();
        }
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

    /// The model moved to a page. Starts the crossfade, or does nothing if we are already going
    /// there.
    pub fn set_page(&mut self, prompt: bool, now: Duration) {
        if self.showing_prompt == prompt {
            return;
        }
        self.showing_prompt = prompt;
        self.page_since = Some((now, prompt));
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
    /// Two bakes rather than one, plus the avatar's icon element: the entry re-bakes on every
    /// keystroke and the rest of the column does not, so folding them together would re-draw a
    /// 160px avatar per character typed.
    pub fn render_prompt(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        scale: f64,
        monitor: Rectangle<f64, Logical>,
        content: &PromptContent,
        t: PageTransform,
    ) -> Vec<TextureRenderElement<VkTexture>> {
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

        let mut elements = Vec::new();

        // --- The entry pill, first (topmost is first). ---
        let entry_rev = widget::Revision::new()
            .of(&content.entry)
            .of(&content.question)
            .of(content.entry_live)
            .px(width)
            .done();
        match widget::Entry::bake(
            renderer,
            &mut self.entry_cache.borrow_mut(),
            scale,
            width,
            &content.entry,
            &content.question,
            widget::EntryStyle::Lockscreen,
            content.entry_live,
            content.peek.is_some(),
            entry_rev,
        ) {
            Ok(texture) => elements.push(Self::element(
                renderer,
                texture,
                scale,
                origin + l.entry.loc,
                t,
                centre,
            )),
            Err(err) => tracing::error!("error drawing the unlock entry: {err:#}"),
        }

        // --- The peek toggle, at the entry's trailing edge. ---
        if let Some(visible) = content.peek {
            let entry_layout = widget::Entry::layout(
                l.entry.loc.x + l.entry.size.w / 2.,
                l.entry.loc.y,
                l.entry.size.w,
                widget::EntryStyle::Lockscreen,
            );
            // `view-reveal-symbolic` while hidden, `view-conceal-symbolic` once shown
            // (`st-password-entry.c:333-346`).
            let icon = if visible {
                "view-conceal-symbolic"
            } else {
                "view-reveal-symbolic"
            };
            if let Some(el) = widget::icon_element_alpha(
                renderer,
                icons,
                &[icon],
                widget::Entry::ICON_PX * t.icon_scale(),
                scale,
                FG,
                origin,
                t.place(origin + entry_layout.secondary_icon.to_f64(), centre) - origin,
                t.alpha as f32,
            ) {
                elements.push(el);
            }
        }

        // --- The avatar's fallback glyph, over its plate. ---
        //
        // AccountsService's per-user icon file is not read yet, so everyone gets the themed
        // fallback; wiring the real picture is additive and does not move anything here.
        if let Some(el) = widget::icon_element_alpha(
            renderer,
            icons,
            &["avatar-default-symbolic"],
            (AVATAR_PX - AVATAR_ICON_PAD * 2.) * t.icon_scale(),
            scale,
            FG,
            origin,
            t.place(
                origin
                    + Point::from((
                        l.avatar.loc.x + l.avatar.size.w / 2.,
                        l.avatar.loc.y + l.avatar.size.h / 2.,
                    )),
                centre,
            ) - origin,
            t.alpha as f32,
        ) {
            elements.push(el);
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
                .of(&content_owned.message)
                .of(content_owned.message_is_error)
                .px(width)
                .done(),
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                let name = shaper.shape(&content_owned.display_name, TextStyle::new(NAME_PT))?;
                // The message **wraps** (`this._message.clutter_text.line_wrap = true`,
                // `authPrompt.js:220`): PAM's strings are sentences, and a single clipped line
                // loses both ends of one. Wrapped to the prompt column, minus its padding.
                let message = shaper.paragraph(
                    &[widget::ParagraphSpan::new(
                        content_owned.message.as_deref().unwrap_or(""),
                        HINT_PT,
                    )],
                    width - MESSAGE_PAD * 2.,
                    HINT_PT,
                )?;
                Ok((name, message))
            },
            |frame, phys, shaped: &(ShapedText, widget::ShapedParagraph)| {
                let (name, message) = shaped;
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;

                // Real metrics now that the runs exist — see `LINE_BOX_ESTIMATE`.
                let (_, _, msg_w, msg_h) = message.ink_bounds();
                let l = prompt_layout(
                    base_px,
                    name.line_box_height() as f64 / scale,
                    msg_h as f64 / scale,
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
                if content_owned.message.is_some() {
                    let fg = if content_owned.message_is_error {
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
                    let (msg_ix, _, _, _) = message.ink_bounds();
                    let origin = Point::<f64, Logical>::from((
                        cx - (msg_ix as f64 + msg_w as f64 / 2.) / scale,
                        l.message.loc.y,
                    ))
                    .to_physical_precise_round(scale);
                    p.paragraph(message, origin, fg)?;
                }
                Ok(())
            },
        );
        match baked {
            Ok(texture) => {
                elements.push(Self::element(renderer, texture, scale, origin, t, centre))
            }
            Err(err) => tracing::error!("error drawing the unlock prompt: {err:#}"),
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
        scale: f64,
        monitor: Rectangle<f64, Logical>,
        content: &ClockContent,
        now: Duration,
        t: PageTransform,
    ) -> Vec<TextureRenderElement<VkTexture>> {
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
        shield.set_shown(Some(T0));
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

        shield.set_shown(Some(T0));
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
        shield.set_shown(None);
        shield.note_activity(t + Duration::from_secs(60));
        assert_eq!(shield.hint_alpha(t + Duration::from_secs(120)), 0.);
        assert!(!shield.is_animating(t + Duration::from_secs(120)));
    }
}
