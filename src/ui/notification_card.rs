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
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::notifications::{Notification, NotificationIcon, NotificationStore, Source, Urgency};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::to_physical_precise_round;

/// `.message` padding = `$base_padding` (`_message-list.scss:83`).
pub const PAD: f64 = 6.;
/// `.message-header-content` min-height (`_message-list.scss:118`).
pub const HEADER_H: f64 = 24.;
/// `.message-icon` icon-size = 48px (`_message-list.scss:168`).
pub const BODY_ICON: f64 = 48.;
/// Header/source/close icons (`$scalable_icon_size`).
pub const SMALL_ICON: f64 = 16.;
/// The circular close button: 16px icon + 6px padding (`_message-list.scss:139-156`).
pub const CLOSE_D: f64 = SMALL_ICON + 2. * PAD;
/// Action button height: bold 11pt + 6px paddings (`%notification_button`).
pub const BTN_H: f64 = 28.;
const BTN_RADIUS: f64 = 8.;
/// Gap between action buttons (`$base_margin`).
const BTN_GAP: f64 = 4.;
const TITLE_PX: f64 = crate::ui::pt_to_px(11.);
const TIME_PX: f64 = crate::ui::pt_to_px(9.);
/// An expanded body shows at most this many wrapped lines
/// (`DEFAULT_EXPAND_LINES`, `js/ui/messageList.js:23`).
pub const EXPAND_LINES: usize = 6;
/// Body line pitch: cosmic-text's `round(px * 1.25)` at the 11pt body size.
pub const LINE_H: f64 = 18.;
/// Gap between the expand and close buttons: header spacing (6) + the close
/// button's balancing margin (3) (`_message-list.scss:101,152-155`).
const EXPAND_GAP: f64 = 9.;

/// `.message` bg, dark variant (`lighten($card_bg_color, 5%)` ≈ `#51515a`).
const CARD_BG: [f32; 4] = [
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
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Close/action button bg (white@15%, `%notification_button`).
const BTN_BG: [f32; 4] = [1., 1., 1., 0.15];
/// `.message-themed-icon` circle bg (white@7%, `_message-list.scss:176`).
const CIRCLE_BG: [f32; 4] = [1., 1., 1., 0.07];
const TRANSPARENT: [f32; 4] = [0., 0., 0., 0.];

/// A display snapshot of one notification (plus its source header), rebuilt
/// from the store on every content change.
#[derive(Debug, Clone, PartialEq)]
pub struct CardContent {
    pub id: u32,
    pub source_title: String,
    pub source_icon: Option<NotificationIcon>,
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

/// The calendar message list's card snapshots, in the store's source order —
/// which already carries gnome-shell's semantics: a source moves to the top
/// only when a notification is ADDED to it (`js/ui/messageList.js:1824-1827`);
/// a replace mutates in place and never reorders groups
/// (`js/ui/messageTray.js:579-581`), and closes don't either. Within a source,
/// newest first (new messages go to a group's top,
/// `js/ui/messageList.js:1120-1123`). (Urgent-group pinning arrives with the
/// grouped stacks slice.)
pub fn message_list_cards(store: &NotificationStore, now: Duration) -> Vec<CardContent> {
    store
        .sources
        .iter()
        .flat_map(|s| s.notifications.iter().rev().map(|n| card_for(s, n, now)))
        .collect()
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
/// TODO(perf): this is no longer pure arithmetic — a non-empty body is shaped
/// (wrap + width measure) under the global measure-font mutex on every call,
/// and callers re-layout per hit-test / per render-element collection. Fine at
/// today's card counts; cache per (body, width, lines) if it ever shows up.
pub fn layout_clamped(
    content: &CardContent,
    width: f64,
    expanded: bool,
    expand_button: bool,
    max_lines: usize,
) -> CardLayout {
    let header_y = PAD;
    let body_y = header_y + HEADER_H + PAD;
    let show_actions = expanded && !content.actions.is_empty();
    let actions_h = if show_actions { BTN_H + PAD } else { 0. };

    let source_icon = Rectangle::new(
        Point::from((PAD * 2., header_y + (HEADER_H - SMALL_ICON) / 2.)),
        Size::from((SMALL_ICON, SMALL_ICON)),
    );
    // Close sits card-padding (6) + its balancing margin (3) from the right
    // edge (`.message-header:ltr { padding-right: 0 }` + `.message` padding +
    // `margin: $base_padding * 0.5`, `_message-list.scss:83,106-108,152-155`).
    let close = Rectangle::new(
        Point::from((
            width - PAD - 3. - CLOSE_D,
            header_y + (HEADER_H - CLOSE_D) / 2.,
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
            Point::from((PAD * 2., body_y)),
            Size::from((BODY_ICON, BODY_ICON)),
        )
    });

    // The body column: wrapped to the space right of the icon, minus the
    // card's edge padding.
    let text_x = PAD * 2. + body_icon.map_or(0., |_| BODY_ICON + PAD);
    let text_w = (width - text_x - PAD).max(1.);
    let body_lines = if content.body.is_empty() {
        Vec::new()
    } else {
        let lines = if expanded {
            max_lines.clamp(1, EXPAND_LINES)
        } else {
            1
        };
        niri_vk::text::wrap_lines_weighted(&content.body, TITLE_PX as f32, false, text_w, lines)
    };
    let can_expand = expand_button
        && (expanded
            || !content.actions.is_empty()
            || (!content.body.is_empty()
                && niri_vk::text::measure_line_width_weighted(
                    &content.body,
                    TITLE_PX as f32,
                    false,
                ) > text_w));

    // Collapsed height is the 48px icon row; every wrapped line past the
    // first grows it (one line expanded == collapsed, so toggling a short
    // body only reveals the action row).
    let body_h = BODY_ICON + (body_lines.len().saturating_sub(1)) as f64 * LINE_H;
    let h = body_y + body_h + PAD + actions_h;

    let mut actions = Vec::new();
    if show_actions {
        let n = content.actions.len() as f64;
        let total_w = width - PAD * 2. - BTN_GAP * (n - 1.);
        let btn_w = total_w / n;
        let y = body_y + body_h + PAD;
        for i in 0..content.actions.len() {
            actions.push(Rectangle::new(
                Point::from((PAD + i as f64 * (btn_w + BTN_GAP), y)),
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
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("notification_card::draw_card");

    let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
    let rect_px = |r: Rectangle<f64, Logical>| {
        Rectangle::new(
            Point::<i32, Physical>::from((px(r.loc.x), px(r.loc.y))),
            Size::<i32, Physical>::from((px(r.size.w), px(r.size.h))),
        )
    };
    let place_left = |ink: (i32, i32, i32, i32), lx: i32, cy: i32| {
        let (ix, iy, _iw, ih) = ink;
        Point::<i32, Physical>::from((lx - ix, cy - ih / 2 - iy))
    };

    let size = Size::<i32, Physical>::from((px(layout.size.w), px(layout.size.h)));
    let full = Rectangle::from_size(size);
    let title_px = (TITLE_PX * scale) as f32;
    let time_px = (TIME_PX * scale) as f32;

    let source_run = renderer.build_glyph_run_weighted(&content.source_title, title_px, true)?;
    let time_run = renderer.build_glyph_run_weighted(&content.time_text, time_px, false)?;
    let title_run = renderer.build_glyph_run_weighted(&content.title, title_px, true)?;
    let body_runs: Vec<_> = layout
        .body_lines
        .iter()
        .map(|line| renderer.build_glyph_run_weighted(line, title_px, false))
        .collect::<Result<_, _>>()?;
    let action_runs: Vec<_> = layout
        .actions
        .iter()
        .zip(&content.actions)
        .map(|(_, (_, label))| renderer.build_glyph_run_weighted(label, title_px, true))
        .collect::<Result<_, _>>()?;

    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((size.w.max(1), size.h.max(1))),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        // Transparent beyond the rounded corners; the card is an SDF fill.
        frame.clear(Color32F::from(TRANSPARENT), &[full])?;
        frame.render_rounded_rect(CARD_BG, (radius * scale) as f32, full, &[full])?;

        // Close-button circle, and the expand caret's when it's live.
        let close = rect_px(layout.close);
        frame.render_rounded_rect(BTN_BG, (CLOSE_D / 2. * scale) as f32, close, &[full])?;
        if let Some(expand) = layout.expand.filter(|_| layout.can_expand) {
            frame.render_rounded_rect(
                BTN_BG,
                (CLOSE_D / 2. * scale) as f32,
                rect_px(expand),
                &[full],
            )?;
        }

        // Themed body icons sit on the `.message-themed-icon` circle.
        if let Some(body_icon) = layout.body_icon {
            if matches!(content.icon, Some(NotificationIcon::Themed(_))) {
                frame.render_rounded_rect(
                    CIRCLE_BG,
                    (BODY_ICON / 2. * scale) as f32,
                    rect_px(body_icon),
                    &[full],
                )?;
            }
        }

        // Header: bold source title after the icon, time right-aligned before
        // the caret slot (or the close button on the banner).
        let header_cy = px(PAD + HEADER_H / 2.);
        let title_x = px(PAD * 2. + SMALL_ICON + PAD);
        let time_anchor = layout.expand.map_or(layout.close.loc.x, |e| e.loc.x);
        let time_w = niri_vk::text::measure_line_width_weighted(&content.time_text, time_px, false);
        let time_x = px(time_anchor - PAD) - time_w.round() as i32;
        let header_clip = Rectangle::new(
            Point::from((title_x, 0)),
            Size::from(((time_x - title_x).max(0), size.h)),
        );
        frame.render_glyphs(
            &source_run,
            place_left(source_run.ink_bounds(), title_x, header_cy),
            HEADER_FG,
            header_clip,
            &[full],
        )?;
        frame.render_glyphs(
            &time_run,
            place_left(time_run.ink_bounds(), time_x, header_cy),
            HEADER_FG,
            full,
            &[full],
        )?;

        // Body: bold title over the pre-wrapped body lines (a single
        // ellipsized line collapsed, up to six expanded).
        let body_y = PAD + HEADER_H + PAD;
        let text_x = px(PAD * 2.
            + if layout.body_icon.is_some() {
                BODY_ICON + PAD
            } else {
                0.
            });
        let text_clip = Rectangle::new(
            Point::from((text_x, 0)),
            Size::from(((size.w - text_x - px(PAD)).max(0), size.h)),
        );
        if body_runs.is_empty() {
            frame.render_glyphs(
                &title_run,
                place_left(title_run.ink_bounds(), text_x, px(body_y + BODY_ICON / 2.)),
                TEXT,
                text_clip,
                &[full],
            )?;
        } else {
            frame.render_glyphs(
                &title_run,
                place_left(title_run.ink_bounds(), text_x, px(body_y + 14.)),
                TEXT,
                text_clip,
                &[full],
            )?;
            for (i, body_run) in body_runs.iter().enumerate() {
                frame.render_glyphs(
                    body_run,
                    place_left(
                        body_run.ink_bounds(),
                        text_x,
                        px(body_y + 33. + i as f64 * LINE_H),
                    ),
                    TEXT,
                    text_clip,
                    &[full],
                )?;
            }
        }

        // Action buttons: pills with centered bold labels.
        for (rect, run) in layout.actions.iter().zip(&action_runs) {
            let r = rect_px(*rect);
            frame.render_rounded_rect(BTN_BG, (BTN_RADIUS * scale) as f32, r, &[full])?;
            let (ix, iy, iw, ih) = run.ink_bounds();
            let origin = Point::<i32, Physical>::from((
                r.loc.x + (r.size.w - iw) / 2 - ix,
                r.loc.y + (r.size.h - ih) / 2 - iy,
            ));
            frame.render_glyphs(run, origin, TEXT, r, &[full])?;
        }

        let _sync = frame.finish()?;
    }

    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}

/// Card textures cached per `(scale, key)` and uploaded pixel icons per `key`.
/// The owner picks the keys (the banner keys by its content revision, the list
/// by revision + card index) and evicts stale ones via [`retain`](Self::retain).
pub struct CardCache {
    context: Option<ContextId<VkTexture>>,
    cards: HashMap<(NotNan<f64>, u64), VkTexture>,
    pixels: HashMap<u64, TextureBuffer<VkTexture>>,
}

impl CardCache {
    pub fn new() -> Self {
        Self {
            context: None,
            cards: HashMap::new(),
            pixels: HashMap::new(),
        }
    }

    /// Drop every entry whose key fails `keep` (both card textures and pixel
    /// uploads share the key space).
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.cards.retain(|(_, key), _| keep(*key));
        self.pixels.retain(|key, _| keep(*key));
    }

    fn ensure_context(&mut self, renderer: &VulkanRenderer) {
        let context = renderer.context_id();
        if self.context.as_ref() != Some(&context) {
            self.cards.clear();
            self.pixels.clear();
            self.context = Some(context);
        }
    }
}

impl Default for CardCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The render elements of one card at `origin`: the (cached) card texture plus
/// the icons that composite on top — source icon, close glyph, and the body
/// icon (themed symbolic on the card's circle, or the app's pixel image).
#[allow(clippy::too_many_arguments)]
pub fn card_elements(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    cache: &mut CardCache,
    key: u64,
    content: &CardContent,
    layout: &CardLayout,
    radius: f64,
    origin: Point<f64, Logical>,
    alpha: f32,
    scale: f64,
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
        let buffer = icons.buffer(name, size_l, scale, color)?;
        let tb = TextureBuffer::from_memory_buffer(renderer, &buffer).ok()?;
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
    let source_name = match &content.source_icon {
        Some(NotificationIcon::Themed(name)) => name.clone(),
        // `MessageHeader.sourceIcon` fallback (`js/ui/messageList.js:355-359`).
        _ => "application-x-executable-symbolic".to_owned(),
    };
    if let Some(elem) = icon_at(renderer, &source_name, SMALL_ICON, HEADER_FG, source_center) {
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
        match draw_card(renderer, scale, content, layout, radius) {
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
    fn message_list_is_newest_first_across_and_within_sources() {
        let mut store = NotificationStore::default();
        let at = |s: u64| Duration::from_secs(s);
        let (a1, _) = store.notify(req("app-a", ":1.1"), true, at(1)).unwrap();
        let (b1, _) = store.notify(req("app-b", ":1.2"), true, at(2)).unwrap();
        let (a2, _) = store.notify(req("app-a", ":1.1"), true, at(3)).unwrap();

        // app-a notified last (a2 at t=3), so its source leads, newest first
        // within it; app-b follows.
        let cards = message_list_cards(&store, at(4));
        let ids: Vec<u32> = cards.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![a2, a1, b1]);
        assert!(cards.iter().all(|c| c.time_text == "Just now"));

        // A replace mutates in place and must NOT reorder the groups —
        // gnome-shell moves a group only on notification-*added*
        // (`js/ui/messageList.js:1824-1827`, `js/ui/messageTray.js:579-581`);
        // progress notifications replace constantly and stay put.
        let mut replace = req("app-b", ":1.2");
        replace.replaces_id = b1;
        replace.title = "updated".to_owned();
        store.notify(replace, true, at(5)).unwrap();
        let ids: Vec<u32> = message_list_cards(&store, at(6))
            .iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec![a2, a1, b1], "a replace never reorders sources");
    }
}
