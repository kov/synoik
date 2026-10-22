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

/// Which keys a key index means: the letter level by name, or the emoji page by page.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LevelId {
    page: String,
    emoji: Option<usize>,
}

/// A key held down. Each presser holds its own (`_touchPressSlot`), so two thumbs type two keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    /// Index into the keys of the level it was pressed on.
    pub key: usize,
    pub by: Presser,
    /// The level it was pressed on: if that is gone by the release, so is the key.
    level: LevelId,
    /// Names this press to its long-press timer, which may outlive it.
    serial: u64,
}

/// Where a press lands while a key's popup is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubkeyHit {
    Key(usize),
    /// In the box, between its keys: the popup's own, so not a press elsewhere.
    Box,
    Outside,
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
    /// The emoji page's: a string from it is an emoji, committed as it is.
    emoji: bool,
}

/// The seat's touch mode (`ClutterSeat:touch-mode`, mutter's `update_touch_mode`): a touchscreen,
/// and either a tablet-mode switch that is on or, with no such switch, no pointer at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TouchMode {
    pub has_touchscreen: bool,
    pub has_pointer: bool,
    pub has_tablet_switch: bool,
    pub tablet_mode: bool,
}

impl TouchMode {
    pub fn active(&self) -> bool {
        if !self.has_touchscreen {
            false
        } else if self.has_tablet_switch {
            self.tablet_mode
        } else {
            !self.has_pointer
        }
    }
}

/// `ShellEdgeDragGesture` on the bottom edge (`shell-edge-drag-gesture.c`): a touch that starts
/// within `EDGE_THRESHOLD` of a display's bottom and is dragged `DRAG_DISTANCE` up it.
const EDGE_THRESHOLD: f64 = 20.;
const EDGE_DRAG_DISTANCE: f64 = 80.;
const EDGE_CANCEL_THRESHOLD: f64 = 100.;
const EDGE_CANCEL_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug)]
struct EdgeDrag {
    slot: i32,
    begin: Point<f64, Logical>,
    last: Point<f64, Logical>,
    /// The display it began on.
    monitor: Rectangle<f64, Logical>,
    /// Past the edge: the gesture has the touch.
    recognizing: bool,
    cancel_timer: Option<TimerToken>,
}

impl EdgeDrag {
    fn near_edge(&self, p: Point<f64, Logical>) -> bool {
        p.y > self.monitor.loc.y + self.monitor.size.h - EDGE_THRESHOLD
    }
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
    /// Each press's long-press clock, by the press's serial.
    long_press_timers: Vec<(u64, TimerToken)>,
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
    pressed: Vec<Press>,
    next_serial: u64,
    /// Touch points the keyboard took at their start: theirs to the end, whatever they did since.
    claimed_touches: BTreeSet<i32>,
    hovered: Option<usize>,
    /// `_longPressed`: the shift long-press already latched, so its release must not switch.
    long_pressed: bool,
    /// `_deleteEnabled` / `_backspacePressed`.
    delete_held: bool,
    /// `_surroundingTextId`: the level waits on the field's next surrounding text.
    awaiting_surrounding: bool,
    /// The caret's rect as last seen (`FocusTracker._rect`).
    caret: Option<Rectangle<f64, Logical>>,
    /// The commit being read carried surrounding text.
    pub(crate) surrounding_in_commit: bool,
    subkeys: Option<Subkeys>,
    emoji: Option<EmojiPage>,
    focus_window: Option<FocusWindow>,
    touch_mode: TouchMode,
    /// `_lastDeviceIsTouchscreen`: the last non-keyboard device used was a touchscreen.
    last_device_touchscreen: bool,
    edge_drag: Option<EdgeDrag>,
    /// The keyboard's progress while the edge drag pulls it up (`gestureProgress`).
    gesture: Option<f64>,
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
            long_press_timers: Vec::new(),
            focus: TextFocus::None,
            group: "us".to_owned(),
            purpose: Purpose::Normal,
            hints: 0,
            layout: None,
            page: "default".to_owned(),
            latched: false,
            modifiers: BTreeSet::new(),
            pressed: Vec::new(),
            next_serial: 0,
            claimed_touches: BTreeSet::new(),
            hovered: None,
            long_pressed: false,
            delete_held: false,
            awaiting_surrounding: false,
            caret: None,
            surrounding_in_commit: false,
            subkeys: None,
            emoji: None,
            focus_window: None,
            touch_mode: TouchMode::default(),
            last_device_touchscreen: false,
            edge_drag: None,
            gesture: None,
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
        if let Some(gesture) = self.gesture {
            return gesture;
        }
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

    /// Whether the key at `index` on the level shown is held down.
    pub fn is_pressed(&self, index: usize) -> bool {
        let level = self.level_id();
        self.pressed
            .iter()
            .any(|p| p.key == index && p.level == level)
    }

    fn level_id(&self) -> LevelId {
        LevelId {
            page: self.page.clone(),
            emoji: self.emoji.as_ref().map(|e| e.current),
        }
    }

    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    pub fn subkeys(&self) -> Option<&Subkeys> {
        self.subkeys.as_ref()
    }

    pub fn touch_mode(&self) -> TouchMode {
        self.touch_mode
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
    /// (`_updateIndicatorPosition`), given the pager's width. The position is the page within its
    /// section: gnome-shell passes the ring-wide `_curPage`, which puts the dot past the end in
    /// every section but the first — a bug, not a behaviour to port.
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
        self.pressed.iter().any(|p| p.by == by) || self.emoji_swipe_presser() == Some(by)
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
        self.pressed.clear();
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
        // `_setActiveLevel` returns before `_disableAllModifiers` when nothing changes.
        if self.page == level {
            return;
        }
        self.latched = false;
        self.page = level.to_owned();
        // Its key is no longer mapped (`notify::mapped`).
        self.subkeys = None;
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
    /// Returns whether it was waiting.
    pub fn surrounding_text_set(&mut self, text: &str, cursor: usize) -> bool {
        if !std::mem::take(&mut self.awaiting_surrounding) {
            return false;
        }
        let before = text.get(..cursor).unwrap_or(text);
        let shift = if text.is_empty() || cursor == 0 {
            true
        } else if self.hints & hints::UPPERCASE_WORDS != 0 {
            before.ends_with(' ')
        } else if self.hints & hints::UPPERCASE_SENTENCES != 0 {
            before.trim_end().ends_with('.')
        } else {
            return true;
        };
        self.set_active_level(if shift { "shift" } else { "default" });
        true
    }

    /// A key went down (`Key._press`). Returns the serial its long-press clock goes by, if it took
    /// the press: a key already held, or a presser already holding one, takes no other.
    pub fn press(&mut self, key: usize, by: Presser) -> Option<u64> {
        let level = self.level_id();
        if self.key(key).is_none()
            || self
                .pressed
                .iter()
                .any(|p| p.by == by || (p.key == key && p.level == level))
        {
            return None;
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        self.pressed.push(Press {
            key,
            by,
            level,
            serial,
        });
        self.bump();
        Some(serial)
    }

    /// A press's long-press timer fired (`keyboard.js:285-299,1525-1536`). A press whose level is
    /// gone, or that ended, has nothing to do.
    pub fn long_press(&mut self, serial: u64) -> Vec<Effect> {
        let level = self.level_id();
        let Some(index) = self
            .pressed
            .iter()
            .find(|p| p.serial == serial && p.level == level)
            .map(|p| p.key)
        else {
            return Vec::new();
        };
        let Some(key) = self.key(index).cloned() else {
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
            self.pressed.retain(|p| p.serial != serial);
            // `keyButton.set_hover(false)`.
            self.hovered = None;
            if let Some(emoji) = self.emoji.as_mut() {
                emoji.swipe = None;
            }
            self.subkeys = Some(Subkeys {
                key: index,
                strings: key.extended.clone(),
                hovered: None,
                pressed: None,
                captured_press: false,
                emoji: key.flat,
            });
            self.bump();
        }
        effects
    }

    /// A press while the popup is up: on one of its keys, or anywhere else (`_onCapturedEvent`).
    pub fn subkeys_press(&mut self, hit: SubkeyHit, by: Presser) {
        let Some(subkeys) = self.subkeys.as_mut() else {
            return;
        };
        match hit {
            SubkeyHit::Key(i) if subkeys.pressed.is_none() => subkeys.pressed = Some((i, by)),
            // Inside the box the event is the popup's own (`boxPointer.bin.contains`).
            SubkeyHit::Key(_) | SubkeyHit::Box => {}
            SubkeyHit::Outside => subkeys.captured_press = true,
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
                let emoji = subkeys.emoji;
                self.subkeys = None;
                match text {
                    // `emoji-selected` → `commit(emoji)`: no modifiers, no level change.
                    Some(text) if emoji => effects.push(Effect::Commit(text, Vec::new())),
                    Some(text) => {
                        effects.push(Effect::Commit(
                            text,
                            self.modifiers.iter().copied().collect(),
                        ));
                        self.modifiers.clear();
                        self.update_level_from_hints(true);
                    }
                    None => {}
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
        let Some(at) = self.pressed.iter().position(|p| p.by == by) else {
            return Vec::new();
        };
        let press = self.pressed.remove(at);
        self.bump();
        // A key whose level went away under it (a hint change, the emoji page turning) went with
        // it: its release is nothing.
        if press.level != self.level_id() {
            return Vec::new();
        }
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
        if self.pressed.is_empty() {
            self.long_pressed = false;
        }
        effects
    }

    /// Drop every press without acting on it (`Key.cancel`). Their timers find nothing to do.
    fn cancel_press(&mut self) {
        if !self.pressed.is_empty() {
            self.pressed.clear();
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
        let setting = self
            .synoik
            .gnome_settings
            .a11y
            .get(crate::gnome::A11yToggle::ScreenKeyboard);
        // Or on its own, while a touchscreen is the seat's way in and the last one used
        // (`autoEnabled`).
        let osk = &mut self.synoik.osk;
        let enabled = setting || (osk.touch_mode.active() && osk.last_device_touchscreen);
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
            // A field that already has the focus, a client's or one of ours, does not open it:
            // GNOME's keyboard is only opened by the next request (`notify::key-focus` is
            // connected at construction, never run for the focus already there).
            self.synoik.osk.focus = self.osk_text_focus();
            self.sync_osk_group();
        } else {
            // `setCursorLocation(null)`, then `destroy`: the window it moved goes back, and the
            // keyboard is gone at once, no slide.
            // A window an edge drag moved has moved with the keyboard still hidden.
            if osk.focus_window.as_ref().is_some_and(|f| f.shift != 0.) {
                self.osk_animate_window(false);
            }
            self.synoik.panel_popover.close_osk_languages();
            let osk = &mut self.synoik.osk;
            let timers: Vec<TimerToken> = [
                osk.rest_timer.take(),
                osk.hide_panel_timer.take(),
                osk.edge_drag.take().and_then(|drag| drag.cancel_timer),
            ]
            .into_iter()
            .flatten()
            .chain(osk.long_press_timers.drain(..).map(|(_, token)| token))
            .collect();
            for token in timers {
                self.synoik.cancel_timer(token);
            }
            let delete_held = self.synoik.osk.delete_held;
            let (touch_mode, last_device_touchscreen) = (
                self.synoik.osk.touch_mode,
                self.synoik.osk.last_device_touchscreen,
            );
            self.synoik.osk = Osk {
                touch_mode,
                last_device_touchscreen,
                ..Osk::default()
            };
            if delete_held {
                self.osk_delete(false);
            }
        }
        self.synoik.queue_redraw_all();
    }

    /// The seat's devices changed: the touch mode follows (`notify::touch-mode`).
    pub fn osk_set_touch_mode(&mut self, touch_mode: TouchMode) {
        if self.synoik.osk.touch_mode != touch_mode {
            self.synoik.osk.touch_mode = touch_mode;
            self.sync_osk();
        }
    }

    /// A tablet-mode switch toggled.
    pub fn osk_set_tablet_mode(&mut self, on: bool) {
        let touch_mode = TouchMode {
            tablet_mode: on,
            ..self.synoik.osk.touch_mode
        };
        self.osk_set_touch_mode(touch_mode);
    }

    /// `last-device-changed`, for anything but a keyboard: whether it was a touchscreen.
    pub fn osk_last_device(&mut self, touchscreen: bool) {
        if self.synoik.osk.last_device_touchscreen != touchscreen {
            self.synoik.osk.last_device_touchscreen = touchscreen;
            self.sync_osk();
        }
    }

    /// A touch went down: it may be the bottom edge drag that pulls the keyboard up. The touch
    /// goes on to whatever is under it all the same until the drag is recognised.
    pub fn osk_edge_drag_down(&mut self, slot: i32, pos: Point<f64, Logical>) {
        // A second point cancels it (`n_points > 1`).
        if self.synoik.osk.edge_drag.is_some() {
            self.osk_edge_drag_cancel();
            return;
        }
        // `may-recognize`: anywhere but the lock screen; and only while there is a keyboard to
        // pull, and it is not up (`_bottomDragGesture.enabled = !visible`).
        let osk = &self.synoik.osk;
        if !osk.enabled || osk.visible || self.synoik.is_locked() {
            return;
        }
        let Some(monitor) = self
            .synoik
            .global_space
            .outputs()
            .filter_map(|o| self.synoik.global_space.output_geometry(o))
            .map(|g| g.to_f64())
            .find(|g| g.contains(pos))
        else {
            return;
        };
        let mut drag = EdgeDrag {
            slot,
            begin: pos,
            last: pos,
            monitor,
            recognizing: false,
            cancel_timer: None,
        };
        if !drag.near_edge(pos) {
            return;
        }
        // Still at the edge after `CANCEL_TIMEOUT_MS` is a touch resting there, not a drag.
        drag.cancel_timer = Some(self.synoik.timer_after(EDGE_CANCEL_TIMEOUT, |state| {
            if let Some(drag) = state.synoik.osk.edge_drag.as_mut() {
                drag.cancel_timer = None;
                if !drag.recognizing && drag.near_edge(drag.last) {
                    state.osk_edge_drag_cancel();
                }
            }
            None
        }));
        self.synoik.osk.edge_drag = Some(drag);
    }

    /// The touch moved. Returns whether the drag has it — then nothing under it sees it.
    pub fn osk_edge_drag_motion(&mut self, slot: i32, pos: Point<f64, Logical>) -> bool {
        let Some(drag) = self
            .synoik
            .osk
            .edge_drag
            .as_mut()
            .filter(|d| d.slot == slot)
        else {
            return false;
        };
        drag.last = pos;
        if (pos.x - drag.begin.x).abs() > EDGE_CANCEL_THRESHOLD {
            self.osk_edge_drag_cancel();
            return false;
        }
        if !drag.recognizing && !drag.near_edge(pos) {
            drag.recognizing = true;
            if let Some(token) = drag.cancel_timer.take() {
                self.synoik.cancel_timer(token);
            }
            // The gesture claims the touch: the client under it loses it.
            if let Some(touch) = self.synoik.seat.get_touch() {
                touch.cancel(self);
            }
        }
        let Some(drag) = self.synoik.osk.edge_drag.as_ref() else {
            return false;
        };
        if !drag.recognizing {
            return false;
        }
        let distance = (pos.y - drag.begin.y).abs();
        let completed = pos.y < drag.monitor.loc.y + drag.monitor.size.h - EDGE_DRAG_DISTANCE;
        self.osk_gesture_progress(distance);
        if completed {
            self.synoik.osk.edge_drag = None;
            self.osk_gesture_activate();
        }
        true
    }

    /// The touch lifted. Returns whether the drag had it.
    pub fn osk_edge_drag_up(&mut self, slot: i32) -> bool {
        let recognizing = match self.synoik.osk.edge_drag.as_ref() {
            Some(drag) if drag.slot == slot => drag.recognizing,
            _ => return false,
        };
        // An end before the distance is a cancel (`point_ended`).
        self.osk_edge_drag_cancel();
        recognizing
    }

    fn osk_edge_drag_cancel(&mut self) {
        let Some(drag) = self.synoik.osk.edge_drag.take() else {
            return;
        };
        if let Some(token) = drag.cancel_timer {
            self.synoik.cancel_timer(token);
        }
        if drag.recognizing {
            self.osk_gesture_cancel();
        }
    }

    /// `gestureProgress`: the keyboard follows the finger up, and the focus window with it.
    fn osk_gesture_progress(&mut self, delta: f64) {
        let Some(height) = self.synoik.osk_full_height() else {
            return;
        };
        let progress = delta.min(height) / height.max(1.);
        self.synoik.osk.gesture = Some(progress);
        self.synoik.osk.bump();
        if let Some(focus) = self.synoik.osk.focus_window.clone() {
            let dy = -height * progress - focus.shift;
            // The actor follows the finger, unanimated.
            let config = synoik_config::Animation {
                off: true,
                kind: synoik_config::animations::Kind::Easing(
                    synoik_config::animations::EasingParams {
                        duration_ms: 0,
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
            self.synoik.osk.watched = self.osk_focused_window_position();
        }
        self.synoik.queue_redraw_all();
    }

    /// `gestureActivate` → `open(true)`: up the rest of the way from where the finger left it.
    fn osk_gesture_activate(&mut self) {
        self.osk_open(true);
        self.synoik.osk.gesture = None;
        self.synoik.queue_redraw_all();
    }

    /// `gestureCancel` → `_animateHide`: back down from where the finger left it.
    fn osk_gesture_cancel(&mut self) {
        if self.synoik.osk.gesture.is_none() {
            return;
        }
        let clock = self.synoik.clock.clone();
        let osk = &mut self.synoik.osk;
        osk.animate_to(&clock, 0., Curve::EaseInQuad);
        osk.gesture = None;
        osk.bump();
        self.osk_animate_window(false);
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
        for (_, token) in std::mem::take(&mut osk.long_press_timers) {
            self.synoik.cancel_timer(token);
        }
        if std::mem::take(&mut self.synoik.osk.delete_held) {
            self.osk_delete(false);
        }
        self.synoik.panel_popover.close_osk_languages();
        self.osk_animate_window(false);
        // `setCursorLocation(null)`. The caret is forgotten with it, so that a reopen at the same
        // caret moves the window again — gnome-shell's tracker keeps it, and does not.
        self.synoik.osk.focus_window = None;
        self.synoik.osk.caret = None;
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
        // `FocusTracker._setCurrentRect`: the same rect again is no move.
        if caret == self.synoik.osk.caret {
            return;
        }
        self.synoik.osk.caret = caret;
        self.osk_place_for_caret(caret);
        // `_onFocusPositionChanged`: a caret moved may mean a new sentence, or a shift to drop.
        self.synoik.osk.update_level_from_hints(true);
        self.synoik.queue_redraw_all();
    }

    fn osk_place_for_caret(&mut self, caret: Option<Rectangle<f64, Logical>>) {
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
                let hit = self.synoik.osk_subkey_press_hit(pos);
                self.synoik.osk.subkeys_press(hit, Presser::Button(button));
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
            if self.synoik.osk.holds(Presser::Button(button)) {
                self.osk_release(Presser::Button(button));
            }
            false
        }
    }

    /// A touch point went down. Returns whether the keyboard took it.
    pub fn osk_touch_down(&mut self, slot: i32, pos: Point<f64, Logical>) -> bool {
        if self.synoik.osk.subkeys.is_some() {
            let hit = self.synoik.osk_subkey_press_hit(pos);
            self.synoik.osk.subkeys_press(hit, Presser::Touch(slot));
            self.synoik.osk.claimed_touches.insert(slot);
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
        self.synoik.osk.claimed_touches.insert(slot);
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
        self.synoik.osk.claimed_touches.contains(&slot)
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
        let claimed = self.synoik.osk.claimed_touches.remove(&slot);
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
            return claimed;
        }
        if self.synoik.osk.holds(Presser::Touch(slot)) {
            self.osk_release(Presser::Touch(slot));
        }
        claimed
    }

    /// The touch sequence was cancelled: the keys touch held let go without acting
    /// (`Key.cancel`), and a swipe settles where it is.
    pub fn osk_touch_cancel(&mut self) {
        let osk = &mut self.synoik.osk;
        osk.claimed_touches.clear();
        let before = osk.pressed.len();
        osk.pressed.retain(|p| !matches!(p.by, Presser::Touch(_)));
        if osk.pressed.len() != before {
            osk.bump();
        }
        if let Some(by @ Presser::Touch(_)) = osk.emoji_swipe_presser() {
            let clock = self.synoik.clock.clone();
            self.synoik.osk.emoji_swipe_end(by, clock.now(), &clock);
        }
        self.synoik.queue_redraw_all();
    }

    fn osk_press(&mut self, key: usize, by: Presser) {
        let Some(serial) = self.synoik.osk.press(key, by) else {
            return;
        };
        let token = self.synoik.timer_after(LONG_PRESS_TIME, move |state| {
            state
                .synoik
                .osk
                .long_press_timers
                .retain(|(s, _)| *s != serial);
            let effects = state.synoik.osk.long_press(serial);
            state.osk_apply(effects);
            state.synoik.queue_redraw_all();
            None
        });
        self.synoik.osk.long_press_timers.push((serial, token));
        self.synoik.queue_redraw_all();
    }

    fn osk_release(&mut self, by: Presser) {
        let osk = &mut self.synoik.osk;
        if let Some(serial) = osk.pressed.iter().find(|p| p.by == by).map(|p| p.serial) {
            if let Some(at) = osk.long_press_timers.iter().position(|(s, _)| *s == serial) {
                let (_, token) = osk.long_press_timers.remove(at);
                self.synoik.cancel_timer(token);
            }
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

    /// Where a press lands while the popup is up.
    fn osk_subkey_press_hit(&self, pos: Point<f64, Logical>) -> SubkeyHit {
        let Some(geometry) = self.osk_subkeys_geometry() else {
            return SubkeyHit::Outside;
        };
        match geometry.keys.iter().position(|k| k.contains(pos)) {
            Some(i) => SubkeyHit::Key(i),
            None if geometry.rect.contains(pos) => SubkeyHit::Box,
            None => SubkeyHit::Outside,
        }
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
        let serial = osk.press(shift, Presser::Touch(0)).unwrap();
        osk.long_press(serial);
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

    /// Each finger holds its own key (`_touchPressSlot`): two thumbs, overlapping, type two
    /// letters, each on its own release; a finger already on one key takes no other.
    #[test]
    fn two_fingers_hold_two_keys() {
        let mut osk = osk();
        let q = index(&osk, |k| k.commit.as_deref() == Some("q"));
        let w = index(&osk, |k| k.commit.as_deref() == Some("w"));
        let first = osk.press(q, Presser::Touch(0)).unwrap();
        let second = osk.press(w, Presser::Touch(1)).unwrap();
        assert_ne!(first, second);
        assert!(osk.press(w, Presser::Touch(2)).is_none(), "w is held");
        assert!(osk.is_pressed(q) && osk.is_pressed(w));
        assert_eq!(
            osk.release(Presser::Touch(0)),
            [Effect::Commit("q".into(), vec![])]
        );
        // The first press's clock, run late, has nothing left to do.
        assert!(osk.long_press(first).is_empty());
        assert_eq!(
            osk.release(Presser::Touch(1)),
            [Effect::Commit("w".into(), vec![])]
        );
    }

    /// A key held while its level goes away (a hint change between press and release) is gone
    /// with it: its release types nothing, and its long press opens nothing.
    #[test]
    fn a_press_does_not_outlive_its_level() {
        let mut osk = osk();
        let e = index(&osk, |k| k.commit.as_deref() == Some("e"));
        let serial = osk.press(e, Presser::Button(1)).unwrap();
        osk.set_active_level("shift");
        assert!(
            !osk.is_pressed(e),
            "the shift level's key there is not held"
        );
        assert!(osk.long_press(serial).is_empty());
        assert!(osk.subkeys().is_none());
        assert!(osk.release(Presser::Button(1)).is_empty());
    }

    /// `_setActiveLevel` to the level already shown is nothing: a latched modifier stays.
    #[test]
    fn the_same_level_again_keeps_the_modifiers() {
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
        osk.set_active_level("default");
        assert!(osk.modifier_latched(0xffe3));
        osk.set_active_level("shift");
        assert!(!osk.modifier_latched(0xffe3));
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
        let serial = osk.press(delete, Presser::Button(1)).unwrap();
        assert_eq!(osk.long_press(serial), [Effect::Delete(true)]);
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
        let serial = osk.press(e, Presser::Button(1)).unwrap();
        assert!(osk.long_press(serial).is_empty());
        assert_eq!(osk.subkeys().unwrap().strings, ["è", "é", "ê", "ë", "ē"]);
        // The press that opened it ends without typing anything, and leaves it up.
        assert!(osk.release(Presser::Button(1)).is_empty());
        assert!(osk.subkeys_release(Presser::Button(1)).is_empty());
        assert!(osk.subkeys().is_some());

        // A press on the box between its keys is the popup's: it stays.
        osk.subkeys_press(SubkeyHit::Box, Presser::Button(1));
        assert!(osk.subkeys_release(Presser::Button(1)).is_empty());
        assert!(osk.subkeys().is_some());
        osk.subkeys_press(SubkeyHit::Key(1), Presser::Button(1));
        assert_eq!(
            osk.subkeys_release(Presser::Button(1)),
            [Effect::Commit("é".into(), vec![])]
        );
        assert!(osk.subkeys().is_none());

        // A press anywhere else closes it on its release, typing nothing.
        let serial = osk.press(e, Presser::Touch(2)).unwrap();
        osk.long_press(serial);
        osk.subkeys_press(SubkeyHit::Outside, Presser::Touch(3));
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
