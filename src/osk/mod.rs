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
//! `focus_out` (`inputMethod.js:209-226`) — or on its hide key.
//!
//! What a key does follows `KeyboardController` (`keyboard.js:1968-2210`): a string goes in through
//! the input method's commit when a text field has it, and as key presses otherwise; a keyval is a
//! virtual key press, through the same pipeline a hardware key takes — mutter's virtual keyboard
//! goes through `meta_seat_impl_notify_key_in_impl`, keyboard accessibility filters included.
//!
//! The model lives here; drawing and hit-testing are [`crate::ui::osk`].

pub mod layout;

use std::collections::BTreeSet;
use std::time::Duration;

use smithay::backend::input::KeyState;
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
        self.progress.as_ref().is_some_and(|a| !a.is_done())
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn level(&self) -> Option<&Level> {
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
        self.set_active_level("default");
        self.bump();
    }

    /// `_setActiveLevel` (`keyboard.js:1669-1693`).
    fn set_active_level(&mut self, level: &str) {
        if self.page != level {
            self.latched = false;
            self.page = level.to_owned();
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
        if user_input && self.page == "shift" {
            self.set_active_level("default");
        }
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
        effects
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
                // The language menu and the emoji page are not ported yet.
                Action::LanguageMenu | Action::Emoji => {}
            },
            (None, Some(keyval), _) => {
                effects.push(Effect::Keyval(keyval));
                self.update_level_from_hints(true);
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

    /// `getCurrentGroup`: the active input source's xkb layout.
    fn sync_osk_group(&mut self) {
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
        osk.hints = hints;
        if osk.group != group || osk.purpose != purpose || osk.layout.is_none() {
            osk.group = group;
            osk.purpose = purpose;
            osk.update_keys();
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
        osk.bump();
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
        osk.bump();
        if let Some(token) = osk.long_press_timer.take() {
            self.synoik.cancel_timer(token);
        }
        if std::mem::take(&mut self.synoik.osk.delete_held) {
            self.osk_delete(false);
        }
        self.synoik.queue_redraw_all();
    }

    /// The pointer moved: light the key under it (`track_hover`).
    pub fn osk_pointer_motion(&mut self, pos: Point<f64, Logical>) {
        let hovered = self.synoik.osk_hit(pos).flatten();
        if self.synoik.osk.set_hovered(hovered) {
            self.synoik.queue_redraw_all();
        }
    }

    /// A pointer button over the keyboard. Returns whether the press goes no further; its release
    /// always does, to be claimed by the caller's `suppressed_buttons`, so the window under the
    /// keyboard never sees a release it was not pressed for.
    pub fn osk_pointer_button(&mut self, button: u32, pressed: bool) -> bool {
        if pressed {
            let pos = self.synoik.seat.get_pointer().unwrap().current_location();
            let Some(hit) = self.synoik.osk_hit(pos) else {
                return false;
            };
            if let Some(key) = hit {
                self.osk_press(key, Presser::Button(button));
            }
            // A press on the keyboard's background is taken all the same.
            self.synoik.suppressed_buttons.insert(button);
            true
        } else {
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
        let Some(hit) = self.synoik.osk_hit(pos) else {
            return false;
        };
        if let Some(key) = hit {
            self.osk_press(key, Presser::Touch(slot));
        }
        true
    }

    /// A touch point went up. Returns whether it was the keyboard's.
    pub fn osk_touch_up(&mut self, slot: i32) -> bool {
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
            }
        }
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
        let level = self.osk.level()?;
        let local = pos - rect.loc;
        Some(crate::ui::osk::key_at(rect.size, level, local))
    }

    /// Whether the keyboard covers a global point — it takes the pointer there, as chrome.
    pub fn osk_contains(&self, pos: Point<f64, Logical>) -> bool {
        self.osk_hit(pos).is_some()
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
    fn heights() {
        assert_eq!(height(Size::from((1920., 1080.))), 360.);
        assert_eq!(height(Size::from((1080., 1920.))), 480.);
    }
}
