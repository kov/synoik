// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The on-screen keyboard — a port of gnome-shell's `js/ui/keyboard.js` (`KeyboardManager`,
//! `Keyboard`, `KeyboardController`).
//!
//! It exists while `org.gnome.desktop.a11y.applications screen-keyboard-enabled` is on
//! (`KeyboardManager._syncEnabled`, `keyboard.js:1097-1117`) and slides up from the bottom of the
//! primary display when a text field asks for it:
//! - a client re-enabling its `zwp_text_input_v3` while it already holds the input method —
//!   mutter's `enable_panel_v1` (`meta-wayland-text-input.c:866-874,970-974`); the protocol version
//!   we serve has no `show_input_panel`, and GTK re-enables on a click instead;
//! - one of our own entries taking the key focus (`_onKeyFocusChanged`, `keyboard.js:1364-1386`).
//!
//! It goes when the text focus leaves — the input method's `HIDE_PANEL_TIME` after its
//! `focus_out` (`inputMethod.js:209-226`) — or on its hide key, or when the focused window moves.
//! When the caret would be under it, the focused window slides up out of its way and back down
//! after (`setCursorLocation`, `_animateWindow`, `keyboard.js:1885-1965`).
//!
//! What a key does follows `KeyboardController` (`keyboard.js:1968-2210`): a string goes in through
//! the input method's commit when a text field has it, and as key presses otherwise; a keyval is a
//! virtual key press, through the same pipeline a hardware key takes — mutter's virtual keyboard
//! goes through `meta_seat_impl_notify_key_in_impl`, keyboard accessibility filters included.
//!
//! The model lives here; drawing and hit-testing are [`crate::ui::osk`].

pub mod emoji;
pub mod layout;

use std::collections::BTreeSet;
use std::time::Duration;

use smithay::backend::input::KeyState;
use smithay::desktop::Window;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use self::layout::{Action, Key, Layout, Level, Purpose};
use crate::animation::{Animation, Clock, Curve};
use crate::dbus::ibus::hints;
use crate::input_method::ShellEntry;
use crate::synoik::{State, Synoik};
use crate::utils::timers::TimerToken;

/// `KEYBOARD_ANIMATION_TIME` (`keyboard.js:20`).
pub const ANIMATION_TIME: Duration = Duration::from_millis(150);
/// `KEYBOARD_REST_TIME` (`keyboard.js:21`): how long an open or close waits for the request to
/// settle.
const REST_TIME: Duration = Duration::from_millis(300);
/// `KEY_LONG_PRESS_TIME` (`keyboard.js:22`).
pub const LONG_PRESS_TIME: Duration = Duration::from_millis(250);
/// `HIDE_PANEL_TIME` (`inputMethod.js:16`): between the text focus leaving and the panel going.
const HIDE_PANEL_TIME: Duration = Duration::from_millis(50);

/// A touch point's id, for telling presses apart.
pub fn touch_id(slot: smithay::backend::input::TouchSlot) -> i32 {
    i32::from(slot)
}

/// The panel state a text field asks for (`Clutter.InputPanelState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelState {
    Off,
    On,
    Toggle,
}

/// Who holds the text focus, as the keyboard sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFocus {
    None,
    /// A client with its text input enabled.
    Client,
    /// One of our own entries, with the key focus.
    Shell(ShellEntry),
}

/// What is holding a key down: the pointer button or touch point that pressed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presser {
    Button(u32),
    Touch(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    /// Index into the current level's keys.
    pub key: usize,
    pub by: Presser,
}

/// What releasing (or long-pressing) a key asks the compositor to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Commit a string, with the latched modifiers' keyvals held around it.
    Commit(String, Vec<u32>),
    /// Press and release a keyval.
    Keyval(u32),
    /// `toggleDelete(true)` / `toggleDelete(false)`.
    Delete(bool),
    Hide,
    /// `_popupLanguageMenu` over the key at this index.
    LanguageMenu(usize),
}

/// The long-press popup of a key's other strings (`Key._showSubkeys`, `keyboard.js:265-370`).
///
/// While it is up it captures every event on the stage: a press outside it is swallowed, and the
/// release that follows closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subkeys {
    /// The key they belong to, on the current level.
    pub key: usize,
    pub strings: Vec<String>,
    pub hovered: Option<usize>,
    pub pressed: Option<(usize, Presser)>,
    /// `_capturedPress`.
    captured_press: bool,
}

/// `EMOJI_PAGE_SEPARATION`: the gap between a page and the one sliding in beside it.
pub const EMOJI_PAGE_SEPARATION: f64 = 32.;

/// The emoji page while it is up (`_emojiActive`).
#[derive(Debug)]
struct EmojiPage {
    pages: Vec<emoji::Page>,
    /// The letter level whose ratio the page takes.
    base: Level,
    cols: u32,
    rows: u32,
    current: usize,
    level: Level,
    /// The pager's `delta`, in logical px: negative while the next page comes in from the right.
    delta: f64,
    /// Easing `delta` at the end of a swipe, and whether the page then turns to the following one.
    slide: Option<(Animation, bool)>,
    swipe: Option<EmojiSwipe>,
}

/// A swipe on the pager (`SwipeTracker` with `allowDrag` and `allowScroll`): a press that may turn
/// into a drag, or a touchpad's two-finger scroll.
#[derive(Debug)]
struct EmojiSwipe {
    /// The press dragging it; `None` for a touchpad scroll.
    by: Option<Presser>,
    start: Point<f64, Logical>,
    /// The pager's width: how far one page of progress moves it.
    width: f64,
    /// Past `DRAG_THRESHOLD_DISTANCE`, sideways. A scroll has begun from its first event.
    begun: bool,
    last_x: f64,
    /// In pages; positive brings the next page in.
    progress: f64,
    tracker: crate::input::swipe_tracker::SwipeTracker,
}

/// `DRAG_THRESHOLD_DISTANCE` (`swipeTracker.js:28`).
const DRAG_THRESHOLD: f64 = 16.;
/// `TOUCHPAD_BASE_DISTANCE`: a touchpad scroll's travel per page (`swipeTracker.js:26`).
const TOUCHPAD_PAGE: f64 = 400.;
/// `SCROLL_MULTIPLIER` (`swipeTracker.js:27`).
const SCROLL_MULTIPLIER: f64 = 10.;

impl EmojiPage {
    fn new(base: Level) -> Self {
        // `setRatio(Math.floor(nCols), Math.floor(nRows) - 1)`.
        let cols = (base.cols.floor() as u32).max(1);
        let rows = base.rows.saturating_sub(1).max(1);
        let pages = emoji::pages((cols * rows) as usize);
        let level = emoji::page_level(&base, cols, rows, &pages[0]);
        Self {
            pages,
            base,
            cols,
            rows,
            current: 0,
            level,
            delta: 0.,
            slide: None,
            swipe: None,
        }
    }

    /// `setCurrentPage`.
    fn set_current_page(&mut self, page: usize) {
        self.current = page % self.pages.len();
        self.delta = 0.;
        self.slide = None;
        self.level = emoji::page_level(&self.base, self.cols, self.rows, &self.pages[self.current]);
    }

    /// `getFollowingPage`: the page the swipe is bringing in, if it is moving.
    fn following_page(&self) -> Option<usize> {
        let n = self.pages.len();
        if self.delta < 0. {
            Some((self.current + 1) % n)
        } else if self.delta > 0. {
            Some((self.current + n - 1) % n)
        } else {
            None
        }
    }
}

/// `_focusWindow`: the window slid out of the keyboard's way, and how far it went — the work-area
/// clamp can stop it short of the keyboard's height, and it has to come back by exactly as much.
#[derive(Debug, Clone, PartialEq)]
struct FocusWindow {
    window: Window,
    shift: f64,
}

#[derive(Debug)]
pub struct Osk {
    /// Whether the keyboard exists (`KeyboardManager._keyboard`).
    enabled: bool,
    /// `_keyboardRequested`.
    requested: bool,
    /// `_keyboardVisible`: shown or showing.
    visible: bool,
    /// The slide: 0 is off the bottom of the screen, 1 fully up. `translation_y` and `opacity`
    /// ease together in `_animateShow` / `_animateHide` (`keyboard.js:1771-1823`).
    progress: Option<Animation>,
    shown: f64,
    rest_timer: Option<TimerToken>,
    hide_panel_timer: Option<TimerToken>,
    long_press_timer: Option<TimerToken>,
    /// The text focus as of the last [`State::sync_osk_focus`].
    focus: TextFocus,
    group: String,
    purpose: Purpose,
    hints: u32,
    layout: Option<Layout>,
    /// The current level's name (`_currentPage`).
    page: String,
    /// `_latched`: the current level stays after a key.
    latched: bool,
    /// `_modifiers`: latched Ctrl/Alt keyvals.
    modifiers: BTreeSet<u32>,
    pressed: Option<Press>,
    hovered: Option<usize>,
    /// `_longPressed`: the shift long-press already latched, so its release must not switch.
    long_pressed: bool,
    /// `_deleteEnabled` / `_backspacePressed`.
    delete_held: bool,
    /// `_surroundingTextId`: the level waits on the field's next surrounding text.
    awaiting_surrounding: bool,
    subkeys: Option<Subkeys>,
    emoji: Option<EmojiPage>,
    focus_window: Option<FocusWindow>,
    /// The `FocusTracker`'s current window and where it sat, to see it move (`window-moved`).
    watched: Option<(Window, Option<Point<f64, Logical>>)>,
    /// Bumped on every change the drawn keyboard shows.
    revision: u64,
}

impl Default for Osk {
    fn default() -> Self {
        Self {
            enabled: false,
            requested: false,
            visible: false,
            progress: None,
            shown: 0.,
            rest_timer: None,
            hide_panel_timer: None,
            long_press_timer: None,
            focus: TextFocus::None,
            group: "us".to_owned(),
            purpose: Purpose::Normal,
            hints: 0,
            layout: None,
            page: "default".to_owned(),
            latched: false,
            modifiers: BTreeSet::new(),
            pressed: None,
            hovered: None,
            long_pressed: false,
            delete_held: false,
            awaiting_surrounding: false,
            subkeys: None,
            emoji: None,
            focus_window: None,
            watched: None,
            revision: 0,
        }
    }
}

impl Osk {
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// `Keyboard.visible`: shown, or on its way up.
    pub fn is_visible(&self) -> bool {
        self.enabled && self.visible
    }

    /// How far up it is, 0..=1.
    pub fn progress(&self) -> f64 {
        self.progress.as_ref().map_or(self.shown, Animation::value)
    }

    /// Whether there is anything to draw.
    pub fn is_drawn(&self) -> bool {
        self.enabled && self.progress() > 0.
    }

    pub fn is_animating(&self) -> bool {
        self.progress.as_ref().is_some_and(|a| !a.is_done()) || self.emoji_is_animating()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn level(&self) -> Option<&Level> {
        if let Some(emoji) = &self.emoji {
            return Some(&emoji.level);
        }
        let layout = self.layout.as_ref()?;
        layout.level(&self.page).or_else(|| layout.levels.first())
    }

    pub fn latched(&self) -> bool {
        self.latched
    }

    pub fn modifier_latched(&self, keyval: u32) -> bool {
        self.modifiers.contains(&keyval)
    }

    pub fn pressed(&self) -> Option<Press> {
        self.pressed
    }

    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    pub fn subkeys(&self) -> Option<&Subkeys> {
        self.subkeys.as_ref()
    }

    pub fn emoji_active(&self) -> bool {
        self.emoji.is_some()
    }

    /// The emoji page's swipe, for drawing: its offset, and the page coming in with the level it
    /// shows.
    pub fn emoji_slide(&self) -> Option<(f64, Option<Level>)> {
        let emoji = self.emoji.as_ref()?;
        let following = emoji
            .following_page()
            .map(|page| emoji::page_level(&emoji.base, emoji.cols, emoji.rows, &emoji.pages[page]));
        Some((emoji.delta, following))
    }

    /// The page dots: how many pages the shown section has, and where between them the pager is
    /// (`_updateIndicatorPosition`), given the pager's width.
    pub fn emoji_indicators(&self, width: f64) -> Option<(usize, f64)> {
        let emoji = self.emoji.as_ref()?;
        let page = &emoji.pages[emoji.current];
        let width = width.max(1.);
        Some((page.n_pages, page.page as f64 - emoji.delta / width))
    }

    /// The section the shown page is in — its key is latched (`_onPageChanged`).
    pub fn emoji_section(&self) -> Option<usize> {
        let emoji = self.emoji.as_ref()?;
        Some(emoji.pages[emoji.current].section)
    }

    /// `_setEmojiActive`. Showing it starts on the first page (`vfunc_map` → `setCurrentPage(0)`).
    fn set_emoji_active(&mut self, active: bool) {
        self.cancel_press();
        self.hovered = None;
        self.subkeys = None;
        self.emoji = if active {
            let base = self.layout.as_ref().and_then(|layout| {
                layout
                    .level(&self.page)
                    .or_else(|| layout.levels.first())
                    .cloned()
            });
            base.map(EmojiPage::new)
        } else {
            None
        };
        self.bump();
    }

    /// `setCurrentSection(section, 0)`.
    fn set_emoji_section(&mut self, section: usize) {
        let Some(emoji) = self.emoji.as_mut() else {
            return;
        };
        if let Some(page) = emoji
            .pages
            .iter()
            .position(|p| p.section == section && p.page == 0)
        {
            if page != emoji.current {
                emoji.set_current_page(page);
                self.subkeys = None;
            }
        }
        self.bump();
    }

    /// A press on the emoji pager, `width` wide: it may become a swipe.
    pub fn emoji_swipe_begin(&mut self, by: Presser, pos: Point<f64, Logical>, width: f64) {
        let Some(emoji) = self.emoji.as_mut() else {
            return;
        };
        if emoji.swipe.is_some() {
            return;
        }
        emoji.swipe = Some(EmojiSwipe {
            by: Some(by),
            start: pos,
            width: width.max(1.),
            begun: false,
            last_x: pos.x,
            progress: 0.,
            tracker: crate::input::swipe_tracker::SwipeTracker::new(),
        });
    }

    /// A touchpad scroll over the pager, `width` wide, `dx` sideways; a stop event (`dx` and `dy`
    /// both 0) ends it. Returns whether anything moved.
    pub fn emoji_scroll(
        &mut self,
        dx: f64,
        dy: f64,
        width: f64,
        time: Duration,
        clock: &Clock,
    ) -> bool {
        let Some(emoji) = self.emoji.as_mut() else {
            return false;
        };
        if dx == 0. && dy == 0. {
            if emoji.swipe.as_ref().is_some_and(|s| s.by.is_none()) {
                return self.emoji_swipe_finish(time, clock, true);
            }
            return false;
        }
        if emoji.swipe.as_ref().is_some_and(|s| s.by.is_some()) {
            return false;
        }
        let swipe = emoji.swipe.get_or_insert_with(|| EmojiSwipe {
            by: None,
            start: Point::default(),
            width: width.max(1.),
            begun: true,
            last_x: 0.,
            progress: 0.,
            tracker: crate::input::swipe_tracker::SwipeTracker::new(),
        });
        emoji.slide = None;
        let delta = dx * SCROLL_MULTIPLIER;
        swipe.tracker.push(delta, time);
        swipe.progress = (swipe.progress + delta / TOUCHPAD_PAGE).clamp(-1., 1.);
        emoji.delta = -swipe.progress * swipe.width;
        self.cancel_press();
        self.bump();
        true
    }

    /// The presser moved. Returns whether the swipe has it.
    pub fn emoji_swipe_motion(
        &mut self,
        by: Presser,
        pos: Point<f64, Logical>,
        time: Duration,
    ) -> bool {
        let Some(emoji) = self.emoji.as_mut() else {
            return false;
        };
        let Some(swipe) = emoji.swipe.as_mut().filter(|s| s.by == Some(by)) else {
            return false;
        };
        if !swipe.begun {
            let d = pos - swipe.start;
            if d.x.hypot(d.y) < DRAG_THRESHOLD {
                return false;
            }
            if d.x.abs() <= d.y.abs() {
                // A vertical drag is not this tracker's.
                emoji.swipe = None;
                return false;
            }
            swipe.begun = true;
            emoji.slide = None;
        }
        // Progress runs against the drag: dragging left brings the next page in.
        swipe.tracker.push(-(pos.x - swipe.last_x), time);
        swipe.last_x = pos.x;
        let width = swipe.width;
        swipe.progress = (-(pos.x - swipe.start.x) / width).clamp(-1., 1.);
        emoji.delta = -swipe.progress * width;
        // `_onSwipeUpdate`: the key under the drag lets go without acting.
        self.cancel_press();
        self.bump();
        true
    }

    /// The presser let go. Returns whether the swipe had it; then the page settles, eased on
    /// `clock` (`_onSwipeEnd`).
    pub fn emoji_swipe_end(&mut self, by: Presser, time: Duration, clock: &Clock) -> bool {
        let Some(emoji) = self.emoji.as_mut() else {
            return false;
        };
        if !emoji.swipe.as_ref().is_some_and(|s| s.by == Some(by)) {
            return false;
        }
        self.emoji_swipe_finish(time, clock, false)
    }

    /// `_endGesture` → `_onSwipeEnd`: settle on the page the release points at.
    fn emoji_swipe_finish(&mut self, time: Duration, clock: &Clock, touchpad: bool) -> bool {
        let Some(emoji) = self.emoji.as_mut() else {
            return false;
        };
        let Some(mut swipe) = emoji.swipe.take() else {
            return false;
        };
        if !swipe.begun {
            return false;
        }
        swipe.tracker.release(time);
        let width = swipe.width;
        let progress = swipe.progress;
        let velocity = swipe.tracker.velocity() / 1000.;
        let end =
            crate::input::swipe_tracker::end_progress(0., progress, (-1., 1.), velocity, touchpad);
        // `|Δprogress| / velocity * DURATION_MULTIPLIER`, the velocity per page of travel, clamped
        // to `[MIN_ANIMATION_DURATION, MAX_ANIMATION_DURATION * log2(1 + nPoints)]`.
        let distance = if touchpad { TOUCHPAD_PAGE } else { width };
        let per_ms = (velocity / distance).abs();
        let max = 400. * 4f64.log2();
        let ms = if per_ms == 0. {
            max
        } else {
            ((progress - end).abs() / per_ms * 3.).clamp(100., max)
        };
        let target = if end == 0. {
            0.
        } else if end < 0. {
            width + EMOJI_PAGE_SEPARATION
        } else {
            -width - EMOJI_PAGE_SEPARATION
        };
        emoji.slide = Some((
            Animation::ease(
                clock.clone(),
                emoji.delta,
                target,
                0.,
                ms.round() as u64,
                Curve::EaseOutCubic,
            ),
            end != 0.,
        ));
        self.bump();
        true
    }

    /// Who holds the emoji pager, if anyone.
    pub fn emoji_swipe_presser(&self) -> Option<Presser> {
        self.emoji.as_ref()?.swipe.as_ref()?.by
    }

    /// Whether a presser is the keyboard's: on a key, or on the pager.
    pub fn holds(&self, by: Presser) -> bool {
        self.pressed.is_some_and(|p| p.by == by) || self.emoji_swipe_presser() == Some(by)
    }

    pub fn emoji_is_animating(&self) -> bool {
        self.emoji.as_ref().is_some_and(|e| e.slide.is_some())
    }

    fn key(&self, index: usize) -> Option<&Key> {
        self.level()?.keys.get(index)
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// `_updateKeys`: rebuild for the group and purpose, back on the default level.
    fn update_keys(&mut self) {
        if let Some(layout) = Layout::load(&self.group, self.purpose) {
            self.layout = Some(layout);
        }
        self.pressed = None;
        self.hovered = None;
        self.subkeys = None;
        self.set_active_level("default");
        // The page takes the new layout's ratio.
        if self.emoji.is_some() {
            self.set_emoji_active(true);
        }
        self.bump();
    }

    /// `_setActiveLevel` (`keyboard.js:1669-1693`).
    fn set_active_level(&mut self, level: &str) {
        if self.page != level {
            self.latched = false;
            self.page = level.to_owned();
            // Its key is no longer mapped (`notify::mapped`).
            self.subkeys = None;
        }
        self.modifiers.clear();
        self.bump();
    }

    /// `_setLatched`.
    fn set_latched(&mut self, latched: bool) {
        self.latched = latched;
        self.bump();
    }

    /// `_updateLevelFromHints` (`keyboard.js:1306-1362`), without the auto-capitalisation that
    /// needs the surrounding text.
    fn update_level_from_hints(&mut self, user_input: bool) {
        if self.latched {
            return;
        }
        if self.hints & hints::LOWERCASE != 0 {
            self.set_active_level("default");
            return;
        }
        let has_shift = self
            .layout
            .as_ref()
            .is_some_and(|l| l.level("shift").is_some());
        if !has_shift {
            return;
        }
        if self.hints & hints::UPPERCASE_CHARS != 0 {
            self.set_active_level("shift");
            return;
        }
        // `AUTO_CAPITALIZATION` and `TITLECASE`: the level waits on the next surrounding text
        // the field sends — a Wayland client sends it with its every state — to see what the
        // caret follows (`surrounding-text-set`, `request_surrounding`).
        if self.hints & (hints::UPPERCASE_SENTENCES | hints::UPPERCASE_WORDS) != 0 {
            self.awaiting_surrounding = true;
            return;
        }
        if user_input && self.page == "shift" {
            self.set_active_level("default");
        }
    }

    /// The field's surrounding text arrived. If the level was waiting on it, the caret at the start
    /// of the text, after a space in title case or after a full stop in a sentence capitalises.
    pub fn surrounding_text_set(&mut self, text: &str, cursor: usize) {
        if !std::mem::take(&mut self.awaiting_surrounding) {
            return;
        }
        let before = text.get(..cursor).unwrap_or(text);
        let shift = if text.is_empty() || cursor == 0 {
            true
        } else if self.hints & hints::UPPERCASE_WORDS != 0 {
            before.ends_with(' ')
        } else if self.hints & hints::UPPERCASE_SENTENCES != 0 {
            before.trim_end().ends_with('.')
        } else {
            return;
        };
        self.set_active_level(if shift { "shift" } else { "default" });
    }

    /// A key went down (`Key._press`). Returns whether it started a long-press clock.
    pub fn press(&mut self, key: usize, by: Presser) -> bool {
        if self.pressed.is_some() || self.key(key).is_none() {
            return false;
        }
        self.pressed = Some(Press { key, by });
        self.bump();
        true
    }

    /// The long-press timer fired (`keyboard.js:285-299,1525-1536`).
    pub fn long_press(&mut self) -> Vec<Effect> {
        let Some(Press { key, .. }) = self.pressed else {
            return Vec::new();
        };
        let Some(key) = self.key(key).cloned() else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        match &key.action {
            Some(Action::LevelSwitch(level)) if key.is_shift() && level == "shift" => {
                self.set_active_level(level);
                self.set_latched(true);
                self.long_pressed = true;
            }
            Some(Action::Delete) => {
                if !self.delete_held {
                    self.delete_held = true;
                    effects.push(Effect::Delete(true));
                }
            }
            _ => {}
        }
        if !key.extended.is_empty() {
            // `fake_release`: the key lets go without acting, and the popup stays up after the
            // press ends — a string in it is picked with a press of its own.
            let index = self.pressed.take().map(|p| p.key);
            // `keyButton.set_hover(false)`.
            self.hovered = None;
            if let Some(emoji) = self.emoji.as_mut() {
                emoji.swipe = None;
            }
            self.subkeys = index.map(|key_index| Subkeys {
                key: key_index,
                strings: key.extended.clone(),
                hovered: None,
                pressed: None,
                captured_press: false,
            });
            self.bump();
        }
        effects
    }

    /// A press while the popup is up: on one of its keys, or anywhere else (`_onCapturedEvent`).
    pub fn subkeys_press(&mut self, inside: Option<usize>, by: Presser) {
        let Some(subkeys) = self.subkeys.as_mut() else {
            return;
        };
        match inside {
            Some(i) if subkeys.pressed.is_none() => subkeys.pressed = Some((i, by)),
            Some(_) => {}
            None => subkeys.captured_press = true,
        }
        self.bump();
    }

    /// A release while the popup is up: its pressed key commits, and either way a release that
    /// ends a press closes it (`Key._release` → `_hideSubkeys`).
    pub fn subkeys_release(&mut self, by: Presser) -> Vec<Effect> {
        let Some(subkeys) = self.subkeys.as_ref() else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        match subkeys.pressed {
            Some((i, presser)) if presser == by => {
                let text = subkeys.strings.get(i).cloned();
                self.subkeys = None;
                if let Some(text) = text {
                    effects.push(Effect::Commit(
                        text,
                        self.modifiers.iter().copied().collect(),
                    ));
                    self.modifiers.clear();
                    self.update_level_from_hints(true);
                }
            }
            Some(_) => {}
            None if subkeys.captured_press => self.subkeys = None,
            None => {}
        }
        self.bump();
        effects
    }

    pub fn set_subkey_hovered(&mut self, hovered: Option<usize>) -> bool {
        let Some(subkeys) = self.subkeys.as_mut() else {
            return false;
        };
        if subkeys.hovered == hovered {
            return false;
        }
        subkeys.hovered = hovered;
        self.bump();
        true
    }

    /// The presser let go (`Key._release` and the handlers `_addRowKeys` connects,
    /// `keyboard.js:306-324,1482-1519`).
    pub fn release(&mut self, by: Presser) -> Vec<Effect> {
        let Some(press) = self.pressed.filter(|p| p.by == by) else {
            return Vec::new();
        };
        self.pressed = None;
        self.bump();
        let Some(key) = self.key(press.key).cloned() else {
            return Vec::new();
        };

        let mut effects = Vec::new();
        match (&key.action, key.keyval, &key.commit) {
            (Some(action), _, _) => match action {
                Action::Hide => {
                    effects.push(Effect::Hide);
                    self.update_level_from_hints(true);
                }
                Action::Modifier => {
                    if let Some(keyval) = key.keyval {
                        if !self.modifiers.remove(&keyval) {
                            self.modifiers.insert(keyval);
                        }
                        self.bump();
                    }
                }
                Action::Delete => {
                    if !self.delete_held {
                        effects.push(Effect::Delete(true));
                    }
                    effects.push(Effect::Delete(false));
                    self.delete_held = false;
                    self.update_level_from_hints(true);
                }
                Action::LevelSwitch(level) => {
                    if !self.long_pressed {
                        self.set_active_level(level);
                        // `key.level === 1`, which no layout's string level ever is: a tap never
                        // latches, only the shift key's long press does.
                        self.set_latched(false);
                    }
                }
                Action::LanguageMenu => effects.push(Effect::LanguageMenu(press.key)),
                // The letters' emoji key and the page's ABC key (`_toggleEmoji`).
                Action::Emoji => self.set_emoji_active(self.emoji.is_none()),
                Action::EmojiSection(section) => self.set_emoji_section(*section),
            },
            (None, Some(keyval), _) => {
                effects.push(Effect::Keyval(keyval));
                self.update_level_from_hints(true);
            }
            // An emoji goes in as it is (`emoji-selected` → `commit(emoji)`).
            (None, None, Some(commit)) if key.flat => {
                effects.push(Effect::Commit(commit.clone(), Vec::new()));
            }
            (None, None, Some(commit)) => {
                effects.push(Effect::Commit(
                    commit.clone(),
                    self.modifiers.iter().copied().collect(),
                ));
                // `commit(...).then(...)`: the modifiers go and the level follows the hints.
                self.modifiers.clear();
                self.update_level_from_hints(true);
            }
            (None, None, None) => {}
        }
        self.long_pressed = false;
        effects
    }

    /// Drop a press without acting on it (`Key.cancel`).
    fn cancel_press(&mut self) {
        if self.pressed.take().is_some() {
            self.bump();
        }
        self.long_pressed = false;
    }

    pub fn set_hovered(&mut self, hovered: Option<usize>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        self.bump();
        true
    }

    fn animate_to(&mut self, clock: &Clock, to: f64, curve: Curve) {
        let from = self.progress();
        self.progress = Some(Animation::ease(
            clock.clone(),
            from,
            to,
            0.,
            ANIMATION_TIME.as_millis() as u64,
            curve,
        ));
        self.shown = to;
    }

    /// Settle a finished slide, so a done animation does not keep the frame clock running.
    pub fn advance(&mut self) {
        if self.progress.as_ref().is_some_and(Animation::is_done) {
            self.progress = None;
        }
        if let Some(emoji) = self.emoji.as_mut() {
            if let Some((anim, turn)) = &emoji.slide {
                emoji.delta = anim.value();
                if anim.is_done() {
                    let turn = *turn;
                    emoji.slide = None;
                    match emoji.following_page() {
                        Some(page) if turn => emoji.set_current_page(page),
                        _ => emoji.delta = 0.,
                    }
                    self.subkeys = None;
                }
                self.bump();
            }
        }
    }
}

/// The keyboard's rectangle on its display, display-local: full width, along the bottom, and up
/// by `progress` of its height (`_relayout`, `keyboard.js:1624-1639`; `_updateKeyboardBox`,
/// `layout.js:580-585`).
pub fn geometry(output_size: Size<f64, Logical>, progress: f64) -> Rectangle<f64, Logical> {
    let height = height(output_size);
    Rectangle::new(
        Point::from((0., output_size.h - height * progress)),
        Size::from((output_size.w, height)),
    )
}

/// `_relayout`: a third of a landscape display, a quarter of a portrait one, and never more than
/// half of it.
pub fn height(output_size: Size<f64, Logical>) -> f64 {
    let wanted = if output_size.w > output_size.h {
        output_size.h / 3.
    } else {
        output_size.h / 4.
    };
    wanted.clamp(
        crate::ui::osk::min_height().min(output_size.h / 2.),
        output_size.h / 2.,
    )
}

impl State {
    /// `KeyboardManager._syncEnabled`: the keyboard exists while the setting is on.
    pub fn sync_osk(&mut self) {
        let enabled = self
            .synoik
            .gnome_settings
            .a11y
            .get(crate::gnome::A11yToggle::ScreenKeyboard);
        let osk = &mut self.synoik.osk;
        if enabled == osk.enabled {
            if enabled {
                self.sync_osk_group();
            }
            return;
        }
        if enabled {
            osk.enabled = true;
            osk.group = String::new();
            self.sync_osk_group();
            // A text field that already has the focus does not ask again; GNOME's keyboard is
            // likewise only opened by the next request.
            self.synoik.osk.focus = TextFocus::None;
            self.sync_osk_focus();
        } else {
            // `destroy`: gone at once, no slide.
            for token in [
                osk.rest_timer.take(),
                osk.hide_panel_timer.take(),
                osk.long_press_timer.take(),
            ]
            .into_iter()
            .flatten()
            {
                self.synoik.cancel_timer(token);
            }
            let delete_held = self.synoik.osk.delete_held;
            self.synoik.osk = Osk::default();
            if delete_held {
                self.osk_delete(false);
            }
        }
        self.synoik.queue_redraw_all();
    }

    /// `getCurrentGroup`: the active input source's xkb layout, and the focused field's content
    /// type — its purpose picks the layout (`purpose-changed` → `_updateKeys`), its hints the
    /// level (`content-hints-changed` → `_updateLevelFromHints(false)`).
    pub(crate) fn sync_osk_group(&mut self) {
        if !self.synoik.osk.enabled {
            return;
        }
        let sources = &self.synoik.gnome_settings.input_sources;
        let group = sources
            .mru_sources
            .first()
            .or_else(|| sources.sources.first())
            // An IBus source's layout is its engine's (`InputSource._getXkbId`); without the
            // engine's description it is the id, which no layout matches, so `us` it is.
            .map_or_else(|| "us".to_owned(), |(_, id)| id.clone());
        let (purpose, hints) = self.osk_content_type();
        let osk = &mut self.synoik.osk;
        let hints_changed = osk.hints != hints;
        osk.hints = hints;
        if osk.group != group || osk.purpose != purpose || osk.layout.is_none() {
            osk.group = group;
            osk.purpose = purpose;
            osk.update_keys();
            self.synoik.queue_redraw_all();
        } else if hints_changed {
            osk.update_level_from_hints(false);
            self.synoik.queue_redraw_all();
        }
    }

    /// The content type of whatever holds the text focus, as `(purpose, hints)`.
    fn osk_content_type(&self) -> (Purpose, u32) {
        let (purpose, hints) = self
            .synoik
            .input_method
            .as_ref()
            .map_or((0, 0), |im| im.content_type());
        (Purpose::from_ibus(purpose), hints)
    }

    /// The text focus as gnome-shell's keyboard would see it.
    ///
    /// One of our entries counts only once it really holds the key focus: the shield's prompt
    /// once its page is up, and the overview's search entry once it is expanded or searching —
    /// both take key focus at that point and not before (`unlockDialog.js:799-800`,
    /// `searchController.js:239`).
    fn osk_text_focus(&self) -> TextFocus {
        match self.shell_im_entry() {
            Some(ShellEntry::Shield) => {
                let prompt = if self.synoik.greeter {
                    matches!(
                        self.synoik.login_page,
                        crate::synoik::LoginPage::Prompt { .. }
                    )
                } else {
                    self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Prompt
                };
                if prompt {
                    TextFocus::Shell(ShellEntry::Shield)
                } else {
                    TextFocus::None
                }
            }
            Some(ShellEntry::OverviewSearch) => {
                let search = &self.synoik.overview_search;
                if search.is_active() || search.is_expanded() {
                    TextFocus::Shell(ShellEntry::OverviewSearch)
                } else {
                    TextFocus::None
                }
            }
            Some(entry) => TextFocus::Shell(entry),
            None if self
                .synoik
                .input_method
                .as_ref()
                .is_some_and(|im| im.is_enabled()) =>
            {
                TextFocus::Client
            }
            None => TextFocus::None,
        }
    }

    /// Follow the text focus: the input method's `focus_in`/`focus_out` and the keyboard's
    /// `notify::key-focus`. Runs with every focus update.
    pub fn sync_osk_focus(&mut self) {
        if !self.synoik.osk.enabled {
            return;
        }
        let focus = self.osk_text_focus();
        let was = self.synoik.osk.focus;
        if focus == was {
            return;
        }
        self.synoik.osk.focus = focus;

        // The new field's purpose picks the layout (`notify::content-purpose`).
        self.sync_osk_group();

        // `_onKeyFocusChanged`: an entry of ours opens it, and the key focus going anywhere else
        // closes it.
        match focus {
            TextFocus::Shell(_) => self.osk_open(false),
            _ if matches!(was, TextFocus::Shell(_)) => self.osk_close(false),
            _ => {}
        }

        // `vfunc_focus_in` cancels a pending hide; `vfunc_focus_out` arms one.
        if focus != TextFocus::None {
            if let Some(token) = self.synoik.osk.hide_panel_timer.take() {
                self.synoik.cancel_timer(token);
            }
        } else if was != TextFocus::None && self.synoik.osk.hide_panel_timer.is_none() {
            let token = self.synoik.timer_after(HIDE_PANEL_TIME, |state| {
                state.synoik.osk.hide_panel_timer = None;
                state.osk_panel_state(PanelState::Off);
                None
            });
            self.synoik.osk.hide_panel_timer = Some(token);
        }
    }

    /// A client enabled its text input. Doing so while it already holds the input method is how a
    /// version-1 client asks for the panel (`enable_panel_v1`).
    pub fn osk_text_input_enabled(&mut self, already_focused: bool) {
        if already_focused {
            self.osk_panel_state(PanelState::On);
        }
    }

    /// `_onKeyboardStateChanged` (`keyboard.js:1652-1667`).
    pub fn osk_panel_state(&mut self, state: PanelState) {
        if !self.synoik.osk.enabled {
            return;
        }
        let open = match state {
            PanelState::Off => false,
            PanelState::On => true,
            PanelState::Toggle => !self.synoik.osk.visible,
        };
        if open {
            self.osk_open(false);
        } else {
            self.osk_close(true);
        }
    }

    fn osk_clear_rest_timer(&mut self) {
        if let Some(token) = self.synoik.osk.rest_timer.take() {
            self.synoik.cancel_timer(token);
        }
    }

    /// `Keyboard.open` (`keyboard.js:1702-1726`).
    pub fn osk_open(&mut self, immediate: bool) {
        let osk = &mut self.synoik.osk;
        if !osk.enabled {
            return;
        }
        osk.requested = true;
        if osk.visible {
            return;
        }
        self.osk_clear_rest_timer();
        if immediate {
            self.osk_open_now();
            return;
        }
        let token = self.synoik.timer_after(REST_TIME, |state| {
            state.synoik.osk.rest_timer = None;
            state.osk_open_now();
            None
        });
        self.synoik.osk.rest_timer = Some(token);
    }

    /// `_open` → `_animateShow`.
    fn osk_open_now(&mut self) {
        let clock = self.synoik.clock.clone();
        let osk = &mut self.synoik.osk;
        if !osk.requested || osk.visible {
            return;
        }
        osk.visible = true;
        osk.animate_to(&clock, 1., Curve::EaseOutQuad);
        // `_open` → `_setEmojiActive(false)`.
        osk.set_emoji_active(false);
        osk.bump();
        self.osk_animate_window(true);
        self.synoik.queue_redraw_all();
    }

    /// `Keyboard.close` (`keyboard.js:1738-1760`).
    pub fn osk_close(&mut self, immediate: bool) {
        let osk = &mut self.synoik.osk;
        osk.requested = false;
        if !osk.visible {
            return;
        }
        self.osk_clear_rest_timer();
        if immediate {
            self.osk_close_now();
            return;
        }
        let token = self.synoik.timer_after(REST_TIME, |state| {
            state.synoik.osk.rest_timer = None;
            state.osk_close_now();
            None
        });
        self.synoik.osk.rest_timer = Some(token);
    }

    /// `_close` → `_animateHide`, and the modifiers let go.
    fn osk_close_now(&mut self) {
        let clock = self.synoik.clock.clone();
        let osk = &mut self.synoik.osk;
        if osk.requested || !osk.visible {
            return;
        }
        osk.visible = false;
        osk.animate_to(&clock, 0., Curve::EaseInQuad);
        osk.modifiers.clear();
        osk.cancel_press();
        osk.hovered = None;
        osk.subkeys = None;
        osk.bump();
        if let Some(token) = osk.long_press_timer.take() {
            self.synoik.cancel_timer(token);
        }
        if std::mem::take(&mut self.synoik.osk.delete_held) {
            self.osk_delete(false);
        }
        self.synoik.panel_popover.close_osk_languages();
        self.osk_animate_window(false);
        // `setCursorLocation(null)`.
        self.synoik.osk.focus_window = None;
        self.synoik.queue_redraw_all();
    }

    /// `_animateWindow`: slide the focus window up by the keyboard's height, or back to where it
    /// was, on the keyboard's own time and curve.
    fn osk_animate_window(&mut self, show: bool) {
        let Some(height) = self.synoik.osk_full_height() else {
            return;
        };
        let Some(focus) = self.synoik.osk.focus_window.clone() else {
            return;
        };
        let dy = if show { -height } else { 0. } - focus.shift;
        if dy == 0. {
            return;
        }
        let config = synoik_config::Animation {
            off: self.synoik.config.borrow().animations.off,
            kind: synoik_config::animations::Kind::Easing(
                synoik_config::animations::EasingParams {
                    duration_ms: ANIMATION_TIME.as_millis() as u32,
                    curve: synoik_config::animations::Curve::EaseOutQuad,
                },
            ),
        };
        let moved = self
            .synoik
            .layout
            .slide_floating_window_y(&focus.window, dy, config)
            .unwrap_or(0.);
        if let Some(focus) = self.synoik.osk.focus_window.as_mut() {
            focus.shift += moved;
        }
        // Our own move is not the window moving (`_windowSlideAnimationComplete` disconnects
        // `window-moved` around its `move_frame`).
        self.synoik.osk.watched = self.osk_focused_window_position();
        self.synoik.queue_redraw_all();
    }

    /// `_setFocusWindow`.
    fn osk_set_focus_window(&mut self, window: Option<Window>) {
        let current = self.synoik.osk.focus_window.as_ref().map(|f| &f.window);
        if current == window.as_ref() {
            return;
        }
        let visible = self.synoik.osk.visible;
        if visible {
            self.osk_animate_window(false);
        }
        self.synoik.osk.focus_window = window.map(|window| FocusWindow { window, shift: 0. });
        if visible {
            self.osk_animate_window(true);
        }
    }

    /// `_onFocusPositionChanged` → `setCursorLocation`: the caret moved. A caret the keyboard
    /// would cover takes its window up; once the window is up, it stays until the caret would be
    /// clear of the keyboard with the window back down.
    pub fn osk_cursor_location_changed(&mut self) {
        if !self.synoik.osk.enabled {
            return;
        }
        let caret = self.text_caret_rect();
        let window = self.synoik.layout.focus().map(|m| m.window.clone());
        let output = self
            .synoik
            .osk_output()
            .and_then(|o| self.synoik.global_space.output_geometry(&o));
        let (Some(caret), Some(window), Some(output)) = (caret, window, output) else {
            self.osk_set_focus_window(None);
            return;
        };
        let height = height(output.size.to_f64());
        let keyboard_y1 = f64::from(output.loc.y + output.size.h) - height;
        let bottom = caret.loc.y + caret.size.h;
        let focus = self.synoik.osk.focus_window.as_ref().map(|f| &f.window);
        if focus == Some(&window) {
            if bottom + height < keyboard_y1 {
                self.osk_set_focus_window(None);
            }
            return;
        }
        self.osk_set_focus_window((bottom >= keyboard_y1).then_some(window));
    }

    fn osk_focused_window_position(&self) -> Option<(Window, Option<Point<f64, Logical>>)> {
        let window = self.synoik.layout.focus()?.window.clone();
        let pos = self.synoik.layout.floating_pos_of(&window);
        Some((window, pos))
    }

    /// The focused window's `position-changed` (`FocusTracker` `window-moved`), and the start of
    /// a move grab on it (`window-grabbed`): either lets go of the window where it is and closes
    /// the keyboard (`_onFocusWindowMoving`). `grabbed` says a move grab began on the focused
    /// window; otherwise this compares its position with the last look. Runs every refresh.
    pub fn osk_watch_focus_window(&mut self, grabbed: bool) {
        if !self.synoik.osk.enabled {
            self.synoik.osk.watched = None;
            return;
        }
        let watched = self.osk_focused_window_position();
        let moved = grabbed
            || matches!(
                (&self.synoik.osk.watched, &watched),
                (Some((was, was_pos)), Some((now, now_pos))) if was == now && was_pos != now_pos
            );
        self.synoik.osk.watched = watched;
        if !moved {
            return;
        }
        let current = self.synoik.osk.watched.as_ref().map(|(w, _)| w);
        if self.synoik.osk.focus_window.as_ref().map(|f| &f.window) == current {
            // Not `_setFocusWindow(null)`: that would move the window under the user's grab.
            self.synoik.osk.focus_window = None;
        }
        self.osk_close(true);
    }

    /// The pointer moved: light the key under it (`track_hover`), or drag the emoji pager.
    pub fn osk_pointer_motion(&mut self, pos: Point<f64, Logical>) {
        if let Some(by @ Presser::Button(_)) = self.synoik.osk.emoji_swipe_presser() {
            let now = self.synoik.clock.now();
            if self.synoik.osk.emoji_swipe_motion(by, pos, now) {
                self.synoik.queue_redraw_all();
                return;
            }
        }
        if self.synoik.osk.subkeys.is_some() {
            let hovered = self.synoik.osk_subkey_hit(pos);
            if self.synoik.osk.set_subkey_hovered(hovered) {
                self.synoik.queue_redraw_all();
            }
            return;
        }
        let hovered = self.synoik.osk_hit(pos).flatten();
        if self.synoik.osk.set_hovered(hovered) {
            self.synoik.queue_redraw_all();
        }
    }

    /// A pointer button over the keyboard. Returns whether the press goes no further; its release
    /// always does, to be claimed by the caller's `suppressed_buttons`, so the window under the
    /// keyboard never sees a release it was not pressed for.
    pub fn osk_pointer_button(&mut self, button: u32, pressed: bool) -> bool {
        if self.synoik.osk.subkeys.is_some() {
            if pressed {
                let pos = self.synoik.seat.get_pointer().unwrap().current_location();
                let inside = self.synoik.osk_subkey_hit(pos);
                self.synoik
                    .osk
                    .subkeys_press(inside, Presser::Button(button));
                self.synoik.suppressed_buttons.insert(button);
                self.synoik.queue_redraw_all();
                return true;
            }
            let effects = self.synoik.osk.subkeys_release(Presser::Button(button));
            self.osk_apply(effects);
            self.synoik.queue_redraw_all();
            return false;
        }
        if pressed {
            let pos = self.synoik.seat.get_pointer().unwrap().current_location();
            let Some(hit) = self.synoik.osk_hit(pos) else {
                return false;
            };
            if let Some(key) = hit {
                self.osk_press(key, Presser::Button(button));
            }
            self.osk_emoji_swipe_begin(Presser::Button(button), pos);
            // A press on the keyboard's background is taken all the same.
            self.synoik.suppressed_buttons.insert(button);
            true
        } else {
            let clock = self.synoik.clock.clone();
            if self
                .synoik
                .osk
                .emoji_swipe_end(Presser::Button(button), clock.now(), &clock)
            {
                self.synoik.queue_redraw_all();
                return false;
            }
            let held = self
                .synoik
                .osk
                .pressed
                .is_some_and(|p| p.by == Presser::Button(button));
            if held {
                self.osk_release(Presser::Button(button));
            }
            false
        }
    }

    /// A touch point went down. Returns whether the keyboard took it.
    pub fn osk_touch_down(&mut self, slot: i32, pos: Point<f64, Logical>) -> bool {
        if self.synoik.osk.subkeys.is_some() {
            let inside = self.synoik.osk_subkey_hit(pos);
            self.synoik.osk.subkeys_press(inside, Presser::Touch(slot));
            self.synoik.queue_redraw_all();
            return true;
        }
        let Some(hit) = self.synoik.osk_hit(pos) else {
            return false;
        };
        if let Some(key) = hit {
            self.osk_press(key, Presser::Touch(slot));
        }
        self.osk_emoji_swipe_begin(Presser::Touch(slot), pos);
        true
    }

    /// A touch point moved. Returns whether it is the keyboard's.
    pub fn osk_touch_motion(&mut self, slot: i32, pos: Point<f64, Logical>) -> bool {
        let by = Presser::Touch(slot);
        let now = self.synoik.clock.now();
        if self.synoik.osk.emoji_swipe_motion(by, pos, now) {
            self.synoik.queue_redraw_all();
            return true;
        }
        self.synoik.osk.holds(by)
    }

    /// A scroll over the keyboard: a touchpad's turns the emoji pages, and whatever the source, the
    /// keyboard takes it. Returns whether it was over the keyboard.
    pub fn osk_axis(
        &mut self,
        pos: Point<f64, Logical>,
        continuous: bool,
        dx: f64,
        dy: f64,
        time: Duration,
    ) -> bool {
        let clock = self.synoik.clock.clone();
        if self.synoik.osk_hit(pos).is_none() {
            // Leaving the keyboard ends a scroll it had going.
            if self.synoik.osk.emoji_scroll(0., 0., 0., time, &clock) {
                self.synoik.queue_redraw_all();
            }
            return false;
        }
        // `ScrollGesture` takes only smooth scrolling, and only over the pager.
        if continuous {
            if let Some(pager) = self.synoik.osk_emoji_pager() {
                let stop = dx == 0. && dy == 0.;
                if (pager.contains(pos) || stop)
                    && self
                        .synoik
                        .osk
                        .emoji_scroll(dx, dy, pager.size.w, time, &clock)
                {
                    self.synoik.queue_redraw_all();
                }
            }
        }
        true
    }

    /// A press on the emoji pager may be the start of a swipe.
    fn osk_emoji_swipe_begin(&mut self, by: Presser, pos: Point<f64, Logical>) {
        if let Some(pager) = self.synoik.osk_emoji_pager() {
            if pager.contains(pos) {
                self.synoik.osk.emoji_swipe_begin(by, pos, pager.size.w);
            }
        }
    }

    /// A touch point went up. Returns whether it was the keyboard's.
    pub fn osk_touch_up(&mut self, slot: i32) -> bool {
        let clock = self.synoik.clock.clone();
        if self
            .synoik
            .osk
            .emoji_swipe_end(Presser::Touch(slot), clock.now(), &clock)
        {
            self.synoik.queue_redraw_all();
            return true;
        }
        if self.synoik.osk.subkeys.is_some() {
            let effects = self.synoik.osk.subkeys_release(Presser::Touch(slot));
            self.osk_apply(effects);
            self.synoik.queue_redraw_all();
            return true;
        }
        let held = self
            .synoik
            .osk
            .pressed
            .is_some_and(|p| p.by == Presser::Touch(slot));
        if held {
            self.osk_release(Presser::Touch(slot));
        }
        held
    }

    fn osk_press(&mut self, key: usize, by: Presser) {
        if !self.synoik.osk.press(key, by) {
            return;
        }
        if let Some(token) = self.synoik.osk.long_press_timer.take() {
            self.synoik.cancel_timer(token);
        }
        let token = self.synoik.timer_after(LONG_PRESS_TIME, |state| {
            state.synoik.osk.long_press_timer = None;
            let effects = state.synoik.osk.long_press();
            state.osk_apply(effects);
            state.synoik.queue_redraw_all();
            None
        });
        self.synoik.osk.long_press_timer = Some(token);
        self.synoik.queue_redraw_all();
    }

    fn osk_release(&mut self, by: Presser) {
        if let Some(token) = self.synoik.osk.long_press_timer.take() {
            self.synoik.cancel_timer(token);
        }
        let effects = self.synoik.osk.release(by);
        self.osk_apply(effects);
        self.synoik.queue_redraw_all();
    }

    fn osk_apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Commit(text, modifiers) => self.osk_commit(&text, &modifiers),
                Effect::Keyval(keyval) => {
                    self.osk_keyval(keyval, KeyState::Pressed);
                    self.osk_keyval(keyval, KeyState::Released);
                }
                Effect::Delete(enabled) => self.osk_delete(enabled),
                Effect::Hide => self.osk_close(true),
                Effect::LanguageMenu(key) => self.osk_language_menu(key),
            }
        }
    }

    /// `_popupLanguageMenu`: the input sources, over the key.
    fn osk_language_menu(&mut self, key: usize) {
        let Some(output) = self.synoik.osk_output() else {
            return;
        };
        let Some(rect) = self.synoik.osk_rect(&output) else {
            return;
        };
        let Some(cap) = self.synoik.osk.level().and_then(|level| {
            crate::ui::osk::key_rects(rect.size, level)
                .get(key)
                .copied()
        }) else {
            return;
        };
        // The popover anchors output-local.
        let geo = self.synoik.global_space.output_geometry(&output).unwrap();
        let anchor = Rectangle::new(rect.loc + cap.loc - geo.loc.to_f64(), cap.size);
        let (items, active) = self.input_source_menu_snapshot();
        self.synoik
            .panel_popover
            .toggle_osk_languages(output, anchor, items, active);
        self.synoik.queue_redraw_all();
    }

    /// Whether a text field has the input method (`Main.inputMethod.currentFocus`).
    fn osk_has_im_focus(&self) -> bool {
        self.synoik.osk.focus != TextFocus::None
    }

    /// `KeyboardController.commit` (`keyboard.js:2063-2098`).
    fn osk_commit(&mut self, text: &str, modifiers: &[u32]) {
        // With no text field, or with modifiers, it is key presses.
        //
        // An IBus source sends its keys through the engine (`handleVirtualKey`); ours do, as key
        // presses through the pipeline, which offers each one to the engine first.
        let ibus_source = self
            .synoik
            .gnome_settings
            .input_sources
            .mru_sources
            .first()
            .is_some_and(|(kind, _)| kind == "ibus");
        if !self.osk_has_im_focus() || !modifiers.is_empty() || ibus_source {
            for &keyval in modifiers {
                self.osk_keyval(keyval, KeyState::Pressed);
            }
            for ch in text.chars() {
                let keysym = smithay::input::keyboard::xkb::utf32_to_keysym(ch as u32);
                if keysym == Keysym::NoSymbol {
                    // `_getKeyvalsFromString`: an unknown character sends nothing at all.
                    break;
                }
                self.osk_keyval(keysym.raw(), KeyState::Pressed);
                self.osk_keyval(keysym.raw(), KeyState::Released);
            }
            for &keyval in modifiers {
                self.osk_keyval(keyval, KeyState::Released);
            }
            return;
        }
        self.commit_to_text_focus(text);
    }

    /// `KeyboardController.keyvalPress`/`keyvalRelease`: a virtual key, through the pipeline a
    /// hardware key takes.
    fn osk_keyval(&mut self, keyval: u32, key_state: KeyState) {
        if !crate::input::synthetic::send_keysym(self, Keysym::new(keyval), key_state) {
            debug!("OSK keyval {keyval:#x} is not in the keymap");
        }
    }

    /// `toggleDelete` (`keyboard.js:2127-2208`), as a held BackSpace.
    ///
    /// **Divergence:** with a text field focused gnome-shell deletes through the input method's
    /// `delete_surrounding`, and on a long press deletes again on every surrounding-text update,
    /// whole words after 50; ours holds BackSpace and the client's key repeat does the rest —
    /// gnome-shell's own fallback for a field without surrounding text.
    fn osk_delete(&mut self, enabled: bool) {
        let state = if enabled {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.osk_keyval(Keysym::BackSpace.raw(), state);
    }
}

impl Synoik {
    /// `keyboardBox.height`: the keyboard's whole height on its display, however far up it is.
    pub fn osk_full_height(&self) -> Option<f64> {
        let output = self.osk_output()?;
        let geo = self.global_space.output_geometry(&output)?;
        Some(height(geo.size.to_f64()))
    }

    /// The keyboard's rectangle in global coordinates, if it is drawn on `output`.
    pub fn osk_rect(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        let osk = &self.osk;
        if !osk.is_drawn() || self.osk_output().as_ref() != Some(output) {
            return None;
        }
        let geo = self.global_space.output_geometry(output)?;
        let mut rect = geometry(geo.size.to_f64(), osk.progress());
        rect.loc += geo.loc.to_f64();
        Some(rect)
    }

    /// The display it goes on: the primary one (`keyboardIndex = primaryIndex`,
    /// `layout.js:595`).
    pub fn osk_output(&self) -> Option<Output> {
        if !self.osk.enabled {
            return None;
        }
        self.layout
            .primary_output()
            .or_else(|| self.global_space.outputs().next())
            .cloned()
    }

    /// The key under a global point, if the keyboard is there.
    pub fn osk_hit(&self, pos: Point<f64, Logical>) -> Option<Option<usize>> {
        let output = self.osk_output()?;
        let rect = self.osk_rect(&output)?;
        if !rect.contains(pos) {
            return None;
        }
        // A menu holding the input grab — its own language menu among them — takes every click,
        // over the keyboard too: inside it acts, anywhere else it closes (the popup's
        // stage-wide `captured-event`, a panel menu's modal grab).
        if self.panel_popover.grabs_input() {
            return None;
        }
        let level = self.osk.level()?;
        let local = pos - rect.loc;
        Some(crate::ui::osk::key_at(rect.size, level, local))
    }

    /// Whether the keyboard takes the pointer at a global point: where it is drawn, and anywhere
    /// at all while a key's popup is up (its stage-wide `captured-event`).
    pub fn osk_contains(&self, pos: Point<f64, Logical>) -> bool {
        self.osk.subkeys.is_some() || self.osk_hit(pos).is_some()
    }

    /// The emoji pager, global, while the emoji page is up.
    pub fn osk_emoji_pager(&self) -> Option<Rectangle<f64, Logical>> {
        let output = self.osk_output()?;
        let rect = self.osk_rect(&output)?;
        let level = self.osk.level()?;
        let geometry = crate::ui::osk::emoji_geometry(rect.size, level)?;
        Some(Rectangle::new(
            rect.loc + geometry.pager.loc,
            geometry.pager.size,
        ))
    }

    /// The popup's geometry, global, if one is up.
    pub fn osk_subkeys_geometry(&self) -> Option<crate::ui::osk::SubkeysGeometry> {
        let subkeys = self.osk.subkeys.as_ref()?;
        let output = self.osk_output()?;
        let rect = self.osk_rect(&output)?;
        let level = self.osk.level()?;
        let cap = *crate::ui::osk::key_rects(rect.size, level).get(subkeys.key)?;
        let bounds = self.global_space.output_geometry(&output)?.to_f64();
        Some(crate::ui::osk::subkeys_geometry(
            Rectangle::new(rect.loc + cap.loc, cap.size),
            subkeys.strings.len(),
            bounds,
        ))
    }

    /// The popup key under a global point.
    pub fn osk_subkey_hit(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let geometry = self.osk_subkeys_geometry()?;
        geometry.keys.iter().position(|k| k.contains(pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osk() -> Osk {
        let mut osk = Osk {
            enabled: true,
            ..Osk::default()
        };
        osk.update_keys();
        osk
    }

    fn index(osk: &Osk, pred: impl Fn(&Key) -> bool) -> usize {
        osk.level().unwrap().keys.iter().position(pred).unwrap()
    }

    #[test]
    fn a_letter_commits_and_shift_falls_back_after_it() {
        let mut osk = osk();
        let shift = index(&osk, |k| {
            k.is_shift() && k.action == Some(Action::LevelSwitch("shift".into()))
        });
        osk.press(shift, Presser::Button(1));
        assert!(osk.release(Presser::Button(1)).is_empty());
        assert_eq!(osk.page, "shift");

        let q = index(&osk, |k| k.commit.as_deref() == Some("Q"));
        osk.press(q, Presser::Button(1));
        assert_eq!(
            osk.release(Presser::Button(1)),
            [Effect::Commit("Q".into(), vec![])]
        );
        assert_eq!(osk.page, "default");
    }

    #[test]
    fn a_long_pressed_shift_latches() {
        let mut osk = osk();
        let shift = index(&osk, |k| {
            k.is_shift() && k.action == Some(Action::LevelSwitch("shift".into()))
        });
        osk.press(shift, Presser::Touch(0));
        osk.long_press();
        osk.release(Presser::Touch(0));
        assert_eq!(osk.page, "shift");
        assert!(osk.latched());

        let q = index(&osk, |k| k.commit.as_deref() == Some("Q"));
        osk.press(q, Presser::Touch(0));
        osk.release(Presser::Touch(0));
        assert_eq!(osk.page, "shift", "a latched level stays");
    }

    #[test]
    fn a_release_from_another_presser_does_nothing() {
        let mut osk = osk();
        osk.press(0, Presser::Button(1));
        assert!(osk.release(Presser::Touch(3)).is_empty());
        assert_eq!(
            osk.release(Presser::Button(1)),
            [Effect::Commit("q".into(), vec![])]
        );
    }

    #[test]
    fn delete_taps_and_holds() {
        let mut osk = osk();
        let delete = index(&osk, |k| k.action == Some(Action::Delete));
        osk.press(delete, Presser::Button(1));
        assert_eq!(
            osk.release(Presser::Button(1)),
            [Effect::Delete(true), Effect::Delete(false)]
        );
        osk.press(delete, Presser::Button(1));
        assert_eq!(osk.long_press(), [Effect::Delete(true)]);
        assert_eq!(osk.release(Presser::Button(1)), [Effect::Delete(false)]);
    }

    #[test]
    fn modifiers_latch_and_ride_the_next_commit() {
        let mut osk = Osk {
            enabled: true,
            purpose: Purpose::Terminal,
            ..Osk::default()
        };
        osk.update_keys();
        let ctrl = index(&osk, |k| {
            k.action == Some(Action::Modifier) && k.keyval == Some(0xffe3)
        });
        osk.press(ctrl, Presser::Button(1));
        osk.release(Presser::Button(1));
        assert!(osk.modifier_latched(0xffe3));
        let c = index(&osk, |k| k.commit.as_deref() == Some("c"));
        osk.press(c, Presser::Button(1));
        assert_eq!(
            osk.release(Presser::Button(1)),
            [Effect::Commit("c".into(), vec![0xffe3])]
        );
        assert!(!osk.modifier_latched(0xffe3));
    }

    #[test]
    fn a_long_press_opens_the_other_strings() {
        let mut osk = osk();
        let e = index(&osk, |k| k.commit.as_deref() == Some("e"));
        osk.press(e, Presser::Button(1));
        assert!(osk.long_press().is_empty());
        assert_eq!(osk.subkeys().unwrap().strings, ["è", "é", "ê", "ë", "ē"]);
        // The press that opened it ends without typing anything, and leaves it up.
        assert!(osk.release(Presser::Button(1)).is_empty());
        assert!(osk.subkeys_release(Presser::Button(1)).is_empty());
        assert!(osk.subkeys().is_some());

        osk.subkeys_press(Some(1), Presser::Button(1));
        assert_eq!(
            osk.subkeys_release(Presser::Button(1)),
            [Effect::Commit("é".into(), vec![])]
        );
        assert!(osk.subkeys().is_none());

        // A press anywhere else closes it on its release, typing nothing.
        osk.press(e, Presser::Touch(2));
        osk.long_press();
        osk.subkeys_press(None, Presser::Touch(3));
        assert!(osk.subkeys().is_some());
        assert!(osk.subkeys_release(Presser::Touch(3)).is_empty());
        assert!(osk.subkeys().is_none());
    }

    #[test]
    fn sentences_and_titles_capitalise_from_the_surrounding_text() {
        let mut osk = osk();
        osk.hints = hints::UPPERCASE_SENTENCES;
        osk.update_level_from_hints(false);
        assert_eq!(osk.page, "default", "waiting on the text");
        osk.surrounding_text_set("", 0);
        assert_eq!(osk.page, "shift", "the start of the text");

        osk.update_level_from_hints(true);
        osk.surrounding_text_set("Hello. ", 7);
        assert_eq!(osk.page, "shift");
        osk.update_level_from_hints(true);
        osk.surrounding_text_set("Hello. W", 8);
        assert_eq!(osk.page, "default");
        // Only the one that was asked for: a stray update changes nothing.
        osk.surrounding_text_set("", 0);
        assert_eq!(osk.page, "default");

        osk.hints = hints::UPPERCASE_WORDS;
        osk.update_level_from_hints(true);
        osk.surrounding_text_set("Grand Hotel ", 12);
        assert_eq!(osk.page, "shift");
        osk.update_level_from_hints(true);
        osk.surrounding_text_set("Grand Hotel", 11);
        assert_eq!(osk.page, "default");
    }

    #[test]
    fn heights() {
        assert_eq!(height(Size::from((1920., 1080.))), 360.);
        assert_eq!(height(Size::from((1080., 1920.))), 480.);
    }
}
