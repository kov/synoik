//! The on-screen notification banner, gnome-shell 50.1's `MessageTray` popup.
//!
//! One banner at a time, top-center on a single output (our stand-in for
//! GNOME's primary monitor), sliding down from above the panel with a fade
//! (`js/ui/messageTray.js:1124-1160`, 200 ms). The widget is the shared
//! message card (`ui/notification_card.rs`) at `.notification-banner` metrics:
//! 34em wide, radius `$modal_radius` (`_notifications.scss:1-17`).
//!
//! Timing is the tray's own — the client's `expire_timeout` is ignored:
//! 4000 ms (`NOTIFICATION_TIMEOUT`, `js/ui/messageTray.js:19`), CRITICAL never
//! auto-expires (`:1211-1214`), hover holds expiry (restarting the countdown
//! on leave — simplified vs GNOME's 20px/600ms heuristics), and a banner shown
//! while the user is idle waits for their first activity, then expires 2000 ms
//! later (`:1092-1133`). The deadline is checked against the pinned clock in
//! `advance_animations` (headless-testable); `Synoik` arms a calloop timer as
//! the idle-machine wake-up. `are_animations_ongoing` is true only while
//! actually animating — a statically Shown banner must not busy-redraw.
//!
//! The banner expands like GNOME's (`js/ui/messageTray.js`): collapsed it
//! shows one ellipsized body line and no action row; CRITICAL auto-expands at
//! show (`:1170-1174`), and hovering expands it (`:1102-1105`) — unless it
//! popped up under the pointer, in which case the pointer must leave and
//! come back first (`:978-991`); it never un-expands while shown. It has no
//! expand caret (`:1137`) — that's the message list's.
//!
//! What the banner does NOT do (deferred, recorded in the plan): animate the
//! expansion (GNOME eases 200 ms; ours is instant, like the other popover
//! chrome pending the animations pass), grab key focus on hover-expand
//! (`:1306-1311`), the `policy.forceExpanded` auto-expand (the same cited
//! line also expands for the per-app force-expanded policy — per-app
//! policies as a whole are deferred), app focus on body click, Escape
//! (GNOME's banner is not key-focusable, `js/ui/messageTray.js:1136` — a
//! global grab would steal Escape from apps).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Transform};
use synoik_config::Config;

use crate::animation::{Animation, Clock};
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::notification_card::{card_elements, layout, CardCache, CardContent, CardLayout};
use crate::ui::widget;
use crate::utils::output_size;

/// 1em — the realized base font (`crate::ui::base_font_pt`), not the theme's nominal 11pt.
fn em() -> f64 {
    crate::ui::pt_to_px(11.)
}
/// The banner's box width. `$notification_banner_width: 34em` (`_notifications.scss:2`) is a
/// **content** width — St's `width` is, and `adjust_preferred_width` adds border and padding on
/// top — so the box is 34em plus `.message`'s padding and its 1px border. Measured 513 on a live
/// 50.3 shell against a 498.64 content width.
///
/// We passed the bare 34em as the box and drew a banner ~14px narrow. Same trap as
/// `.message-list`'s 29em; see [`crate::ui::calendar`].
fn width_px() -> f64 {
    34. * em() + 2. * (crate::ui::notification_card::PAD + crate::ui::notification_card::BORDER)
}
/// `$modal_radius` (`_notifications.scss:11`).
const RADIUS: f64 = 16.;
/// GNOME `.notification-banner` `box-shadow: 0 2px 4px 2px $shadow_color`
/// (`_notifications.scss:10`); `$shadow_color` (dark) = `rgba(0,0,0,0.2)`.
const SHADOW: widget::DropShadowSpec = widget::DropShadowSpec {
    blur: 4.,
    offset: (0., 2.),
    spread: 2.,
    color: [0., 0., 0., 0.2],
};
/// `.notification-banner` margin (`_notifications.scss:12`).
const MARGIN: f64 = 4.;

/// `NOTIFICATION_TIMEOUT` (`js/ui/messageTray.js:19`).
const TIMEOUT: Duration = Duration::from_millis(4000);
/// The re-armed timeout once an idle user becomes active (`js/ui/messageTray.js:1120`).
const ACTIVE_TIMEOUT: Duration = Duration::from_millis(2000);
/// `IDLE_TIME`: the user counts as idle at show time past this (`js/ui/messageTray.js:31,1118`).
pub const IDLE_TIME_MS: u64 = 1000;

/// What a click inside the banner hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerHit {
    Close,
    /// Index into [`CardContent::actions`].
    Action(usize),
    Body,
}

/// What `advance_animations` just did, for `Synoik` to act on.
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

pub struct NotificationBanner {
    state: State,
    content: Option<CardContent>,
    output: Option<Output>,
    hovered: bool,
    blocked: bool,
    idle_at_show: bool,
    /// Expanded body + action row. Set at show for CRITICAL
    /// (`js/ui/messageTray.js:1170-1174`), or by hover once fully shown
    /// (`:1102-1105`); never cleared until the next show.
    expanded: bool,
    /// The banner appeared under a stationary pointer: hovering must not
    /// expand it until the pointer has left and come back
    /// (`js/ui/messageTray.js:978-991`).
    popped_under_pointer: bool,
    /// The user became active while the banner was still sliding in — the
    /// idle gate resolves at the `Showing`→`Shown` transition by arming the
    /// short [`ACTIVE_TIMEOUT`] (GNOME's user-active watch fires during the
    /// show animation just the same, `js/ui/messageTray.js:1118-1122`).
    active_during_show: bool,
    revision: u64,
    cache: RefCell<CardCache>,
    /// The drop shadow, baked into its own texture and cached alongside the card.
    shadow_cache: RefCell<widget::BakeCache>,
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
            expanded: false,
            popped_under_pointer: false,
            active_during_show: false,
            revision: 0,
            cache: RefCell::new(CardCache::new()),
            shadow_cache: RefCell::new(widget::BakeCache::new()),
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

    /// The banner's card layout: 34em wide, no expand-caret slot
    /// (`js/ui/messageTray.js:1137`); the action row appears when expanded.
    fn layout(&self, content: &CardContent) -> CardLayout {
        layout(content, width_px(), self.expanded, false)
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Expand in place (one-way until the next show). The revision bump
    /// invalidates the cached card texture.
    fn expand(&mut self) {
        if !self.expanded {
            self.expanded = true;
            self.revision += 1;
        }
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
    /// [`IDLE_TIME_MS`] at show time; `pointer` = the pointer's output-local
    /// position when it sits on the banner's output — a pointer already
    /// inside where the banner lands arms the hover-expand guard
    /// (`js/ui/messageTray.js:1149-1156`).
    pub fn show(
        &mut self,
        content: CardContent,
        output: Output,
        user_idle: bool,
        pointer: Option<Point<f64, Logical>>,
    ) {
        // CRITICAL auto-expands, without animation (`js/ui/messageTray.js:1170-1174`).
        self.expanded = content.critical;
        self.content = Some(content);
        self.output = Some(output);
        self.popped_under_pointer = pointer.is_some_and(|p| {
            let output = self.output.clone().unwrap();
            self.shown_rect(&output).is_some_and(|r| r.contains(p))
        });
        self.hovered = false;
        self.idle_at_show = user_idle;
        self.active_during_show = false;
        self.revision += 1;
        self.state = State::Showing(self.animation(0., 1.));
    }

    /// The shown notification was replaced in place: update content and re-arm
    /// the timeout, like `_updateShowingNotification` (`js/ui/messageTray.js:938-943`).
    pub fn refresh(&mut self, content: CardContent, user_idle: bool) {
        let critical = content.critical;
        if self.content.as_ref() != Some(&content) {
            self.content = Some(content);
            self.revision += 1;
        }
        // A replace to CRITICAL auto-expands, like the initial show
        // (`_updateShowingNotification` re-runs on update,
        // `js/ui/messageTray.js:938-943,1170-1174`).
        if critical {
            self.expand();
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
    pub fn sync_content(&mut self, content: CardContent) {
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

    /// Hover holds expiry and expands the (fully shown) banner; leaving
    /// restarts the full countdown (`js/ui/messageTray.js:970-1050,1102-1105`,
    /// simplified). A banner that popped up under the pointer doesn't expand
    /// until the pointer leaves and comes back (`:978-991`; our hover cycle
    /// stands in for GNOME's mouse-away tracking). Returns true when anything
    /// changed (re-arm the wake-up timer and redraw).
    pub fn set_hovered(&mut self, hovered: bool) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        // The card body darkens while hovered (`%card:hover`); re-bake it even
        // when the hover doesn't also expand the banner (popped-under-pointer or
        // an already-expanded critical banner).
        self.revision += 1;
        let mut changed = false;
        if hovered {
            if self.popped_under_pointer {
                // The guarded hover still holds expiry (GNOME switches to its
                // longer hide timeout here) — it just doesn't expand.
            } else if matches!(self.state, State::Shown { .. }) {
                changed = !self.expanded;
                self.expand();
            }
        } else {
            self.popped_under_pointer = false;
        }
        let critical = self.content.as_ref().is_some_and(|c| c.critical);
        if let State::Shown {
            deadline,
            waiting_for_activity,
        } = &mut self.state
        {
            if hovered {
                return deadline.take().is_some() || changed;
            } else if !critical && !*waiting_for_activity {
                *deadline = Some(self.clock.now_unadjusted() + TIMEOUT);
                return true;
            }
        }
        changed
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
                    // Hover-expand only applies once fully shown (GNOME's
                    // `_updateState`, `js/ui/messageTray.js:1102-1105`); a
                    // pointer that settled on the banner mid-slide expands it
                    // now.
                    if self.hovered && !self.popped_under_pointer {
                        self.expand();
                    }
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
        let layout = self.layout(content);
        let ow = output_size(output).w;
        let x = ((ow - layout.size.w) / 2.).max(0.);
        let y = crate::ui::panel::panel_height() + MARGIN;
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
        let layout = self.layout(content);
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

    /// Whether the pointer is currently over the banner's (final) area. This
    /// includes the slide-in: GNOME tracks hover on the banner bin from the
    /// SHOWING state (`js/ui/messageTray.js:970-996`) — a pointer that
    /// settles on the banner mid-slide must hold expiry, expand at the
    /// SHOWN transition, and clear the popped-under-pointer guard when it
    /// leaves during the slide.
    pub fn pointer_inside(&self, output: &Output, pos: Point<f64, Logical>) -> bool {
        matches!(self.state, State::Showing(_) | State::Shown { .. })
            && self.output.as_ref() == Some(output)
            && self
                .shown_rect(output)
                .is_some_and(|rect| rect.contains(pos))
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if matches!(self.state, State::Hidden) || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        let Some(content) = &self.content else {
            return Vec::new();
        };

        let scale = output.current_scale().fractional_scale();
        let layout = self.layout(content);
        let ow = output_size(output).w;

        let progress = match &self.state {
            State::Hidden => unreachable!(),
            State::Showing(anim) => anim.value().clamp(0., 1.),
            State::Shown { .. } => 1.,
            State::Hiding(anim) => anim.value().clamp(0., 1.),
        };
        let alpha = progress as f32;

        let x = ((ow - layout.size.w) / 2.).max(0.);
        let y_shown = crate::ui::panel::panel_height() + MARGIN;
        // Slide down from fully off-screen above, like the tray banner.
        let y = -layout.size.h + progress * (layout.size.h + y_shown);
        let origin = Point::<f64, Logical>::from((x, y));
        let origin = origin.to_physical_precise_round(scale).to_logical(scale);

        let mut elements = {
            let mut cache = self.cache.borrow_mut();
            cache.retain(|key| key == self.revision);
            card_elements(
                renderer,
                icons,
                app_icons,
                &mut cache,
                self.revision,
                content,
                &layout,
                RADIUS,
                origin,
                alpha,
                scale,
                // The whole banner darkens while hovered (`%card:hover`); it tracks
                // only whole-banner hover, so no per-button highlight (a minor gap
                // vs the popover list, which does light individual buttons).
                self.hovered,
                None,
            )
        };

        // The drop shadow goes *behind* the card, i.e. last in the FIRST=topmost Vec.
        if !elements.is_empty() {
            match widget::bake_card_shadow(
                renderer,
                &mut self.shadow_cache.borrow_mut(),
                scale,
                self.revision,
                layout.size,
                RADIUS,
                SHADOW,
            ) {
                Ok((tex, off)) => {
                    let loc = origin - off.to_f64().to_logical(scale);
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        tex,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::warn!("error baking notification-banner shadow: {err:?}"),
            }
        }

        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The banner box matches what a live GNOME 50.3 shell allocates: **513** wide.
    ///
    /// `$notification_banner_width: 34em` is a *content* width — St's `width` is, and
    /// `adjust_preferred_width` adds border and padding on top. Measured on a mapped actor dump:
    /// the theme reports `width: 498.64` (34em) and the actor is allocated 513. Passing the bare
    /// 34em as the box drew a banner ~14px narrow.
    ///
    /// Same trap as `.message-list`'s `29em`, which cost 15px on the date popover.
    #[test]
    fn the_banner_box_matches_the_live_shell() {
        let content = 34. * em();
        assert!(
            (content - 498.64).abs() < 0.1,
            "34em must be the shell's 498.64 content width, got {content}"
        );
        assert!(
            (width_px() - 512.64).abs() < 0.1,
            "the box adds .message's padding and border on top of 34em (513 live), got {}",
            width_px()
        );
    }
}
