// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Synthetic input: fabricated events fed through the real input pipeline.
//!
//! [`SyntheticInputBackend`] is a minimal [`InputBackend`] whose events are
//! constructed in process rather than read from hardware. The headless test
//! fixture builds events directly; the IPC `InjectInput` request (`synoik msg
//! input`) goes through [`inject`], which additionally resolves key and
//! button names against the seat's active keymap.
//!
//! Only the event types we actually synthesize are real; the rest are
//! [`UnusedEvent`].

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
    DeviceCapability, Event, GestureBeginEvent, GestureEndEvent, GestureHoldBeginEvent,
    GestureHoldEndEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    InputBackend, InputEvent, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent,
    PointerButtonEvent, PointerMotionAbsoluteEvent, PointerMotionEvent, TouchDownEvent, TouchEvent,
    TouchSlot, TouchUpEvent, UnusedEvent,
};
use smithay::input::keyboard::{xkb, Keysym};
use smithay::output::Output;
use smithay::utils::{Logical, Point};
use synoik_ipc::InjectedEvent;

use crate::synoik::State;
use crate::utils::get_monotonic_time;

/// The offset between evdev keycodes and the X11/xkb keycode space keyboards
/// speak.
const XKB_KEYCODE_OFFSET: u32 = 8;

const KEY_LEFTSHIFT: u32 = 42;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Injects one IPC-described input event through the real input pipeline.
///
/// Events go through `process_input_event` exactly like hardware input, so
/// keyboard state, binds, grabs and focus all behave as if the keys were
/// physically pressed.
pub fn inject(state: &mut State, event: &InjectedEvent) -> Result<(), String> {
    match event {
        InjectedEvent::KeyPress { key } => {
            let key_code = resolve_key(state, key)?;
            send_key(state, key_code, KeyState::Pressed);
        }
        InjectedEvent::KeyRelease { key } => {
            let key_code = resolve_key(state, key)?;
            send_key(state, key_code, KeyState::Released);
        }
        InjectedEvent::Text { text } => {
            let shift = Keycode::new(KEY_LEFTSHIFT + XKB_KEYCODE_OFFSET);
            for ch in text.chars() {
                let (key_code, level) = resolve_char(state, ch)?;
                if level == 1 {
                    send_key(state, shift, KeyState::Pressed);
                }
                send_key(state, key_code, KeyState::Pressed);
                send_key(state, key_code, KeyState::Released);
                if level == 1 {
                    send_key(state, shift, KeyState::Released);
                }
            }
        }
        InjectedEvent::PointerMotion { dx, dy } => {
            let event = InputEvent::<SyntheticInputBackend>::PointerMotion {
                event: SyntheticPointerMotionEvent {
                    time: now(),
                    dx: *dx,
                    dy: *dy,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::PointerMoveTo { x, y } => {
            let pos = Point::<f64, Logical>::from((*x, *y));
            if state.synoik.output_under(pos).is_none() {
                return Err(format!("({x}, {y}) is not on any output"));
            }
            // The pipeline maps an absolute event with no output of its own into the bounding
            // rectangle of all outputs, so hand it the position relative to that.
            let origin = state.global_bounding_rectangle().unwrap().loc.to_f64();
            let event = InputEvent::<SyntheticInputBackend>::PointerMotionAbsolute {
                event: SyntheticPointerMotionAbsoluteEvent {
                    time: now(),
                    x: x - origin.x,
                    y: y - origin.y,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::ButtonPress { button } => {
            send_button(state, resolve_button(button)?, ButtonState::Pressed);
        }
        InjectedEvent::ButtonRelease { button } => {
            send_button(state, resolve_button(button)?, ButtonState::Released);
        }
        InjectedEvent::Scroll { notches } => {
            let event = InputEvent::<SyntheticInputBackend>::PointerAxis {
                event: SyntheticPointerAxisEvent {
                    time: now(),
                    v120: notches * 120.0,
                    finger: None,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::FingerScroll { dx, dy } => send_finger_scroll(state, *dx, *dy),
        InjectedEvent::ScrollStop => send_finger_scroll(state, 0., 0.),
        InjectedEvent::HoldBegin { fingers } => {
            let event = InputEvent::<SyntheticInputBackend>::GestureHoldBegin {
                event: SyntheticGestureHoldBeginEvent {
                    time: now(),
                    fingers: *fingers,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::HoldEnd { cancelled } => {
            let event = InputEvent::<SyntheticInputBackend>::GestureHoldEnd {
                event: SyntheticGestureHoldEndEvent {
                    time: now(),
                    cancelled: *cancelled,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::SwipeBegin { fingers } => {
            let event = InputEvent::<SyntheticInputBackend>::GestureSwipeBegin {
                event: SyntheticGestureSwipeBeginEvent {
                    time: now(),
                    fingers: *fingers,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::SwipeUpdate { dx, dy } => {
            let event = InputEvent::<SyntheticInputBackend>::GestureSwipeUpdate {
                event: SyntheticGestureSwipeUpdateEvent {
                    time: now(),
                    dx: *dx,
                    dy: *dy,
                },
            };
            state.process_input_event(event);
        }
        InjectedEvent::SwipeEnd { cancelled } => {
            let event = InputEvent::<SyntheticInputBackend>::GestureSwipeEnd {
                event: SyntheticGestureSwipeEndEvent {
                    time: now(),
                    cancelled: *cancelled,
                },
            };
            state.process_input_event(event);
        }
    }
    Ok(())
}

fn send_finger_scroll(state: &mut State, dx: f64, dy: f64) {
    let event = InputEvent::<SyntheticInputBackend>::PointerAxis {
        event: SyntheticPointerAxisEvent {
            time: now(),
            v120: 0.,
            finger: Some((dx, dy)),
        },
    };
    state.process_input_event(event);
}

fn now() -> u64 {
    get_monotonic_time().as_micros() as u64
}

fn send_key(state: &mut State, key_code: Keycode, key_state: KeyState) {
    let event = InputEvent::<SyntheticInputBackend>::Keyboard {
        event: SyntheticKeyboardKeyEvent {
            time: now(),
            key_code,
            state: key_state,
        },
    };
    state.process_input_event(event);
}

fn send_button(state: &mut State, button_code: u32, button_state: ButtonState) {
    let event = InputEvent::<SyntheticInputBackend>::PointerButton {
        event: SyntheticPointerButtonEvent {
            time: now(),
            button_code,
            state: button_state,
        },
    };
    state.process_input_event(event);
}

/// Resolves a key given as an XKB keysym name, or as `code:N` for a raw decimal evdev
/// keycode, to an xkb-space keycode.
///
/// **A bare number is the digit key, not a keycode.** It used to be the keycode, which made
/// `input key Super+8` press `KEY_7` — evdev 8 *is* `KEY_7` — and every accelerator written the
/// way a user says it out loud silently activated its neighbour. That cost an hour of chasing a
/// favourites off-by-one that did not exist, so the reading that matches the accelerator string
/// wins and raw keycodes moved behind `code:`. Nothing is lost: evdev 1-9 are `ESC` and the digit
/// row, all reachable by name.
fn resolve_key(state: &mut State, key: &str) -> Result<Keycode, String> {
    if let Some(raw) = key.strip_prefix("code:") {
        let code = raw
            .parse::<u32>()
            .map_err(|_| format!("not a decimal evdev keycode: {raw:?}"))?;
        return Ok(Keycode::new(code + XKB_KEYCODE_OFFSET));
    }

    // Bare modifier shorthands, for `input key Alt+F2` ergonomics; the XKB
    // names are `Alt_L` etc.
    let key = match key.to_ascii_lowercase().as_str() {
        "alt" => "Alt_L",
        "ctrl" | "control" => "Control_L",
        "shift" => "Shift_L",
        "super" | "win" | "logo" => "Super_L",
        _ => key,
    };

    let keysym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym == Keysym::NoSymbol {
        return Err(format!("unknown key: {key:?}"));
    }

    let found = find_keysym(state, keysym);
    found
        .map(|(key_code, _level)| key_code)
        .ok_or_else(|| format!("key not reachable in the active keymap: {key:?}"))
}

/// Resolves a character to a keycode plus the shift level it lives on.
fn resolve_char(state: &mut State, ch: char) -> Result<(Keycode, u32), String> {
    let keysym = xkb::utf32_to_keysym(ch as u32);
    if keysym == Keysym::NoSymbol {
        return Err(format!("no keysym for character {ch:?}"));
    }

    let (key_code, level) = find_keysym(state, keysym)
        .ok_or_else(|| format!("character not reachable in the active keymap: {ch:?}"))?;
    if level > 1 {
        return Err(format!(
            "character {ch:?} needs shift level {level}; only levels 0 and 1 are supported"
        ));
    }
    Ok((key_code, level))
}

/// Scans the active layout for a keycode producing `keysym`, preferring the
/// base shift level.
fn find_keysym(state: &mut State, keysym: Keysym) -> Option<(Keycode, u32)> {
    let keyboard = state.synoik.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        let layout = xkb.active_layout().0;
        // SAFETY: neither the keymap reference nor anything derived from it
        // outlives the lock guard.
        let keymap = unsafe { xkb.keymap() };

        let keycodes = keymap.min_keycode().raw()..=keymap.max_keycode().raw();
        let mut fallback = None;
        for raw in keycodes {
            let key_code = Keycode::new(raw);
            for level in 0..keymap.num_levels_for_key(key_code, layout) {
                if !keymap
                    .key_get_syms_by_level(key_code, layout, level)
                    .contains(&keysym)
                {
                    continue;
                }
                if level == 0 {
                    return Some((key_code, 0));
                }
                if fallback.is_none() {
                    fallback = Some((key_code, level));
                }
            }
        }
        fallback
    })
}

fn resolve_button(button: &str) -> Result<u32, String> {
    match button.to_ascii_lowercase().as_str() {
        "left" => Ok(BTN_LEFT),
        "right" => Ok(BTN_RIGHT),
        "middle" => Ok(BTN_MIDDLE),
        other => other
            .parse()
            .map_err(|_| format!("unknown button: {button:?}")),
    }
}

pub struct SyntheticInputBackend;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntheticInputDevice;

impl crate::input::backend_ext::SynoikInputDevice for SyntheticInputDevice {
    fn output(&self, _state: &crate::synoik::State) -> Option<Output> {
        None
    }
}

impl Device for SyntheticInputDevice {
    fn id(&self) -> String {
        String::from("synthetic input device")
    }

    fn name(&self) -> String {
        String::from("synthetic input device")
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(
            capability,
            DeviceCapability::Keyboard | DeviceCapability::Pointer | DeviceCapability::Touch
        )
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub struct SyntheticKeyboardKeyEvent {
    pub time: u64,
    pub key_code: Keycode,
    pub state: KeyState,
}

impl Event<SyntheticInputBackend> for SyntheticKeyboardKeyEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl KeyboardKeyEvent<SyntheticInputBackend> for SyntheticKeyboardKeyEvent {
    fn key_code(&self) -> Keycode {
        self.key_code
    }

    fn state(&self) -> KeyState {
        self.state
    }

    fn count(&self) -> u32 {
        1
    }
}

pub struct SyntheticPointerButtonEvent {
    pub time: u64,
    pub button_code: u32,
    pub state: ButtonState,
}

impl Event<SyntheticInputBackend> for SyntheticPointerButtonEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl PointerButtonEvent<SyntheticInputBackend> for SyntheticPointerButtonEvent {
    fn button_code(&self) -> u32 {
        self.button_code
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

pub struct SyntheticPointerMotionEvent {
    pub time: u64,
    pub dx: f64,
    pub dy: f64,
}

impl Event<SyntheticInputBackend> for SyntheticPointerMotionEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl PointerMotionEvent<SyntheticInputBackend> for SyntheticPointerMotionEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }

    fn delta_x_unaccel(&self) -> f64 {
        self.dx
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.dy
    }
}

/// An absolute pointer position, relative to the origin of the outputs' bounding rectangle
/// (which is where the pipeline maps an absolute event from a device with no output).
pub struct SyntheticPointerMotionAbsoluteEvent {
    pub time: u64,
    pub x: f64,
    pub y: f64,
}

impl Event<SyntheticInputBackend> for SyntheticPointerMotionAbsoluteEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl AbsolutePositionEvent<SyntheticInputBackend> for SyntheticPointerMotionAbsoluteEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, _width: i32) -> f64 {
        self.x
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        self.y
    }
}

impl PointerMotionAbsoluteEvent<SyntheticInputBackend> for SyntheticPointerMotionAbsoluteEvent {}

/// A scroll: either a discrete wheel notch (`v120 / 120` notches on the vertical axis)
/// or a continuous finger scroll, which is what a touchpad two-finger swipe produces and
/// what the app grid's page swipe rides on.
pub struct SyntheticPointerAxisEvent {
    pub time: u64,
    pub v120: f64,
    /// `Some((dx, dy))` makes this a continuous [`AxisSource::Finger`] scroll instead of a
    /// wheel; `(0., 0.)` is the gesture-end event libinput sends when the fingers lift.
    pub finger: Option<(f64, f64)>,
}

impl SyntheticPointerAxisEvent {
    fn is_finger_stop(&self) -> bool {
        self.finger == Some((0., 0.))
    }
}

impl Event<SyntheticInputBackend> for SyntheticPointerAxisEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl PointerAxisEvent<SyntheticInputBackend> for SyntheticPointerAxisEvent {
    /// Like libinput, a finger scroll carries only the axes it moved on, and the lift carries
    /// a zero on both. A zero is what `wl_pointer.axis_stop` is made of, so reporting one for
    /// the idle axis of every vertical scroll would stop a horizontal scroll that never began.
    fn amount(&self, axis: Axis) -> Option<f64> {
        let (dx, dy) = self.finger?;
        let amount = match axis {
            Axis::Vertical => dy,
            Axis::Horizontal => dx,
        };
        (amount != 0. || self.is_finger_stop()).then_some(amount)
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        if self.finger.is_some() {
            return None;
        }
        Some(match axis {
            Axis::Vertical => self.v120,
            Axis::Horizontal => 0.0,
        })
    }

    fn source(&self) -> AxisSource {
        if self.finger.is_some() {
            AxisSource::Finger
        } else {
            AxisSource::Wheel
        }
    }

    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

pub struct SyntheticTouchDownEvent {
    pub time: u64,
    pub x: f64,
    pub y: f64,
}

impl Event<SyntheticInputBackend> for SyntheticTouchDownEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl TouchEvent<SyntheticInputBackend> for SyntheticTouchDownEvent {
    fn slot(&self) -> TouchSlot {
        TouchSlot::from(Some(0))
    }
}

impl AbsolutePositionEvent<SyntheticInputBackend> for SyntheticTouchDownEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, _width: i32) -> f64 {
        self.x
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        self.y
    }
}

impl TouchDownEvent<SyntheticInputBackend> for SyntheticTouchDownEvent {}

pub struct SyntheticTouchUpEvent {
    pub time: u64,
}

impl Event<SyntheticInputBackend> for SyntheticTouchUpEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl TouchEvent<SyntheticInputBackend> for SyntheticTouchUpEvent {
    fn slot(&self) -> TouchSlot {
        TouchSlot::from(Some(0))
    }
}

impl TouchUpEvent<SyntheticInputBackend> for SyntheticTouchUpEvent {}

/// Fingers landing for a touchpad swipe.
pub struct SyntheticGestureSwipeBeginEvent {
    pub time: u64,
    pub fingers: u32,
}

impl Event<SyntheticInputBackend> for SyntheticGestureSwipeBeginEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl GestureBeginEvent<SyntheticInputBackend> for SyntheticGestureSwipeBeginEvent {
    fn fingers(&self) -> u32 {
        self.fingers
    }
}

impl GestureSwipeBeginEvent<SyntheticInputBackend> for SyntheticGestureSwipeBeginEvent {}

/// A touchpad swipe's travel. The synthetic device is not a libinput one, so natural
/// scrolling never flips it: `(dx, dy)` is the delta the swipe trackers see as is.
pub struct SyntheticGestureSwipeUpdateEvent {
    pub time: u64,
    pub dx: f64,
    pub dy: f64,
}

impl Event<SyntheticInputBackend> for SyntheticGestureSwipeUpdateEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl GestureSwipeUpdateEvent<SyntheticInputBackend> for SyntheticGestureSwipeUpdateEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }
}

/// The fingers lifting off a touchpad swipe, or libinput cancelling it.
pub struct SyntheticGestureSwipeEndEvent {
    pub time: u64,
    pub cancelled: bool,
}

impl Event<SyntheticInputBackend> for SyntheticGestureSwipeEndEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl GestureEndEvent<SyntheticInputBackend> for SyntheticGestureSwipeEndEvent {
    fn cancelled(&self) -> bool {
        self.cancelled
    }
}

impl GestureSwipeEndEvent<SyntheticInputBackend> for SyntheticGestureSwipeEndEvent {}

/// Fingers coming to rest on the touchpad.
pub struct SyntheticGestureHoldBeginEvent {
    pub time: u64,
    pub fingers: u32,
}

impl Event<SyntheticInputBackend> for SyntheticGestureHoldBeginEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl GestureBeginEvent<SyntheticInputBackend> for SyntheticGestureHoldBeginEvent {
    fn fingers(&self) -> u32 {
        self.fingers
    }
}

impl GestureHoldBeginEvent<SyntheticInputBackend> for SyntheticGestureHoldBeginEvent {}

/// A hold ending: the fingers lifting, or libinput cancelling it when they start moving.
pub struct SyntheticGestureHoldEndEvent {
    pub time: u64,
    pub cancelled: bool,
}

impl Event<SyntheticInputBackend> for SyntheticGestureHoldEndEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> SyntheticInputDevice {
        SyntheticInputDevice
    }
}

impl GestureEndEvent<SyntheticInputBackend> for SyntheticGestureHoldEndEvent {
    fn cancelled(&self) -> bool {
        self.cancelled
    }
}

impl GestureHoldEndEvent<SyntheticInputBackend> for SyntheticGestureHoldEndEvent {}

impl InputBackend for SyntheticInputBackend {
    type Device = SyntheticInputDevice;

    type KeyboardKeyEvent = SyntheticKeyboardKeyEvent;
    type PointerButtonEvent = SyntheticPointerButtonEvent;
    type PointerAxisEvent = SyntheticPointerAxisEvent;
    type PointerMotionEvent = SyntheticPointerMotionEvent;

    type PointerMotionAbsoluteEvent = SyntheticPointerMotionAbsoluteEvent;

    type GestureSwipeBeginEvent = SyntheticGestureSwipeBeginEvent;
    type GestureSwipeUpdateEvent = SyntheticGestureSwipeUpdateEvent;
    type GestureSwipeEndEvent = SyntheticGestureSwipeEndEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = SyntheticGestureHoldBeginEvent;
    type GestureHoldEndEvent = SyntheticGestureHoldEndEvent;

    type TouchDownEvent = SyntheticTouchDownEvent;
    type TouchUpEvent = SyntheticTouchUpEvent;

    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}

#[cfg(test)]
mod tests {
    use synoik_ipc::InjectedEvent;

    use super::*;
    use crate::tests::fixture::Fixture;

    fn inject_all(f: &mut Fixture, events: &[InjectedEvent]) {
        for event in events {
            inject(f.synoik_state(), event).unwrap();
        }
    }

    /// A bare digit is the digit key, and `code:` is how you ask for a raw keycode.
    ///
    /// These two spellings used to be the same one, and the collision is silent where it hurts:
    /// evdev 8 is `KEY_7`, so `input key Super+8` fired `switch-to-application-7` and looked for
    /// all the world like a compositor off-by-one — which is exactly how it was investigated.
    #[test]
    fn a_bare_digit_is_the_digit_key_not_a_keycode() {
        const KEY_8: u32 = 9;
        const KEY_LEFTALT: u32 = 56;

        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));

        assert_eq!(
            resolve_key(f.synoik_state(), "8").unwrap(),
            Keycode::new(KEY_8 + XKB_KEYCODE_OFFSET),
            "`8` must press the 8 key, as `Super+8` reads"
        );
        assert_eq!(
            resolve_key(f.synoik_state(), "code:56").unwrap(),
            Keycode::new(KEY_LEFTALT + XKB_KEYCODE_OFFSET),
            "`code:N` is the raw evdev keycode"
        );
        assert!(
            resolve_key(f.synoik_state(), "code:nope").is_err(),
            "a `code:` that is not a number is an error, not a keysym lookup"
        );
    }

    /// Keys resolve as `code:`-prefixed evdev codes, keysym names and modifier shorthands, all
    /// landing in the real input pipeline (here: the `<Alt>F2` bind), and
    /// `Text` types into the focused UI, synthesizing Shift for level-1
    /// characters.
    #[test]
    fn keys_and_text_resolve_through_the_keymap() {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));

        // Deliberately mixed spellings: raw evdev code, keysym name, shorthand.
        inject_all(
            &mut f,
            &[
                InjectedEvent::KeyPress {
                    key: String::from("code:56"), // KEY_LEFTALT
                },
                InjectedEvent::KeyPress {
                    key: String::from("F2"),
                },
                InjectedEvent::KeyRelease {
                    key: String::from("f2"),
                },
                InjectedEvent::KeyRelease {
                    key: String::from("Alt"),
                },
            ],
        );
        assert!(
            f.synoik().run_dialog.is_open(),
            "injected <Alt>F2 must open the run dialog"
        );

        inject(
            f.synoik_state(),
            &InjectedEvent::Text {
                text: String::from("Kitty!"),
            },
        )
        .unwrap();
        assert_eq!(
            f.synoik().run_dialog.entry(),
            "Kitty!",
            "injected text must reach the dialog, including shifted characters"
        );
    }

    /// Finger scrolls, the lift and the hold gesture reach the client under the pointer the way
    /// a touchpad's do: a kinetic-scrolling client measures the flick from the finger-source
    /// axis values, starts its glide at `axis_stop`, and stops it on `hold.begin`.
    #[test]
    fn finger_scroll_stop_and_hold_reach_the_client() {
        use wayland_client::protocol::wl_pointer::{Axis as WlAxis, AxisSource as WlAxisSource};

        use crate::tests::client::PointerEvent;

        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        let id = f.add_client();
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);
        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.set_size(100, 100);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
        f.client(id).get_pointer();
        f.roundtrip(id);
        // Placed the way a `synoik msg` rig does it, onto the (centred) window. Not by slamming
        // into the top-left first: resting there fires the hot corner, and the overview it opens
        // consumes finger scrolls.
        inject(
            f.synoik_state(),
            &InjectedEvent::PointerMoveTo { x: 960., y: 540. },
        )
        .unwrap();
        f.double_roundtrip(id);
        assert_eq!(
            f.client(id).take_pointer_events(),
            vec![PointerEvent::Enter],
            "precondition: the pointer entered the window"
        );

        inject(
            f.synoik_state(),
            &InjectedEvent::FingerScroll { dx: 0., dy: 30. },
        )
        .unwrap();
        f.double_roundtrip(id);
        assert_eq!(
            f.client(id).take_pointer_events(),
            vec![
                PointerEvent::AxisSource(WlAxisSource::Finger),
                PointerEvent::Axis {
                    axis: WlAxis::VerticalScroll,
                    value: 30.,
                },
            ],
            "a vertical finger scroll carries the vertical axis only, in the Wayland sign — an \
             idle axis reported as zero would stop a horizontal scroll that never began"
        );

        inject(f.synoik_state(), &InjectedEvent::ScrollStop).unwrap();
        f.double_roundtrip(id);
        let mut stop = f.client(id).take_pointer_events();
        assert_eq!(
            stop.first(),
            Some(&PointerEvent::AxisSource(WlAxisSource::Finger))
        );
        stop.remove(0);
        stop.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(
            stop,
            vec![
                PointerEvent::AxisStop(WlAxis::HorizontalScroll),
                PointerEvent::AxisStop(WlAxis::VerticalScroll),
            ],
            "the lift is `axis_stop` on both axes and nothing else"
        );

        inject(f.synoik_state(), &InjectedEvent::HoldBegin { fingers: 2 }).unwrap();
        inject(
            f.synoik_state(),
            &InjectedEvent::HoldEnd { cancelled: true },
        )
        .unwrap();
        f.double_roundtrip(id);
        assert_eq!(
            f.client(id).take_pointer_events(),
            vec![
                PointerEvent::HoldBegin { fingers: 2 },
                PointerEvent::HoldEnd { cancelled: true },
            ],
            "the hold reaches the surface under the pointer with no grab active"
        );
    }

    /// Unresolvable keys and characters are reported as errors, not dropped.
    #[test]
    fn unresolvable_input_errors() {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));

        let err = inject(
            f.synoik_state(),
            &InjectedEvent::KeyPress {
                key: String::from("NoSuchKeysym"),
            },
        );
        assert!(err.is_err(), "an unknown keysym name must be an error");

        // Note '€' would *not* be an error: the evdev ruleset maps the
        // dedicated KEY_EURO key even on the us layout. Cyrillic is safely
        // out of reach.
        let err = inject(
            f.synoik_state(),
            &InjectedEvent::Text {
                text: String::from("ф"),
            },
        );
        assert!(
            err.is_err(),
            "a character unreachable in the active (us) keymap must be an error"
        );

        let err = inject(
            f.synoik_state(),
            &InjectedEvent::ButtonPress {
                button: String::from("pinky"),
            },
        );
        assert!(err.is_err(), "an unknown button name must be an error");
    }
}
