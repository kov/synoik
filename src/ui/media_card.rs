//! The MPRIS media card — gnome-shell's `MediaMessage` (`js/ui/messageList.js:770-853`).
//!
//! It is a `.message` like a notification card and shares that geometry: the same header row and
//! the same 48px-icon body row. Three things differ, all from `MediaMessage` and
//! `.media-message` (`_message-list.scss:224-269`):
//!
//! - **no close button** — `Message.canClose()` returns false and only `NotificationMessage`
//!   overrides it (`messageList.js:668-670`), so the header is title-only;
//! - **transport controls** in the body row, after the content column: prev / play-pause / next,
//!   added through `addMediaControl` (`:605-613,778-791`), each `padding: 0 18px` with an 8px
//!   radius and a 16px symbolic icon;
//! - **album art** in the icon slot, rounded 8px instead of circular, falling back to
//!   `audio-x-generic-symbolic` at 32px (`_message-list.scss:260-269`).
//!
//! The card's title is the track title and its body the artists joined with `', '`
//! (`messageList.js:825-830`). Clicking the body raises the player (`:799-804`).
//!
//! **Album art is not loaded yet** (recorded in `docs/fork/osd-media-port.md`): every player draws
//! the themed fallback. The art path needs a rounded *texture* element in the message list, whose
//! element type is `TextureRenderElement` today — a toolkit change, not a card change.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::notification_card::{
    BODY_ICON, BTN_RADIUS, CARD_BG, CARD_HOVER_BG, CIRCLE_BG, HEADER_FG, HEADER_H, LINE_H, PAD,
    SMALL_ICON, TEXT, TRANSPARENT,
};
use crate::ui::widget::{self, Align, Painter, ShapedText, TextShaper, TextStyle};

/// `.message-media-control` padding: `0 $base_padding * 3` around a 16px icon
/// (`_message-list.scss:225-227,254`).
const CTRL_PAD_X: f64 = 18.;
const CTRL_W: f64 = SMALL_ICON + 2. * CTRL_PAD_X;
/// The controls are `y_align: FILL` children of `.message-box`, so they are as tall as the row the
/// 48px art sets.
const CTRL_H: f64 = BODY_ICON;
/// The album-art slot is rounded, not circular: `.media-message .message-icon` overrides the
/// radius to `$base_border_radius` (`_message-list.scss:262`).
const ART_RADIUS: f64 = BTN_RADIUS;
/// The fallback glyph is `$large_icon_size` (`_message-list.scss:265-267`).
const ART_FALLBACK_ICON: f64 = 32.;
const TITLE_PT: f64 = 11.;

/// A hovered media control: `lighten($hover_bg_color, 5%)` — the SCSS lightens the usual hover
/// because the controls sit on a card (`_message-list.scss:239-242`). On the dark variant
/// `$hover_bg_color = lighten($bg_color #36363a, 10%)`, so this is that lightened twice ≈
/// `#5b5b62`. The resting state has no background at all (only a transparent 1px border, `:228`).
const CTRL_HOVER_BG: [f32; 4] = [
    0x5b as f32 / 255.,
    0x5b as f32 / 255.,
    0x62 as f32 / 255.,
    1.,
];
/// An insensitive control's glyph: `lighten($insensitive_fg_color, 5%)` (`:249`), which on dark is
/// `lighten(mix(#ffffff, #36363a, 50%), 5%)` ≈ `#a7a7a9`.
const CTRL_INSENSITIVE_FG: [f32; 4] = [
    0xa7 as f32 / 255.,
    0xa7 as f32 / 255.,
    0xa9 as f32 / 255.,
    1.,
];

/// The three transport buttons, in `MediaMessage`'s construction order
/// (`js/ui/messageList.js:778-791`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaControl {
    Previous,
    PlayPause,
    Next,
}

impl MediaControl {
    pub const ALL: [MediaControl; 3] = [Self::Previous, Self::PlayPause, Self::Next];

    /// The icon, which for play-pause depends on the player's state: pause iff it is playing
    /// (`messageList.js:831-835`).
    fn icon(self, playing: bool) -> &'static str {
        match self {
            Self::Previous => "media-skip-backward-symbolic",
            Self::Next => "media-skip-forward-symbolic",
            Self::PlayPause if playing => "media-playback-pause-symbolic",
            Self::PlayPause => "media-playback-start-symbolic",
        }
    }
}

/// A display snapshot of one player, rebuilt from [`crate::mpris::MprisStore`] whenever it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaCardContent {
    /// The player's bus name — its identity here and the address of its controls.
    pub bus_name: String,
    /// The app's name, or `Identity` (`mpris.js:175`).
    pub source_title: String,
    /// Themed icon-name candidates for the header, from the resolved app.
    pub source_icon: Vec<String>,
    /// The track title.
    pub title: String,
    /// The artists, joined `', '`.
    pub body: String,
    pub playing: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

impl MediaCardContent {
    /// `_updateNavButton` (`messageList.js:836-838`): prev/next go insensitive when the player
    /// says it cannot skip. Play-pause is always live.
    pub fn is_sensitive(&self, control: MediaControl) -> bool {
        match control {
            MediaControl::Previous => self.can_go_previous,
            MediaControl::Next => self.can_go_next,
            MediaControl::PlayPause => true,
        }
    }
}

/// The card snapshots for the open message list, in view order: **newest player first**, because
/// gnome-shell inserts each new player's message at index 0 (`js/ui/messageList.js:1780-1784`) and
/// the store keeps them in discovery order. Only players with `CanPlay` get a card.
pub fn media_card_contents(store: &crate::mpris::MprisStore) -> Vec<MediaCardContent> {
    store
        .visible()
        .rev()
        .map(|player| MediaCardContent {
            bus_name: player.bus_name.clone(),
            source_title: player.source_name().to_owned(),
            // GNOME uses the app's full-colour `GIcon` here; ours takes its themed names and
            // falls back with the header's own fallback. A file-icon app draws that fallback
            // until the art path lands.
            source_icon: match player.app.as_ref().map(|app| &app.icon) {
                Some(crate::app_system::AppIconRef::Themed(names)) => names.clone(),
                _ => Vec::new(),
            },
            title: player.state.title.clone(),
            body: player.artists_line(),
            playing: player.state.status.is_playing(),
            can_go_next: player.state.can_go_next,
            can_go_previous: player.state.can_go_previous,
        })
        .collect()
}

/// A media card's geometry: card-relative logical rects shared by draw and hit-test.
pub struct MediaLayout {
    pub size: Size<f64, Logical>,
    pub source_icon: Rectangle<f64, Logical>,
    /// The album-art slot (always present — the fallback fills it).
    pub art: Rectangle<f64, Logical>,
    /// Prev / play-pause / next, in [`MediaControl::ALL`] order.
    pub controls: [Rectangle<f64, Logical>; 3],
    /// The title/body column, between the art and the controls.
    pub text: Rectangle<f64, Logical>,
}

impl MediaLayout {
    pub fn control_at(&self, local: Point<f64, Logical>) -> Option<MediaControl> {
        MediaControl::ALL
            .iter()
            .zip(&self.controls)
            .find(|(_, rect)| rect.contains(local))
            .map(|(control, _)| *control)
    }
}

/// Lay out a media card of `width`. Fixed height: one icon row, no expansion — a media card has no
/// action bin and its body is a single line.
pub fn layout(width: f64) -> MediaLayout {
    let header_y = PAD;
    let body_y = header_y + HEADER_H + PAD;

    let source_icon = Rectangle::new(
        Point::from((PAD * 2., header_y + (HEADER_H - SMALL_ICON) / 2.)),
        Size::from((SMALL_ICON, SMALL_ICON)),
    );
    let art = Rectangle::new(
        Point::from((PAD * 2., body_y)),
        Size::from((BODY_ICON, BODY_ICON)),
    );

    // The controls sit at the row's right edge, adjacent to each other: `_mediaControls` is a
    // plain `St.BoxLayout` with no spacing of its own (`messageList.js:502-503`).
    let controls_x = width - PAD * 2. - CTRL_W * 3.;
    let controls = std::array::from_fn(|i| {
        Rectangle::new(
            Point::from((controls_x + i as f64 * CTRL_W, body_y)),
            Size::from((CTRL_W, CTRL_H)),
        )
    });

    let text_x = art.loc.x + BODY_ICON + PAD;
    let text = Rectangle::new(
        Point::from((text_x, body_y)),
        Size::from(((controls_x - PAD - text_x).max(1.), BODY_ICON)),
    );

    MediaLayout {
        size: Size::from((width, body_y + BODY_ICON + PAD)),
        source_icon,
        art,
        controls,
        text,
    }
}

/// Rasterize the card: rounded background, the art slot's rounded backdrop, the hovered control's
/// wash, and all the text. Icons composite on top (see [`media_card_elements`]).
pub fn draw_media_card(
    renderer: &mut VulkanRenderer,
    scale: f64,
    content: &MediaCardContent,
    layout: &MediaLayout,
    radius: f64,
    card_hovered: bool,
    hovered_control: Option<MediaControl>,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("media_card::draw_media_card");

    let bold = TextStyle::new(TITLE_PT).bold();
    let plain = TextStyle::new(TITLE_PT);
    let (source_run, title_run, body_run) = {
        let mut shaper = TextShaper::new(renderer, scale);
        let source_run = shaper.shape(&content.source_title, bold)?;
        let title_run = shaper.shape(&content.title, bold)?;
        let body_run: Option<ShapedText> = if content.body.is_empty() {
            None
        } else {
            Some(shaper.shape(&content.body, plain)?)
        };
        (source_run, title_run, body_run)
    };

    widget::bake_uncached(renderer, scale, layout.size, |frame, size| {
        let mut p = Painter::new(frame, scale, size);

        p.clear(TRANSPARENT)?;
        let card_bg = if card_hovered { CARD_HOVER_BG } else { CARD_BG };
        p.fill_rounded(Rectangle::from_size(layout.size), radius, card_bg)?;

        // The art slot's backdrop is the `.message-themed-icon` fill, but rounded rather than
        // circular. It shows through wherever the fallback glyph does not cover.
        p.fill_rounded(layout.art, ART_RADIUS, CIRCLE_BG)?;

        // Only a hovered control has a background at all.
        if let Some(control) = hovered_control {
            let index = MediaControl::ALL.iter().position(|c| *c == control);
            if let Some(rect) = index.map(|i| layout.controls[i]) {
                p.fill_rounded(rect, BTN_RADIUS, CTRL_HOVER_BG)?;
            }
        }

        // Header: bold source title after the icon. No close button and no time label, so the
        // title runs to the card's padding.
        let header_cy = PAD + HEADER_H / 2.;
        let title_x = PAD * 2. + SMALL_ICON + PAD;
        let header_clip = Rectangle::new(
            Point::from((title_x, 0.)),
            Size::from(((layout.size.w - PAD - title_x).max(0.), layout.size.h)),
        );
        p.text_clipped(
            &source_run,
            Point::from((title_x, header_cy)),
            Align::LEFT_MIDDLE,
            HEADER_FG,
            header_clip,
        )?;

        // Body: the track title over the artists, on the same baselines a one-line notification
        // body uses, so the two card kinds line up in the list.
        let body_y = layout.text.loc.y;
        let title_cy = if body_run.is_some() {
            body_y + 14.
        } else {
            body_y + BODY_ICON / 2.
        };
        p.text_clipped(
            &title_run,
            Point::from((layout.text.loc.x, title_cy)),
            Align::LEFT_MIDDLE,
            TEXT,
            layout.text,
        )?;
        if let Some(body_run) = &body_run {
            p.text_clipped(
                body_run,
                Point::from((layout.text.loc.x, body_y + LINE_H + 15.)),
                Align::LEFT_MIDDLE,
                TEXT,
                layout.text,
            )?;
        }

        Ok(())
    })
}

/// The render elements of one media card at `origin`: the card texture plus the icons that
/// composite on top — the header source icon, the art fallback, and the three control glyphs.
#[allow(clippy::too_many_arguments)]
pub fn media_card_elements(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    cache: &mut crate::ui::notification_card::CardCache,
    key: u64,
    content: &MediaCardContent,
    layout: &MediaLayout,
    radius: f64,
    origin: Point<f64, Logical>,
    alpha: f32,
    scale: f64,
    card_hovered: bool,
    hovered_control: Option<MediaControl>,
) -> Vec<TextureRenderElement<VkTexture>> {
    use ordered_float::NotNan;
    use smithay::backend::renderer::element::Kind;
    use smithay::utils::Transform;

    // FIRST = topmost, as everywhere else in the UI: icons first, the card texture last.
    let mut elements = Vec::new();
    let Ok(scale_key) = NotNan::new(scale) else {
        return elements;
    };
    cache.ensure_context(renderer);

    let center_of = |rect: Rectangle<f64, Logical>| {
        Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
    };

    // Header icon: the app's, falling back like `MessageHeader.sourceIcon`
    // (`js/ui/messageList.js:355-359`).
    let mut candidates: Vec<&str> = content.source_icon.iter().map(String::as_str).collect();
    candidates.push("application-x-executable-symbolic");
    if let Some(elem) = widget::icon_element_alpha(
        renderer,
        icons,
        &candidates,
        SMALL_ICON,
        scale,
        HEADER_FG,
        origin,
        center_of(layout.source_icon),
        alpha,
    ) {
        elements.push(elem);
    }

    // The album-art fallback, at `$large_icon_size` on the slot's rounded backdrop.
    if let Some(elem) = widget::icon_element_alpha(
        renderer,
        icons,
        &["audio-x-generic-symbolic"],
        ART_FALLBACK_ICON,
        scale,
        TEXT,
        origin,
        center_of(layout.art),
        alpha,
    ) {
        elements.push(elem);
    }

    for (control, rect) in MediaControl::ALL.iter().zip(&layout.controls) {
        let color = if content.is_sensitive(*control) {
            TEXT
        } else {
            CTRL_INSENSITIVE_FG
        };
        if let Some(elem) = widget::icon_element_alpha(
            renderer,
            icons,
            &[control.icon(content.playing)],
            SMALL_ICON,
            scale,
            color,
            origin,
            center_of(*rect),
            alpha,
        ) {
            elements.push(elem);
        }
    }

    #[allow(clippy::map_entry)]
    if !cache.has_card(scale_key, key) {
        match draw_media_card(
            renderer,
            scale,
            content,
            layout,
            radius,
            card_hovered,
            hovered_control,
        ) {
            Ok(texture) => cache.insert_card(scale_key, key, texture),
            Err(err) => warn!("error rendering a media card: {err:#}"),
        }
    }
    if let Some(card) = cache.get_card(scale_key, key) {
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

    fn content() -> MediaCardContent {
        MediaCardContent {
            bus_name: "org.mpris.MediaPlayer2.rhythmbox".into(),
            source_title: "Rhythmbox".into(),
            source_icon: vec!["org.gnome.Rhythmbox3".into()],
            title: "So What".into(),
            body: "Miles Davis".into(),
            playing: true,
            can_go_next: true,
            can_go_previous: false,
        }
    }

    /// The three controls are adjacent, right-aligned, and the text column ends before them —
    /// otherwise a long track title would run under the buttons it must not reach.
    #[test]
    fn the_controls_are_right_aligned_and_the_text_stops_short() {
        let width = 388.;
        let l = layout(width);

        assert_eq!(l.controls[0].size, Size::from((CTRL_W, CTRL_H)));
        assert_eq!(l.controls[1].loc.x, l.controls[0].loc.x + CTRL_W);
        assert_eq!(l.controls[2].loc.x, l.controls[1].loc.x + CTRL_W);
        assert_eq!(l.controls[2].loc.x + CTRL_W, width - PAD * 2.);
        // All three sit on the art's row.
        for rect in &l.controls {
            assert_eq!(rect.loc.y, l.art.loc.y);
        }

        assert!(l.text.loc.x >= l.art.loc.x + l.art.size.w);
        assert!(l.text.loc.x + l.text.size.w <= l.controls[0].loc.x);

        // A media card is the height of a single-line notification card: header + icon row.
        assert_eq!(l.size.h, PAD + HEADER_H + PAD + BODY_ICON + PAD);
    }

    /// Hit-testing resolves a point to the control under it, and to nothing off the row.
    #[test]
    fn controls_hit_test_in_construction_order() {
        let l = layout(388.);
        let mid = |rect: Rectangle<f64, Logical>| {
            Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
        };

        assert_eq!(
            l.control_at(mid(l.controls[0])),
            Some(MediaControl::Previous)
        );
        assert_eq!(
            l.control_at(mid(l.controls[1])),
            Some(MediaControl::PlayPause)
        );
        assert_eq!(l.control_at(mid(l.controls[2])), Some(MediaControl::Next));
        assert_eq!(l.control_at(mid(l.art)), None);
        assert_eq!(l.control_at(mid(l.text)), None);
    }

    /// The play-pause glyph is the *pause* icon while playing (`messageList.js:831-835`) — the
    /// button shows what pressing it does, not what the player is doing.
    #[test]
    fn the_play_pause_icon_follows_the_playback_status() {
        assert_eq!(
            MediaControl::PlayPause.icon(true),
            "media-playback-pause-symbolic"
        );
        assert_eq!(
            MediaControl::PlayPause.icon(false),
            "media-playback-start-symbolic"
        );

        // Skip buttons go insensitive on the player's say-so; play-pause never does.
        let c = content();
        assert!(!c.is_sensitive(MediaControl::Previous));
        assert!(c.is_sensitive(MediaControl::Next));
        assert!(c.is_sensitive(MediaControl::PlayPause));
    }
}
