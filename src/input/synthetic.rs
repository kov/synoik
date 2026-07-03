//! Synthetic input: fabricated events fed through the real input pipeline.
//!
//! [`SyntheticInputBackend`] is a minimal [`InputBackend`] whose events are
//! constructed in process rather than read from hardware. The headless test
//! fixture builds events directly; the IPC `InjectInput` request (`niri msg
//! input`) goes through [`inject`], which additionally resolves key and
//! button names against the seat's active keymap.
//!
//! Only the event types we actually synthesize are real; the rest are
//! [`UnusedEvent`].

use niri_ipc::InjectedEvent;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
    DeviceCapability, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, Keycode,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchDownEvent, TouchEvent,
    TouchSlot, TouchUpEvent, UnusedEvent,
};
use smithay::input::keyboard::{xkb, Keysym};
use smithay::output::Output;

use crate::niri::State;
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
                },
            };
            state.process_input_event(event);
        }
    }
    Ok(())
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

/// Resolves a key given as a decimal evdev keycode or an XKB keysym name to
/// an xkb-space keycode.
fn resolve_key(state: &mut State, key: &str) -> Result<Keycode, String> {
    if let Ok(code) = key.parse::<u32>() {
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
    let keyboard = state.niri.seat.get_keyboard().unwrap();
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

impl crate::input::backend_ext::NiriInputDevice for SyntheticInputDevice {
    fn output(&self, _state: &crate::niri::State) -> Option<Output> {
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

/// A discrete (wheel) scroll of `v120 / 120` notches on the vertical axis.
pub struct SyntheticPointerAxisEvent {
    pub time: u64,
    pub v120: f64,
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
    fn amount(&self, _axis: Axis) -> Option<f64> {
        None
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        Some(match axis {
            Axis::Vertical => self.v120,
            Axis::Horizontal => 0.0,
        })
    }

    fn source(&self) -> AxisSource {
        AxisSource::Wheel
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

impl InputBackend for SyntheticInputBackend {
    type Device = SyntheticInputDevice;

    type KeyboardKeyEvent = SyntheticKeyboardKeyEvent;
    type PointerButtonEvent = SyntheticPointerButtonEvent;
    type PointerAxisEvent = SyntheticPointerAxisEvent;
    type PointerMotionEvent = SyntheticPointerMotionEvent;

    type PointerMotionAbsoluteEvent = UnusedEvent;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

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
    use niri_ipc::InjectedEvent;

    use super::*;
    use crate::tests::fixture::Fixture;

    fn inject_all(f: &mut Fixture, events: &[InjectedEvent]) {
        for event in events {
            inject(f.niri_state(), event).unwrap();
        }
    }

    /// Keys resolve as evdev codes, keysym names and modifier shorthands, all
    /// landing in the real input pipeline (here: the `<Alt>F2` bind), and
    /// `Text` types into the focused UI, synthesizing Shift for level-1
    /// characters.
    #[test]
    fn keys_and_text_resolve_through_the_keymap() {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));

        // Deliberately mixed spellings: evdev code, keysym name, shorthand.
        inject_all(
            &mut f,
            &[
                InjectedEvent::KeyPress {
                    key: String::from("56"), // KEY_LEFTALT
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
            f.niri().run_dialog.is_open(),
            "injected <Alt>F2 must open the run dialog"
        );

        inject(
            f.niri_state(),
            &InjectedEvent::Text {
                text: String::from("Kitty!"),
            },
        )
        .unwrap();
        assert_eq!(
            f.niri().run_dialog.entry(),
            "Kitty!",
            "injected text must reach the dialog, including shifted characters"
        );
    }

    /// Unresolvable keys and characters are reported as errors, not dropped.
    #[test]
    fn unresolvable_input_errors() {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));

        let err = inject(
            f.niri_state(),
            &InjectedEvent::KeyPress {
                key: String::from("NoSuchKeysym"),
            },
        );
        assert!(err.is_err(), "an unknown keysym name must be an error");

        // Note '€' would *not* be an error: the evdev ruleset maps the
        // dedicated KEY_EURO key even on the us layout. Cyrillic is safely
        // out of reach.
        let err = inject(
            f.niri_state(),
            &InjectedEvent::Text {
                text: String::from("ф"),
            },
        );
        assert!(
            err.is_err(),
            "a character unreachable in the active (us) keymap must be an error"
        );

        let err = inject(
            f.niri_state(),
            &InjectedEvent::ButtonPress {
                button: String::from("pinky"),
            },
        );
        assert!(err.is_err(), "an unknown button name must be an error");
    }
}
