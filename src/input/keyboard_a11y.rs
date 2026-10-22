// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The keyboard accessibility filters — mutter's `MetaKeyboardA11y`
//! (`src/backends/native/meta-keyboard-a11y.c`), fed from `org.gnome.desktop.a11y.keyboard`
//! the way `meta-input-settings.c:1291-1340` reads it. libinput reports keys untouched and
//! xkbcommon has no AccessX, so these live in the compositor, between the device and the keymap.
//!
//! - **Sticky Keys**: a modifier tapped on its own latches onto the next key; tapped twice, it
//!   locks until tapped again. It acts after the keymap, latching and locking in the xkb state
//!   itself, so clients and our own keybindings see the modifier.
//! - **Bounce Keys**: a press of the key last released less than `bouncekeys-delay` ago is dropped.
//! - **Slow Keys**: a press counts only once held for `slowkeys-delay`; released sooner, it never
//!   happened.
//! - **Mouse Keys**: with Num Lock off, the keypad moves the pointer and clicks.
//! - **Toggle Keys**: a beep when a key changes the lock LEDs.
//! - **The enable shortcuts** (`enable`): Shift pressed five times toggles Sticky Keys, Shift held
//!   for eight seconds toggles Slow Keys. Each writes its key back, so the menu follows
//!   (`meta_input_settings_notify_kbd_a11y_change`).
//!
//! The latched and locked modifiers get no indicator: mutter announces them
//! (`kbd-a11y-mods-state-changed`) and nothing in gnome-shell listens.
//!
//! Every beep is the system bell, `meta_bell_notify (display, NULL)`
//! (`meta_seat_native_bell_notify`): the visual and audible halves as the bell settings say.
//!
//! **Divergence:** mutter filters after the key has updated the xkb state
//! (`meta-seat-impl.c:636-646`), so a Shift that Slow Keys holds back, or Bounce Keys drops,
//! has still changed the modifiers clients see. Here a filtered key never reaches the keymap,
//! and a slow key, once accepted, is delivered through the whole input path as if pressed
//! then. A release goes the way its press went: swallowed if the press was, so xkb never sees
//! a release without its press. Gustavo's call.
//!
//! `timeout-enable` / `disable-timeout` are read by mutter and acted on nowhere, so not here
//! either.

use std::collections::HashSet;
use std::time::Duration;

use smithay::backend::input::{ButtonState, InputEvent, KeyState, Keycode};
use smithay::input::keyboard::{xkb, Keysym};

use crate::input::synthetic::{
    SyntheticInputBackend, SyntheticKeyboardKeyEvent, SyntheticPointerButtonEvent,
    SyntheticPointerMotionEvent,
};
use crate::synoik::State;
use crate::utils::timers::TimerToken;

/// `org.gnome.desktop.a11y.keyboard`, as `MetaKbdA11ySettings` holds it. Defaults are the
/// schema's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardA11ySettings {
    /// `enable`: the keyboard shortcuts that toggle Sticky and Slow Keys.
    pub enable: bool,
    /// `feature-state-change-beep`: beep when a shortcut toggles a feature.
    pub feature_state_change_beep: bool,
    pub bouncekeys_enable: bool,
    /// `bouncekeys-delay`, ms.
    pub bouncekeys_delay: u32,
    pub bouncekeys_beep_reject: bool,
    pub mousekeys_enable: bool,
    /// `mousekeys-max-speed`, pixels per second.
    pub mousekeys_max_speed: u32,
    /// `mousekeys-accel-time`, ms to reach the top speed.
    pub mousekeys_accel_time: u32,
    /// `mousekeys-init-delay`, ms from the first step to the second.
    pub mousekeys_init_delay: u32,
    pub slowkeys_enable: bool,
    /// `slowkeys-delay`, ms.
    pub slowkeys_delay: u32,
    pub slowkeys_beep_press: bool,
    pub slowkeys_beep_accept: bool,
    pub slowkeys_beep_reject: bool,
    pub stickykeys_enable: bool,
    pub stickykeys_two_key_off: bool,
    pub stickykeys_modifier_beep: bool,
    pub togglekeys_enable: bool,
}

impl Default for KeyboardA11ySettings {
    fn default() -> Self {
        Self {
            enable: false,
            feature_state_change_beep: false,
            bouncekeys_enable: false,
            bouncekeys_delay: 300,
            bouncekeys_beep_reject: false,
            mousekeys_enable: false,
            mousekeys_max_speed: 10,
            mousekeys_accel_time: 300,
            mousekeys_init_delay: 300,
            slowkeys_enable: false,
            slowkeys_delay: 300,
            slowkeys_beep_press: false,
            slowkeys_beep_accept: false,
            slowkeys_beep_reject: false,
            stickykeys_enable: false,
            stickykeys_two_key_off: false,
            stickykeys_modifier_beep: false,
            togglekeys_enable: false,
        }
    }
}

/// Holding Shift this long toggles Slow Keys (`start_toggle_slowkeys`).
const TOGGLE_SLOW_KEYS_HOLD: Duration = Duration::from_secs(8);
/// Shift presses this far apart restart the count to five (`handle_enablekeys_press`).
const SHIFT_COUNT_WINDOW_MS: u32 = 15_000;
/// Between Mouse Keys steps once the first delay is over (`trigger_mousekeys_move`).
const MOUSE_KEYS_INTERVAL: Duration = Duration::from_millis(100);
/// `MOUSEKEYS_CURVE`: the exponent of the acceleration curve.
const MOUSE_KEYS_CURVE: f64 = 1.0 + 50.0 * 0.001;

/// Mouse Keys' button selection (`CLUTTER_BUTTON_*`), as evdev codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const MOUSE_KEYS_BUTTONS: [u32; 3] = [BTN_LEFT, BTN_MIDDLE, BTN_RIGHT];

/// The filters' state (`struct _MetaKeyboardA11y`).
#[derive(Default)]
pub struct KeyboardA11y {
    /// The settings last applied, against which a change is measured
    /// (`meta_keyboard_a11y_apply_settings_in_impl`).
    applied: KeyboardA11ySettings,
    /// Keys whose press a filter swallowed: their release goes the same way.
    swallowed: HashSet<Keycode>,
    /// The next key event bypasses the filters: an accepted slow key being delivered.
    replaying: bool,
    /// Slow keys waiting out the delay, with the timer that accepts each.
    slow_keys: Vec<(Keycode, TimerToken)>,
    /// The key last released and when its bounce window closes, on the compositor's clock.
    debounce: Option<(Keycode, Duration)>,
    toggle_slow_keys_timer: Option<TimerToken>,
    shift_count: u16,
    last_shift_time: u32,
    /// Sticky Keys' modifier masks (`stickykeys_*_mask`): the modifiers held when the last
    /// modifier went down, and those it latched or locked into the xkb state.
    sticky_depressed: xkb::ModMask,
    sticky_latched: xkb::ModMask,
    sticky_locked: xkb::ModMask,
    mouse: MouseKeys,
}

struct MouseKeys {
    /// The evdev button that clicks act on.
    button: u32,
    /// Which of [`MOUSE_KEYS_BUTTONS`] Mouse Keys holds down.
    held: [bool; 3],
    /// ms on the compositor's clock; 0 while not moving.
    first_motion_time: u64,
    last_motion_time: u64,
    curve_factor: f64,
    move_timer: Option<TimerToken>,
    last_key: Keysym,
}

impl Default for MouseKeys {
    fn default() -> Self {
        Self {
            button: BTN_LEFT,
            held: [false; 3],
            first_motion_time: 0,
            last_motion_time: 0,
            curve_factor: 0.,
            move_timer: None,
            last_key: Keysym::NoSymbol,
        }
    }
}

impl State {
    /// Run a key through the filters: whether it goes on to the keymap and the rest of the
    /// input path (`meta_keyboard_a11y_process_event_in_impl`, in its order).
    pub fn keyboard_a11y_admits(
        &mut self,
        keycode: Keycode,
        state: KeyState,
        time_ms: u32,
    ) -> bool {
        self.sync_keyboard_a11y();
        if std::mem::take(&mut self.synoik.keyboard_a11y.replaying) {
            return true;
        }
        let pressed = state == KeyState::Pressed;
        if !pressed && self.synoik.keyboard_a11y.swallowed.contains(&keycode) {
            // Its press never reached the keymap. The filters still see the release, as mutter's
            // do: it ends a Mouse Keys move and opens a bounce window.
            self.keyboard_a11y_filter(keycode, false, time_ms);
            self.synoik.keyboard_a11y.swallowed.remove(&keycode);
            return false;
        }
        let admitted = self.keyboard_a11y_filter(keycode, pressed, time_ms);
        if pressed && !admitted {
            self.synoik.keyboard_a11y.swallowed.insert(keycode);
        }
        admitted
    }

    fn keyboard_a11y_filter(&mut self, keycode: Keycode, pressed: bool, time_ms: u32) -> bool {
        let settings = self.synoik.keyboard_a11y.applied;
        let (keysym, num_lock) = self.keyboard_a11y_lookup(keycode);

        if settings.enable {
            if pressed {
                self.enable_keys_press(keysym, time_ms);
            } else {
                self.enable_keys_release(keysym);
            }
        }

        if settings.mousekeys_enable {
            let swallow = if pressed {
                self.mouse_keys_press(keysym, num_lock)
            } else {
                mouse_keys_release(keysym, num_lock) && {
                    self.stop_mouse_keys_move();
                    true
                }
            };
            if swallow {
                return false;
            }
        }

        if settings.bouncekeys_enable && settings.bouncekeys_delay != 0 {
            let now = self.synoik.clock.now_unadjusted();
            let a11y = &mut self.synoik.keyboard_a11y;
            if pressed {
                if a11y
                    .debounce
                    .is_some_and(|(key, until)| key == keycode && now < until)
                {
                    if settings.bouncekeys_beep_reject {
                        self.ring_bell(None);
                    }
                    return false;
                }
            } else {
                let delay = Duration::from_millis(u64::from(settings.bouncekeys_delay));
                a11y.debounce = Some((keycode, now + delay));
            }
        }

        if settings.slowkeys_enable && settings.slowkeys_delay != 0 {
            if pressed {
                self.start_slow_key(keycode, settings);
                return false;
            }
            let a11y = &mut self.synoik.keyboard_a11y;
            if let Some(i) = a11y.slow_keys.iter().position(|(key, _)| *key == keycode) {
                let (_, token) = a11y.slow_keys.remove(i);
                self.synoik.cancel_timer(token);
                if settings.slowkeys_beep_reject {
                    self.ring_bell(None);
                }
                return false;
            }
        }

        true
    }

    /// The keysym `keycode` makes under the current state, and whether Num Lock is locked
    /// (mutter's `is_numlock_active`: `Mod2`, `XKB_STATE_MODS_LOCKED`).
    fn keyboard_a11y_lookup(&mut self, keycode: Keycode) -> (Keysym, bool) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: nothing derived from the state outlives the lock guard.
            let state = unsafe { xkb.state() };
            (
                state.key_get_one_sym(keycode),
                state.mod_name_is_active(xkb::MOD_NAME_NUM, xkb::STATE_MODS_LOCKED),
            )
        })
    }

    /// Apply the settings if they changed since last time, clearing what each changed
    /// feature had going (`meta_keyboard_a11y_apply_settings_in_impl`).
    pub fn sync_keyboard_a11y(&mut self) {
        let new = self.synoik.gnome_settings.keyboard_a11y;
        let old = self.synoik.keyboard_a11y.applied;
        if new == old {
            return;
        }

        if new.enable != old.enable || new.slowkeys_enable != old.slowkeys_enable {
            for (_, token) in std::mem::take(&mut self.synoik.keyboard_a11y.slow_keys) {
                self.synoik.cancel_timer(token);
            }
        }
        if new.enable != old.enable || new.bouncekeys_enable != old.bouncekeys_enable {
            self.synoik.keyboard_a11y.debounce = None;
        }
        if new.enable != old.enable || new.stickykeys_enable != old.stickykeys_enable {
            self.synoik.keyboard_a11y.sticky_depressed = 0;
            self.set_sticky_mods(0, 0);
        }
        if new.enable != old.enable {
            if let Some(token) = self.synoik.keyboard_a11y.toggle_slow_keys_timer.take() {
                self.synoik.cancel_timer(token);
            }
            self.synoik.keyboard_a11y.shift_count = 0;
            self.synoik.keyboard_a11y.last_shift_time = 0;
        }
        // mutter keeps its virtual pointer while either flag is on, so the shortcuts'
        // switch resets Mouse Keys too.
        let mouse_on = |s: &KeyboardA11ySettings| s.enable || s.mousekeys_enable;
        if mouse_on(&new) != mouse_on(&old) || new.enable != old.enable {
            if mouse_on(&new) {
                self.enable_mouse_keys();
            } else {
                self.disable_mouse_keys();
            }
        }
        let a11y = &mut self.synoik.keyboard_a11y;
        a11y.mouse.curve_factor = f64::from(new.mousekeys_max_speed.max(1))
            / f64::from(new.mousekeys_accel_time.max(1)).powf(MOUSE_KEYS_CURVE);
        a11y.applied = new;
    }

    fn start_slow_key(&mut self, keycode: Keycode, settings: KeyboardA11ySettings) {
        let delay = Duration::from_millis(u64::from(settings.slowkeys_delay));
        let token = self.synoik.timer_after(delay, move |state| {
            state.accept_slow_key(keycode);
            None
        });
        self.synoik.keyboard_a11y.slow_keys.push((keycode, token));
        if settings.slowkeys_beep_press {
            self.ring_bell(None);
        }
    }

    /// `trigger_slow_keys`: the key was held long enough, so it is pressed now.
    fn accept_slow_key(&mut self, keycode: Keycode) {
        let a11y = &mut self.synoik.keyboard_a11y;
        a11y.slow_keys.retain(|(key, _)| *key != keycode);
        // Its release now goes to the keymap like any other.
        a11y.swallowed.remove(&keycode);
        a11y.replaying = true;
        let time = self.synoik.clock.now_unadjusted().as_micros() as u64;
        self.process_input_event(InputEvent::<SyntheticInputBackend>::Keyboard {
            event: SyntheticKeyboardKeyEvent {
                time,
                key_code: keycode,
                state: KeyState::Pressed,
            },
        });
        if self.synoik.keyboard_a11y.applied.slowkeys_beep_accept {
            self.ring_bell(None);
        }
    }

    /// `handle_enablekeys_press`.
    fn enable_keys_press(&mut self, keysym: Keysym, time_ms: u32) {
        let a11y = &mut self.synoik.keyboard_a11y;
        if is_shift(keysym) {
            if a11y.toggle_slow_keys_timer.is_none() {
                let token = self.synoik.timer_after(TOGGLE_SLOW_KEYS_HOLD, |state| {
                    state.synoik.keyboard_a11y.toggle_slow_keys_timer = None;
                    if state.synoik.keyboard_a11y.applied.feature_state_change_beep {
                        state.ring_bell(None);
                    }
                    let on = !state.synoik.keyboard_a11y.applied.slowkeys_enable;
                    state.keyboard_a11y_shortcut(KeyboardA11yFeature::SlowKeys, on);
                    None
                });
                self.synoik.keyboard_a11y.toggle_slow_keys_timer = Some(token);
            }
            let a11y = &mut self.synoik.keyboard_a11y;
            if time_ms > a11y.last_shift_time.saturating_add(SHIFT_COUNT_WINDOW_MS) {
                a11y.shift_count = 1;
            } else {
                a11y.shift_count += 1;
            }
            a11y.last_shift_time = time_ms;
        } else {
            a11y.shift_count = 0;
            if let Some(token) = a11y.toggle_slow_keys_timer.take() {
                self.synoik.cancel_timer(token);
            }
        }
    }

    /// `handle_enablekeys_release`.
    fn enable_keys_release(&mut self, keysym: Keysym) {
        if !is_shift(keysym) {
            return;
        }
        let a11y = &mut self.synoik.keyboard_a11y;
        if let Some(token) = a11y.toggle_slow_keys_timer.take() {
            self.synoik.cancel_timer(token);
        }
        let a11y = &mut self.synoik.keyboard_a11y;
        if a11y.shift_count >= 5 {
            a11y.shift_count = 0;
            let on = !a11y.applied.stickykeys_enable;
            if a11y.applied.feature_state_change_beep {
                self.ring_bell(None);
            }
            self.keyboard_a11y_shortcut(KeyboardA11yFeature::StickyKeys, on);
        }
    }

    /// A shortcut switched `feature` (`set_stickykeys_on`, `trigger_toggle_slowkeys`, …): switch
    /// it, and say so with the dialog that offers to undo it — mutter's `kbd-a11y-flags-changed`,
    /// which only a shortcut emits, never a settings change (`KbdA11yDialog`).
    fn keyboard_a11y_shortcut(&mut self, feature: KeyboardA11yFeature, on: bool) {
        self.set_keyboard_a11y_feature(feature, on);
        self.synoik.kbd_a11y_dialog.show(feature, on);
        self.synoik.queue_redraw_all();
    }

    /// Switch `feature`: take effect now, and write the key so the menu and Settings follow
    /// (`meta_input_settings_notify_kbd_a11y_change`, and the dialog's buttons).
    pub fn set_keyboard_a11y_feature(&mut self, feature: KeyboardA11yFeature, on: bool) {
        let toggle = match feature {
            KeyboardA11yFeature::StickyKeys => crate::gnome::A11yToggle::StickyKeys,
            KeyboardA11yFeature::SlowKeys => crate::gnome::A11yToggle::SlowKeys,
        };
        let settings = &mut self.synoik.gnome_settings;
        match feature {
            KeyboardA11yFeature::StickyKeys => settings.keyboard_a11y.stickykeys_enable = on,
            KeyboardA11yFeature::SlowKeys => settings.keyboard_a11y.slowkeys_enable = on,
        }
        // Ahead of the write coming back, so nothing reads the old value in between.
        settings.a11y.set(toggle, on);
        self.sync_keyboard_a11y();
        if let Some(writer) = &self.synoik.gnome_settings_writer {
            writer.set_a11y_toggle(toggle, on);
        }
    }

    /// `handle_mousekeys_press`: whether Mouse Keys took the key.
    fn mouse_keys_press(&mut self, keysym: Keysym, num_lock: bool) -> bool {
        self.stop_mouse_keys_move();
        if num_lock {
            return false;
        }

        let button = match keysym {
            Keysym::KP_Divide => Some(BTN_LEFT),
            Keysym::KP_Multiply => Some(BTN_MIDDLE),
            Keysym::KP_Subtract => Some(BTN_RIGHT),
            _ => None,
        };
        if let Some(button) = button {
            self.synoik.keyboard_a11y.mouse.button = button;
            return true;
        }

        match keysym {
            Keysym::KP_Begin | Keysym::KP_5 => {
                self.mouse_keys_button(true);
                self.mouse_keys_button(false);
            }
            Keysym::KP_Insert | Keysym::KP_0 => self.mouse_keys_button(true),
            Keysym::KP_Decimal | Keysym::KP_Delete => self.mouse_keys_button(false),
            Keysym::KP_Add => {
                for _ in 0..2 {
                    self.mouse_keys_button(true);
                    self.mouse_keys_button(false);
                }
            }
            _ if mouse_keys_direction(keysym).is_some() => self.start_mouse_keys_move(keysym),
            _ => return false,
        }
        true
    }

    /// `emulate_button_press` / `_release`: press or release the selected button, unless it is
    /// already that way.
    fn mouse_keys_button(&mut self, press: bool) {
        let button = self.synoik.keyboard_a11y.mouse.button;
        let index = MOUSE_KEYS_BUTTONS
            .iter()
            .position(|b| *b == button)
            .unwrap();
        let held = &mut self.synoik.keyboard_a11y.mouse.held[index];
        if *held == press {
            return;
        }
        *held = press;
        let time = self.synoik.clock.now_unadjusted().as_micros() as u64;
        self.process_input_event(InputEvent::<SyntheticInputBackend>::PointerButton {
            event: SyntheticPointerButtonEvent {
                time,
                button_code: button,
                state: if press {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        });
    }

    /// `start_mousekeys_move`.
    fn start_mouse_keys_move(&mut self, keysym: Keysym) {
        self.synoik.keyboard_a11y.mouse.last_key = keysym;
        if self.synoik.keyboard_a11y.mouse.move_timer.is_some() {
            return;
        }
        let next = self.mouse_keys_step();
        let token = self
            .synoik
            .timer_after(next, |state| Some(state.mouse_keys_step()));
        self.synoik.keyboard_a11y.mouse.move_timer = Some(token);
    }

    /// `trigger_mousekeys_move`: one step, and how long until the next — the first delay before
    /// the second, then the steady interval.
    fn mouse_keys_step(&mut self) -> Duration {
        let mouse = &self.synoik.keyboard_a11y.mouse;
        let next = if mouse.first_motion_time == 0 {
            Duration::from_millis(u64::from(
                self.synoik.keyboard_a11y.applied.mousekeys_init_delay,
            ))
        } else {
            MOUSE_KEYS_INTERVAL
        };
        if let Some((dx, dy)) = mouse_keys_direction(mouse.last_key) {
            self.mouse_keys_motion(dx, dy);
        }
        next
    }

    /// `emulate_pointer_motion`, with the speed `mousekeys_get_speed_factor` gives.
    fn mouse_keys_motion(&mut self, dx: i8, dy: i8) {
        let now = self.synoik.clock.now_unadjusted();
        let time = now.as_millis() as u64;
        let settings = self.synoik.keyboard_a11y.applied;
        let mouse = &mut self.synoik.keyboard_a11y.mouse;

        let speed = if mouse.first_motion_time == 0 {
            // Acceleration starts after the first step, so the clock starts at the delay.
            mouse.first_motion_time = time + u64::from(settings.mousekeys_init_delay);
            mouse.last_motion_time = mouse.first_motion_time;
            1.0
        } else if time < mouse.last_motion_time {
            0.0
        } else {
            let init_time = time.saturating_sub(mouse.first_motion_time);
            let delta_t = (time - mouse.last_motion_time) as f64;
            mouse.last_motion_time = time;
            if init_time < u64::from(settings.mousekeys_accel_time.max(1)) {
                mouse.curve_factor * (init_time as f64).powf(MOUSE_KEYS_CURVE) * delta_t / 1000.
            } else {
                f64::from(settings.mousekeys_max_speed.max(1)) * delta_t / 1000.
            }
        };

        // Away from zero, so any speed at all moves at least a pixel.
        let away = |d: i8| {
            let v = f64::from(d) * speed;
            if d < 0 {
                v.floor()
            } else {
                v.ceil()
            }
        };
        let event = SyntheticPointerMotionEvent {
            time: now.as_micros() as u64,
            dx: away(dx),
            dy: away(dy),
        };
        // A virtual device's move, which pointer accessibility does not count
        // (`CLUTTER_EVENT_FLAG_SYNTHETIC`).
        let moving = std::mem::replace(&mut self.synoik.pointer_a11y.moving, true);
        self.process_input_event(InputEvent::<SyntheticInputBackend>::PointerMotion { event });
        self.synoik.pointer_a11y.moving = moving;
    }

    /// `stop_mousekeys_move`.
    fn stop_mouse_keys_move(&mut self) {
        let mouse = &mut self.synoik.keyboard_a11y.mouse;
        mouse.first_motion_time = 0;
        mouse.last_motion_time = 0;
        if let Some(token) = mouse.move_timer.take() {
            self.synoik.cancel_timer(token);
        }
    }

    /// `enable_mousekeys`.
    fn enable_mouse_keys(&mut self) {
        self.stop_mouse_keys_move();
        let mouse = &mut self.synoik.keyboard_a11y.mouse;
        mouse.button = BTN_LEFT;
        mouse.last_key = Keysym::NoSymbol;
    }

    /// `disable_mousekeys`: and no button left held behind.
    fn disable_mouse_keys(&mut self) {
        self.stop_mouse_keys_move();
        for (i, button) in MOUSE_KEYS_BUTTONS.into_iter().enumerate() {
            if self.synoik.keyboard_a11y.mouse.held[i] {
                self.synoik.keyboard_a11y.mouse.button = button;
                self.mouse_keys_button(false);
            }
        }
    }

    /// Sticky Keys, once a key has gone through the keymap (`handle_stickykeys_press` /
    /// `_release`): a modifier pressed and released on its own latches, again locks, again
    /// unlocks; the next other key's release ends a latch. A modifier pressed together with a key
    /// stays an ordinary modifier, and two modifiers at once latch once.
    pub fn sticky_keys_after(&mut self, keycode: Keycode, pressed: bool) {
        let settings = self.synoik.keyboard_a11y.applied;
        if !settings.stickykeys_enable {
            return;
        }
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let (keysym, depressed, lock) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: nothing derived from the state or keymap outlives the lock guard.
            let (state, keymap) = unsafe { (xkb.state(), xkb.keymap()) };
            let lock = keymap.mod_get_index(xkb::MOD_NAME_CAPS);
            (
                state.key_get_one_sym(keycode),
                state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                1u32.checked_shl(lock).unwrap_or(0),
            )
        });
        let modifier = is_modifier(keysym);
        let a11y = &mut self.synoik.keyboard_a11y;

        if pressed {
            if !modifier {
                return;
            }
            if a11y.sticky_depressed != 0 && settings.stickykeys_two_key_off {
                self.keyboard_a11y_shortcut(KeyboardA11yFeature::StickyKeys, false);
                return;
            }
            // Lock itself cannot be sticky, though Caps Lock counts as a modifier key: it may be
            // remapped to one that can.
            a11y.sticky_depressed = depressed & !lock;
            return;
        }

        let held = std::mem::take(&mut a11y.sticky_depressed);
        let (mut latched, mut locked) = (a11y.sticky_latched, a11y.sticky_locked);
        if modifier {
            if held == 0 {
                return;
            }
            if locked & held != 0 {
                locked &= !held;
            } else if latched & held != 0 {
                locked |= held;
                latched &= !held;
            } else {
                latched |= held;
            }
            if settings.stickykeys_modifier_beep {
                self.ring_bell(None);
            }
        } else {
            if latched == 0 {
                return;
            }
            latched = 0;
        }
        self.set_sticky_mods(latched, locked);
    }

    /// `update_internal_xkb_state`: swap Sticky Keys' latched and locked modifiers in the xkb
    /// state for `latched` and `locked`, leaving whatever else latched or locked them alone. The
    /// focused client hears of it in a `wl_keyboard.modifiers`.
    fn set_sticky_mods(&mut self, latched: xkb::ModMask, locked: xkb::ModMask) {
        let a11y = &mut self.synoik.keyboard_a11y;
        let (old_latched, old_locked) = (a11y.sticky_latched, a11y.sticky_locked);
        if (old_latched, old_locked) == (latched, locked) {
            return;
        }
        a11y.sticky_latched = latched;
        a11y.sticky_locked = locked;
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        keyboard.with_xkb_state(self, |mut context| {
            let (current_latched, current_locked) = {
                let xkb = context.xkb().lock().unwrap();
                // SAFETY: nothing derived from the state outlives the lock guard.
                let state = unsafe { xkb.state() };
                (
                    state.serialize_mods(xkb::STATE_MODS_LATCHED),
                    state.serialize_mods(xkb::STATE_MODS_LOCKED),
                )
            };
            context.set_latched_and_locked_mods(
                (current_latched & !old_latched) | latched,
                (current_locked & !old_locked) | locked,
            );
        });
    }

    /// `meta_keyboard_a11y_maybe_notify_toggle_keys_in_impl`: a key changed the lock LEDs.
    pub fn keyboard_a11y_leds_changed(&mut self) {
        if self.synoik.keyboard_a11y.applied.togglekeys_enable {
            self.ring_bell(None);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardA11yFeature {
    StickyKeys,
    SlowKeys,
}

/// `key_event_is_modifier`.
fn is_modifier(keysym: Keysym) -> bool {
    matches!(
        keysym,
        Keysym::Shift_L
            | Keysym::Shift_R
            | Keysym::Control_L
            | Keysym::Control_R
            | Keysym::Alt_L
            | Keysym::Alt_R
            | Keysym::Meta_L
            | Keysym::Meta_R
            | Keysym::Super_L
            | Keysym::Super_R
            | Keysym::Hyper_L
            | Keysym::Hyper_R
            | Keysym::Caps_Lock
            | Keysym::Shift_Lock
    )
}

fn is_shift(keysym: Keysym) -> bool {
    matches!(keysym, Keysym::Shift_L | Keysym::Shift_R)
}

/// `handle_mousekeys_release`: whether the release is one of the keypad keys Mouse Keys owns.
fn mouse_keys_release(keysym: Keysym, num_lock: bool) -> bool {
    !num_lock
        && matches!(
            keysym,
            Keysym::KP_0
                | Keysym::KP_1
                | Keysym::KP_2
                | Keysym::KP_3
                | Keysym::KP_4
                | Keysym::KP_5
                | Keysym::KP_6
                | Keysym::KP_7
                | Keysym::KP_8
                | Keysym::KP_9
                | Keysym::KP_Add
                | Keysym::KP_Begin
                | Keysym::KP_Decimal
                | Keysym::KP_Delete
                | Keysym::KP_Divide
                | Keysym::KP_Down
                | Keysym::KP_End
                | Keysym::KP_Home
                | Keysym::KP_Insert
                | Keysym::KP_Left
                | Keysym::KP_Multiply
                | Keysym::KP_Page_Down
                | Keysym::KP_Page_Up
                | Keysym::KP_Right
                | Keysym::KP_Subtract
                | Keysym::KP_Up
        )
}

/// Which way a Mouse Keys key moves the pointer (`trigger_mousekeys_move`), or `None` for a key
/// that does not move it.
fn mouse_keys_direction(keysym: Keysym) -> Option<(i8, i8)> {
    Some(match keysym {
        Keysym::KP_Home | Keysym::KP_7 => (-1, -1),
        Keysym::KP_Up | Keysym::KP_8 => (0, -1),
        Keysym::KP_Page_Up | Keysym::KP_9 => (1, -1),
        Keysym::KP_Left | Keysym::KP_4 => (-1, 0),
        Keysym::KP_Right | Keysym::KP_6 => (1, 0),
        Keysym::KP_End | Keysym::KP_1 => (-1, 1),
        Keysym::KP_Down | Keysym::KP_2 => (0, 1),
        Keysym::KP_Page_Down | Keysym::KP_3 => (1, 1),
        _ => return None,
    })
}
