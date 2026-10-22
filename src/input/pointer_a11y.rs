// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Pointer accessibility — clutter's `clutter-input-pointer-a11y.c`, fed from
//! `org.gnome.desktop.a11y.mouse` the way `clutter-settings.c:296-343` reads it.
//!
//! - **Simulated secondary click** (`secondary-click-enabled`): hold the primary button for
//!   `secondary-click-time` and its release becomes a secondary click.
//! - **Dwell click** (`dwell-click-enabled`): rest the pointer for `dwell-time` within
//!   `dwell-threshold` pixels and it clicks. In window mode the click is the type the panel's
//!   dwell-click indicator picked; a double, secondary or middle click is one-shot and falls back
//!   to a primary (`update_dwell_click_type`), and a drag presses on one dwell and releases on the
//!   next. In gesture mode, a dwell starts a second timeout during which a stroke away from the
//!   spot picks the click by its direction (`dwell-gesture-*`), and the pointer goes back where it
//!   rested before clicking.
//!
//! The clicks are synthetic button events, and like mutter's virtual-device clicks they come back
//! through this accounting; only the pointer moves this makes are skipped (the virtual pointer's
//! motion is `CLUTTER_EVENT_FLAG_SYNTHETIC`, `meta-seat-impl.c:978-980`). Each click is emitted
//! after the state it changes, as mutter's queued events are seen after.
//!
//! The gesture direction is mutter's as written (`get_dwell_direction`): a stroke to the right
//! reads as `left`, and up the screen as `down`. It has been so since the feature landed, and the
//! settings people have chosen are against it.

use std::time::Duration;

use smithay::backend::input::{ButtonState, InputEvent};
use smithay::utils::{Logical, Point};

use crate::input::synthetic::{
    SyntheticInputBackend, SyntheticPointerButtonEvent, SyntheticPointerMotionAbsoluteEvent,
};
use crate::synoik::State;
use crate::utils::timers::TimerToken;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// How long the pointer must stay put before a dwell's own timeout starts
/// (`start_dwell_position_timeout`).
const DWELL_POSITION_DELAY: Duration = Duration::from_millis(100);

/// `GDesktopMouseDwellMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DwellMode {
    #[default]
    Window,
    Gesture,
}

/// `ClutterPointerA11yDwellDirection`; `None` is a stroke inside the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwellDirection {
    None,
    Left,
    Right,
    Up,
    Down,
}

impl DwellDirection {
    pub fn from_nick(nick: &str) -> Self {
        match nick {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            _ => Self::None,
        }
    }
}

/// `ClutterPointerA11yDwellClickType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwellClickType {
    None,
    Primary,
    Secondary,
    Middle,
    Double,
    Drag,
}

/// `ClutterPointerA11yTimeoutType`: which timeout the pie at the pointer is counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutType {
    SecondaryClick,
    Dwell,
    Gesture,
}

/// `org.gnome.desktop.a11y.mouse`, as `ClutterPointerA11ySettings` holds it. Defaults are the
/// schema's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerA11ySettings {
    pub secondary_click_enabled: bool,
    /// `secondary-click-time`, ms.
    pub secondary_click_delay: u32,
    pub dwell_click_enabled: bool,
    /// `dwell-time`, ms.
    pub dwell_delay: u32,
    /// `dwell-threshold`, px.
    pub dwell_threshold: u32,
    pub dwell_mode: DwellMode,
    pub dwell_gesture_single: DwellDirection,
    pub dwell_gesture_double: DwellDirection,
    pub dwell_gesture_drag: DwellDirection,
    pub dwell_gesture_secondary: DwellDirection,
}

impl Default for PointerA11ySettings {
    fn default() -> Self {
        Self {
            secondary_click_enabled: false,
            secondary_click_delay: 1200,
            dwell_click_enabled: false,
            dwell_delay: 1200,
            dwell_threshold: 10,
            dwell_mode: DwellMode::Window,
            dwell_gesture_single: DwellDirection::Left,
            dwell_gesture_double: DwellDirection::Up,
            dwell_gesture_drag: DwellDirection::Down,
            dwell_gesture_secondary: DwellDirection::Right,
        }
    }
}

impl PointerA11ySettings {
    fn enabled(&self) -> bool {
        self.secondary_click_enabled || self.dwell_click_enabled
    }

    /// `get_dwell_click_type_for_direction`.
    fn click_for(&self, direction: DwellDirection) -> DwellClickType {
        if direction == self.dwell_gesture_single {
            DwellClickType::Primary
        } else if direction == self.dwell_gesture_double {
            DwellClickType::Double
        } else if direction == self.dwell_gesture_drag {
            DwellClickType::Drag
        } else if direction == self.dwell_gesture_secondary {
            DwellClickType::Secondary
        } else {
            DwellClickType::None
        }
    }
}

/// The running timeout the pie at the pointer draws (`PointerA11yTimeout`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PieTimeout {
    pub kind: TimeoutType,
    /// Where the pointer was when it started: the pie stays there.
    pub at: Point<f64, Logical>,
    pub started: Duration,
    pub delay: Duration,
}

/// `ClutterPtrA11yData`.
#[derive(Debug)]
pub struct PointerA11y {
    /// The settings as last applied.
    pub settings: PointerA11ySettings,
    /// What a window-mode dwell clicks. The panel's indicator picks it; it starts as a primary
    /// click, as the indicator sets it when it is made (`dwellClick.js:62`).
    pub click_type: DwellClickType,
    n_btn_pressed: u32,
    current: Point<f64, Logical>,
    dwell: Point<f64, Logical>,
    dwell_drag_started: bool,
    dwell_gesture_started: bool,
    dwell_timer: Option<TimerToken>,
    dwell_position_timer: Option<TimerToken>,
    secondary_click_timer: Option<TimerToken>,
    secondary_click_triggered: bool,
    /// Set while this injects a pointer move, which it must not see come back.
    pub(crate) moving: bool,
    pub pie: Option<PieTimeout>,
}

impl Default for PointerA11y {
    fn default() -> Self {
        Self {
            settings: PointerA11ySettings::default(),
            click_type: DwellClickType::Primary,
            n_btn_pressed: 0,
            current: Point::default(),
            dwell: Point::default(),
            dwell_drag_started: false,
            dwell_gesture_started: false,
            dwell_timer: None,
            dwell_position_timer: None,
            secondary_click_timer: None,
            secondary_click_triggered: false,
            moving: false,
            pie: None,
        }
    }
}

impl PointerA11y {
    /// `pointer_has_moved`: further than the threshold from where it rested.
    fn has_moved(&self) -> bool {
        let d = self.dwell - self.current;
        let threshold = f64::from(self.settings.dwell_threshold);
        d.x * d.x + d.y * d.y > threshold * threshold
    }

    /// `get_dwell_direction`, as written (see the module doc).
    fn direction(&self) -> DwellDirection {
        if !self.has_moved() {
            return DwellDirection::None;
        }
        let dx = (self.dwell.x - self.current.x).abs();
        let dy = (self.dwell.y - self.current.y).abs();
        if self.dwell.x < self.current.x {
            if dx > dy {
                return DwellDirection::Left;
            }
        } else if dx > dy {
            return DwellDirection::Right;
        }
        if self.dwell.y < self.current.y {
            DwellDirection::Up
        } else {
            DwellDirection::Down
        }
    }
}

impl State {
    /// Take new settings (`sync_pointer_a11y_settings`). Turning a feature off ends what it had
    /// running, a held dwell drag included (`_clutter_seat_shutdown_a11y`).
    pub fn sync_pointer_a11y(&mut self) {
        let settings = self.synoik.gnome_settings.pointer_a11y;
        if self.synoik.pointer_a11y.settings == settings {
            return;
        }
        let a11y = &self.synoik.pointer_a11y;
        if !settings.dwell_click_enabled && a11y.settings.dwell_click_enabled {
            if a11y.dwell_drag_started {
                self.emit_dwell_click(DwellClickType::Drag);
            }
            self.stop_dwell_position_timeout();
            self.stop_dwell_timeout();
        }
        if !settings.secondary_click_enabled {
            self.stop_secondary_click_timeout();
        }
        self.synoik.pointer_a11y.settings = settings;
    }

    /// The panel indicator picked what a window-mode dwell clicks
    /// (`clutter_seat_set_pointer_a11y_dwell_click_type`).
    pub fn set_dwell_click_type(&mut self, click_type: DwellClickType) {
        self.synoik.pointer_a11y.click_type = click_type;
        self.synoik.queue_redraw_all();
    }

    /// `_clutter_seat_a11y_on_motion_event`, with the pointer where it now is.
    pub fn pointer_a11y_motion(&mut self) {
        let a11y = &self.synoik.pointer_a11y;
        if !a11y.settings.enabled() || a11y.moving {
            return;
        }
        let pos = self.synoik.seat.get_pointer().unwrap().current_location();
        self.synoik.pointer_a11y.current = pos;

        let a11y = &self.synoik.pointer_a11y;
        if a11y.settings.secondary_click_enabled && a11y.has_moved() {
            self.stop_secondary_click_timeout();
        }
        if self.synoik.pointer_a11y.settings.dwell_click_enabled {
            self.stop_dwell_position_timeout();
            // `should_stop_dwell`: unless a gesture is being drawn from the spot.
            let a11y = &self.synoik.pointer_a11y;
            if a11y.has_moved() && !a11y.dwell_gesture_started {
                self.stop_dwell_timeout();
            }
            // `should_start_dwell`: none pending, and no button held but a dwell drag's.
            let a11y = &self.synoik.pointer_a11y;
            if a11y.dwell_timer.is_none() && (a11y.dwell_drag_started || a11y.n_btn_pressed == 0) {
                self.start_dwell_position_timeout();
            }
        }
        // `should_update_dwell_position`.
        let a11y = &mut self.synoik.pointer_a11y;
        if !a11y.dwell_gesture_started
            && a11y.dwell_timer.is_none()
            && a11y.secondary_click_timer.is_none()
        {
            a11y.dwell = a11y.current;
        }
    }

    /// `_clutter_seat_a11y_on_button_event`.
    pub fn pointer_a11y_button(&mut self, button: u32, pressed: bool) {
        if !self.synoik.pointer_a11y.settings.enabled() {
            return;
        }
        if pressed {
            self.synoik.pointer_a11y.n_btn_pressed += 1;
            self.stop_dwell_position_timeout();
            let a11y = &self.synoik.pointer_a11y;
            if a11y.settings.dwell_click_enabled || a11y.dwell_drag_started {
                self.stop_dwell_timeout();
            }
            let a11y = &self.synoik.pointer_a11y;
            if a11y.settings.secondary_click_enabled {
                if button == BTN_LEFT {
                    if !a11y.dwell_drag_started {
                        self.start_secondary_click_timeout();
                    }
                } else if a11y.secondary_click_timer.is_some() {
                    self.stop_secondary_click_timeout();
                }
            }
        } else {
            let a11y = &mut self.synoik.pointer_a11y;
            a11y.n_btn_pressed = a11y.n_btn_pressed.saturating_sub(1);
            if a11y.secondary_click_triggered {
                self.stop_secondary_click_timeout();
                self.emit_button_click(BTN_RIGHT);
            }
            if self.synoik.pointer_a11y.secondary_click_timer.is_some() {
                self.stop_secondary_click_timeout();
            }
            if self.synoik.pointer_a11y.dwell_drag_started {
                self.emit_dwell_click(DwellClickType::Drag);
            }
        }
    }

    fn timeout_started(&mut self, kind: TimeoutType, delay: u32) {
        let at = self.synoik.seat.get_pointer().unwrap().current_location();
        self.synoik.pointer_a11y.pie = Some(PieTimeout {
            kind,
            at,
            started: self.synoik.clock.now_unadjusted(),
            delay: Duration::from_millis(u64::from(delay)),
        });
        self.synoik.queue_redraw_all();
    }

    /// A timeout ended: a pie whose click went through plays out (it is at its end anyway), one
    /// cut short goes at once (`pointerA11yTimeout.js:124-131`).
    fn timeout_stopped(&mut self, clicked: bool) {
        if !clicked {
            self.synoik.pointer_a11y.pie = None;
        }
        self.synoik.queue_redraw_all();
    }

    fn start_secondary_click_timeout(&mut self) {
        let delay = self.synoik.pointer_a11y.settings.secondary_click_delay;
        let token = self
            .synoik
            .timer_after(Duration::from_millis(u64::from(delay)), |state| {
                // `trigger_secondary_click`: the release will be a secondary click.
                let a11y = &mut state.synoik.pointer_a11y;
                a11y.secondary_click_triggered = true;
                a11y.secondary_click_timer = None;
                state.timeout_stopped(true);
                None
            });
        self.synoik.pointer_a11y.secondary_click_timer = Some(token);
        self.timeout_started(TimeoutType::SecondaryClick, delay);
    }

    fn stop_secondary_click_timeout(&mut self) {
        if let Some(token) = self.synoik.pointer_a11y.secondary_click_timer.take() {
            self.synoik.cancel_timer(token);
            self.timeout_stopped(false);
        }
        self.synoik.pointer_a11y.secondary_click_triggered = false;
    }

    fn start_dwell_position_timeout(&mut self) {
        let token = self.synoik.timer_after(DWELL_POSITION_DELAY, |state| {
            // `trigger_dwell_position_timeout`: still resting, so the dwell itself starts.
            let a11y = &mut state.synoik.pointer_a11y;
            a11y.dwell_position_timer = None;
            if a11y.settings.dwell_click_enabled && !a11y.has_moved() {
                state.start_dwell_timeout();
            }
            None
        });
        self.synoik.pointer_a11y.dwell_position_timer = Some(token);
    }

    fn stop_dwell_position_timeout(&mut self) {
        if let Some(token) = self.synoik.pointer_a11y.dwell_position_timer.take() {
            self.synoik.cancel_timer(token);
        }
    }

    fn start_dwell_timeout(&mut self) {
        let delay = self.synoik.pointer_a11y.settings.dwell_delay;
        let token = self
            .synoik
            .timer_after(Duration::from_millis(u64::from(delay)), |state| {
                state.trigger_dwell_click();
                None
            });
        self.synoik.pointer_a11y.dwell_timer = Some(token);
        self.timeout_started(TimeoutType::Dwell, delay);
    }

    fn stop_dwell_timeout(&mut self) {
        if let Some(token) = self.synoik.pointer_a11y.dwell_timer.take() {
            self.synoik.cancel_timer(token);
            self.synoik.pointer_a11y.dwell_gesture_started = false;
            self.timeout_stopped(false);
        }
    }

    /// `trigger_dwell_click`.
    fn trigger_dwell_click(&mut self) {
        self.synoik.pointer_a11y.dwell_timer = None;
        self.timeout_stopped(true);
        let a11y = &self.synoik.pointer_a11y;
        if a11y.settings.dwell_mode == DwellMode::Gesture {
            if a11y.dwell_drag_started {
                self.emit_dwell_click(DwellClickType::Drag);
            } else {
                self.start_dwell_gesture_timeout();
            }
        } else {
            let click_type = a11y.click_type;
            // `update_dwell_click_type` after the click; set first, since the click is seen
            // at once here and would read the old type.
            self.synoik.pointer_a11y.click_type = match click_type {
                DwellClickType::Double | DwellClickType::Secondary | DwellClickType::Middle => {
                    DwellClickType::Primary
                }
                other => other,
            };
            self.emit_dwell_click(click_type);
            if click_type == DwellClickType::Drag && !self.synoik.pointer_a11y.dwell_drag_started {
                // The drag's release: a drag is one-shot too.
                self.synoik.pointer_a11y.click_type = DwellClickType::Primary;
            }
            if self.synoik.pointer_a11y.click_type != click_type {
                self.synoik.queue_redraw_all();
            }
        }
    }

    fn start_dwell_gesture_timeout(&mut self) {
        let delay = self.synoik.pointer_a11y.settings.dwell_delay;
        let token = self
            .synoik
            .timer_after(Duration::from_millis(u64::from(delay)), |state| {
                state.trigger_dwell_gesture();
                None
            });
        let a11y = &mut self.synoik.pointer_a11y;
        a11y.dwell_timer = Some(token);
        a11y.dwell_gesture_started = true;
        self.timeout_started(TimeoutType::Gesture, delay);
    }

    /// `trigger_dwell_gesture`: back to where it rested, then the click the stroke named.
    fn trigger_dwell_gesture(&mut self) {
        let direction = self.synoik.pointer_a11y.direction();
        let click = self.synoik.pointer_a11y.settings.click_for(direction);
        self.restore_dwell_position();
        self.emit_dwell_click(click);
        // Not cleared at once, or the next motion would start another.
        let delay = self.synoik.pointer_a11y.settings.dwell_delay;
        let token = self
            .synoik
            .timer_after(Duration::from_millis(u64::from(delay)), |state| {
                let a11y = &mut state.synoik.pointer_a11y;
                a11y.dwell_timer = None;
                a11y.dwell_gesture_started = false;
                None
            });
        self.synoik.pointer_a11y.dwell_timer = Some(token);
        self.timeout_stopped(true);
    }

    fn restore_dwell_position(&mut self) {
        let dwell = self.synoik.pointer_a11y.dwell;
        let origin = self
            .global_bounding_rectangle()
            .map(|r| r.loc.to_f64())
            .unwrap_or_default();
        let time = self.synoik.clock.now_unadjusted().as_micros() as u64;
        self.synoik.pointer_a11y.moving = true;
        self.process_input_event(InputEvent::<SyntheticInputBackend>::PointerMotionAbsolute {
            event: SyntheticPointerMotionAbsoluteEvent {
                time,
                x: dwell.x - origin.x,
                y: dwell.y - origin.y,
            },
        });
        self.synoik.pointer_a11y.moving = false;
    }

    /// `emit_dwell_click`.
    fn emit_dwell_click(&mut self, click_type: DwellClickType) {
        match click_type {
            DwellClickType::Primary => self.emit_button_click(BTN_LEFT),
            DwellClickType::Double => {
                self.emit_button_click(BTN_LEFT);
                self.emit_button_click(BTN_LEFT);
            }
            DwellClickType::Drag => {
                let dragging = !self.synoik.pointer_a11y.dwell_drag_started;
                self.synoik.pointer_a11y.dwell_drag_started = dragging;
                self.emit_button(BTN_LEFT, dragging);
            }
            DwellClickType::Secondary => self.emit_button_click(BTN_RIGHT),
            DwellClickType::Middle => self.emit_button_click(BTN_MIDDLE),
            DwellClickType::None => (),
        }
    }

    fn emit_button_click(&mut self, button: u32) {
        self.emit_button(button, true);
        self.emit_button(button, false);
    }

    fn emit_button(&mut self, button: u32, pressed: bool) {
        let time = self.synoik.clock.now_unadjusted().as_micros() as u64;
        self.process_input_event(InputEvent::<SyntheticInputBackend>::PointerButton {
            event: SyntheticPointerButtonEvent {
                time,
                button_code: button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        });
    }
}
