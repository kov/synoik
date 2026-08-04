// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The notification message card, shared by the banner and the calendar list.
//!
//! gnome-shell draws one `.message` card design everywhere a notification
//! shows (`js/ui/messageList.js:444-529`, `_message-list.scss:81-218`): a
//! rounded card with a header row (16px source icon, bold app title, 9pt
//! relative-time label, expand caret, circular close button) over a body row
//! (48px icon, bold title, body). The card is expandable: collapsed it shows
//! one ellipsized body line and no action row; expanded the body wraps to up
//! to six lines and the action buttons appear (`LabelExpanderLayout` +
//! `js/ui/messageList.js:598-666`). The banner (`ui/notification_banner.rs`)
//! and the calendar message list (`ui/calendar.rs`) both render it through
//! this module, parameterized by width / corner radius / expansion; the
//! banner hides the caret (`js/ui/messageTray.js:1137`) and expands on hover
//! or CRITICAL urgency instead.

use std::collections::HashMap;
use std::time::Duration;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::notifications::{Notification, NotificationIcon, NotificationStore, Source, Urgency};
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, Align, Painter, ShapedText, TextShaper, TextStyle};

/// `.message` padding = `$base_padding` (`_message-list.scss:83`).
pub const PAD: f64 = 6.;

/// `.message` `border: 1px` — `$card_shadow_border_color`, transparent in the dark theme but
/// still reserved, since St is border-box: it sizes the box (the banner's `width_px`) *and* insets
/// every edge-relative point in [`layout_clamped`].
pub const BORDER: f64 = 1.;
/// `.message-close-button` `margin: $base_padding * 0.5` (`_message-list.scss:152-155`).
pub const CLOSE_MARGIN: f64 = 3.;

/// The header row's height — the tallest child's **margin box**, not `.message-header-content`.
///
/// That content is `HEADER_H + HEADER_PAD_B` = 30, but the close button is 28 with a 3px margin
/// on each side, so the row is 34 and the card is 4px taller than the content alone implies.
/// Measured on a live 50.3 shell: `.message-header` 34 inside a 108-tall banner.
pub fn header_band() -> f64 {
    (HEADER_H + HEADER_PAD_B).max(CLOSE_D + 2. * CLOSE_MARGIN)
}
/// `.message-header-content` min-height (`_message-list.scss:118`).
pub const HEADER_H: f64 = 24.;
/// `.message-header-content` padding-bottom (`_message-list.scss:120`). Separate from [`PAD`]
/// because it stacks *with* `.message-box`'s own top padding: the body row starts
/// `PAD + HEADER_H + HEADER_PAD_B + PAD` down, not `PAD + HEADER_H + PAD`.
pub const HEADER_PAD_B: f64 = 6.;
/// `.message-icon` margin (`_message-list.scss:168-170`), which stacks with `.message-box`'s
/// `spacing` — so the gap between the 48px icon and the text column is two of these, not one.
pub const ICON_MARGIN: f64 = 6.;
/// `.message-icon` icon-size = 48px (`_message-list.scss:168`).
pub const BODY_ICON: f64 = 48.;
/// Header/source/close icons (`$scalable_icon_size`).
pub const SMALL_ICON: f64 = 16.;
/// The circular close button: 16px icon + 6px padding (`_message-list.scss:139-156`).
pub const CLOSE_D: f64 = SMALL_ICON + 2. * PAD;
/// Action button height: bold 11pt + 6px paddings (`%notification_button`).
pub const BTN_H: f64 = 28.;
pub(crate) const BTN_RADIUS: f64 = 8.;
/// Gap between action buttons (`$base_margin`).
const BTN_GAP: f64 = 4.;
/// Body/title/source/action font size (11pt) and the header time label (9pt),
/// GNOME points; shaping routes them through [`TextShaper`]. `title_px()` is the
/// logical px, used by [`layout`] for wrap/measure (no scale — logical geometry).
const TITLE_PT: f64 = 11.;
const TIME_PT: f64 = 9.;
fn title_px() -> f64 {
    crate::ui::pt_to_px(TITLE_PT)
}
/// An expanded body shows at most this many wrapped lines
/// (`DEFAULT_EXPAND_LINES`, `js/ui/messageList.js:23`).
pub const EXPAND_LINES: usize = 6;
/// Body line pitch: cosmic-text's `round(px * 1.25)` at the 11pt body size.
pub const LINE_H: f64 = 18.;
/// Gap between the expand and close buttons: header spacing (6) + the close
/// button's balancing margin (3) (`_message-list.scss:101,152-155`).
const EXPAND_GAP: f64 = 9.;

/// Fanned-stack geometry for a collapsed multi-notification group
/// (`js/ui/messageList.js:26-30`): at most three cards peek, each lower card
/// inset [`STACK_WIDTH_INSET`] per side and revealed [`STACK_HEIGHT_OFFSET`]
/// (then divided by [`STACK_HEIGHT_REDUCTION`] each step) below the one above.
pub const STACK_MAX_VISIBLE: usize = 3;
pub const STACK_WIDTH_INSET: f64 = 6.;
pub const STACK_HEIGHT_OFFSET: f64 = 10.;
pub const STACK_HEIGHT_REDUCTION: f64 = 1.4;
/// Extra space below an expanded group (`ADDITIONAL_BOTTOM_MARGIN_EXPANDED_GROUP`,
/// `js/ui/messageList.js:27`).
pub const GROUP_BOTTOM_MARGIN: f64 = 15.;

/// `.message` bg, dark variant (`lighten($card_bg_color, 5%)` ≈ `#51515a`).
pub(crate) const CARD_BG: [f32; 4] = [
    0x51 as f32 / 255.,
    0x51 as f32 / 255.,
    0x5a as f32 / 255.,
    1.,
];
/// `.message-header` fg (`$card_insensitive_fg_color` ≈ `#b1b1b3`).
pub const HEADER_FG: [f32; 4] = [
    0xb1 as f32 / 255.,
    0xb1 as f32 / 255.,
    0xb3 as f32 / 255.,
    1.,
];
pub(crate) const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Close/action button bg (white@15%, `%notification_button` normal =
/// `transparentize($fg_color, .85)`).
const BTN_BG: [f32; 4] = [1., 1., 1., 0.15];
/// A hovered card button lightens to white@30% (`%notification_button:hover` =
/// `transparentize($fg_color, .7)`, `_drawing.scss:228`).
pub(crate) const BTN_BG_HOVER: [f32; 4] = [1., 1., 1., 0.30];
/// `.message-themed-icon` circle bg (white@7%, `_message-list.scss:176`).
pub(crate) const CIRCLE_BG: [f32; 4] = [1., 1., 1., 0.07];
pub(crate) const TRANSPARENT: [f32; 4] = [0., 0., 0., 0.];
/// A hovered card body darkens slightly: `.message` extends `%card`, whose
/// `:hover` sets the bg to `button(hover, card)` = `lighten($card_bg, 4%)`,
/// while `.message` overrides its *normal* bg to `lighten($card_bg, 5%)`
/// ([`CARD_BG`]) — so on the dark theme hover is ~1% darker than resting
/// (`_common.scss:154-161`, `_drawing.scss:193`, `_message-list.scss:87`).
pub(crate) const CARD_HOVER_BG: [f32; 4] = [
    0x4d as f32 / 255.,
    0x4d as f32 / 255.,
    0x56 as f32 / 255.,
    1.,
];

/// A hoverable zone within a single card, resolved by the owner's hit-test and
/// fed back so [`draw_card`] can highlight it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardZone {
    Close,
    Caret,
    Action(usize),
}

/// Darkened backgrounds for the lower cards peeking under a collapsed stack:
/// `second-in-stack` = `darken($card_bg, 1%)`, `lower-in-stack` =
/// `darken($card_bg, 4%)` on the dark variant (`_message-list.scss:89-98`),
/// i.e. ~6%/~9% below the normal `.message` fill.
const STACK_SECOND_BG: [f32; 4] = [
    0x47 as f32 / 255.,
    0x47 as f32 / 255.,
    0x4f as f32 / 255.,
    1.,
];
const STACK_LOWER_BG: [f32; 4] = [
    0x42 as f32 / 255.,
    0x42 as f32 / 255.,
    0x4a as f32 / 255.,
    1.,
];

/// A display snapshot of one notification (plus its source header), rebuilt
/// from the store on every content change.
#[derive(Debug, Clone, PartialEq)]
pub struct CardContent {
    pub id: u32,
    pub source_title: String,
    pub source_icon: Option<NotificationIcon>,
    /// The resolved app's icon, which wins over `source_icon` — see
    /// [`crate::notifications::NotifyRequest::app_icon`].
    pub source_app_icon: Option<AppIconRef>,
    pub title: String,
    pub body: String,
    pub icon: Option<NotificationIcon>,
    /// Capped at gnome-shell's `MAX_NOTIFICATION_BUTTONS = 3` (`js/ui/messageList.js:19`).
    pub actions: Vec<(String, String)>,
    pub has_default_action: bool,
    pub critical: bool,
    pub time_text: String,
}

/// Build the card snapshot for one notification of `source`; `now` is the
/// pinned clock, for the header's relative-time label.
pub fn card_for(source: &Source, n: &Notification, now: Duration) -> CardContent {
    CardContent {
        id: n.id,
        // The UI falls back like `MessageHeader` (`js/ui/messageList.js:396-403`).
        source_title: if source.title.is_empty() {
            "Unknown App".to_owned()
        } else {
            source.title.clone()
        },
        source_icon: source.icon.clone(),
        source_app_icon: source.app_icon.clone(),
        title: n.title.clone(),
        body: n.body.clone(),
        icon: n.icon.clone(),
        actions: n.actions.iter().take(3).cloned().collect(),
        has_default_action: n.has_default_action,
        critical: n.urgency == Urgency::Critical,
        time_text: crate::notifications::format_time_span(now.saturating_sub(n.timestamp)),
    }
}

/// Build the card snapshot for the notification `id`, or `None` when it's
/// gone from the store.
pub fn content_for(store: &NotificationStore, id: u32, now: Duration) -> Option<CardContent> {
    let source = store
        .sources
        .iter()
        .find(|s| s.notifications.iter().any(|n| n.id == id))?;
    let n = source.notifications.iter().find(|n| n.id == id)?;
    Some(card_for(source, n, now))
}

/// One source's notifications rendered as a group (`NotificationMessageGroup`,
/// `js/ui/messageList.js:858-949`): a fanned card stack when it holds more than
/// one, a plain card when it holds exactly one.
#[derive(Debug, Clone, PartialEq)]
pub struct CardGroup {
    /// Stable identity across snapshots, for the per-group expansion state
    /// (source order reshuffles as notifications arrive; the key does not).
    pub key: crate::notifications::SourceKey,
    pub source_title: String,
    pub source_icon: Option<NotificationIcon>,
    /// Any notification in the group is CRITICAL — urgent groups sort first
    /// (`js/ui/messageList.js:1815-1826`).
    pub has_urgent: bool,
    /// Newest-first, criticals first within the source
    /// (`js/ui/messageList.js:1120-1123` newest-to-top, `:1078-1082` urgent-to-0).
    pub cards: Vec<CardContent>,
}

/// The calendar message list's groups, one per source. Source order already
/// carries gnome-shell's semantics: a source moves to the top only when a
/// notification is ADDED (`js/ui/messageList.js:1824-1827`); a replace mutates
/// in place and never reorders (`js/ui/messageTray.js:579-581`), nor do closes.
/// Urgent groups are then pinned to the front, stably
/// (`js/ui/messageList.js:1815-1832`).
pub fn message_list_groups(store: &NotificationStore, now: Duration) -> Vec<CardGroup> {
    let mut groups: Vec<CardGroup> = store
        .sources
        .iter()
        .map(|s| {
            // Newest-first, then stably float criticals to the top — the net of
            // gnome-shell's newest-to-top insert plus urgent-to-index-0 move.
            let mut cards: Vec<CardContent> = s
                .notifications
                .iter()
                .rev()
                .map(|n| card_for(s, n, now))
                .collect();
            cards.sort_by_key(|c| !c.critical);
            CardGroup {
                key: s.key.clone(),
                source_title: if s.title.is_empty() {
                    "Unknown App".to_owned()
                } else {
                    s.title.clone()
                },
                source_icon: s.icon.clone(),
                has_urgent: s
                    .notifications
                    .iter()
                    .any(|n| n.urgency == Urgency::Critical),
                cards,
            }
        })
        .collect();
    // Urgent groups first, preserving relative order (`!has_urgent`: urgent=false sorts first).
    groups.sort_by_key(|g| !g.has_urgent);
    groups
}

/// A card's geometry: card-relative logical rects shared by the draw and the
/// hit-test, plus the body's pre-wrapped lines (breaks computed once, at
/// logical size, so hit-test and draw agree at every scale).
pub struct CardLayout {
    pub size: Size<f64, Logical>,
    pub source_icon: Rectangle<f64, Logical>,
    pub close: Rectangle<f64, Logical>,
    /// The expand-caret slot, before the close button; `None` on the banner,
    /// which hides the button (`js/ui/messageTray.js:1137`). List cards always
    /// reserve the slot (GNOME parks it at opacity 0 to avoid relayout,
    /// `js/ui/messageList.js:531-538`); the chevron only draws/hits when
    /// [`can_expand`](Self::can_expand).
    pub expand: Option<Rectangle<f64, Logical>>,
    /// The caret is live: the collapsed body is ellipsized, or there are
    /// action buttons to reveal, or the card is already expanded
    /// (`js/ui/messageList.js:531-538`).
    pub can_expand: bool,
    pub expanded: bool,
    /// The 48px body-icon slot; `None` when the notification has no icon.
    pub body_icon: Option<Rectangle<f64, Logical>>,
    /// Body text, one entry per visual line: a single ellipsized line
    /// collapsed, up to [`EXPAND_LINES`] wrapped lines expanded
    /// (`LabelExpanderLayout`, `js/ui/messageList.js:220-275`).
    pub body_lines: Vec<String>,
    /// Empty unless expanded — the action row stays hidden until expand
    /// (`js/ui/messageList.js:598-601,620`).
    pub actions: Vec<Rectangle<f64, Logical>>,
    /// Vertical centre of the header row — where the source title and time sit.
    ///
    /// These last three are here so the *painter* stops re-deriving them. It used to spell out
    /// `PAD + HEADER_H / 2.`, `PAD * 2. + ...` and so on itself, which is duplicated arithmetic
    /// that drifts silently: when the card grew its 1px border and the header row was resized to
    /// the close button's margin box, `layout_clamped` learned both and the painter learned
    /// neither, so every glyph sat a pixel left of the icons it lines up with. One owner.
    pub header_cy: f64,
    /// Left edge of the body text column (past the 48px icon when there is one).
    pub text_x: f64,
    /// Top of the body row.
    pub body_y: f64,
}

/// Lay out a card of `width`. `expanded` grows the body to its wrapped lines
/// and reveals the action row; `expand_button` reserves the header caret slot
/// (list cards true, the banner false).
pub fn layout(
    content: &CardContent,
    width: f64,
    expanded: bool,
    expand_button: bool,
) -> CardLayout {
    layout_clamped(content, width, expanded, expand_button, EXPAND_LINES)
}

/// [`layout`] with the expanded line budget clamped below [`EXPAND_LINES`] —
/// the no-scroll message list caps an expanded card to the space left above
/// its controls row (gnome-shell scrolls instead; recorded divergence).
///
/// This is not pure arithmetic — a non-empty body is wrapped and measured, and
/// callers re-layout per hit-test and per render-element collection, so a pointer
/// crossing the message list runs this over every visible card on every motion
/// event. Both text calls are memoized in `synoik_vk::text` on their full argument
/// lists (`wrap_cache`, `measure_cache`), so a repeat layout of an unchanged card
/// shapes nothing; what is left here is arithmetic and a few small allocations.
pub fn layout_clamped(
    content: &CardContent,
    width: f64,
    expanded: bool,
    expand_button: bool,
    max_lines: usize,
) -> CardLayout {
    let header_y = BORDER + PAD;
    let body_y = header_y + header_band() + PAD;
    let show_actions = expanded && !content.actions.is_empty();
    let actions_h = if show_actions { BTN_H + PAD } else { 0. };

    let source_icon = Rectangle::new(
        Point::from((
            BORDER + PAD * 2.,
            header_y + (header_band() - SMALL_ICON) / 2.,
        )),
        Size::from((SMALL_ICON, SMALL_ICON)),
    );
    // Close sits card-padding (6) + its balancing margin (3) from the right
    // edge (`.message-header:ltr { padding-right: 0 }` + `.message` padding +
    // `margin: $base_padding * 0.5`, `_message-list.scss:83,106-108,152-155`).
    let close = Rectangle::new(
        Point::from((
            width - BORDER - PAD - CLOSE_MARGIN - CLOSE_D,
            header_y + (header_band() - CLOSE_D) / 2.,
        )),
        Size::from((CLOSE_D, CLOSE_D)),
    );
    let expand = expand_button.then(|| {
        Rectangle::new(
            Point::from((close.loc.x - EXPAND_GAP - CLOSE_D, close.loc.y)),
            close.size,
        )
    });
    let body_icon = content.icon.is_some().then(|| {
        Rectangle::new(
            Point::from((BORDER + PAD * 2., body_y)),
            Size::from((BODY_ICON, BODY_ICON)),
        )
    });

    // The body column: wrapped to the space right of the icon, minus the
    // card's edge padding.
    let text_x = BORDER + PAD * 2. + body_icon.map_or(0., |_| BODY_ICON + PAD + ICON_MARGIN);
    let text_w = (width - text_x - PAD - BORDER).max(1.);
    let body_lines = if content.body.is_empty() {
        Vec::new()
    } else {
        let lines = if expanded {
            max_lines.clamp(1, EXPAND_LINES)
        } else {
            1
        };
        // A body is prose: GNOME wraps it `WORD_CHAR` (`_bodyLabel`), so a long word
        // breaks across lines rather than being cut.
        synoik_vk::text::wrap_lines_weighted(
            &content.body,
            title_px() as f32,
            false,
            text_w,
            lines,
            true,
        )
    };
    let can_expand = expand_button
        && (expanded
            || !content.actions.is_empty()
            || (!content.body.is_empty()
                && synoik_vk::text::measure_line_width_weighted(
                    &content.body,
                    title_px() as f32,
                    false,
                ) > text_w));

    // Collapsed height is the 48px icon row; every wrapped line past the
    // first grows it (one line expanded == collapsed, so toggling a short
    // body only reveals the action row).
    let body_h = BODY_ICON + (body_lines.len().saturating_sub(1)) as f64 * LINE_H;
    // `.message-box`'s bottom padding, then the card's own, then the border.
    let h = body_y + body_h + PAD * 2. + BORDER + actions_h;

    let mut actions = Vec::new();
    if show_actions {
        let n = content.actions.len() as f64;
        let total_w = width - 2. * (BORDER + PAD) - BTN_GAP * (n - 1.);
        let btn_w = total_w / n;
        let y = body_y + body_h + PAD;
        for i in 0..content.actions.len() {
            actions.push(Rectangle::new(
                Point::from((BORDER + PAD + i as f64 * (btn_w + BTN_GAP), y)),
                Size::from((btn_w, BTN_H)),
            ));
        }
    }

    CardLayout {
        size: Size::from((width, h)),
        source_icon,
        close,
        expand,
        can_expand,
        expanded,
        body_icon,
        body_lines,
        actions,
        header_cy: header_y + header_band() / 2.,
        text_x,
        body_y,
    }
}

/// Rasterize the card: rounded background, themed-icon circle, close-button
/// circle, action-button pills, and all the text. Icons composite on top
/// (see [`card_elements`]).
pub fn draw_card(
    renderer: &mut VulkanRenderer,
    scale: f64,
    content: &CardContent,
    layout: &CardLayout,
    radius: f64,
    // Whether the pointer is over this card at all (the card body darkens) and,
    // if over one of its buttons, which one (that button lightens on top).
    card_hovered: bool,
    button: Option<CardZone>,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("notification_card::draw_card");

    // Shape every run up front (needs `&mut renderer`, which the frame holds, so
    // it must precede the bake). `TextShaper` owns the single pt → physical-px
    // multiply — no `* scale` at this call site.
    let bold = TextStyle::new(TITLE_PT).bold();
    let plain = TextStyle::new(TITLE_PT);
    let (source_run, time_run, title_run, body_runs, action_runs) = {
        let mut shaper = TextShaper::new(renderer, scale);
        let source_run = shaper.shape(&content.source_title, bold)?;
        let time_run = shaper.shape(&content.time_text, TextStyle::new(TIME_PT))?;
        let title_run = shaper.shape(&content.title, bold)?;
        let body_runs: Vec<ShapedText> = layout
            .body_lines
            .iter()
            .map(|line| shaper.shape(line, plain))
            .collect::<Result<_, _>>()?;
        let action_runs: Vec<ShapedText> = content
            .actions
            .iter()
            .take(layout.actions.len())
            .map(|(_, label)| shaper.shape(label, bold))
            .collect::<Result<_, _>>()?;
        (source_run, time_run, title_run, body_runs, action_runs)
    };

    widget::bake_uncached(renderer, scale, layout.size, |frame, size| {
        let mut p = Painter::new(frame, scale, size);

        // Transparent beyond the rounded corners; the card is an SDF fill. A
        // hovered card body darkens (`%card:hover`).
        p.clear(TRANSPARENT)?;
        let card_bg = if card_hovered { CARD_HOVER_BG } else { CARD_BG };
        p.fill_rounded(Rectangle::from_size(layout.size), radius, card_bg)?;

        // Close-button circle, and the expand caret's when it's live. A hovered
        // button lightens (`%notification_button:hover`), behind its glyph.
        let btn_bg = |zone: CardZone| {
            if button == Some(zone) {
                BTN_BG_HOVER
            } else {
                BTN_BG
            }
        };
        p.fill_rounded(layout.close, CLOSE_D / 2., btn_bg(CardZone::Close))?;
        if let Some(expand) = layout.expand.filter(|_| layout.can_expand) {
            p.fill_rounded(expand, CLOSE_D / 2., btn_bg(CardZone::Caret))?;
        }

        // Themed body icons sit on the `.message-themed-icon` circle.
        if let Some(body_icon) = layout.body_icon {
            if matches!(content.icon, Some(NotificationIcon::Themed(_))) {
                p.fill_rounded(body_icon, BODY_ICON / 2., CIRCLE_BG)?;
            }
        }

        // Header: bold source title after the icon, time right-aligned before the caret slot
        // (or the close button on the banner). The title is clipped to stop short of the time,
        // whose logical ink width sets the clip's right edge.
        let header_cy = layout.header_cy;
        // The title follows the source icon the layout already placed, by `.message-header`'s
        // spacing — deriving it from the card edge instead is how it lost the border.
        let title_x = layout.source_icon.loc.x + SMALL_ICON + PAD;
        let time_anchor = layout.expand.map_or(layout.close.loc.x, |e| e.loc.x);
        let time_w = time_run.ink_bounds().2 as f64 / scale;
        let time_x = time_anchor - PAD - time_w;
        let header_clip = Rectangle::new(
            Point::from((title_x, 0.)),
            Size::from(((time_x - title_x).max(0.), layout.size.h)),
        );
        p.text_clipped(
            &source_run,
            Point::from((title_x, header_cy)),
            Align::LEFT_MIDDLE,
            HEADER_FG,
            header_clip,
        )?;
        p.text(
            &time_run,
            Point::from((time_anchor - PAD, header_cy)),
            Align::RIGHT_MIDDLE,
            HEADER_FG,
        )?;

        // Body: bold title over the pre-wrapped body lines (a single
        // ellipsized line collapsed, up to six expanded).
        let body_y = layout.body_y;
        let text_x = layout.text_x;
        let text_clip = Rectangle::new(
            Point::from((text_x, 0.)),
            Size::from((
                (layout.size.w - text_x - PAD - BORDER).max(0.),
                layout.size.h,
            )),
        );
        // Single-line body: the title vertically centers on the 48px icon row; multi-line: the
        // title sits above the first body line and each line steps down by LINE_H.
        let title_cy = if body_runs.is_empty() {
            body_y + BODY_ICON / 2.
        } else {
            body_y + 14.
        };
        p.text_clipped(
            &title_run,
            Point::from((text_x, title_cy)),
            Align::LEFT_MIDDLE,
            TEXT,
            text_clip,
        )?;
        for (i, body_run) in body_runs.iter().enumerate() {
            p.text_clipped(
                body_run,
                Point::from((text_x, body_y + 33. + i as f64 * LINE_H)),
                Align::LEFT_MIDDLE,
                TEXT,
                text_clip,
            )?;
        }

        // Action buttons: pills with centered bold labels.
        for (i, (rect, run)) in layout.actions.iter().zip(&action_runs).enumerate() {
            p.fill_rounded(*rect, BTN_RADIUS, btn_bg(CardZone::Action(i)))?;
            let center =
                Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));
            p.text_clipped(run, center, Align::CENTER, TEXT, *rect)?;
        }

        Ok(())
    })
}

/// Card textures cached per `(scale, key)` and uploaded pixel icons per `key`.
/// The owner picks the keys (the banner keys by its content revision, the list
/// by revision + card index) and evicts stale ones via [`retain`](Self::retain).
pub struct CardCache {
    context: Option<ContextId<VkTexture>>,
    cards: HashMap<(NotNan<f64>, u64), VkTexture>,
    pixels: HashMap<u64, TextureBuffer<VkTexture>>,
    /// Uploads of images loaded *from a file* (album art), keyed by card key **and source path**.
    /// The path is in the key on purpose: unlike `pixels`, whose bytes arrive inside the card
    /// content, these are fetched behind the content's back, so a card whose key survived a change
    /// of image must not be served the previous one.
    images: crate::ui::widget::ImageUploads,
    /// Uploads of full-colour source icons. `pub(crate)` so the media card, which shares this
    /// cache, can hand it to [`source_icon_element`].
    ///
    /// Deliberately *not* wired to the shell-wide [`widget::SharedAppIconUploads`], unlike the
    /// dash/grid/search: that map is keyed by size, and those draw at 64 and 96 while a header
    /// icon is 16, so there is nothing to reuse. It needs no pruning either — the key space is one
    /// entry per notification *source*, not per notification.
    pub(crate) app_icons: crate::ui::widget::SharedAppIconUploads,
}

impl CardCache {
    pub fn new() -> Self {
        Self {
            context: None,
            cards: HashMap::new(),
            pixels: HashMap::new(),
            images: crate::ui::widget::ImageUploads::new(),
            app_icons: Default::default(),
        }
    }

    /// Drop every entry whose key fails `keep` (card textures, pixel uploads and
    /// image uploads all share the key space).
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.cards.retain(|(_, key), _| keep(*key));
        self.pixels.retain(|key, _| keep(*key));
        self.images.retain_slots(&keep);
    }

    /// The image-upload slots, for a card kind that composites a file-backed image
    /// ([`crate::ui::widget::image_element`]).
    pub fn images(&mut self) -> &mut crate::ui::widget::ImageUploads {
        &mut self.images
    }

    /// Drop textures if the renderer context changed (callers building their
    /// own textures — e.g. the group header — must call this too).
    pub fn ensure_context(&mut self, renderer: &VulkanRenderer) {
        let context = renderer.context_id();
        if self.context.as_ref() != Some(&context) {
            self.cards.clear();
            self.pixels.clear();
            self.images.clear();
            self.context = Some(context);
        }
    }

    /// Whether a `(scale, key)` card texture is cached (for owners that draw
    /// their own textures into the shared slot, like the group header).
    pub fn has_card(&self, scale_key: NotNan<f64>, key: u64) -> bool {
        self.cards.contains_key(&(scale_key, key))
    }

    pub fn insert_card(&mut self, scale_key: NotNan<f64>, key: u64, texture: VkTexture) {
        self.cards.insert((scale_key, key), texture);
    }

    pub fn get_card(&self, scale_key: NotNan<f64>, key: u64) -> Option<VkTexture> {
        self.cards.get(&(scale_key, key)).cloned()
    }
}

impl Default for CardCache {
    fn default() -> Self {
        Self::new()
    }
}

/// `MessageHeader.sourceIcon`'s `fallback_icon_name` (`js/ui/messageList.js:359`).
const SOURCE_FALLBACK: &str = "application-x-executable-symbolic";

/// Which symbolic icon the card's header shows for `content`'s source.
///
/// `.message-source-icon` is `-st-icon-style: symbolic` (`_message-list.scss:111-114`), and that
/// is not decoration: it puts `ST_ICON_LOOKUP_FORCE_SYMBOLIC` on the lookup, which rewrites every
/// non-symbolic name to `<name>-symbolic` and tries **all** of those before falling back to the
/// bare names (`st-icon-theme.c:1489-1503`). Only if none resolve does St draw
/// `fallback_icon_name`.
///
/// Missing that rewrite is why the header icon was *blank* rather than wrong: apps send
/// `app_icon` as a plain name — `dialog-information`, `firefox` — and our resolver is
/// symbolic-only, so it looked for `dialog-information.svg`, which Adwaita does not ship (it ships
/// `dialog-information-symbolic.svg`). The old code then used the fallback *only* when there was
/// no name at all, so a name that failed to resolve drew nothing and left the reserved 16px empty.
///
/// St's second pass — the bare, non-symbolic name — needs full-colour theme lookup (real
/// `index.theme` inheritance, size dirs, PNG decode), which is [`AppIconCache`]'s job rather than
/// the symbolic cache's. That is [`SourceIcon::Color`]: Firefox and Chromium ship only
/// `firefox.png` / `chromium.png`, so before it they both landed on the executable glyph.
enum SourceIcon {
    /// A symbolic icon, recoloured to the header foreground.
    Symbolic(String),
    /// The app's own icon, in its own colours.
    Color(AppIconRef),
}

/// The gicon a notification card's header shows: `app?.get_icon() ?? appIcon`
/// (`js/ui/notificationDaemon.js:398`) — the resolved app's icon wins, and the `app_icon` call
/// parameter is only the fallback.
fn card_gicon(content: &CardContent) -> Option<AppIconRef> {
    content.source_app_icon.clone().or_else(|| {
        match &content.source_icon {
            Some(NotificationIcon::Themed(name)) => Some(AppIconRef::Themed(vec![name.clone()])),
            // A file-backed `app_icon` was already decoded to pixels upstream; there is no name to
            // look up, and no themed icon to prefer.
            _ => None,
        }
    })
}

fn source_icon(
    icons: &IconCache,
    app_icons: &AppIconCache,
    gicon: Option<AppIconRef>,
    scale: f64,
) -> SourceIcon {
    let Some(gicon) = gicon else {
        return SourceIcon::Symbolic(SOURCE_FALLBACK.to_owned());
    };
    let names: &[String] = match &gicon {
        AppIconRef::Themed(names) => names,
        _ => &[],
    };

    // Every probe is `provides`, never a texture/buffer probe: a texture miss also means "queued"
    // (`IconCache::provides`), and `AppIconCache::buffer` never misses at all — it substitutes its
    // own full-colour fallback, which is not the symbolic one St would use here.
    //
    // Two passes over the whole name list, not one name tried both ways: FORCE_SYMBOLIC appends
    // `-symbolic` to *every* name and tries all of those before *any* bare name
    // (`st-icon-theme.c:1489-1503`).
    for name in names {
        if name.ends_with("-symbolic") {
            if icons.provides(name) {
                return SourceIcon::Symbolic(name.clone());
            }
        } else {
            let symbolic = format!("{name}-symbolic");
            if icons.provides(&symbolic) {
                return SourceIcon::Symbolic(symbolic);
            }
        }
    }
    for name in names {
        if icons.provides(name) {
            return SourceIcon::Symbolic(name.clone());
        }
    }
    if app_icons.provides(&gicon, SMALL_ICON, scale) {
        return SourceIcon::Color(gicon);
    }
    SourceIcon::Symbolic(SOURCE_FALLBACK.to_owned())
}

/// The header source-icon element for `gicon`, resolved and drawn the way a symbolic-styled
/// `St.Icon` would — see [`source_icon`].
///
/// Shared with the MPRIS media card, whose header is the same `.message-header` widget
/// (`js/ui/mpris.js` builds a `MessageHeader` too): it had its own symbolic-only spelling of this
/// and so showed the executable glyph for every player.
#[allow(clippy::too_many_arguments)]
pub fn source_icon_element(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    app_icons: &AppIconCache,
    uploads: &mut widget::AppIconUploads,
    gicon: Option<AppIconRef>,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    match source_icon(icons, app_icons, gicon, scale) {
        SourceIcon::Symbolic(name) => {
            let tb = icons.texture(renderer, &name, SMALL_ICON, scale, HEADER_FG)?;
            let logical = tb.logical_size();
            let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
            Some(TextureRenderElement::from_texture_buffer(
                tb,
                loc,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ))
        }
        SourceIcon::Color(icon) => widget::app_icon_element(
            renderer, uploads, app_icons, &icon, SMALL_ICON, scale, origin, center, alpha,
        ),
    }
}

/// The render elements of one card at `origin`: the (cached) card texture plus
/// the icons that composite on top — source icon, close glyph, and the body
/// icon (themed symbolic on the card's circle, or the app's pixel image).
#[allow(clippy::too_many_arguments)]
pub fn card_elements(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    app_icons: &AppIconCache,
    cache: &mut CardCache,
    key: u64,
    content: &CardContent,
    layout: &CardLayout,
    radius: f64,
    origin: Point<f64, Logical>,
    alpha: f32,
    scale: f64,
    card_hovered: bool,
    button: Option<CardZone>,
) -> Vec<TextureRenderElement<VkTexture>> {
    // The returned Vec is in the output stacking order: FIRST = topmost. The
    // icons composite over the card, so they go in first and the card texture
    // last (the quick-settings menu pushes icons before its chrome the same
    // way).
    let mut elements = Vec::new();
    let Ok(scale_key) = NotNan::new(scale) else {
        return elements;
    };
    cache.ensure_context(renderer);

    // Icons composite on top of the card, from the shared icon cache.
    let icon_at = |renderer: &mut VulkanRenderer,
                   name: &str,
                   size_l: f64,
                   color: [f32; 4],
                   center: Point<f64, Logical>|
     -> Option<TextureRenderElement<VkTexture>> {
        let tb = icons.texture(renderer, name, size_l, scale, color)?;
        let logical = tb.logical_size();
        let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
        Some(TextureRenderElement::from_texture_buffer(
            tb,
            loc,
            alpha,
            None,
            None,
            Kind::Unspecified,
        ))
    };

    // Source icon (16px symbolic, header fg); themed only — a pixel source
    // icon reuses the body-icon path in a later slice.
    let source_center = Point::from((
        layout.source_icon.loc.x + layout.source_icon.size.w / 2.,
        layout.source_icon.loc.y + layout.source_icon.size.h / 2.,
    ));
    if let Some(elem) = source_icon_element(
        renderer,
        icons,
        app_icons,
        &mut cache.app_icons.borrow_mut(),
        card_gicon(content),
        scale,
        origin,
        source_center,
        alpha,
    ) {
        elements.push(elem);
    }

    // Close button glyph.
    let close_center = Point::from((
        layout.close.loc.x + layout.close.size.w / 2.,
        layout.close.loc.y + layout.close.size.h / 2.,
    ));
    if let Some(elem) = icon_at(
        renderer,
        "window-close-symbolic",
        SMALL_ICON,
        TEXT,
        close_center,
    ) {
        elements.push(elem);
    }

    // Expand caret: a chevron that flips when expanded (GNOME rotates the
    // button actor 180°, `js/ui/messageList.js:635-638`; ours is a baked
    // rotated SVG). Only drawn while live (`can_expand`).
    if let Some(expand) = layout.expand.filter(|_| layout.can_expand) {
        let center = Point::from((
            expand.loc.x + expand.size.w / 2.,
            expand.loc.y + expand.size.h / 2.,
        ));
        let name = if layout.expanded {
            "notification-collapse-symbolic"
        } else {
            "notification-expand-symbolic"
        };
        if let Some(elem) = icon_at(renderer, name, SMALL_ICON, TEXT, center) {
            elements.push(elem);
        }
    }

    // Body icon: pixels (app image) or a symbolic on the circle the card
    // already drew (`.message-themed-icon`).
    if let Some(body_icon) = layout.body_icon {
        let center = Point::from((
            body_icon.loc.x + body_icon.size.w / 2.,
            body_icon.loc.y + body_icon.size.h / 2.,
        ));
        match &content.icon {
            Some(NotificationIcon::Themed(name)) => {
                if let Some(elem) = icon_at(renderer, name, SMALL_ICON, TEXT, center) {
                    elements.push(elem);
                }
            }
            Some(NotificationIcon::Pixels(pix)) if pix.width > 0 && pix.height > 0 => {
                #[allow(clippy::map_entry)]
                if !cache.pixels.contains_key(&key) {
                    // Display at 48 logical on the long side: the buffer
                    // scale maps pixels to logical size.
                    let long = pix.width.max(pix.height) as f64;
                    let tb = TextureBuffer::from_memory(
                        renderer,
                        &pix.rgba,
                        Fourcc::Abgr8888,
                        Size::<i32, BufferCoord>::from((pix.width as i32, pix.height as i32)),
                        false,
                        long / BODY_ICON,
                        Transform::Normal,
                        Vec::new(),
                    );
                    match tb {
                        Ok(tb) => {
                            cache.pixels.insert(key, tb);
                        }
                        Err(err) => {
                            warn!("error uploading notification image: {err:#}");
                        }
                    }
                }
                if let Some(tb) = cache.pixels.get(&key) {
                    let logical = tb.logical_size();
                    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
            }
            _ => (),
        }
    }

    // The card texture itself, below every icon.
    #[allow(clippy::map_entry)]
    if !cache.cards.contains_key(&(scale_key, key)) {
        match draw_card(
            renderer,
            scale,
            content,
            layout,
            radius,
            card_hovered,
            button,
        ) {
            Ok(texture) => {
                cache.cards.insert((scale_key, key), texture);
            }
            Err(err) => {
                warn!("error rendering a notification card: {err:#}");
            }
        }
    }
    if let Some(card) = cache.cards.get(&(scale_key, key)).cloned() {
        let buffer =
            TextureBuffer::from_texture(renderer, card, scale, Transform::Normal, Vec::new());
        elements.push(TextureRenderElement::from_texture_buffer(
            buffer,
            origin,
            alpha,
            None,
            None,
            Kind::Unspecified,
        ));
    }

    elements
}

/// The darkened background color for a card at `depth` in a collapsed stack:
/// depth 1 = `second-in-stack`, depth ≥2 = `lower-in-stack`
/// (`_message-list.scss:89-98`). Depth 0 (the top card) uses the normal fill.
pub fn stack_bg(depth: usize) -> [f32; 4] {
    match depth {
        0 => CARD_BG,
        1 => STACK_SECOND_BG,
        _ => STACK_LOWER_BG,
    }
}

/// A lower card peeking under a collapsed stack shows only its inset, offset
/// bottom edge — which is card background, no content — so it renders as a
/// cached darkened rounded rect rather than a full card. Returns the composited
/// element (below the cards above it).
#[allow(clippy::too_many_arguments)]
pub fn stack_shadow_element(
    renderer: &mut VulkanRenderer,
    cache: &mut CardCache,
    key: u64,
    size: Size<f64, Logical>,
    radius: f64,
    bg: [f32; 4],
    origin: Point<f64, Logical>,
    scale: f64,
) -> Option<TextureRenderElement<VkTexture>> {
    let scale_key = NotNan::new(scale).ok()?;
    cache.ensure_context(renderer);
    #[allow(clippy::map_entry)]
    if !cache.cards.contains_key(&(scale_key, key)) {
        match draw_solid_rounded(renderer, scale, size, radius, bg) {
            Ok(texture) => {
                cache.cards.insert((scale_key, key), texture);
            }
            Err(err) => {
                warn!("error rendering a stack shadow: {err:#}");
                return None;
            }
        }
    }
    let texture = cache.cards.get(&(scale_key, key))?.clone();
    let buffer =
        TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());
    Some(TextureRenderElement::from_texture_buffer(
        buffer,
        origin,
        1.,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// Draw a solid rounded-rect fill into a fresh sampleable texture (the stacked
/// cards' darkened peeks; the corners outside the radius stay transparent).
fn draw_solid_rounded(
    renderer: &mut VulkanRenderer,
    scale: f64,
    size: Size<f64, Logical>,
    radius: f64,
    bg: [f32; 4],
) -> anyhow::Result<VkTexture> {
    widget::bake_uncached(renderer, scale, size, |frame, phys| {
        let mut p = Painter::new(frame, scale, phys);
        p.clear(TRANSPARENT)?;
        p.fill_rounded(Rectangle::from_size(size), radius, bg)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::NotifyRequest;

    fn req(app: &str, sender: &str) -> NotifyRequest {
        NotifyRequest {
            sender: Some(sender.to_owned()),
            pid: 100,
            app_name: app.to_owned(),
            replaces_id: 0,
            desktop_entry: None,
            source_icon: None,
            app_icon: None,
            title: "title".to_owned(),
            body: "body".to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            urgency: Urgency::Normal,
            resident: false,
            transient: false,
        }
    }

    /// The expansion geometry rules (`js/ui/messageList.js:531-666`): the
    /// caret is live iff the body is ellipsized / there are actions / already
    /// expanded; actions only lay out when expanded; every wrapped line past
    /// the first grows the card by LINE_H; the banner variant has no caret
    /// slot (`js/ui/messageTray.js:1137`).
    #[test]
    fn layout_expansion_rules() {
        let mut content = CardContent {
            id: 1,
            source_title: "app".to_owned(),
            source_icon: None,
            source_app_icon: None,
            title: "title".to_owned(),
            body: "short".to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            critical: false,
            time_text: "Just now".to_owned(),
        };
        let w = 400.;

        // Short body, no actions: nothing to expand.
        let collapsed = layout(&content, w, false, true);
        assert!(collapsed.expand.is_some(), "the caret slot is reserved");
        assert!(!collapsed.can_expand);
        assert!(collapsed.actions.is_empty());
        assert_eq!(collapsed.body_lines, vec!["short"]);

        // Actions alone make the caret live; expanding reveals them and only
        // them (one body line: no height change from text).
        content.actions = vec![("ok".to_owned(), "OK".to_owned())];
        let collapsed = layout(&content, w, false, true);
        assert!(collapsed.can_expand);
        assert!(collapsed.actions.is_empty());
        let expanded = layout(&content, w, true, true);
        assert_eq!(expanded.actions.len(), 1);
        assert_eq!(
            expanded.size.h,
            collapsed.size.h + BTN_H + PAD,
            "one-line body: expanding only adds the action row"
        );

        // A long body ellipsizes collapsed and wraps expanded, growing the
        // card by LINE_H per extra line, capped at EXPAND_LINES.
        content.actions = Vec::new();
        content.body = "word ".repeat(120).trim_end().to_owned();
        let collapsed = layout(&content, w, false, true);
        assert!(collapsed.can_expand);
        assert_eq!(collapsed.body_lines.len(), 1);
        assert!(collapsed.body_lines[0].ends_with('\u{2026}'));
        let expanded = layout(&content, w, true, true);
        assert_eq!(expanded.body_lines.len(), EXPAND_LINES);
        assert!(expanded.body_lines.last().unwrap().ends_with('\u{2026}'));
        assert_eq!(
            expanded.size.h,
            collapsed.size.h + (EXPAND_LINES - 1) as f64 * LINE_H
        );
        // The no-scroll list can clamp the line budget below EXPAND_LINES.
        let clamped = layout_clamped(&content, w, true, true, 3);
        assert_eq!(clamped.body_lines.len(), 3);

        // The banner variant reserves no caret slot and its time label
        // anchors on the close button.
        let banner = layout(&content, w, false, false);
        assert!(banner.expand.is_none());
        assert!(!banner.can_expand);
    }

    #[test]
    fn message_list_groups_by_source_newest_first_within() {
        let mut store = NotificationStore::default();
        let at = |s: u64| Duration::from_secs(s);
        let (a1, _) = store.notify(req("app-a", ":1.1"), true, at(1)).unwrap();
        let (b1, _) = store.notify(req("app-b", ":1.2"), true, at(2)).unwrap();
        let (a2, _) = store.notify(req("app-a", ":1.1"), true, at(3)).unwrap();

        // Two groups: app-a leads (it notified last), holding both of its
        // notifications newest-first; app-b follows with one.
        let groups = message_list_groups(&store, at(4));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].source_title, "app-a");
        let a_ids: Vec<u32> = groups[0].cards.iter().map(|c| c.id).collect();
        assert_eq!(a_ids, vec![a2, a1], "newest first within the group");
        assert_eq!(
            groups[1].cards.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![b1]
        );
        assert!(groups
            .iter()
            .flat_map(|g| &g.cards)
            .all(|c| c.time_text == "Just now"));

        // A replace mutates in place and must NOT reorder the groups —
        // gnome-shell moves a group only on notification-*added*
        // (`js/ui/messageList.js:1824-1827`, `js/ui/messageTray.js:579-581`).
        let mut replace = req("app-b", ":1.2");
        replace.replaces_id = b1;
        replace.title = "updated".to_owned();
        store.notify(replace, true, at(5)).unwrap();
        let regrouped = message_list_groups(&store, at(6));
        let titles: Vec<&str> = regrouped.iter().map(|g| g.source_title.as_str()).collect();
        assert_eq!(titles, vec!["app-a", "app-b"], "a replace never reorders");
    }

    #[test]
    fn message_list_groups_pin_urgent_first_and_criticals_within() {
        let mut store = NotificationStore::default();
        let at = |s: u64| Duration::from_secs(s);
        // A normal group, then a group that gains a critical: the critical
        // group must sort ahead, and its critical card must lead the group.
        store.notify(req("calm", ":1.1"), true, at(1)).unwrap();
        let (loud1, _) = store.notify(req("loud", ":1.2"), true, at(2)).unwrap();
        let mut crit = req("loud", ":1.2");
        crit.urgency = Urgency::Critical;
        let (loud_crit, _) = store.notify(crit, true, at(3)).unwrap();

        let groups = message_list_groups(&store, at(4));
        assert_eq!(groups.len(), 2);
        assert!(groups[0].has_urgent, "the urgent group is pinned first");
        assert_eq!(groups[0].source_title, "loud");
        let ids: Vec<u32> = groups[0].cards.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![loud_crit, loud1],
            "criticals lead within the group"
        );
        assert!(!groups[1].has_urgent);
    }
}

#[cfg(test)]
mod live_shell_tests {
    use super::*;

    /// A card with a 48px icon matches what a live GNOME 50.3 shell allocates: **513 x 108**.
    ///
    /// Decomposed from a mapped actor dump: border 1, padding 6, then header 34 and a
    /// message-box of 60 (`6 + 48 icon + 6`), then padding 6 and border 1. Two things were wrong
    /// and both made the card shorter: the header row was taken as `.message-header-content` (30)
    /// rather than the close button's margin box (34), and the 1px border was never reserved.
    ///
    /// The earlier "`.message` is already correct at 102" reading came from an iconless
    /// notification, where the shell's content row is text-height rather than 48. Like for like,
    /// we were 6px short.
    #[test]
    fn an_iconned_card_matches_the_live_shell() {
        assert_eq!(
            header_band(),
            34.,
            "the header row is the close button's margin box"
        );

        let content = CardContent {
            id: 1,
            source_title: "Audit".into(),
            source_icon: None,
            source_app_icon: None,
            title: "Audit probe with icon".into(),
            body: "A body long enough to occupy the content column next to the icon.".into(),
            icon: Some(crate::notifications::NotificationIcon::Themed(
                "firefox".into(),
            )),
            actions: Vec::new(),
            has_default_action: false,
            critical: false,
            time_text: "now".into(),
        };
        let l = layout(&content, 513., false, true);
        assert_eq!(
            l.size,
            Size::from((513., 108.)),
            "513x108 live, with a 48px icon"
        );

        // Content is inset by border+padding on every edge, not padding alone.
        assert_eq!(l.source_icon.loc.x, BORDER + PAD * 2.);
        assert_eq!(
            l.close.loc.x + l.close.size.w,
            513. - BORDER - PAD - CLOSE_MARGIN,
            "the close button's margin box stops inside the border"
        );
    }

    /// The header's source icon resolves the way `-st-icon-style: symbolic` makes St resolve it:
    /// `<name>-symbolic` first, the bare name second, `fallback_icon_name` last.
    ///
    /// This is the rule whose absence left the header icon **blank** on the live seat — apps send
    /// plain names (`dialog-information`, `firefox`), our resolver is symbolic-only, and the
    /// fallback was reached only when there was no name at all. A name that failed to resolve
    /// drew nothing into the 16px the layout had already reserved.
    ///
    /// Deliberately probed with our **embedded** icons: they exist in no theme on disk
    /// (`embedded_icon`), so this asserts the rewrite itself rather than whatever Adwaita happens
    /// to ship on the machine running the test.
    #[test]
    fn the_source_icon_resolves_like_a_symbolic_styled_st_icon() {
        let icons = IconCache::new("Adwaita");
        let app_icons = AppIconCache::new("Adwaita");
        let with = |icon: Option<&str>| {
            let content = CardContent {
                id: 1,
                source_title: "Probe".into(),
                source_icon: icon.map(|n| crate::notifications::NotificationIcon::Themed(n.into())),
                source_app_icon: None,
                title: String::new(),
                body: String::new(),
                icon: None,
                actions: Vec::new(),
                has_default_action: false,
                critical: false,
                time_text: "now".into(),
            };
            match source_icon(&icons, &app_icons, card_gicon(&content), 1.) {
                SourceIcon::Symbolic(name) => name,
                SourceIcon::Color(icon) => format!("color:{icon:?}"),
            }
        };

        // The bug: a plain name is rewritten to its symbolic variant, which is what exists.
        assert_eq!(with(Some("no-notifications")), "no-notifications-symbolic");
        // Already symbolic: used as-is, not double-suffixed.
        assert_eq!(
            with(Some("no-notifications-symbolic")),
            "no-notifications-symbolic"
        );
        // No symbolic either way, and nothing full-colour either: St's fallback.
        assert_eq!(with(Some("synoik-no-such-icon")), SOURCE_FALLBACK);
        assert_eq!(with(None), SOURCE_FALLBACK);

        // The resolved app's icon WINS over the `app_icon` parameter, and takes the same
        // symbolic-first rewrite — `get icon() { app?.get_icon() ?? appIcon }`
        // (`notificationDaemon.js:398`) hands St one gicon and the widget's style decides the
        // rest. Without the precedence a browser's web notification, whose `app_icon` is empty,
        // never reaches its app's icon at all.
        let with_app = |app: &[&str], param: Option<&str>| {
            let content = CardContent {
                id: 1,
                source_title: "Probe".into(),
                source_icon: param
                    .map(|n| crate::notifications::NotificationIcon::Themed(n.into())),
                source_app_icon: Some(AppIconRef::Themed(
                    app.iter().map(|n| (*n).to_owned()).collect(),
                )),
                title: String::new(),
                body: String::new(),
                icon: None,
                actions: Vec::new(),
                has_default_action: false,
                critical: false,
                time_text: "now".into(),
            };
            match source_icon(&icons, &app_icons, card_gicon(&content), 1.) {
                SourceIcon::Symbolic(name) => name,
                SourceIcon::Color(icon) => format!("color:{icon:?}"),
            }
        };
        assert_eq!(
            with_app(&["no-notifications"], Some("message-indicator")),
            "no-notifications-symbolic",
            "the app's icon beats the app_icon parameter"
        );
        // FORCE_SYMBOLIC tries `-symbolic` for EVERY name before ANY bare name, so a later name
        // with a symbolic variant beats an earlier one without (`st-icon-theme.c:1489-1503`).
        assert_eq!(
            with_app(&["synoik-no-such-icon", "group-collapse"], None),
            "group-collapse-symbolic"
        );

        // Not asserted: the `SourceIcon::Color` branch. Reaching it needs a name with a
        // non-symbolic file and NO symbolic one — `firefox` here — which is a property of the
        // machine's installed themes, not of this code. Live-verified instead (the Firefox logo
        // draws in the header); the deterministic half is the ordering above.
    }
}
