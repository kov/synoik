//! The on-screen notification banner, gnome-shell 50.1's `MessageTray` popup.
//!
//! One banner at a time, top-center on a single output (our stand-in for
//! GNOME's primary monitor), sliding down from above the panel with a fade
//! (`js/ui/messageTray.js:1124-1160`, 200 ms). The widget is visually the
//! message card + `.notification-banner` (34em wide, radius `$modal_radius`,
//! `_notifications.scss:1-17`): header row (16px source icon, bold app title,
//! 9pt time label, circular close button) over a body row (48px icon, bold
//! title, single-line body) and up to 3 action buttons
//! (`js/ui/messageList.js:19,444-529`).
//!
//! Timing is the tray's own — the client's `expire_timeout` is ignored:
//! 4000 ms (`NOTIFICATION_TIMEOUT`, `js/ui/messageTray.js:19`), CRITICAL never
//! auto-expires (`:1211-1214`), hover holds expiry (restarting the countdown
//! on leave — simplified vs GNOME's 20px/600ms heuristics), and a banner shown
//! while the user is idle waits for their first activity, then expires 2000 ms
//! later (`:1092-1133`). The deadline is checked against the pinned clock in
//! `advance_animations` (headless-testable); `Niri` arms a calloop timer as
//! the idle-machine wake-up. `are_animations_ongoing` is true only while
//! actually animating — a statically Shown banner must not busy-redraw.
//!
//! What the banner does NOT do (deferred, recorded in the plan): expand /
//! 6-line body clamp / CRITICAL auto-expand (renders collapsed), app focus on
//! body click, per-app policies, Escape (GNOME's banner is not key-focusable,
//! `js/ui/messageTray.js:1136` — a global grab would steal Escape from apps).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use niri_config::Config;
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::notifications::{NotificationIcon, NotificationStore, Urgency};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

/// 1em at the 11pt base font.
const EM: f64 = crate::ui::pt_to_px(11.);
/// `$notification_banner_width: 34em` (`_notifications.scss:2`).
const WIDTH: f64 = 34. * EM;
/// `$modal_radius` (`_notifications.scss:11`).
const RADIUS: f64 = 16.;
/// `.notification-banner` margin (`_notifications.scss:12`).
const MARGIN: f64 = 4.;
/// `.message` padding = `$base_padding` (`_message-list.scss:83`).
const PAD: f64 = 6.;
/// `.message-header-content` min-height (`_message-list.scss:118`).
const HEADER_H: f64 = 24.;
/// `.message-icon` icon-size = 48px (`_message-list.scss:168`).
const BODY_ICON: f64 = 48.;
/// Header/source/close icons (`$scalable_icon_size`).
const SMALL_ICON: f64 = 16.;
/// The circular close button: 16px icon + 6px padding (`_message-list.scss:139-156`).
const CLOSE_D: f64 = SMALL_ICON + 2. * PAD;
/// Action button height: bold 11pt + 6px paddings (`%notification_button`).
const BTN_H: f64 = 28.;
const BTN_RADIUS: f64 = 8.;
const TITLE_PX: f64 = crate::ui::pt_to_px(11.);
const TIME_PX: f64 = crate::ui::pt_to_px(9.);

/// `.message` bg, dark variant (`lighten($card_bg_color, 5%)` ≈ `#51515a`).
const CARD_BG: [f32; 4] = [
    0x51 as f32 / 255.,
    0x51 as f32 / 255.,
    0x5a as f32 / 255.,
    1.,
];
/// `.message-header` fg (`$card_insensitive_fg_color` ≈ `#b1b1b3`).
const HEADER_FG: [f32; 4] = [
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

/// `NOTIFICATION_TIMEOUT` (`js/ui/messageTray.js:19`).
const TIMEOUT: Duration = Duration::from_millis(4000);
/// The re-armed timeout once an idle user becomes active (`js/ui/messageTray.js:1120`).
const ACTIVE_TIMEOUT: Duration = Duration::from_millis(2000);
/// `IDLE_TIME`: the user counts as idle at show time past this (`js/ui/messageTray.js:31,1118`).
pub const IDLE_TIME_MS: u64 = 1000;

/// A display snapshot of the shown notification (plus its source header),
/// rebuilt from the store on show/refresh/content-sync.
#[derive(Debug, Clone, PartialEq)]
pub struct BannerContent {
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

/// Build the banner snapshot for `id` from the store; `now` is the pinned
/// clock, for the header's relative-time label (a banner usually shows "Just
/// now", but one drained after a long popover block is older).
pub fn content_for(store: &NotificationStore, id: u32, now: Duration) -> Option<BannerContent> {
    let source = store
        .sources
        .iter()
        .find(|s| s.notifications.iter().any(|n| n.id == id))?;
    let n = source.notifications.iter().find(|n| n.id == id)?;
    Some(BannerContent {
        id,
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
    })
}

/// What a click inside the banner hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerHit {
    Close,
    /// Index into [`BannerContent::actions`].
    Action(usize),
    Body,
}

/// What `advance_animations` just did, for `Niri` to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerEvent {
    /// The hide animation completed on its own (timeout/hover-out/blocked) —
    /// run the store's `banner_hidden` (transient-destroy) path. A hide caused
    /// by the model already removing the notification never animates
    /// ([`NotificationBanner::hide_removed`] snaps to `Hidden`, mirroring
    /// `js/ui/messageTray.js:909-917,1282`), so it never produces an event —
    /// which is what guarantees no double-close of a transient.
    HiddenNaturally,
}

enum State {
    Hidden,
    Showing(Animation),
    Shown {
        /// `None` = never auto-expires (CRITICAL, or waiting for activity).
        deadline: Option<Duration>,
        /// Shown while the user was idle: the first activity arms
        /// [`ACTIVE_TIMEOUT`] (`js/ui/messageTray.js:1118-1133`).
        waiting_for_activity: bool,
    },
    Hiding(Animation),
}

struct Cache {
    context: Option<ContextId<VkTexture>>,
    /// The card texture per (scale, content revision).
    cards: HashMap<(NotNan<f64>, u64), VkTexture>,
    /// Uploaded pixel-icon buffers per content revision.
    pixels: HashMap<u64, TextureBuffer<VkTexture>>,
}

pub struct NotificationBanner {
    state: State,
    content: Option<BannerContent>,
    output: Option<Output>,
    hovered: bool,
    blocked: bool,
    idle_at_show: bool,
    /// The user became active while the banner was still sliding in — the
    /// idle gate resolves at the `Showing`→`Shown` transition by arming the
    /// short [`ACTIVE_TIMEOUT`] (GNOME's user-active watch fires during the
    /// show animation just the same, `js/ui/messageTray.js:1118-1122`).
    active_during_show: bool,
    revision: u64,
    cache: RefCell<Cache>,
    clock: Clock,
    config: Rc<RefCell<Config>>,
}

impl NotificationBanner {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            content: None,
            output: None,
            hovered: false,
            blocked: false,
            idle_at_show: false,
            active_during_show: false,
            revision: 0,
            cache: RefCell::new(Cache {
                context: None,
                cards: HashMap::new(),
                pixels: HashMap::new(),
            }),
            clock,
            config,
        }
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.notification_open_close.0,
        )
    }

    pub fn is_visible(&self) -> bool {
        !matches!(self.state, State::Hidden)
    }

    /// Ready to pop the next queued banner: hidden and not popover-blocked.
    pub fn can_show(&self) -> bool {
        matches!(self.state, State::Hidden) && !self.blocked
    }

    pub fn content_id(&self) -> Option<u32> {
        self.content.as_ref().map(|c| c.id)
    }

    pub fn action_key(&self, idx: usize) -> Option<String> {
        let content = self.content.as_ref()?;
        content.actions.get(idx).map(|(key, _)| key.clone())
    }

    pub fn has_default_action(&self) -> bool {
        self.content.as_ref().is_some_and(|c| c.has_default_action)
    }

    /// Show a freshly popped banner. `user_idle` = idletime exceeded
    /// [`IDLE_TIME_MS`] at show time.
    pub fn show(&mut self, content: BannerContent, output: Output, user_idle: bool) {
        self.content = Some(content);
        self.output = Some(output);
        self.hovered = false;
        self.idle_at_show = user_idle;
        self.active_during_show = false;
        self.revision += 1;
        self.state = State::Showing(self.animation(0., 1.));
    }

    /// The shown notification was replaced in place: update content and re-arm
    /// the timeout, like `_updateShowingNotification` (`js/ui/messageTray.js:938-943`).
    pub fn refresh(&mut self, content: BannerContent, user_idle: bool) {
        let critical = content.critical;
        if self.content.as_ref() != Some(&content) {
            self.content = Some(content);
            self.revision += 1;
        }
        match &mut self.state {
            State::Shown {
                deadline,
                waiting_for_activity,
            } => {
                if critical {
                    *deadline = None;
                    *waiting_for_activity = false;
                } else if user_idle {
                    *deadline = None;
                    *waiting_for_activity = true;
                } else {
                    *deadline = Some(self.clock.now_unadjusted() + TIMEOUT);
                    *waiting_for_activity = false;
                }
            }
            // "If a new notification is updated while it is being hidden, we
            // stop hiding it and show it again" (`js/ui/messageTray.js:938-943`)
            // — resume the slide-in from wherever the hide got to.
            State::Hiding(anim) => {
                let from = anim.value();
                self.active_during_show = false;
                self.state = State::Showing(self.animation(from, 1.));
            }
            State::Hidden | State::Showing(_) => (),
        }
        self.idle_at_show = user_idle;
    }

    /// Update content WITHOUT re-arming the timeout — for mutations that don't
    /// re-enter banner admission (e.g. a replace to LOW urgency; GNOME's
    /// live-bound widget keeps displaying updated properties).
    pub fn sync_content(&mut self, content: BannerContent) {
        if self.content.as_ref() != Some(&content) {
            self.content = Some(content);
            self.revision += 1;
        }
    }

    /// Start the natural hide (slide-up + fade); the transient-destroy rule
    /// applies at completion.
    pub fn hide(&mut self) {
        // Start from wherever the show animation got to — hiding a banner
        // mid-slide-in must not snap it to full opacity first.
        let from = match &self.state {
            State::Hidden | State::Hiding(_) => return,
            State::Showing(anim) => anim.value(),
            State::Shown { .. } => 1.,
        };
        self.state = State::Hiding(self.animation(from, 0.));
    }

    /// The model already destroyed the shown notification: hide instantly,
    /// without the transient-destroy at completion (`js/ui/messageTray.js:1097-1101`
    /// hides without animation when the notification was removed).
    pub fn hide_removed(&mut self) {
        if matches!(self.state, State::Hidden) {
            return;
        }
        self.state = State::Hidden;
        self.content = None;
        self.output = None;
    }

    /// The banner's output was removed: move to `fallback` (the new active
    /// output — GNOME re-parents its banner to the new primary on monitor
    /// changes). With no output left, the banner keeps its state invisibly
    /// and [`adopt_output`](Self::adopt_output) picks up the next hotplug.
    pub fn retarget_output(&mut self, removed: &Output, fallback: Option<Output>) {
        if self.output.as_ref() == Some(removed) {
            self.output = fallback;
        }
    }

    /// An output appeared; a visible banner orphaned by `retarget_output`
    /// adopts it.
    pub fn adopt_output(&mut self, output: &Output) {
        if self.output.is_none() && self.is_visible() {
            self.output = Some(output.clone());
        }
    }

    /// Banners are blocked while a panel popover is open (GNOME blocks for the
    /// dateMenu; blocking for quick settings too is a recorded divergence that
    /// avoids un-clickable banners under our modal popover grab). A visible
    /// banner finishes early through the natural-hide path.
    pub fn set_blocked(&mut self, blocked: bool) {
        if self.blocked == blocked {
            return;
        }
        self.blocked = blocked;
        if blocked {
            self.hide();
        }
    }

    /// Hover holds expiry; leaving restarts the full countdown
    /// (`js/ui/messageTray.js:970-1050`, simplified). Returns true when the
    /// deadline changed (re-arm the wake-up timer).
    pub fn set_hovered(&mut self, hovered: bool) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        let critical = self.content.as_ref().is_some_and(|c| c.critical);
        if let State::Shown {
            deadline,
            waiting_for_activity,
        } = &mut self.state
        {
            if hovered {
                return deadline.take().is_some();
            } else if !critical && !*waiting_for_activity {
                *deadline = Some(self.clock.now_unadjusted() + TIMEOUT);
                return true;
            }
        }
        false
    }

    /// User input arrived: a banner waiting on idle gating arms its (shorter)
    /// timeout (`js/ui/messageTray.js:1118-1122`). Returns true when the
    /// deadline changed.
    pub fn on_activity(&mut self) -> bool {
        match &mut self.state {
            State::Shown {
                deadline,
                waiting_for_activity: waiting @ true,
            } => {
                *waiting = false;
                if !self.hovered {
                    *deadline = Some(self.clock.now_unadjusted() + ACTIVE_TIMEOUT);
                    return true;
                }
            }
            // Activity while the banner is still sliding in also resolves the
            // idle gate; the Showing→Shown transition arms [`ACTIVE_TIMEOUT`].
            State::Showing(_) if self.idle_at_show => {
                self.active_during_show = true;
            }
            _ => (),
        }
        false
    }

    /// The next wall-clock moment the banner needs waking for (the calloop
    /// timer's target); `None` while animating (frames drive us) or unarmed.
    pub fn next_wakeup(&self) -> Option<Duration> {
        match &self.state {
            State::Shown {
                deadline: Some(d), ..
            } if !self.hovered => Some(*d),
            _ => None,
        }
    }

    pub fn advance_animations(&mut self) -> Option<BannerEvent> {
        match &mut self.state {
            State::Hidden => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    let critical = self.content.as_ref().is_some_and(|c| c.critical);
                    let waiting = self.idle_at_show && !self.active_during_show && !critical;
                    let timeout = if self.active_during_show {
                        ACTIVE_TIMEOUT
                    } else {
                        TIMEOUT
                    };
                    let deadline = (!critical && !waiting && !self.hovered)
                        .then(|| self.clock.now_unadjusted() + timeout);
                    self.state = State::Shown {
                        deadline,
                        waiting_for_activity: waiting,
                    };
                }
            }
            State::Shown { deadline, .. } => {
                if let Some(d) = deadline {
                    if !self.hovered && self.clock.now_unadjusted() >= *d {
                        self.hide();
                    }
                }
            }
            State::Hiding(anim) => {
                if anim.is_clamped_done() {
                    self.state = State::Hidden;
                    self.content = None;
                    self.output = None;
                    return Some(BannerEvent::HiddenNaturally);
                }
            }
        }
        None
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding { .. })
    }

    /// The banner's rectangle on its output when fully shown (for hit-tests).
    fn shown_rect(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        let content = self.content.as_ref()?;
        let layout = layout(content);
        let ow = output_size(output).w;
        let x = ((ow - layout.size.w) / 2.).max(0.);
        let y = crate::ui::panel::PANEL_HEIGHT + MARGIN;
        Some(Rectangle::new(Point::from((x, y)), layout.size))
    }

    /// Hit-test a click at output-local `pos`. `None` = not on the banner
    /// (banners never swallow outside clicks); only interactive while fully
    /// shown.
    pub fn hit_test(&self, output: &Output, pos: Point<f64, Logical>) -> Option<BannerHit> {
        if !matches!(self.state, State::Shown { .. }) {
            return None;
        }
        if self.output.as_ref() != Some(output) {
            return None;
        }
        let rect = self.shown_rect(output)?;
        if !rect.contains(pos) {
            return None;
        }
        let content = self.content.as_ref()?;
        let layout = layout(content);
        let local = pos - rect.loc;
        if layout.close.contains(local) {
            return Some(BannerHit::Close);
        }
        for (i, action) in layout.actions.iter().enumerate() {
            if action.contains(local) {
                return Some(BannerHit::Action(i));
            }
        }
        Some(BannerHit::Body)
    }

    /// Whether the pointer is currently over the (fully shown) banner.
    pub fn pointer_inside(&self, output: &Output, pos: Point<f64, Logical>) -> bool {
        matches!(self.state, State::Shown { .. })
            && self.output.as_ref() == Some(output)
            && self
                .shown_rect(output)
                .is_some_and(|rect| rect.contains(pos))
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if matches!(self.state, State::Hidden) || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        let Some(content) = &self.content else {
            return Vec::new();
        };

        let scale = output.current_scale().fractional_scale();
        let layout = layout(content);
        let ow = output_size(output).w;

        let progress = match &self.state {
            State::Hidden => unreachable!(),
            State::Showing(anim) => anim.value().clamp(0., 1.),
            State::Shown { .. } => 1.,
            State::Hiding(anim) => anim.value().clamp(0., 1.),
        };
        let alpha = progress as f32;

        let x = ((ow - layout.size.w) / 2.).max(0.);
        let y_shown = crate::ui::panel::PANEL_HEIGHT + MARGIN;
        // Slide down from fully off-screen above, like the tray banner.
        let y = -layout.size.h + progress * (layout.size.h + y_shown);
        let origin = Point::<f64, Logical>::from((x, y));
        let origin = origin.to_physical_precise_round(scale).to_logical(scale);

        let mut elements = Vec::new();

        // The card texture, cached per (scale, content revision).
        let scale_key = match NotNan::new(scale) {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let card = {
            let mut cache = self.cache.borrow_mut();
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.cards.clear();
                cache.pixels.clear();
                cache.context = Some(context);
            }
            cache.cards.retain(|(_, rev), _| *rev == self.revision);
            #[allow(clippy::map_entry)]
            if !cache.cards.contains_key(&(scale_key, self.revision)) {
                match draw_card(renderer, scale, content, &layout) {
                    Ok(texture) => {
                        cache.cards.insert((scale_key, self.revision), texture);
                    }
                    Err(err) => {
                        warn!("error rendering the notification banner: {err:#}");
                    }
                }
            }
            cache.cards.get(&(scale_key, self.revision)).cloned()
        };
        if let Some(card) = card {
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
                    let mut cache = self.cache.borrow_mut();
                    cache.pixels.retain(|rev, _| *rev == self.revision);
                    #[allow(clippy::map_entry)]
                    if !cache.pixels.contains_key(&self.revision) {
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
                                cache.pixels.insert(self.revision, tb);
                            }
                            Err(err) => {
                                warn!("error uploading notification image: {err:#}");
                            }
                        }
                    }
                    if let Some(tb) = cache.pixels.get(&self.revision) {
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

        elements
    }
}

/// The banner's pure geometry, card-relative logical rects shared by the draw
/// and the hit-test.
struct Layout {
    size: Size<f64, Logical>,
    source_icon: Rectangle<f64, Logical>,
    close: Rectangle<f64, Logical>,
    /// The 48px body-icon slot; `None` when the notification has no icon.
    body_icon: Option<Rectangle<f64, Logical>>,
    actions: Vec<Rectangle<f64, Logical>>,
}

fn layout(content: &BannerContent) -> Layout {
    let header_y = PAD;
    let body_y = header_y + HEADER_H + PAD;
    let body_h = BODY_ICON;
    let actions_h = if content.actions.is_empty() {
        0.
    } else {
        BTN_H + PAD
    };
    let h = body_y + body_h + PAD + actions_h;

    let source_icon = Rectangle::new(
        Point::from((PAD * 2., header_y + (HEADER_H - SMALL_ICON) / 2.)),
        Size::from((SMALL_ICON, SMALL_ICON)),
    );
    let close = Rectangle::new(
        Point::from((WIDTH - PAD - CLOSE_D, header_y + (HEADER_H - CLOSE_D) / 2.)),
        Size::from((CLOSE_D, CLOSE_D)),
    );
    let body_icon = content.icon.is_some().then(|| {
        Rectangle::new(
            Point::from((PAD * 2., body_y)),
            Size::from((BODY_ICON, BODY_ICON)),
        )
    });

    let mut actions = Vec::new();
    if !content.actions.is_empty() {
        let n = content.actions.len() as f64;
        let gap = MARGIN;
        let total_w = WIDTH - PAD * 2. - gap * (n - 1.);
        let btn_w = total_w / n;
        let y = body_y + body_h + PAD;
        for i in 0..content.actions.len() {
            actions.push(Rectangle::new(
                Point::from((PAD + i as f64 * (btn_w + gap), y)),
                Size::from((btn_w, BTN_H)),
            ));
        }
    }

    Layout {
        size: Size::from((WIDTH, h)),
        source_icon,
        close,
        body_icon,
        actions,
    }
}

/// Rasterize the card: rounded background, themed-icon circle, close-button
/// circle, action-button pills, and all the text. Icons composite on top.
fn draw_card(
    renderer: &mut VulkanRenderer,
    scale: f64,
    content: &BannerContent,
    layout: &Layout,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("notification_banner::draw_card");

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
    let body_run = (!content.body.is_empty())
        .then(|| renderer.build_glyph_run_weighted(&content.body, title_px, false))
        .transpose()?;
    let action_runs: Vec<_> = content
        .actions
        .iter()
        .map(|(_, label)| renderer.build_glyph_run_weighted(label, title_px, true))
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
        frame.render_rounded_rect(CARD_BG, (RADIUS * scale) as f32, full, &[full])?;

        // Close-button circle.
        let close = rect_px(layout.close);
        frame.render_rounded_rect(BTN_BG, (CLOSE_D / 2. * scale) as f32, close, &[full])?;

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
        // the close button.
        let header_cy = px(PAD + HEADER_H / 2.);
        let title_x = px(PAD * 2. + SMALL_ICON + PAD);
        let time_w = niri_vk::text::measure_line_width_weighted(&content.time_text, time_px, false);
        let time_x = px(layout.close.loc.x - PAD) - time_w.round() as i32;
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

        // Body: bold title over the single-line body, clipped at the card edge
        // (gnome-shell ellipsizes; clipping is the minimal faithful bound).
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
        if let Some(body_run) = &body_run {
            frame.render_glyphs(
                &title_run,
                place_left(title_run.ink_bounds(), text_x, px(body_y + 14.)),
                TEXT,
                text_clip,
                &[full],
            )?;
            frame.render_glyphs(
                body_run,
                place_left(body_run.ink_bounds(), text_x, px(body_y + 33.)),
                TEXT,
                text_clip,
                &[full],
            )?;
        } else {
            frame.render_glyphs(
                &title_run,
                place_left(title_run.ink_bounds(), text_x, px(body_y + BODY_ICON / 2.)),
                TEXT,
                text_clip,
                &[full],
            )?;
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
