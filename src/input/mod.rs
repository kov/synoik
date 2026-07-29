use std::any::Any;
use std::collections::hash_map::Entry;
use std::collections::HashSet;
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use input::event::gesture::GestureEventCoordinates as _;
use niri_config::{
    Action, Bind, Binds, Config, Key, ModKey, Modifiers, MruDirection, MruFilter, MruScope,
    SwitchBinds, Trigger, WorkspaceReference,
};
use niri_ipc::LayoutSwitchTarget;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
    GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _,
    InputEvent, KeyState, KeyboardKeyEvent, Keycode, MouseButton, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent, ProximityState, Switch, SwitchState, SwitchToggleEvent,
    TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent,
    TabletToolTipState, TouchEvent,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::input::dnd::DnDGrab;
use smithay::input::keyboard::{keysyms, FilterResult, Keysym, Layout, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, RelativeMotionEvent,
};
use smithay::input::touch::{
    DownEvent, GrabStartData as TouchGrabStartData, MotionEvent as TouchMotionEvent, UpEvent,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Transform, SERIAL_COUNTER};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};
use smithay::wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait};
use touch_overview_grab::TouchOverviewGrab;

use self::move_grab::MoveGrab;
use self::pick_color_grab::PickColorGrab;
use self::pick_window_grab::PickWindowGrab;
use self::resize_grab::ResizeGrab;
use self::spatial_movement_grab::SpatialMovementGrab;
use self::thumb_grab::ThumbGrab;
use crate::app_system::LaunchMode;
#[cfg(feature = "dbus")]
use crate::dbus::freedesktop_a11y::KbMonBlock;
use crate::gnome::{
    Accel, AccelGrab, AccelMods, AccelTrigger, GnomeKeyAction, GnomeKeybinding, TileSide,
};
use crate::layout::scrolling::ScrollDirection;
use crate::layout::workspace::WorkspaceId;
use crate::layout::{ActivateWindow, LayoutElement};
use crate::niri::{AppDrag, CastTarget, PointerVisibility, State};
use crate::ui::app_grid::{
    AppGridEntry, DragLocation, FocusDir, PageArrow, SwipeSource, DELAYED_MOVE_MS, EDGE_BUMP_PX,
    FOLDER_PREVIEW_MS, PAGE_SWITCH_INITIAL_MS, PAGE_SWITCH_REPEAT_MS,
};
use crate::ui::dash::DashHit;
use crate::ui::end_session_dialog::DialogOutcome;
use crate::ui::folder_dialog::{DialogHit, POPDOWN_DIALOG_MS};
use crate::ui::mru::{WindowMru, WindowMruUi};
use crate::ui::overview_search::SearchHit;
use crate::ui::popover::PopoverSide;
use crate::ui::run_dialog::{self, KeyOutcome};
use crate::ui::screenshot_ui::ScreenshotUi;
use crate::ui::window_preview;
use crate::utils::spawning::{spawn, spawn_sh};
use crate::utils::{center, get_monotonic_time, output_size, CastSessionId, ResizeEdge};

pub mod backend_ext;
pub mod move_grab;
pub mod pick_color_grab;
pub mod pick_window_grab;
pub mod resize_grab;
pub mod scroll_swipe_gesture;
pub mod scroll_tracker;
pub mod spatial_movement_grab;
pub mod swipe_tracker;
pub mod synthetic;
pub mod thumb_grab;
pub mod touch_overview_grab;
pub mod touch_resize_grab;

use backend_ext::{NiriInputBackend as InputBackend, NiriInputDevice as _};

pub const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);

/// How far the pointer must leave a press before it becomes a drag —
/// `org.gnome.desktop.peripherals.mouse drag-threshold` (default 8), compared
/// per axis (`st-dnd-start-gesture.c:73-90`).
///
/// DIVERGENCE: read from the setting in GNOME (`St.Settings`); we hardcode the
/// default until the mouse schema joins the inspectable gsettings model.
const DRAG_THRESHOLD: f64 = 8.;

/// How soon a second overlay-key tap escalates into the app grid rather than
/// closing the overview, when animations are off and there is no open
/// transition to test against: `Overview.ANIMATION_TIME` (`overview.js:12`),
/// the same constant gnome-shell compares to (`overviewControls.js:433`).
const OVERLAY_KEY_SHIFT_WINDOW: Duration = Duration::from_millis(250);

/// A widget of the overview's chrome under the pointer. The overview's controls
/// are St.Buttons, which act on release rather than press, so a click needs a
/// press-time target to compare the release against — this is that target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewHit {
    /// The close button of a window preview in the picker.
    PreviewClose(smithay::desktop::Window),
    /// The dash (favorites bar).
    Dash(DashHit),
    /// The search entry / results card.
    Search(SearchHit),
    /// An app grid tile at index `.0`.
    GridApp(usize),
    /// A page-indicator dot for page `.0`.
    GridPage(usize),
    /// A page navigation arrow.
    GridArrow(PageArrow),
    /// The open app-folder dialog, which is modal over the rest of the overview.
    Folder(DialogHit),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabletData {
    pub aspect_ratio: f64,
}

pub enum PointerOrTouchStartData<D: SeatHandler> {
    Pointer(PointerGrabStartData<D>),
    Touch(TouchGrabStartData<D>),
}

impl<D: SeatHandler> PointerOrTouchStartData<D> {
    pub fn location(&self) -> Point<f64, Logical> {
        match self {
            PointerOrTouchStartData::Pointer(x) => x.location,
            PointerOrTouchStartData::Touch(x) => x.location,
        }
    }

    pub fn unwrap_pointer(&self) -> &PointerGrabStartData<D> {
        match self {
            PointerOrTouchStartData::Pointer(x) => x,
            PointerOrTouchStartData::Touch(_) => panic!("start_data is not Pointer"),
        }
    }

    pub fn unwrap_touch(&self) -> &TouchGrabStartData<D> {
        match self {
            PointerOrTouchStartData::Pointer(_) => panic!("start_data is not Touch"),
            PointerOrTouchStartData::Touch(x) => x,
        }
    }

    pub fn is_pointer(&self) -> bool {
        matches!(self, Self::Pointer(_))
    }

    pub fn is_touch(&self) -> bool {
        matches!(self, Self::Touch(_))
    }
}

impl State {
    pub fn process_input_event<I: InputBackend + 'static>(&mut self, event: InputEvent<I>)
    where
        I::Device: 'static, // Needed for downcasting.
    {
        let _span = tracy_client::span!("process_input_event");

        // Make sure some logic like workspace clean-up has a chance to run before doing actions.
        self.niri.advance_animations();

        if self.niri.monitors_active {
            // Notify the idle-notifier of activity.
            if should_notify_activity(&event) {
                self.niri.notify_activity();
            }
        } else {
            // Power on monitors if they were off.
            if should_activate_monitors(&event) {
                self.niri.activate_monitors(&mut self.backend);

                // Notify the idle-notifier of activity only if we're also powering on the
                // monitors.
                self.niri.notify_activity();
            }
        }

        if should_reset_pointer_inactivity_timer(&event) {
            self.niri.reset_pointer_inactivity_timer();
        }

        let hide_hotkey_overlay =
            self.niri.hotkey_overlay.is_open() && should_hide_hotkey_overlay(&event);

        let hide_exit_confirm_dialog =
            self.niri.exit_confirm_dialog.is_open() && should_hide_exit_confirm_dialog(&event);

        // GNOME overlay key: pointer button, scroll, and touch begin/end cancel a
        // pending Super tap, matching mutter's meta_keybindings_process_event (it
        // clears overlay_key_only_pressed for exactly these event types). Plain
        // pointer motion deliberately does not cancel it.
        if event_cancels_overlay_key(&event) {
            self.niri.overlay_key_armed = None;
        }

        let mut consumed_by_a11y = false;
        use InputEvent::*;
        match event {
            DeviceAdded { device } => self.on_device_added(device),
            DeviceRemoved { device } => self.on_device_removed(device),
            Keyboard { event } => self.on_keyboard::<I>(event, &mut consumed_by_a11y),
            PointerMotion { event } => self.on_pointer_motion::<I>(event),
            PointerMotionAbsolute { event } => self.on_pointer_motion_absolute::<I>(event),
            PointerButton { event } => self.on_pointer_button::<I>(event),
            PointerAxis { event } => self.on_pointer_axis::<I>(event),
            TabletToolAxis { event } => self.on_tablet_tool_axis::<I>(event),
            TabletToolTip { event } => self.on_tablet_tool_tip::<I>(event),
            TabletToolProximity { event } => self.on_tablet_tool_proximity::<I>(event),
            TabletToolButton { event } => self.on_tablet_tool_button::<I>(event),
            GestureSwipeBegin { event } => self.on_gesture_swipe_begin::<I>(event),
            GestureSwipeUpdate { event } => self.on_gesture_swipe_update::<I>(event),
            GestureSwipeEnd { event } => self.on_gesture_swipe_end::<I>(event),
            GesturePinchBegin { event } => self.on_gesture_pinch_begin::<I>(event),
            GesturePinchUpdate { event } => self.on_gesture_pinch_update::<I>(event),
            GesturePinchEnd { event } => self.on_gesture_pinch_end::<I>(event),
            GestureHoldBegin { event } => self.on_gesture_hold_begin::<I>(event),
            GestureHoldEnd { event } => self.on_gesture_hold_end::<I>(event),
            TouchDown { event } => self.on_touch_down::<I>(event),
            TouchMotion { event } => self.on_touch_motion::<I>(event),
            TouchUp { event } => self.on_touch_up::<I>(event),
            TouchCancel { event } => self.on_touch_cancel::<I>(event),
            TouchFrame { event } => self.on_touch_frame::<I>(event),
            SwitchToggle { event } => self.on_switch_toggle::<I>(event),
            Special(_) => (),
        }

        // Don't hide overlays if consumed by a11y, so that you can use the screen reader
        // navigation keys.
        if consumed_by_a11y {
            return;
        }

        // Do this last so that screenshot still gets it.
        if hide_hotkey_overlay && self.niri.hotkey_overlay.hide() {
            self.niri.queue_redraw_all();
        }

        if hide_exit_confirm_dialog && self.niri.exit_confirm_dialog.hide() {
            self.niri.queue_redraw_all();
        }
    }

    pub fn process_libinput_event(&mut self, event: &mut InputEvent<LibinputInputBackend>) {
        let _span = tracy_client::span!("process_libinput_event");

        match event {
            InputEvent::DeviceAdded { device } => {
                self.niri.devices.insert(device.clone());

                if device.has_capability(input::DeviceCapability::TabletTool) {
                    match device.size() {
                        Some((w, h)) => {
                            let aspect_ratio = w / h;
                            let data = TabletData { aspect_ratio };
                            self.niri.tablets.insert(device.clone(), data);
                        }
                        None => {
                            warn!("tablet tool device has no size");
                        }
                    }
                }

                if device.has_capability(input::DeviceCapability::Keyboard) {
                    if let Some(led_state) = self
                        .niri
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                    {
                        device.led_update(led_state.into());
                    }
                }

                if device.has_capability(input::DeviceCapability::Touch) {
                    self.niri.touch.insert(device.clone());
                }

                apply_libinput_settings(&self.niri.config.borrow().input, device);
            }
            InputEvent::DeviceRemoved { device } => {
                self.niri.touch.remove(device);
                self.niri.tablets.remove(device);
                self.niri.devices.remove(device);
            }
            _ => (),
        }
    }

    fn on_device_added(&mut self, device: impl Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.niri.seat.tablet_seat();

            let desc = TabletDescriptor::from(&device);
            tablet_seat.add_tablet::<Self>(&self.niri.display_handle, &desc);
        }
        if device.has_capability(DeviceCapability::Touch) && self.niri.seat.get_touch().is_none() {
            self.niri.seat.add_touch();
        }
    }

    fn on_device_removed(&mut self, device: impl Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.niri.seat.tablet_seat();

            let desc = TabletDescriptor::from(&device);
            tablet_seat.remove_tablet(&desc);

            // If there are no tablets in seat we can remove all tools
            if tablet_seat.count_tablets() == 0 {
                tablet_seat.clear_tools();
            }
        }
        if device.has_capability(DeviceCapability::Touch) && self.niri.touch.is_empty() {
            self.niri.seat.remove_touch();
        }
    }

    /// Computes the rectangle that covers all outputs in global space.
    fn global_bounding_rectangle(&self) -> Option<Rectangle<i32, Logical>> {
        self.niri.global_space.outputs().fold(
            None,
            |acc: Option<Rectangle<i32, Logical>>, output| {
                self.niri
                    .global_space
                    .output_geometry(output)
                    .map(|geo| acc.map(|acc| acc.merge(geo)).unwrap_or(geo))
            },
        )
    }

    /// Computes the cursor position for the tablet event.
    ///
    /// This function handles the tablet output mapping, as well as coordinate clamping and aspect
    /// ratio correction.
    fn compute_tablet_position<I: InputBackend>(
        &self,
        event: &(impl Event<I> + TabletToolEvent<I>),
    ) -> Option<Point<f64, Logical>>
    where
        I::Device: 'static,
    {
        let device_output = event.device().output(self);
        let device_output = device_output.filter(|output| self.niri.output_exists(output));
        let device_output = device_output.as_ref();
        let mapped_output = device_output.or_else(|| self.niri.output_for_tablet());

        // If the tablet is configured to map to the focused window, use that window's geometry on
        // the mapped output (or on the focused output if no specific output is mapped).
        let map_to_focused_window = self.niri.config.borrow().input.tablet.map_to_focused_window;
        // But only if the keyboard focus is on the layout, so that it doesn't trigger on the lock
        // screen and such.
        let window_target = if map_to_focused_window && self.niri.keyboard_focus.is_layout() {
            let output = mapped_output.or_else(|| self.niri.layout.active_output());
            output.and_then(|output| {
                let monitor = self.niri.layout.monitor_for_output(output)?;
                let mut rect = monitor.active_window_visual_rectangle()?;
                let output_geo = self.niri.global_space.output_geometry(output)?;
                rect.loc += output_geo.loc.to_f64();
                Some((rect, output))
            })
        } else {
            None
        };

        let (target_geo, keep_ratio, px, transform) = if let Some((rect, output)) = window_target {
            (
                rect,
                true,
                1. / output.current_scale().fractional_scale(),
                output.current_transform(),
            )
        } else if let Some(output) = mapped_output {
            let geo = self.niri.global_space.output_geometry(output).unwrap();
            (
                geo.to_f64(),
                true,
                1. / output.current_scale().fractional_scale(),
                output.current_transform(),
            )
        } else {
            let geo = self.global_bounding_rectangle()?.to_f64();

            // FIXME: this 1 px size should ideally somehow be computed for the rightmost output
            // corresponding to the position on the right when clamping.
            let output = self.niri.global_space.outputs().next().unwrap();
            let scale = output.current_scale().fractional_scale();

            // Do not keep ratio for the unified mode as this is what OpenTabletDriver expects.
            (geo, false, 1. / scale, Transform::Normal)
        };

        let mut pos = {
            let size = transform.invert().transform_size(target_geo.size);
            transform.transform_point_in(event.position_transformed(size.to_i32_round()), &size)
        };

        if keep_ratio {
            pos.x /= target_geo.size.w;
            pos.y /= target_geo.size.h;

            let device = event.device();
            if let Some(device) = (&device as &dyn Any).downcast_ref::<input::Device>() {
                if let Some(data) = self.niri.tablets.get(device) {
                    // This code does the same thing as mutter with "keep aspect ratio" enabled.
                    let size = transform.invert().transform_size(target_geo.size);
                    let output_aspect_ratio = size.w / size.h;
                    let ratio = data.aspect_ratio / output_aspect_ratio;

                    if ratio > 1. {
                        pos.x *= ratio;
                    } else {
                        pos.y /= ratio;
                    }
                }
            };

            pos.x *= target_geo.size.w;
            pos.y *= target_geo.size.h;
        }

        pos.x = pos.x.clamp(0.0, target_geo.size.w - px);
        pos.y = pos.y.clamp(0.0, target_geo.size.h - px);
        Some(pos + target_geo.loc)
    }

    fn is_inhibiting_shortcuts(&self) -> bool {
        self.niri
            .keyboard_focus
            .surface()
            .and_then(|surface| {
                self.niri
                    .keyboard_shortcuts_inhibiting_surfaces
                    .get(surface)
            })
            .is_some_and(KeyboardShortcutsInhibitor::is_active)
    }

    fn on_keyboard<I: InputBackend>(
        &mut self,
        event: I::KeyboardKeyEvent,
        consumed_by_a11y: &mut bool,
    ) {
        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let pressed = event.state() == KeyState::Pressed;

        // Stop bind key repeat on any release. This won't work 100% correctly in cases like:
        // 1. Press Mod
        // 2. Press Left (repeat starts)
        // 3. Press PgDown (new repeat starts)
        // 4. Release Left (PgDown repeat stops)
        // But it's good enough for now.
        // FIXME: handle this properly.
        if !pressed {
            if let Some(token) = self.niri.bind_repeat_timer.take() {
                self.niri.event_loop.remove(token);
            }
        }

        if pressed {
            self.hide_cursor_if_needed();

            // A key press is a real user interaction: advance the
            // focus-stealing-prevention clocks (mutter's `last_user_time` and
            // the focused window's `net_wm_user_time`).
            let now = get_monotonic_time();
            self.niri.last_user_action_time = Some(now);
            if let Some(mapped) = self.niri.layout.focus_mut() {
                mapped.bump_user_time(now);
            }
        }

        let is_inhibiting_shortcuts = self.is_inhibiting_shortcuts();

        // Accessibility modifier grabs should override XKB state changes (e.g. Caps Lock), so we
        // need to process them before keyboard.input() below.
        //
        // Other accessibility-grabbed keys should still update our XKB state, but not cause any
        // other changes.
        #[cfg(feature = "dbus")]
        let block = {
            let block = self.a11y_process_key(
                Duration::from_millis(u64::from(time)),
                event.key_code(),
                event.state(),
            );
            if block != KbMonBlock::Pass {
                *consumed_by_a11y = true;
            }
            // The accessibility modifier first press must not change XKB state, so we return
            // early here.
            if block == KbMonBlock::ModifierFirstPress {
                return;
            }
            block
        };
        #[cfg(not(feature = "dbus"))]
        let _ = consumed_by_a11y;

        let Some(Some(bind)) = self.niri.seat.get_keyboard().unwrap().input(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |this, mods, keysym| {
                let key_code = event.key_code();
                let modified = keysym.modified_sym();
                let raw = keysym.raw_latin_sym_or_raw_current_sym();
                let modifiers = modifiers_from_state(*mods);

                // After updating XKB state from accessibility-grabbed keys, return right away and
                // don't handle them.
                #[cfg(feature = "dbus")]
                if block != KbMonBlock::Pass {
                    // HACK: there's a slight problem with this code. Here we filter out keys
                    // consumed by accessibility from getting sent to the Wayland client. However,
                    // the Wayland client can still receive these keys from the wl_keyboard
                    // enter/modifiers events. In particular, this can easily happen when opening
                    // the Orca actions menu with Orca + Shift + A: in most cases, when this menu
                    // opens, Shift is still held down, so the menu receives it in
                    // wl_keyboard.enter/modifiers. Then the menu won't react to Enter presses
                    // until the user taps Shift again to "release" it (since the initial Shift
                    // release will be intercepted here).
                    //
                    // I don't think there's any good way of dealing with this apart from keeping a
                    // separate xkb state for accessibility, so that we can track the pressed
                    // modifiers without accidentally leaking them to wl_keyboard.enter. So for now
                    // let's forward modifier releases to the clients here to deal with the most
                    // common case.
                    if !pressed
                        && matches!(
                            modified,
                            Keysym::Shift_L
                                | Keysym::Shift_R
                                | Keysym::Control_L
                                | Keysym::Control_R
                                | Keysym::Super_L
                                | Keysym::Super_R
                                | Keysym::Alt_L
                                | Keysym::Alt_R
                        )
                    {
                        return FilterResult::Forward;
                    } else {
                        return FilterResult::Intercept(None);
                    }
                }

                // GNOME "overlay key": a lone tap of the configured overlay key
                // (`org.gnome.mutter overlay-key`, default Super_L) toggles the
                // Activities overview. Mirrors mutter's process_special_modifier_key
                // — arm on a bare overlay-key press, cancel the moment any other key
                // participates, and fire on the matching release. A client with an
                // active keyboard-shortcuts-inhibit prevents arming, but only arming:
                // mutter (process_overlay_key) also skips the check for a tap already
                // in flight, so it still completes.
                // The overlay key is read from the inspectable gnome_settings model,
                // which tracks the GSettings store live. The "no other modifiers"
                // check ignores Super itself, which is right for the Super_L/Super_R
                // settings but approximate for other keys.
                if pressed {
                    let is_overlay_key = raw
                        .is_some_and(|raw| this.niri.gnome_settings.overlay_keys.contains(&raw))
                        && !is_inhibiting_shortcuts
                        && !mods.ctrl
                        && !mods.shift
                        && !mods.alt
                        && !mods.iso_level3_shift
                        && !mods.iso_level5_shift;
                    this.niri.overlay_key_armed = is_overlay_key.then_some(key_code);
                } else if this.niri.overlay_key_armed.take() == Some(key_code) {
                    // A second tap that comes quickly enough shifts a state *up* —
                    // window picker → app grid — instead of toggling the overview
                    // back shut (`overviewControls.js:419-438`).
                    //
                    // With animations on, "quickly enough" is not a timer at all:
                    // gnome-shell asks whether its state adjustment is mid-transition
                    // upward, so the escalation window is exactly the open animation.
                    // With animations off there is no transition to catch, so it falls
                    // back to "the overview is up and the previous overlay-key fired
                    // less than ANIMATION_TIME ago" (`overview.js:12`, 250 ms). Miss
                    // the window either way and Super closes the overview as always.
                    let now = Duration::from_millis(u64::from(time));
                    let prev = this.niri.overlay_key_last_fired.replace(now);
                    let should_shift = if this.niri.config.borrow().animations.off {
                        this.niri.layout.is_overview_open()
                            && prev.is_some_and(|prev| {
                                now.saturating_sub(prev) < OVERLAY_KEY_SHIFT_WINDOW
                            })
                    } else {
                        this.niri.layout.is_overview_opening()
                    };

                    if should_shift {
                        this.niri.layout.open_app_grid();
                    } else {
                        this.do_action(Action::ToggleOverview, false);
                    }
                    return FilterResult::Intercept(None);
                }

                if this.niri.exit_confirm_dialog.is_open() && pressed {
                    if raw == Some(Keysym::Return) {
                        info!("quitting after confirming exit dialog");
                        this.niri.stop_signal.stop();
                    }

                    // Don't send this press to any clients.
                    this.niri.suppressed_keys.insert(key_code);
                    return FilterResult::Intercept(None);
                }

                // The run dialog is modal: while open, every key goes to it
                // and none reach the clients (gnome-shell holds a modal grab).
                if this.niri.run_dialog.is_open() {
                    let text = modified
                        .key_char()
                        .filter(|_| !mods.ctrl && !mods.alt && !mods.logo);
                    let outcome = this.niri.run_dialog.handle_key(
                        raw,
                        text,
                        pressed,
                        &this.niri.gnome_settings.command_history,
                    );
                    match outcome {
                        KeyOutcome::Handled => {}
                        KeyOutcome::Close => this.niri.run_dialog.close(),
                        KeyOutcome::Run(input) => this.run_dialog_execute(&input),
                    }
                    this.niri.queue_redraw_all();

                    if pressed {
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    } else if this.niri.suppressed_keys.remove(&key_code) {
                        return FilterResult::Intercept(None);
                    } else {
                        // Release of a key pressed before the dialog opened;
                        // the client saw the press, give it the release.
                        return FilterResult::Forward;
                    }
                }

                // The end-session (logout/shutdown/restart) confirmation is modal like the run
                // dialog: while open, every key goes to it and none reach the clients.
                if this.niri.end_session_dialog.is_open() {
                    match this.niri.end_session_dialog.handle_key(raw, pressed) {
                        DialogOutcome::Handled => this.niri.queue_redraw_all(),
                        DialogOutcome::Confirm => this.niri.confirm_end_session(),
                        DialogOutcome::Cancel => this.niri.cancel_end_session(),
                    }

                    if pressed {
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    } else if this.niri.suppressed_keys.remove(&key_code) {
                        return FilterResult::Intercept(None);
                    } else {
                        // Release of a key pressed before the dialog opened; the client saw the
                        // press, give it the release.
                        return FilterResult::Forward;
                    }
                }

                // A panel popover (the dateMenu calendar, …) grabs the keyboard modally like the
                // dialogs above: Escape closes it, every other key is swallowed. The one exception
                // is the hardcoded VT-switch chord (Ctrl+Alt+F1..F12): switching to a text console
                // must never be blocked by an in-compositor overlay, so let it through to the
                // backend even while the popover holds the grab.
                if this.niri.panel_popover.is_open() {
                    #[allow(non_upper_case_globals)]
                    if let keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12 =
                        modified.raw()
                    {
                        if pressed {
                            let vt = (modified.raw() - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                            this.backend.change_vt(vt);
                            this.niri.suppressed_keys.insert(key_code);
                            return FilterResult::Intercept(None);
                        } else if this.niri.suppressed_keys.remove(&key_code) {
                            return FilterResult::Intercept(None);
                        } else {
                            return FilterResult::Forward;
                        }
                    }

                    this.niri.panel_popover.handle_key(raw, pressed);
                    this.niri.queue_redraw_all();

                    if pressed {
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    } else if this.niri.suppressed_keys.remove(&key_code) {
                        return FilterResult::Intercept(None);
                    } else {
                        return FilterResult::Forward;
                    }
                }

                // Check if all modifiers were released while the MRU UI was open. If so, close the
                // UI (which will also transfer the focus to the current MRU UI selection).
                if this.niri.window_mru_ui.is_open() && !pressed && modifiers.is_empty() {
                    this.do_action(Action::MruConfirm, false);

                    if this.niri.suppressed_keys.remove(&key_code) {
                        return FilterResult::Intercept(None);
                    } else {
                        return FilterResult::Forward;
                    }
                }

                if pressed && raw == Some(Keysym::Escape) {
                    // Cancel certain grabs on Escape.
                    let pointer = this.niri.seat.get_pointer().unwrap();
                    if pointer
                        .with_grab(|_, grab| Self::grab_can_be_cancelled_with_esc(grab))
                        .unwrap_or(false)
                    {
                        pointer.unset_grab(this, serial, time);
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    }
                }

                if let Some(Keysym::space) = raw {
                    this.niri.screenshot_ui.set_space_down(pressed);
                }

                // A grabbed accelerator's release notifies the grabber; the
                // release itself stays suppressed through the normal path.
                if !pressed {
                    if let Some(action) = this.niri.accel_grab_release_pending.remove(&key_code) {
                        this.niri.emit_accelerator_signal(action, false);
                    }
                }

                let res = {
                    let config = this.niri.config.borrow();
                    let mru_is_open = this.niri.window_mru_ui.is_open();
                    let bindings =
                        make_binds_iter(&config, &mut this.niri.window_mru_ui, modifiers);

                    should_intercept_key(
                        &mut this.niri.suppressed_keys,
                        bindings,
                        &this.niri.gnome_settings.keybindings,
                        &this.niri.accel_grabs,
                        mru_is_open,
                        mod_key,
                        key_code,
                        modified,
                        raw,
                        pressed,
                        *mods,
                        &this.niri.screenshot_ui,
                        this.niri.config.borrow().input.disable_power_key_handling,
                        is_inhibiting_shortcuts,
                    )
                };

                if matches!(res, FilterResult::Forward) {
                    // Escape cancels an item drag in flight and goes no further: the icon
                    // flows home and the grid keeps its old order (`_onEvent` →
                    // `_cancelDrag`, `dnd.js:567-573`). It is consumed rather than passed
                    // on because gnome-shell's drag holds a stage grab, so the key never
                    // reaches the overview's own Escape either — and cancelling a drag is
                    // a whole intent, not a step toward closing the grid.
                    if pressed && raw == Some(Keysym::Escape) && this.niri.app_drag.is_some() {
                        this.cancel_app_drag();
                        this.niri.suppressed_keys.insert(key_code);
                        return FilterResult::Intercept(None);
                    }
                    if this.niri.keyboard_focus.is_overview() && pressed {
                        // Overview search: typing engages the search entry (GNOME's
                        // `_onStageKeyPress`/`_shouldTriggerSearch`, searchController.js:145-236).
                        // Press-only + shared `suppressed_keys`: `should_intercept_key` above
                        // already owns key releases globally (a suppressed release returns
                        // Intercept there and never reaches here), so a copied run_dialog-style
                        // release arm would leak/double-forward — do NOT add one. The Enter →
                        // launch → close release is leak-free by construction (the release is
                        // suppressed and swallowed before the new window sees it).
                        //
                        // `keyboard_focus == Overview` is only reached when nothing above
                        // (lock/screenshot/dialogs/popover) claimed focus (niri.rs
                        // update_keyboard_focus), so the search can't engage while invisible.
                        let text = modified
                            .key_char()
                            .filter(|_| !mods.ctrl && !mods.alt && !mods.logo);
                        // The folder rename entry comes first: while it is up it holds the
                        // key focus, so the search never sees a keystroke
                        // (`_showFolderEntry`'s `grab_key_focus`, `appDisplay.js:2643-2648`).
                        if this.niri.folder_dialog.is_renaming() {
                            use crate::ui::folder_dialog::RenameKey;
                            let typed = text.map(String::from);
                            match this.niri.folder_dialog.rename_key(raw, typed.as_deref()) {
                                RenameKey::Ignored => {}
                                RenameKey::Took => {
                                    this.niri.suppressed_keys.insert(key_code);
                                    this.niri.queue_redraw_all();
                                    return FilterResult::Intercept(None);
                                }
                                RenameKey::Commit => {
                                    if let Some((folder, name)) =
                                        this.niri.folder_dialog.finish_rename()
                                    {
                                        this.rename_folder(&folder, &name);
                                    }
                                    this.niri.suppressed_keys.insert(key_code);
                                    this.niri.queue_redraw_all();
                                    return FilterResult::Intercept(None);
                                }
                            }
                        }
                        let active = this.niri.overview_search.is_active();
                        // A non-whitespace printable starts a search; once active, every key
                        // routes to the entry (Backspace/nav/Escape). Modifiers/Tab/arrows while
                        // inactive fall through to the overview binds below.
                        let starts = text.is_some_and(|c| !c.is_whitespace() && !c.is_control());
                        if active || starts {
                            use crate::ui::overview_search::SearchOutcome;
                            let plain = !mods.ctrl && !mods.alt && !mods.logo;
                            let outcome = this
                                .niri
                                .overview_search
                                .handle_key(raw, text, plain, mods.shift);
                            // Ignored = a key the search doesn't handle (bare modifier, F-key);
                            // let it fall through to the hardcoded overview binds, unconsumed.
                            if !matches!(outcome, SearchOutcome::Ignored) {
                                match outcome {
                                    SearchOutcome::Handled | SearchOutcome::Ignored => {}
                                    SearchOutcome::QueryChanged | SearchOutcome::Cleared => {
                                        this.niri.sync_overview_search();
                                    }
                                    SearchOutcome::Activate(id) => {
                                        this.launch_app(
                                            &id,
                                            crate::app_system::LaunchMode::Activate,
                                            None,
                                            "search",
                                        );
                                        this.niri.overview_search.clear();
                                        this.niri.layout.close_overview();
                                    }
                                    SearchOutcome::Close => {
                                        this.niri.overview_search.clear();
                                        // Escape tiers (`searchController.js:153-159`):
                                        // search → grid → hide. The entry only returns
                                        // Close with no query, so fall through to the
                                        // app grid, then the overview.
                                        if !this.niri.layout.close_app_grid() {
                                            this.niri.layout.close_overview();
                                        }
                                    }
                                }
                                this.niri.queue_redraw_all();
                                this.niri.suppressed_keys.insert(key_code);
                                return FilterResult::Intercept(None);
                            }
                        }
                    }

                    // If we didn't find any bind, try other hardcoded keys.
                    if this.niri.keyboard_focus.is_overview() && pressed {
                        // Escape closes an open folder first — the dialog holds a
                        // `GrabHelper` grab whose `onUngrab` pops it down
                        // (`appDisplay.js:2879-2883`), so it is the innermost tier of
                        // the overview's Escape ladder. Then the app grid returns to the
                        // window picker (`searchController.js:153-159`); only when
                        // already in the picker does it fall through to the
                        // CloseOverview bind below.
                        if raw == Some(Keysym::Escape) && this.niri.folder_dialog.popdown() {
                            this.niri.suppressed_keys.insert(key_code);
                            this.niri.queue_redraw_all();
                            return FilterResult::Intercept(None);
                        }
                        if raw == Some(Keysym::Escape) && this.niri.layout.close_app_grid() {
                            this.niri.suppressed_keys.insert(key_code);
                            this.niri.queue_redraw_all();
                            return FilterResult::Intercept(None);
                        }
                        // Then the app grid's own keyboard navigation, which has to come
                        // before the arrow binds below: those move *window* focus behind
                        // the grid.
                        if raw.is_some_and(|raw| this.overview_grid_key(raw, *mods)) {
                            this.niri.suppressed_keys.insert(key_code);
                            this.niri.queue_redraw_all();
                            return FilterResult::Intercept(None);
                        }
                        if let Some(bind) = raw.and_then(|raw| hardcoded_overview_bind(raw, *mods))
                        {
                            this.niri.suppressed_keys.insert(key_code);
                            return FilterResult::Intercept(Some(bind));
                        }
                    }

                    // Interaction with the active window, immediately update the active window's
                    // focus timestamp without waiting for a possible pending MRU lock-in delay.
                    this.niri.mru_apply_keyboard_commit();
                }

                res
            },
        ) else {
            return;
        };

        if !pressed {
            return;
        }

        // Remember which key fired a grabbed accelerator, so its release can
        // send AcceleratorDeactivated (mutter's external-grab handler is
        // TRIGGER_RELEASE: press activates, release deactivates).
        if let Action::ActivateAcceleratorGrab(action) = bind.action {
            self.niri
                .accel_grab_release_pending
                .insert(event.key_code(), action);
        }

        self.handle_bind(bind.clone());

        self.start_key_repeat(bind);
    }

    fn start_key_repeat(&mut self, bind: Bind) {
        if !bind.repeat {
            return;
        }

        // Stop the previous key repeat if any.
        if let Some(token) = self.niri.bind_repeat_timer.take() {
            self.niri.event_loop.remove(token);
        }

        let config = self.niri.config.borrow();
        let config = &config.input.keyboard;

        let repeat_rate = config.repeat_rate;
        if repeat_rate == 0 {
            return;
        }
        let repeat_duration = Duration::from_secs_f64(1. / f64::from(repeat_rate));

        let repeat_timer =
            Timer::from_duration(Duration::from_millis(u64::from(config.repeat_delay)));

        let token = self
            .niri
            .event_loop
            .insert_source(repeat_timer, move |_, _, state| {
                state.handle_bind(bind.clone());
                TimeoutAction::ToDuration(repeat_duration)
            })
            .unwrap();

        self.niri.bind_repeat_timer = Some(token);
    }

    fn hide_cursor_if_needed(&mut self) {
        // If the pointer is already invisible, don't reset it back to Hidden causing one frame
        // of hover.
        if !self.niri.pointer_visibility.is_visible() {
            return;
        }

        if !self.niri.config.borrow().cursor.hide_when_typing {
            return;
        }

        // niri keeps this set only while actively using a tablet, which means the cursor position
        // is likely to change almost immediately, causing pointer_visibility to just flicker back
        // and forth.
        if self.niri.tablet_cursor_location.is_some() {
            return;
        }

        self.niri.pointer_visibility = PointerVisibility::Hidden;
        self.niri.queue_redraw_all();
    }

    pub fn handle_bind(&mut self, bind: Bind) {
        let Some(cooldown) = bind.cooldown else {
            self.do_action(bind.action, bind.allow_when_locked);
            return;
        };

        // Check this first so that it doesn't trigger the cooldown.
        if self.niri.is_locked() && !(bind.allow_when_locked || allowed_when_locked(&bind.action)) {
            return;
        }

        match self.niri.bind_cooldown_timers.entry(bind.key) {
            // The bind is on cooldown.
            Entry::Occupied(_) => (),
            Entry::Vacant(entry) => {
                let timer = Timer::from_duration(cooldown);
                let token = self
                    .niri
                    .event_loop
                    .insert_source(timer, move |_, _, state| {
                        if state.niri.bind_cooldown_timers.remove(&bind.key).is_none() {
                            error!("bind cooldown timer entry disappeared");
                        }
                        TimeoutAction::Drop
                    })
                    .unwrap();
                entry.insert(token);

                self.do_action(bind.action, bind.allow_when_locked);
            }
        }
    }

    /// Execute a run dialog command line, mirroring gnome-shell's `_run`: the
    /// input enters the history whether or not it runs (persisted to
    /// GSettings), then either spawns and closes the dialog, or shows the
    /// error in-dialog and leaves it open.
    fn run_dialog_execute(&mut self, input: &str) {
        let trimmed = run_dialog::history_add(&mut self.niri.gnome_settings.command_history, input);
        if let Some(writer) = &self.niri.gnome_settings_writer {
            writer.set_command_history(self.niri.gnome_settings.command_history.clone());
        }

        match run_dialog::resolve_command_line(&trimmed) {
            Ok(argv) => {
                spawn(argv, None);
                self.niri.run_dialog.close();
            }
            Err(message) => self.niri.run_dialog.set_error(message),
        }
        self.niri.queue_redraw_all();
    }

    /// Apply a quick-settings popover click outcome: write the backing gsettings
    /// key (mirrored locally so the tile and the panel indicator update before the
    /// change round-trips through the watcher), or spawn a system-row command.
    /// A click landed inside the shown notification banner: gnome-shell's exact
    /// semantics — close destroys DISMISSED even resident
    /// (`js/ui/messageList.js:725-728`); an action button (or a body click with
    /// a `default` action) emits ActivationToken+ActionInvoked and destroys
    /// unless resident (`js/ui/messageTray.js:431-447,475-492`); a body click
    /// with no default action runs `source.open()`'s destroy-all-non-resident
    /// (app focus deferred, `js/ui/notificationDaemon.js:369-373`).
    fn on_banner_hit(&mut self, hit: crate::ui::notification_banner::BannerHit) {
        use crate::notifications::CloseReason;
        use crate::ui::notification_banner::BannerHit;

        let Some(id) = self.niri.notification_banner.content_id() else {
            return;
        };
        let mut activated = false;
        let effects = match hit {
            BannerHit::Close => self.niri.notifications.close(id, CloseReason::Dismissed),
            BannerHit::Action(idx) => {
                let Some(action) = self.niri.notification_banner.action_key(idx) else {
                    return;
                };
                activated = self.niri.emit_notification_action(id, action);
                self.niri.notifications.activate(id)
            }
            BannerHit::Body => {
                if self.niri.notification_banner.has_default_action() {
                    activated = self.niri.emit_notification_action(id, "default".to_owned());
                    self.niri.notifications.activate(id)
                } else {
                    activated = self.niri.open_notification_app(id);
                    self.niri.notifications.activate_source(id)
                }
            }
        };
        if activated {
            self.niri.layout.close_overview();
        }
        self.niri.apply_notification_effects(effects);
    }

    /// `pub(crate)` so the corpus can drive an action straight in: several of them are
    /// only reachable through a click that would really launch something.
    pub(crate) fn apply_popover_action(&mut self, action: crate::ui::popover::PopoverAction) {
        use crate::ui::popover::PopoverAction;

        let set_toggle = |state: &mut Self, f: fn(&mut crate::gnome::QuickToggles, bool), v| {
            f(&mut state.niri.gnome_settings.quick_toggles, v);
            state
                .niri
                .panel
                .set_quick_toggles(state.niri.gnome_settings.quick_toggles);
        };

        match action {
            PopoverAction::Consumed => {}
            PopoverAction::SetDarkStyle(v) => {
                if let Some(w) = &self.niri.gnome_settings_writer {
                    w.set_dark_style(v);
                }
                set_toggle(self, |t, v| t.dark_style = v, v);
            }
            PopoverAction::SetDoNotDisturb(v) => {
                if let Some(w) = &self.niri.gnome_settings_writer {
                    w.set_do_not_disturb(v);
                }
                set_toggle(self, |t, v| t.do_not_disturb = v, v);
                // DND gates the panel's messages dot (`js/ui/dateMenu.js:796-797`).
                // `set_toggle` already updated `quick_toggles`; recompute the dot
                // from it now so the toggle reflects immediately even without a
                // gsettings writer (the settings round-trip is the only other path,
                // and it's absent headless).
                self.niri.update_messages_indicator();
            }
            PopoverAction::SetNightLight(v) => {
                if let Some(w) = &self.niri.gnome_settings_writer {
                    w.set_night_light(v);
                }
                set_toggle(self, |t, v| t.night_light = v, v);
            }
            // The screenshot UI deliberately does NOT leave the overview: gnome-shell's
            // screenshot button only closes the quick-settings menu and calls
            // `Main.screenshotUI.open()` (`js/ui/status/system.js:120-127`), which has no
            // `Main.overview.hide()` anywhere in it.
            PopoverAction::Screenshot => self.open_screenshot_ui(true, None),
            // Every shell surface that starts an app leaves the overview first —
            // `Main.overview.hide(); Main.panel.close…(); app.activate()`, in the
            // quick-settings system rows (`js/ui/status/system.js:53-57,150-154`), the
            // settings actions (`js/ui/popupMenu.js:709-720`) and the dateMenu's cards
            // (`js/ui/dateMenu.js:300-302,376-381,597-600`). Ours all funnel through
            // `Spawn`, so this one call covers them.
            PopoverAction::Spawn(command) => {
                spawn(command, None);
                self.niri.layout.close_overview();
            }
            #[cfg(feature = "pipewire")]
            PopoverAction::SetVolume(volume) => {
                if let Some(pw) = self.niri.pw_audio.as_ref() {
                    let status = pw.set_volume(volume);
                    self.on_audio_status(status);
                }
            }
            #[cfg(feature = "pipewire")]
            PopoverAction::ToggleMute => {
                if let Some(pw) = self.niri.pw_audio.as_ref() {
                    let status = pw.toggle_muted();
                    self.on_audio_status(status);
                }
            }
            // Setting the default sink is fire-and-forget: the metadata write echoes back through
            // the watcher, which moves the picker's check and the bound-sink volume. Not applied
            // optimistically (a rejected write has no corrective echo).
            #[cfg(feature = "pipewire")]
            PopoverAction::SetDefaultSink(name) => {
                if let Some(pw) = self.niri.pw_audio.as_ref() {
                    pw.set_default_sink(&name);
                }
            }
            #[cfg(feature = "pipewire")]
            PopoverAction::SetInputVolume(volume) => {
                if let Some(status) = self
                    .niri
                    .pw_audio
                    .as_ref()
                    .and_then(|pw| pw.set_input_volume(volume))
                {
                    self.on_mic_status(status);
                }
            }
            #[cfg(feature = "pipewire")]
            PopoverAction::ToggleInputMute => {
                if let Some(status) = self
                    .niri
                    .pw_audio
                    .as_ref()
                    .and_then(|pw| pw.toggle_input_muted())
                {
                    self.on_mic_status(status);
                }
            }
            // Fire-and-forget, like SetDefaultSink.
            #[cfg(feature = "pipewire")]
            PopoverAction::SetDefaultSource(name) => {
                if let Some(pw) = self.niri.pw_audio.as_ref() {
                    pw.set_default_source(&name);
                }
            }
            #[cfg(not(feature = "pipewire"))]
            PopoverAction::SetVolume(_)
            | PopoverAction::ToggleMute
            | PopoverAction::SetDefaultSink(_)
            | PopoverAction::SetInputVolume(_)
            | PopoverAction::ToggleInputMute
            | PopoverAction::SetDefaultSource(_) => {}
            // Airplane mode: fire-and-forget property write on gsd-rfkill's connection (never a
            // blocking Set on this thread); the tile updates when gsd echoes `PropertiesChanged`.
            #[cfg(feature = "dbus")]
            PopoverAction::SetAirplaneMode(active) => {
                if let Some(conn) = self.niri.dbus.as_ref().and_then(|d| d.conn_rfkill.as_ref()) {
                    crate::dbus::rfkill::set_airplane_mode(conn, active);
                }
            }
            #[cfg(not(feature = "dbus"))]
            PopoverAction::SetAirplaneMode(_) => {}
            // Power Mode body toggle: Balanced ↔ the compositor-owned last-selected profile
            // (gnome-shell's `clicked`). Fire-and-forget write on the system-status connection; the
            // tile updates on the daemon's echo.
            #[cfg(feature = "dbus")]
            PopoverAction::TogglePowerProfile => {
                let target = if self.niri.system_status.power.is_active() {
                    "balanced".to_string()
                } else {
                    self.niri.last_power_profile.clone()
                };
                if let Some(conn) = self
                    .niri
                    .dbus
                    .as_ref()
                    .and_then(|d| d.conn_system_status.as_ref())
                {
                    crate::dbus::system_status::set_active_profile(conn, target);
                }
            }
            // Power Mode picker row: set the chosen profile directly.
            #[cfg(feature = "dbus")]
            PopoverAction::SetPowerProfile(profile) => {
                if let Some(conn) = self
                    .niri
                    .dbus
                    .as_ref()
                    .and_then(|d| d.conn_system_status.as_ref())
                {
                    crate::dbus::system_status::set_active_profile(conn, profile);
                }
            }
            #[cfg(not(feature = "dbus"))]
            PopoverAction::TogglePowerProfile | PopoverAction::SetPowerProfile(_) => {}
            // Message-list card interactions: the same store paths as the
            // banner's clicks (`on_banner_hit`); `apply_notification_effects`
            // pushes the shrunk snapshot back into the open popover.
            PopoverAction::CloseNotification(id) => {
                let effects = self
                    .niri
                    .notifications
                    .close(id, crate::notifications::CloseReason::Dismissed);
                self.niri.apply_notification_effects(effects);
            }
            PopoverAction::CloseNotificationGroup(ids) => {
                // Closing a collapsed group closes every notification in it
                // (`js/ui/messageList.js:1236-1242`), each reason Dismissed.
                let mut effects = crate::notifications::Effects::default();
                for id in ids {
                    let e = self
                        .niri
                        .notifications
                        .close(id, crate::notifications::CloseReason::Dismissed);
                    effects.merge(e);
                }
                self.niri.apply_notification_effects(effects);
            }
            PopoverAction::ActivateNotification { id, has_default } => {
                let (activated, effects) = if has_default {
                    let activated = self.niri.emit_notification_action(id, "default".to_owned());
                    (activated, self.niri.notifications.activate(id))
                } else {
                    let activated = self.niri.open_notification_app(id);
                    (activated, self.niri.notifications.activate_source(id))
                };
                if activated {
                    self.niri.layout.close_overview();
                }
                self.niri.apply_notification_effects(effects);
            }
            PopoverAction::InvokeNotificationAction { id, key } => {
                if self.niri.emit_notification_action(id, key) {
                    self.niri.layout.close_overview();
                }
                let effects = self.niri.notifications.activate(id);
                self.niri.apply_notification_effects(effects);
            }
            PopoverAction::ClearNotifications => {
                let effects = self.niri.notifications.clear_all();
                self.niri.apply_notification_effects(effects);
            }
            PopoverAction::SetInputSource(idx) => self.set_input_source(idx),

            // The app menu's launch rows. Both leave the overview, like every
            // `Main.overview.hide()` in `AppMenu` (`appMenu.js:60,94,240`).
            PopoverAction::AppNewWindow(id) => {
                self.launch_from_app_menu(&id, LaunchMode::NewWindow);
            }
            PopoverAction::AppLaunchAction { id, action } => {
                self.launch_from_app_menu(&id, LaunchMode::Action(action));
            }
            // Raising a window from the "Open Windows" section is `Main.activateWindow`,
            // which leaves the overview.
            PopoverAction::AppActivateWindow(window) => {
                self.activate_window_by_id(window);
                self.niri.layout.close_overview();
            }
            // `shell_app_request_quit` (`shell-app.c:1210-1243`) minus its first
            // branch: we have no `org.gtk.Application` action muxer, so an app that
            // exports `app.quit` is closed the same way as one that does not — by
            // closing every window (the fallback GNOME takes for every non-GTK app).
            // Recorded in `docs/fork/app-lifecycle-port.md` §5.
            PopoverAction::AppQuit(id) => self.request_app_quit(&id),
            PopoverAction::AppDetails(id) => self.show_app_details(&id),
            // Pinning does *not* leave the overview — gnome-shell hides it only for
            // the rows that raise a window.
            PopoverAction::AppToggleFavorite(id) => {
                if self.niri.app_system.is_favorite(&id) {
                    self.unpin_app(&id);
                } else if self.niri.app_system.add_favorite(&id) {
                    self.commit_favorites();
                }
            }
        }
    }

    /// Pop up the app context menu for an overview hit, if that hit is an app icon.
    /// Returns whether a menu opened — anything else (the show-apps button, a page
    /// control, the search entry) has no menu and falls through to the normal press.
    ///
    /// The arrow side is per-surface, as in gnome-shell: a dash icon's menu opens
    /// *upward* (`popupMenuSide: St.Side.BOTTOM`, `dash.js:27`) because the dash sits at
    /// the bottom of the screen, while a grid or search icon takes `AppIcon`'s default
    /// `St.Side.LEFT` and opens to the icon's right (`appDisplay.js:2928`).
    fn open_app_menu_for(&mut self, output: &Output, hit: OverviewHit) -> bool {
        let Some(controls) = self.niri.layout.controls_layout_for_output(output) else {
            return false;
        };
        let (id, anchor, side) = match hit {
            OverviewHit::Dash(DashHit::App(i)) => (
                self.niri.dash.item_id(i).map(str::to_owned),
                self.niri.dash.tile_rect(i, controls.dash),
                PopoverSide::Bottom,
            ),
            OverviewHit::GridApp(i) => (
                self.niri.app_grid.entry_id(i).map(str::to_owned),
                self.niri.app_grid.entry_rect(i, controls.app_display),
                PopoverSide::Left,
            ),
            OverviewHit::Search(SearchHit::Result(i)) => (
                self.niri.overview_search.result_id(i).map(str::to_owned),
                self.niri.overview_search.result_rect(i, controls.into()),
                PopoverSide::Left,
            ),
            _ => return false,
        };
        let (Some(id), Some(anchor)) = (id, anchor) else {
            return false;
        };
        // The menu is built from the catalog entry, not from the surface's snapshot:
        // the `.desktop` actions it lists live on the entry (`AppMenu.setApp`).
        let Some(entry) = self.niri.app_system.lookup(&id) else {
            return false;
        };
        let is_favorite = self.niri.app_system.is_favorite(&id);
        let state = self.niri.app_system.app_state(&id);
        // `showSingleWindows: true` for an app-grid / dash icon
        // (`appDisplay.js:3033`), so one window is already a section.
        let windows = self
            .niri
            .app_system
            .running_app(&id)
            .map(|a| a.windows.clone())
            .unwrap_or_default();
        // `_updateDetailsVisibility` (`appMenu.js:182-185`).
        let has_software = self
            .niri
            .app_system
            .lookup("org.gnome.Software.desktop")
            .is_some();
        let ctx = crate::ui::app_menu::AppMenuContext {
            entry: &entry,
            is_favorite,
            state,
            windows: &windows,
            has_software,
        };
        self.niri
            .panel_popover
            .open_app_menu(output.clone(), anchor, side, &ctx);
        // Remembered so the icon can stay highlighted for as long as its menu is up.
        self.niri.app_menu_source = Some(hit);
        true
    }

    /// Raise a window by id — `Main.activateWindow(window)` (`appMenu.js:285`).
    fn activate_window_by_id(&mut self, id: crate::window::mapped::MappedId) {
        let window = self
            .niri
            .layout
            .windows()
            .find(|(_, m)| m.id() == id)
            .map(|(_, m)| m.window.clone());
        if let Some(window) = window {
            self.focus_window(&window);
        }
    }

    /// `shell_app_request_quit` (`shell-app.c:1210-1243`), fallback branch: close
    /// every window of the app. GNOME tries an exported `app.quit` action first; we
    /// have no action muxer, so this is the only branch — and it is the one GNOME
    /// itself takes for every app that does not export one.
    ///
    /// Not running is a no-op, matching the early return at `:1216`; the menu row is
    /// hidden in that case anyway.
    fn request_app_quit(&mut self, id: &str) {
        let Some(app) = self.niri.app_system.running_app(id) else {
            return;
        };
        let ids: Vec<_> = app.windows.iter().map(|w| w.id).collect();
        for window_id in ids {
            let window = self
                .niri
                .layout
                .windows()
                .find(|(_, m)| m.id() == window_id);
            if let Some((_, mapped)) = window {
                mapped.toplevel().send_close();
            }
        }
    }

    /// "App Details" (`appMenu.js:84-95`): activate `org.gnome.Software`'s `details`
    /// action over `org.gtk.Actions`, then leave the overview.
    fn show_app_details(&mut self, id: &str) {
        crate::dbus::show_app_details(id.to_owned());
        self.niri.layout.close_overview();
    }

    /// Launch `id` from a context-menu row and leave the overview.
    fn launch_from_app_menu(&mut self, id: &str, mode: LaunchMode) {
        self.launch_app(id, mode, None, "app-menu");
        self.niri.layout.close_overview();
    }

    /// The one place an app is launched — our `shell_app_launch`
    /// (`shell-app.c:1354`) plus the launch context it needs.
    ///
    /// Every launch mints an xdg-activation token, exactly as mutter's launch
    /// context does on Wayland (`meta-launch-context.c:158-184`): it is the child's
    /// permission to raise its own first window, and it is the id of the startup
    /// sequence that puts the app in `AppState::Starting` until that window shows
    /// up. `workspace` is the sequence's target workspace, if the launch asked for
    /// one (an icon dropped on a workspace thumbnail).
    ///
    /// `origin` only labels the warning on failure.
    fn launch_app(
        &mut self,
        id: &str,
        mode: LaunchMode,
        workspace: Option<crate::layout::workspace::WorkspaceId>,
        origin: &str,
    ) -> bool {
        let (token, _) = self.niri.activation_state.create_external_token(None);
        let ctx = crate::app_system::LaunchContext {
            token: Some(token.as_str().to_owned()),
            workspace,
            now: get_monotonic_time(),
        };
        match self.niri.app_system.launch(id, mode, &ctx) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!("{origin} launch of {id} failed: {err:?}");
                false
            }
        }
    }

    /// Start recording the active output, or stop if one is already running (the
    /// `ToggleScreenRecord` action / a future record button).
    #[cfg(feature = "xdp-gnome-screencast")]
    fn toggle_screen_record(&mut self) {
        use crate::screencasting::RecordingKind;

        let recording = self
            .niri
            .casting
            .recordings
            .iter()
            .any(|r| matches!(r.kind, RecordingKind::Native(_)));
        if recording {
            self.niri.stop_screen_recordings();
        } else if let Some(output) = self.niri.layout.active_output().cloned() {
            match crate::recording::default_recording_path() {
                Ok(path) => {
                    // The keybind/pill records the whole active output at 30fps, cursor drawn.
                    if let Err(err) = self
                        .niri
                        .start_native_recording(&output, path, 30, true, None)
                    {
                        warn!("could not start screen recording: {err:?}");
                    }
                }
                Err(err) => warn!("could not choose a recording path: {err:?}"),
            }
        }
        self.niri.queue_redraw_all();
    }

    pub fn do_action(&mut self, action: Action, allow_when_locked: bool) {
        if self.niri.is_locked() && !(allow_when_locked || allowed_when_locked(&action)) {
            return;
        }

        if let Some(touch) = self.niri.seat.get_touch() {
            touch.cancel(self);
        }

        match action {
            Action::Quit(skip_confirmation) => {
                if !skip_confirmation && self.niri.exit_confirm_dialog.show() {
                    self.niri.queue_redraw_all();
                    return;
                }

                info!("quitting as requested");
                self.niri.stop_signal.stop()
            }
            Action::ChangeVt(vt) => {
                self.backend.change_vt(vt);
                // Changing VT may not deliver the key releases, so clear the state.
                self.niri.suppressed_keys.clear();
            }
            Action::Suspend => {
                self.backend.suspend();
                // Suspend may not deliver the key releases, so clear the state.
                self.niri.suppressed_keys.clear();
            }
            Action::PowerOffMonitors => {
                self.niri.deactivate_monitors(&mut self.backend);
            }
            Action::PowerOnMonitors => {
                self.niri.activate_monitors(&mut self.backend);
            }
            Action::Logout => self
                .niri
                .request_session_action(crate::end_session::SessionRequest::Logout),
            Action::PowerOff => self
                .niri
                .request_session_action(crate::end_session::SessionRequest::PowerOff),
            Action::Reboot => self
                .niri
                .request_session_action(crate::end_session::SessionRequest::Reboot),
            Action::ToggleDebugTint => {
                self.backend.toggle_debug_tint();
                self.niri.queue_redraw_all();
            }
            Action::DebugToggleOpaqueRegions => {
                self.niri.debug_draw_opaque_regions = !self.niri.debug_draw_opaque_regions;
                self.niri.queue_redraw_all();
            }
            Action::DebugToggleDamage => {
                self.niri.debug_toggle_damage();
            }
            Action::Spawn(command) => {
                let (token, _) = self.niri.activation_state.create_external_token(None);
                spawn(command, Some(token.clone()));
            }
            Action::SpawnSh(command) => {
                let (token, _) = self.niri.activation_state.create_external_token(None);
                spawn_sh(command, Some(token.clone()));
            }
            Action::DoScreenTransition(delay_ms) => {
                // Capture the Output-target neutral buffers through the owned renderer first.
                let neutrals = self
                    .backend
                    .with_vulkan_renderer(|vk| self.niri.capture_screen_transition_neutrals(vk))
                    .unwrap_or_default();
                // The neutrals are already captured, so the transition must not depend on a
                // renderer being available here, or it would silently never run.
                self.niri.do_screen_transition(neutrals, delay_ms);
            }
            Action::ScreenshotScreen(write_to_disk, show_pointer, path) => {
                let active = self.niri.layout.active_output().cloned();
                if let Some(active) = active {
                    let res = self.backend.with_vulkan_renderer(|renderer| {
                        self.niri
                            .screenshot(renderer, &active, write_to_disk, show_pointer, path)
                    });
                    match res {
                        Some(Err(err)) => warn!("error taking screenshot: {err:?}"),
                        None => warn!("renderer unavailable for screenshot"),
                        Some(Ok(())) => {}
                    }
                }
            }
            Action::ConfirmScreenshot { write_to_disk } => {
                self.confirm_screenshot(write_to_disk);
            }
            Action::CancelScreenshot => {
                if !self.niri.screenshot_ui.is_open() {
                    return;
                }

                self.niri.screenshot_ui.close();
                self.niri
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
                self.niri.queue_redraw_all();
            }
            Action::ScreenshotTogglePointer => {
                self.niri.screenshot_ui.toggle_pointer();
                self.niri.queue_redraw_all();
            }
            Action::Screenshot(show_cursor, path) => {
                self.open_screenshot_ui(show_cursor, path);
                self.niri.cancel_mru();
            }
            Action::ScreenshotWindow(write_to_disk, show_pointer, path) => {
                let focus = self.niri.layout.focus_with_output();
                if let Some((mapped, output)) = focus {
                    let res = self.backend.with_vulkan_renderer(|renderer| {
                        self.niri.screenshot_window(
                            renderer,
                            output,
                            mapped,
                            write_to_disk,
                            show_pointer,
                            path,
                        )
                    });
                    match res {
                        Some(Err(err)) => warn!("error taking screenshot: {err:?}"),
                        None => warn!("renderer unavailable for screenshot"),
                        Some(Ok(())) => {}
                    }
                }
            }
            Action::ScreenshotWindowById {
                id,
                write_to_disk,
                show_pointer,
                path,
            } => {
                let mut windows = self.niri.layout.windows();
                let window = windows.find(|(_, m)| m.id().get() == id);
                if let Some((Some(monitor), mapped)) = window {
                    let output = monitor.output();
                    let res = self.backend.with_vulkan_renderer(|renderer| {
                        self.niri.screenshot_window(
                            renderer,
                            output,
                            mapped,
                            write_to_disk,
                            show_pointer,
                            path,
                        )
                    });
                    match res {
                        Some(Err(err)) => warn!("error taking screenshot: {err:?}"),
                        None => warn!("renderer unavailable for screenshot"),
                        Some(Ok(())) => {}
                    }
                }
            }
            Action::ToggleKeyboardShortcutsInhibit => {
                if let Some(inhibitor) = self.niri.keyboard_focus.surface().and_then(|surface| {
                    self.niri
                        .keyboard_shortcuts_inhibiting_surfaces
                        .get(surface)
                }) {
                    if inhibitor.is_active() {
                        inhibitor.inactivate();
                    } else {
                        inhibitor.activate();
                    }
                }
            }
            Action::CloseWindow => {
                if let Some(mapped) = self.niri.layout.focus() {
                    mapped.toplevel().send_close();
                }
            }
            Action::CloseWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                if let Some((_, mapped)) = window {
                    mapped.toplevel().send_close();
                }
            }
            Action::FullscreenWindow => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FullscreenWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowedFullscreen => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_windowed_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleWindowedFullscreenById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_windowed_fullscreen(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusWindow(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.focus_window(&window);
                }
            }
            Action::FocusWindowInColumn(index) => {
                self.niri.layout.focus_window_in_column(index);
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowPrevious => {
                let current = self.niri.layout.focus().map(|win| win.id());
                if let Some(window) = self
                    .niri
                    .layout
                    .windows()
                    .map(|(_, win)| win)
                    .filter(|win| Some(win.id()) != current)
                    .max_by_key(|win| win.get_focus_timestamp())
                    .map(|win| win.window.clone())
                {
                    // Commit current focus so repeated focus-window-previous works as expected.
                    self.niri.mru_apply_keyboard_commit();

                    self.focus_window(&window);
                }
            }
            Action::SwitchLayout(action) => {
                let keyboard = &self.niri.seat.get_keyboard().unwrap();
                keyboard.with_xkb_state(self, |mut state| match action {
                    LayoutSwitchTarget::Next => state.cycle_next_layout(),
                    LayoutSwitchTarget::Prev => state.cycle_prev_layout(),
                    LayoutSwitchTarget::Index(layout) => {
                        let num_layouts = state.xkb().lock().unwrap().layouts().count();
                        if usize::from(layout) >= num_layouts {
                            warn!("requested layout doesn't exist")
                        } else {
                            state.set_layout(Layout(layout.into()))
                        }
                    }
                });
            }
            Action::MoveColumnLeft => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_left();
                } else {
                    self.niri.layout.move_left();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnRight => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_right();
                } else {
                    self.niri.layout.move_right();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToFirst => {
                self.niri.layout.move_column_to_first();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToLast => {
                self.niri.layout.move_column_to_last();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnLeftOrToMonitorLeft => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_left();
                } else if let Some(output) = self.niri.output_left() {
                    if self.niri.layout.move_column_left_or_to_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.move_left();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnRightOrToMonitorRight => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_right();
                } else if let Some(output) = self.niri.output_right() {
                    if self.niri.layout.move_column_right_or_to_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.move_right();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowDown => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_down();
                } else {
                    self.niri.layout.move_down();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowUp => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_up();
                } else {
                    self.niri.layout.move_up();
                    self.maybe_warp_cursor_to_focus();
                }

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowDownOrToWorkspaceDown => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_down();
                } else {
                    self.niri.layout.move_down_or_to_workspace_down();
                    self.maybe_warp_cursor_to_focus();
                }
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowUpOrToWorkspaceUp => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.move_up();
                } else {
                    self.niri.layout.move_up_or_to_workspace_up();
                    self.maybe_warp_cursor_to_focus();
                }
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowLeft => {
                self.niri.layout.consume_or_expel_window_left(None);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowLeftById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.consume_or_expel_window_left(Some(&window));
                    self.maybe_warp_cursor_to_focus();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ConsumeOrExpelWindowRight => {
                self.niri.layout.consume_or_expel_window_right(None);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ConsumeOrExpelWindowRightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri
                        .layout
                        .consume_or_expel_window_right(Some(&window));
                    self.maybe_warp_cursor_to_focus();
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusColumnLeft => {
                self.niri.layout.focus_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLeftUnderMouse => {
                if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                    let ws_id = ws.id();
                    let ws = {
                        let mut workspaces = self.niri.layout.workspaces_mut();
                        workspaces.find(|ws| ws.id() == ws_id).unwrap()
                    };
                    ws.focus_left();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusColumnRight => {
                self.niri.layout.focus_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnRightUnderMouse => {
                if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                    let ws_id = ws.id();
                    let ws = {
                        let mut workspaces = self.niri.layout.workspaces_mut();
                        workspaces.find(|ws| ws.id() == ws_id).unwrap()
                    };
                    ws.focus_right();
                    self.maybe_warp_cursor_to_focus();
                    self.niri.layer_shell_on_demand_focus = None;
                    self.niri.queue_redraw(&output);
                }
            }
            Action::FocusColumnFirst => {
                self.niri.layout.focus_column_first();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLast => {
                self.niri.layout.focus_column_last();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnRightOrFirst => {
                self.niri.layout.focus_column_right_or_first();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnLeftOrLast => {
                self.niri.layout.focus_column_left_or_last();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumn(index) => {
                self.niri.layout.focus_column(index);
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    if self.niri.layout.focus_window_up_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_up();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    if self.niri.layout.focus_window_down_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_down();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnOrMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    if self.niri.layout.focus_column_left_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_left();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusColumnOrMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    if self.niri.layout.focus_column_right_or_output(&output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&output);
                    } else {
                        self.maybe_warp_cursor_to_focus();
                    }
                } else {
                    self.niri.layout.focus_right();
                    self.maybe_warp_cursor_to_focus();
                }
                self.niri.layer_shell_on_demand_focus = None;

                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDown => {
                self.niri.layout.focus_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUp => {
                self.niri.layout.focus_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrColumnLeft => {
                self.niri.layout.focus_down_or_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrColumnRight => {
                self.niri.layout.focus_down_or_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrColumnLeft => {
                self.niri.layout.focus_up_or_left();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrColumnRight => {
                self.niri.layout.focus_up_or_right();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrWorkspaceDown => {
                self.niri.layout.focus_window_or_workspace_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowOrWorkspaceUp => {
                self.niri.layout.focus_window_or_workspace_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowTop => {
                self.niri.layout.focus_window_top();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowBottom => {
                self.niri.layout.focus_window_bottom();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowDownOrTop => {
                self.niri.layout.focus_window_down_or_top();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWindowUpOrBottom => {
                self.niri.layout.focus_window_up_or_bottom();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspaceDown(focus) => {
                self.niri.layout.move_to_workspace_down(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspaceUp(focus) => {
                self.niri.layout.move_to_workspace_up(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToWorkspace(reference, focus) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    // The source output is always the active output, so if the target output is
                    // also the active output, we don't need to use move_to_output().
                    if let Some(active) = self.niri.layout.active_output() {
                        if output.as_ref() == Some(active) {
                            output = None;
                        }
                    }

                    let activate = if focus {
                        ActivateWindow::Smart
                    } else {
                        ActivateWindow::No
                    };

                    if let Some(output) = output {
                        self.niri
                            .layout
                            .move_to_output(None, &output, Some(index), activate);

                        if focus {
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&output);
                            }
                        } else {
                            self.maybe_warp_cursor_to_focus();
                        }
                    } else {
                        self.niri.layout.move_to_workspace(None, index, activate);
                        self.maybe_warp_cursor_to_focus();
                    }

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToWorkspaceById {
                window_id: id,
                reference,
                focus,
            } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    if let Some((output, index)) =
                        self.niri.find_output_and_workspace_index(reference)
                    {
                        let target_was_active = self
                            .niri
                            .layout
                            .active_output()
                            .is_some_and(|active| output.as_ref() == Some(active));

                        let activate = if focus {
                            ActivateWindow::Smart
                        } else {
                            ActivateWindow::No
                        };

                        if let Some(output) = output {
                            self.niri.layout.move_to_output(
                                Some(&window),
                                &output,
                                Some(index),
                                activate,
                            );

                            // If the active output changed (window was moved and focused).
                            #[allow(clippy::collapsible_if)]
                            if !target_was_active
                                && self.niri.layout.active_output() == Some(&output)
                            {
                                if !self.maybe_warp_cursor_to_focus_centered() {
                                    self.move_cursor_to_output(&output);
                                }
                            }
                        } else {
                            self.niri
                                .layout
                                .move_to_workspace(Some(&window), index, activate);

                            // If we focused the target window.
                            let new_focus = self.niri.layout.focus();
                            if new_focus.is_some_and(|win| win.window == window) {
                                self.maybe_warp_cursor_to_focus();
                            }
                        }

                        // FIXME: granular
                        self.niri.queue_redraw_all();
                    }
                }
            }
            Action::MoveColumnToWorkspaceDown(focus) => {
                self.niri.layout.move_column_to_workspace_down(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToWorkspaceUp(focus) => {
                self.niri.layout.move_column_to_workspace_up(focus);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveColumnToWorkspace(reference, focus) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    if let Some(active) = self.niri.layout.active_output() {
                        if output.as_ref() == Some(active) {
                            output = None;
                        }
                    }

                    if let Some(output) = output {
                        self.niri
                            .layout
                            .move_column_to_output(&output, Some(index), focus);
                        if focus && !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    } else {
                        self.niri.layout.move_column_to_workspace(index, focus);
                        if focus {
                            self.maybe_warp_cursor_to_focus();
                        }
                    }

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveColumnToIndex(idx) => {
                self.niri.layout.move_column_to_index(idx);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceDown => {
                self.niri.layout.switch_workspace_down();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceDownUnderMouse => {
                if let Some(output) = self.niri.output_under_cursor() {
                    if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
                        mon.switch_workspace_down();
                        self.maybe_warp_cursor_to_focus();
                        self.niri.layer_shell_on_demand_focus = None;
                        self.niri.queue_redraw(&output);
                    }
                }
            }
            Action::FocusWorkspaceUp => {
                self.niri.layout.switch_workspace_up();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusWorkspaceUpUnderMouse => {
                if let Some(output) = self.niri.output_under_cursor() {
                    if let Some(mon) = self.niri.layout.monitor_for_output_mut(&output) {
                        mon.switch_workspace_up();
                        self.maybe_warp_cursor_to_focus();
                        self.niri.layer_shell_on_demand_focus = None;
                        self.niri.queue_redraw(&output);
                    }
                }
            }
            Action::FocusWorkspace(reference) => {
                if let Some((mut output, index)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    if let Some(active) = self.niri.layout.active_output() {
                        if output.as_ref() == Some(active) {
                            output = None;
                        }
                    }

                    if let Some(output) = output {
                        self.niri.layout.focus_output(&output);
                        self.niri.layout.switch_workspace(index);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    } else {
                        let config = &self.niri.config;
                        if config.borrow().input.workspace_auto_back_and_forth {
                            self.niri.layout.switch_workspace_auto_back_and_forth(index);
                        } else {
                            self.niri.layout.switch_workspace(index);
                        }
                        self.maybe_warp_cursor_to_focus();
                    }
                    self.niri.layer_shell_on_demand_focus = None;

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusWorkspacePrevious => {
                self.niri.layout.switch_workspace_previous();
                self.maybe_warp_cursor_to_focus();
                self.niri.layer_shell_on_demand_focus = None;
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceDown => {
                self.niri.layout.move_workspace_down();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceUp => {
                self.niri.layout.move_workspace_up();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceToIndex(new_idx) => {
                let new_idx = new_idx.saturating_sub(1);
                self.niri.layout.move_workspace_to_idx(None, new_idx);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWorkspaceToIndexByRef { new_idx, reference } => {
                if let Some(res) = self.niri.find_output_and_workspace_index(reference) {
                    let new_idx = new_idx.saturating_sub(1);
                    self.niri.layout.move_workspace_to_idx(Some(res), new_idx);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::SetWorkspaceName(name) => {
                self.niri.layout.set_workspace_name(name, None);
            }
            Action::SetWorkspaceNameByRef { name, reference } => {
                self.niri.layout.set_workspace_name(name, Some(reference));
            }
            Action::UnsetWorkspaceName => {
                self.niri.layout.unset_workspace_name(None);
            }
            Action::UnsetWorkSpaceNameByRef(reference) => {
                self.niri.layout.unset_workspace_name(Some(reference));
            }
            Action::ConsumeWindowIntoColumn => {
                self.niri.layout.consume_into_column();
                // This does not cause immediate focus or window size change, so warping mouse to
                // focus won't do anything here.
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ExpelWindowFromColumn => {
                self.niri.layout.expel_from_column();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwapWindowRight => {
                self.niri
                    .layout
                    .swap_window_in_direction(ScrollDirection::Right);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwapWindowLeft => {
                self.niri
                    .layout
                    .swap_window_in_direction(ScrollDirection::Left);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleColumnTabbedDisplay => {
                self.niri.layout.toggle_column_tabbed_display();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SetColumnDisplay(display) => {
                self.niri.layout.set_column_display(display);
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwitchPresetColumnWidth => {
                self.niri.layout.toggle_width(true);
            }
            Action::SwitchPresetColumnWidthBack => {
                self.niri.layout.toggle_width(false);
            }
            Action::SwitchPresetWindowWidth => {
                self.niri.layout.toggle_window_width(None, true);
            }
            Action::SwitchPresetWindowWidthBack => {
                self.niri.layout.toggle_window_width(None, false);
            }
            Action::SwitchPresetWindowWidthById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_width(Some(&window), true);
                }
            }
            Action::SwitchPresetWindowWidthBackById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_width(Some(&window), false);
                }
            }
            Action::SwitchPresetWindowHeight => {
                self.niri.layout.toggle_window_height(None, true);
            }
            Action::SwitchPresetWindowHeightBack => {
                self.niri.layout.toggle_window_height(None, false);
            }
            Action::SwitchPresetWindowHeightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_height(Some(&window), true);
                }
            }
            Action::SwitchPresetWindowHeightBackById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_height(Some(&window), false);
                }
            }
            Action::CenterColumn => {
                self.niri.layout.center_column();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::CenterWindow => {
                self.niri.layout.center_window(None);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::CenterWindowById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.center_window(Some(&window));
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::CenterVisibleColumns => {
                self.niri.layout.center_visible_columns();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MaximizeColumn => {
                self.niri.layout.toggle_full_width();
            }
            Action::Maximize => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.set_maximized(&window, true);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::Unmaximize => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.set_maximized(&window, false);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleTiledLeft => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_tiled(&window, TileSide::Left);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::ToggleTiledRight => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_tiled(&window, TileSide::Right);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MaximizeWindowToEdges => {
                let focus = self.niri.layout.focus().map(|m| m.window.clone());
                if let Some(window) = focus {
                    self.niri.layout.toggle_maximized(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MaximizeWindowToEdgesById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_maximized(&window);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorPrevious => {
                if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitorNext => {
                if let Some(output) = self.niri.output_next() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::FocusMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                    self.niri.layer_shell_on_demand_focus = None;
                }
            }
            Action::MoveWindowToMonitorLeft => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_left_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_left() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorRight => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_right_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_right() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorDown => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_down_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_down() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorUp => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_up_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_up() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorPrevious => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_previous_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_previous() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitorNext => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_next_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_next() {
                    self.niri
                        .layout
                        .move_to_output(None, &output, None, ActivateWindow::Smart);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWindowToMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    if self.niri.screenshot_ui.is_open() {
                        self.move_cursor_to_output(&output);
                        self.niri.screenshot_ui.move_to_output(output);
                    } else {
                        self.niri
                            .layout
                            .move_to_output(None, &output, None, ActivateWindow::Smart);
                        self.niri.layout.focus_output(&output);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    }
                }
            }
            Action::MoveWindowToMonitorById { id, output } => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                    let window = window.map(|(_, m)| m.window.clone());

                    if let Some(window) = window {
                        let target_was_active = self
                            .niri
                            .layout
                            .active_output()
                            .is_some_and(|active| output == *active);

                        self.niri.layout.move_to_output(
                            Some(&window),
                            &output,
                            None,
                            ActivateWindow::Smart,
                        );

                        // If the active output changed (window was moved and focused).
                        #[allow(clippy::collapsible_if)]
                        if !target_was_active && self.niri.layout.active_output() == Some(&output) {
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&output);
                            }
                        }
                    }
                }
            }
            Action::MoveColumnToMonitorLeft => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_left_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_left() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorRight => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_right_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_right() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorDown => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_down_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_down() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorUp => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_up_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_up() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorPrevious => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_previous_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitorNext => {
                if let Some(current_output) = self.niri.screenshot_ui.selection_output() {
                    if let Some(target_output) = self.niri.output_next_of(current_output) {
                        self.move_cursor_to_output(&target_output);
                        self.niri.screenshot_ui.move_to_output(target_output);
                    }
                } else if let Some(output) = self.niri.output_next() {
                    self.niri.layout.move_column_to_output(&output, None, true);
                    self.niri.layout.focus_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveColumnToMonitor(output) => {
                if let Some(output) = self.niri.output_by_name_match(&output).cloned() {
                    if self.niri.screenshot_ui.is_open() {
                        self.move_cursor_to_output(&output);
                        self.niri.screenshot_ui.move_to_output(output);
                    } else {
                        self.niri.layout.move_column_to_output(&output, None, true);
                        self.niri.layout.focus_output(&output);
                        if !self.maybe_warp_cursor_to_focus_centered() {
                            self.move_cursor_to_output(&output);
                        }
                    }
                }
            }
            Action::SetColumnWidth(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_width(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_column_width(change);
                }
            }
            Action::SetWindowWidth(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_width(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_window_width(None, change);
                }
            }
            Action::SetWindowWidthById { id, change } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_width(Some(&window), change);
                }
            }
            Action::SetWindowHeight(change) => {
                if self.niri.screenshot_ui.is_open() {
                    self.niri.screenshot_ui.set_height(change);

                    // FIXME: granular
                    self.niri.queue_redraw_all();
                } else {
                    self.niri.layout.set_window_height(None, change);
                }
            }
            Action::SetWindowHeightById { id, change } => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_height(Some(&window), change);
                }
            }
            Action::ResetWindowHeight => {
                self.niri.layout.reset_window_height(None);
            }
            Action::ResetWindowHeightById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.reset_window_height(Some(&window));
                }
            }
            Action::ExpandColumnToAvailableWidth => {
                self.niri.layout.expand_column_to_available_width();
            }
            Action::ShowHotkeyOverlay => {
                if self.niri.hotkey_overlay.show() {
                    self.niri.queue_redraw_all();

                    #[cfg(feature = "dbus")]
                    self.niri.a11y_announce_hotkey_overlay();
                }
            }
            Action::MoveWorkspaceToMonitorLeft => {
                if let Some(output) = self.niri.output_left() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorRight => {
                if let Some(output) = self.niri.output_right() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorDown => {
                if let Some(output) = self.niri.output_down() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorUp => {
                if let Some(output) = self.niri.output_up() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorPrevious => {
                if let Some(output) = self.niri.output_previous() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorNext => {
                if let Some(output) = self.niri.output_next() {
                    self.niri.layout.move_workspace_to_output(&output);
                    if !self.maybe_warp_cursor_to_focus_centered() {
                        self.move_cursor_to_output(&output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitor(new_output) => {
                if let Some(new_output) = self.niri.output_by_name_match(&new_output).cloned() {
                    if self.niri.layout.move_workspace_to_output(&new_output)
                        && !self.maybe_warp_cursor_to_focus_centered()
                    {
                        self.move_cursor_to_output(&new_output);
                    }
                }
            }
            Action::MoveWorkspaceToMonitorByRef {
                output_name,
                reference,
            } => {
                if let Some((output, old_idx)) =
                    self.niri.find_output_and_workspace_index(reference)
                {
                    if let Some(new_output) = self.niri.output_by_name_match(&output_name).cloned()
                    {
                        if self.niri.layout.move_workspace_to_output_by_id(
                            old_idx,
                            output,
                            &new_output,
                        ) {
                            // Cursor warp already calls `queue_redraw_all`
                            if !self.maybe_warp_cursor_to_focus_centered() {
                                self.move_cursor_to_output(&new_output);
                            }
                        }
                    }
                }
            }
            Action::ToggleWindowFloating => {
                self.niri.layout.toggle_window_floating(None);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleWindowFloatingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.toggle_window_floating(Some(&window));
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToFloating => {
                self.niri.layout.set_window_floating(None, true);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToFloatingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_floating(Some(&window), true);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::MoveWindowToTiling => {
                self.niri.layout.set_window_floating(None, false);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveWindowToTilingById(id) => {
                let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                let window = window.map(|(_, m)| m.window.clone());
                if let Some(window) = window {
                    self.niri.layout.set_window_floating(Some(&window), false);
                    // FIXME: granular
                    self.niri.queue_redraw_all();
                }
            }
            Action::FocusFloating => {
                self.niri.layout.focus_floating();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::FocusTiling => {
                self.niri.layout.focus_tiling();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::SwitchFocusBetweenFloatingAndTiling => {
                self.niri.layout.switch_focus_floating_tiling();
                self.maybe_warp_cursor_to_focus();
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::MoveFloatingWindowById { id, x, y } => {
                let window = if let Some(id) = id {
                    let window = self.niri.layout.windows().find(|(_, m)| m.id().get() == id);
                    let window = window.map(|(_, m)| m.window.clone());
                    if window.is_none() {
                        return;
                    }
                    window
                } else {
                    None
                };

                self.niri
                    .layout
                    .move_floating_window(window.as_ref(), x, y, true);
                // FIXME: granular
                self.niri.queue_redraw_all();
            }
            Action::ToggleWindowRuleOpacity => {
                let active_window = self
                    .niri
                    .layout
                    .active_workspace_mut()
                    .and_then(|ws| ws.active_window_mut());
                if let Some(window) = active_window {
                    if window.rules().opacity.is_some_and(|o| o != 1.) {
                        window.toggle_ignore_opacity_window_rule();
                        // FIXME: granular
                        self.niri.queue_redraw_all();
                    }
                }
            }
            Action::ToggleWindowRuleOpacityById(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    if window.rules().opacity.is_some_and(|o| o != 1.) {
                        window.toggle_ignore_opacity_window_rule();
                        // FIXME: granular
                        self.niri.queue_redraw_all();
                    }
                }
            }
            Action::SetDynamicCastWindow => {
                let id = self
                    .niri
                    .layout
                    .active_workspace()
                    .and_then(|ws| ws.active_window())
                    .map(|mapped| mapped.id().get());
                if let Some(id) = id {
                    self.set_dynamic_cast_target(CastTarget::Window { id });
                }
            }
            Action::SetDynamicCastWindowById(id) => {
                let layout = &self.niri.layout;
                if layout.windows().any(|(_, mapped)| mapped.id().get() == id) {
                    self.set_dynamic_cast_target(CastTarget::Window { id });
                }
            }
            Action::SetDynamicCastMonitor(output) => {
                let output = match output {
                    None => self.niri.layout.active_output(),
                    Some(name) => self.niri.output_by_name_match(&name),
                };
                if let Some(output) = output {
                    self.set_dynamic_cast_target(CastTarget::output(output));
                }
            }
            Action::ClearDynamicCastTarget => {
                self.set_dynamic_cast_target(CastTarget::Nothing);
            }
            Action::StopCast(session_id) => {
                self.niri.stop_cast(CastSessionId::from(session_id));
            }
            Action::ToggleOverview => {
                self.niri.layout.toggle_overview();
                self.niri.queue_redraw_all();
            }
            #[cfg(feature = "xdp-gnome-screencast")]
            Action::ToggleScreenRecord => {
                self.toggle_screen_record();
            }
            #[cfg(not(feature = "xdp-gnome-screencast"))]
            Action::ToggleScreenRecord => {
                warn!("screen recording requires the xdp-gnome-screencast feature");
            }
            Action::OpenOverview => {
                if self.niri.layout.open_overview() {
                    self.niri.queue_redraw_all();
                }
            }
            Action::CloseOverview => {
                if self.niri.layout.close_overview() {
                    self.niri.queue_redraw_all();
                }
            }
            Action::ShowRunDialog => {
                // gnome-shell refuses to open the run dialog when the lockdown
                // key is set (`RunDialog.open`), and outside the unlocked user
                // session (`sessionMode.hasRunDialog`).
                if self.niri.gnome_settings.disable_command_line || self.niri.is_locked() {
                    return;
                }
                self.niri.run_dialog.open();
                self.niri.queue_redraw_all();
            }
            Action::ActivateAcceleratorGrab(action) => {
                self.niri.emit_accelerator_signal(action, true);
            }
            Action::ToggleWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    let urgent = window.is_urgent();
                    window.set_urgent(!urgent);
                }
                self.niri.queue_redraw_all();
            }
            Action::SetWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    window.set_urgent(true);
                }
                self.niri.queue_redraw_all();
            }
            Action::UnsetWindowUrgent(id) => {
                let window = self
                    .niri
                    .layout
                    .workspaces_mut()
                    .find_map(|ws| ws.windows_mut().find(|w| w.id().get() == id));
                if let Some(window) = window {
                    window.set_urgent(false);
                }
                self.niri.queue_redraw_all();
            }
            Action::LoadConfigFile(path) => {
                if let Some(watcher) = &self.niri.config_file_watcher {
                    watcher.load_config(path);
                }
            }
            Action::MruConfirm => {
                self.confirm_mru();
            }
            Action::MruCancel => {
                self.niri.cancel_mru();
            }
            Action::MruAdvance {
                direction,
                scope,
                filter,
            } => {
                if self.niri.window_mru_ui.is_open() {
                    self.niri.window_mru_ui.advance(direction, filter);
                    self.niri.queue_redraw_mru_output();
                } else if self.niri.config.borrow().recent_windows.on {
                    self.niri.mru_apply_keyboard_commit();

                    let config = self.niri.config.borrow();
                    let scope = scope.unwrap_or(self.niri.window_mru_ui.scope());

                    let mut wmru = WindowMru::new(&self.niri);
                    if !wmru.is_empty() {
                        wmru.set_scope(scope);
                        if let Some(filter) = filter {
                            wmru.set_filter(filter);
                        }

                        if let Some(output) = self.niri.layout.active_output() {
                            self.niri.window_mru_ui.open(
                                self.niri.clock.clone(),
                                wmru,
                                output.clone(),
                            );

                            // Only select the *next* window if some window (which should be the
                            // first one) is already focused. If nothing is focused, keep the first
                            // window (which is logically the "previously selected" one).
                            let keep_first = direction == MruDirection::Forward
                                && self.niri.layout.focus().is_none();
                            if !keep_first {
                                self.niri.window_mru_ui.advance(direction, None);
                            }

                            drop(config);
                            self.niri.queue_redraw_all();
                        }
                    }
                }
            }
            Action::MruCloseCurrentWindow => {
                if self.niri.window_mru_ui.is_open() {
                    if let Some(id) = self.niri.window_mru_ui.current_window_id() {
                        if let Some(w) = self.niri.find_window_by_id(id) {
                            if let Some(tl) = w.toplevel() {
                                tl.send_close();
                            }
                        }
                    }
                }
            }
            Action::MruFirst => {
                if self.niri.window_mru_ui.is_open() {
                    self.niri.window_mru_ui.first();
                    self.niri.queue_redraw_mru_output();
                }
            }
            Action::MruLast => {
                if self.niri.window_mru_ui.is_open() {
                    self.niri.window_mru_ui.last();
                    self.niri.queue_redraw_mru_output();
                }
            }
            Action::MruSetScope(scope) => {
                if self.niri.window_mru_ui.is_open() {
                    self.niri.window_mru_ui.set_scope(scope);
                    self.niri.queue_redraw_mru_output();
                }
            }
            Action::MruCycleScope => {
                if self.niri.window_mru_ui.is_open() {
                    self.niri.window_mru_ui.cycle_scope();
                    self.niri.queue_redraw_mru_output();
                }
            }
        }
    }

    /// Light up the panel button under the pointer (gnome-shell `panel_button:hover`).
    /// `pos` is the global-space pointer location. Off any button — or outside GNOME
    /// mode — clears the hover. Redraws only when the hovered button actually changed.
    fn update_panel_hover(&mut self, pos: Point<f64, Logical>) {
        let role = if self.niri.layout.is_gnome_mode() {
            self.niri.output_under(pos).and_then(|(output, p)| {
                let ws = self.niri.workspace_state_for(output);
                let output_w = output_size(output).w;
                self.niri.panel.hit_test(p, output_w, ws)
            })
        } else {
            None
        };
        if self.niri.panel.set_hovered_role(role) {
            self.niri.queue_redraw_all();
        }

        // The overview dash + search track their hovered element so they can paint its
        // hover fill (`.overview-icon:hover` / `.overview-tile:hover`). Only when the
        // overview UI is actually on screen — see `Niri::overview_ui_visible` (matches
        // the render gate); otherwise this is churn (redraws) over UI nobody sees.
        let overview_visible = self.niri.overview_ui_visible();
        // The grid is reactive only while it's open and no search is covering it.
        let grid_reactive =
            self.niri.layout.is_app_grid_open() && !self.niri.overview_search.is_active();
        // An app-icon drag grabs the pointer, so nothing under it tracks `:hover`:
        // gnome-shell's DND routes motion to the drag monitor instead of to the actors,
        // and the one thing that lights up is the show-apps button while it is offering
        // to unpin the app being dragged (`ShowAppsIcon.setDragApp`, `dash.js:236-247`,
        // called from `_onItemDragMotion`, `dash.js:447-450`).
        // An open app-folder dialog is modal over the overview, so nothing beneath it
        // tracks `:hover` either — its own view does, below.
        let folder_open = self.niri.folder_dialog.is_open();
        let (dash_hit, search_hit, grid_hit, arrow_hit) = if folder_open {
            (None, None, None, None)
        } else if let Some(drag) = &self.niri.app_drag {
            (drag.unpin.then_some(DashHit::ShowApps), None, None, None)
        } else if self.niri.panel_popover.is_open() {
            // An open menu holds a `Clutter.Grab` (`PopupMenuManager`), so motion never
            // reaches the actors beneath it — not the icon under the box, and not the
            // ones beside it either. Without this an app's own context menu lights up
            // the icon it is covering.
            //
            // The one exception is that icon itself: `popupMenu()` pins it with
            // `setForcedHighlight(true)` (`appDisplay.js:3028`) and only releases it when
            // the menu pops down (`_onMenuPoppedDown`, `appDisplay.js:3055-3058`), so the
            // menu visibly belongs to something for as long as it is up.
            match self.niri.app_menu_source.as_ref().filter(|_| {
                // Read only while an app menu is actually up: the source is left behind
                // when the menu closes rather than cleared from every close path.
                self.niri.panel_popover.is_app_menu()
            }) {
                Some(OverviewHit::Dash(hit)) => (Some(*hit), None, None, None),
                Some(OverviewHit::GridApp(i)) => (None, None, Some(*i), None),
                Some(OverviewHit::Search(hit)) => (None, Some(*hit), None, None),
                _ => (None, None, None, None),
            }
        } else if overview_visible {
            match self.niri.output_under(pos) {
                Some((output, p)) => match self.niri.layout.controls_layout_for_output(output) {
                    Some(controls) => (
                        self.niri.dash.hit_test(p, controls.dash),
                        self.niri.overview_search.hit_test(p, controls.into()),
                        grid_reactive
                            .then(|| self.niri.app_grid.hit_test(p, controls.app_display))
                            .flatten(),
                        grid_reactive
                            .then(|| self.niri.app_grid.arrow_hit(p, controls.app_display))
                            .flatten(),
                    ),
                    None => (None, None, None, None),
                },
                None => (None, None, None, None),
            }
        } else {
            (None, None, None, None)
        };
        if self.niri.dash.set_hovered(dash_hit) {
            self.niri.queue_redraw_all();
        }
        if self.niri.overview_search.set_hovered(search_hit) {
            self.niri.queue_redraw_all();
        }
        if self.niri.app_grid.set_hovered(grid_hit) {
            self.niri.queue_redraw_all();
        }
        if self.niri.app_grid.set_arrow_hovered(arrow_hit) {
            self.niri.queue_redraw_all();
        }
        if folder_open {
            let under = self
                .niri
                .output_under(pos)
                .map(|(output, p)| (output_size(output), p));
            let (view, p) = match under {
                Some((size, p)) => (Rectangle::from_size(size), Some(p)),
                None => (Rectangle::default(), None),
            };
            if self.niri.folder_dialog.set_pointer(p, view) {
                self.niri.queue_redraw_all();
            }
        }

        // Hovering a window preview in the overview's picker grows it and raises it
        // above its neighbours (`showOverlay`, `windowPreview.js:310-352`). The
        // hit is the picker slot the click would activate, so what grows is always
        // what a click would pick — falling back to the preview whose overlay the
        // pointer is on, which is what keeps the close button from fading out from
        // under a pointer that has left the slot to reach its overhanging half.
        let hovered = self
            .niri
            .layout
            .is_overview_open()
            .then(|| {
                let (output, p) = self.niri.output_under(pos)?;
                let output = output.clone();
                match self.niri.layout.window_under(&output, p) {
                    Some((window, _)) => Some(LayoutElement::id(window).clone()),
                    // `preview_overlays` already yields the layout id.
                    None => self.preview_hover_under(&output, p),
                }
            })
            .flatten();
        if self.niri.layout.set_expose_hover(hovered.as_ref()) {
            self.niri.queue_redraw_all();
        }

        // ...and its close button lightens under the pointer (`.window-close:hover`,
        // `_window-picker.scss:46-48`).
        let close_hovered = self
            .niri
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p))
            .and_then(|(output, p)| self.preview_close_under(&output, p));
        if self.niri.preview_close_hovered != close_hovered {
            self.niri.preview_close_hovered = close_hovered;
            self.niri.queue_redraw_all();
        }

        // Hovering the notification banner holds its expiry and expands the
        // banner; leaving restarts the countdown
        // (`js/ui/messageTray.js:970-1050,1102-1105`, simplified).
        if self.niri.layout.is_gnome_mode() {
            let inside = self
                .niri
                .output_under(pos)
                .is_some_and(|(output, p)| self.niri.notification_banner.pointer_inside(output, p));
            if self.niri.notification_banner.set_hovered(inside) {
                self.niri.reschedule_notification_banner_timer();
                // Hover-expand changes the banner's layout.
                self.niri.queue_redraw_all();
            }
            // The banner takes the pointer (`contents_under` suppresses the window
            // beneath it), so force the arrow — the app can no longer paint its
            // own cursor (e.g. an I-beam) under the banner.
            if inside {
                self.niri
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
            }
        }

        // A panel popover grabs input modally: no window under it receives pointer focus
        // (see `contents_under`), so the app can't set the cursor image while we're open.
        // Force the default arrow so a stale client cursor (e.g. a terminal's I-beam that was
        // showing when the popover opened) doesn't linger over the popover.
        if self.niri.panel_popover.is_open() {
            self.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());

            if let Some((output, p)) = self.niri.output_under(pos).map(|(o, p)| (o.clone(), p)) {
                // Highlight the control under the pointer (or clear it when the
                // pointer leaves the popover content).
                if self.niri.panel_popover.pointer_hover(&output, p) {
                    self.niri.queue_redraw_all();
                }
                // While a quick-settings slider is being dragged, route motion to it.
                if let Some(action) = self.niri.panel_popover.pointer_drag(&output, p) {
                    self.apply_popover_action(action);
                    self.niri.queue_redraw_all();
                }
            }
        }
    }

    fn on_pointer_motion<I: InputBackend>(&mut self, event: I::PointerMotionEvent) {
        let was_inside_hot_corner = self.niri.pointer_inside_hot_corner;
        // Any of the early returns here mean that the pointer is not inside the hot corner.
        self.niri.pointer_inside_hot_corner = false;

        // We need an output to be able to move the pointer.
        if self.niri.global_space.outputs().next().is_none() {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();

        let pointer = self.niri.seat.get_pointer().unwrap();

        let pos = pointer.current_location();

        // We have an output, so we can compute the new location and focus.
        let mut new_pos = pos + event.delta();

        // We received an event for the regular pointer, so show it now.
        self.niri.pointer_visibility = PointerVisibility::Visible;
        self.niri.tablet_cursor_location = None;

        // Check if we have an active pointer constraint.
        //
        // FIXME: ideally this should use the pointer focus with up-to-date global location.
        let mut pointer_confined = None;
        if let Some(under) = &self.niri.pointer_contents.surface {
            // No need to check if the pointer focus surface matches, because here we're checking
            // for an already-active constraint, and the constraint is deactivated when the focused
            // surface changes.
            let pos_within_surface = pos - under.1;

            let mut pointer_locked = false;
            with_pointer_constraint(&under.0, &pointer, |constraint| {
                let Some(constraint) = constraint else { return };
                if !constraint.is_active() {
                    return;
                }

                // Constraint does not apply if not within region.
                if let Some(region) = constraint.region() {
                    if !region.contains(pos_within_surface.to_i32_round()) {
                        return;
                    }
                }

                match &*constraint {
                    PointerConstraint::Locked(_locked) => {
                        pointer_locked = true;
                    }
                    PointerConstraint::Confined(confine) => {
                        pointer_confined = Some((under.clone(), confine.region().cloned()));
                    }
                }
            });

            // If the pointer is locked, only send relative motion.
            if pointer_locked {
                pointer.relative_motion(
                    self,
                    Some(under.clone()),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                // I guess a redraw to hide the tablet cursor could be nice? Doesn't matter too
                // much here I think.
                return;
            }
        }

        // Warp pointer across the screen during the spatial movement grabs.
        let spatial_grab = pointer.with_grab(|_, grab| {
            let grab = grab.as_any();
            if let Some(grab) = grab.downcast_ref::<SpatialMovementGrab>() {
                if let Some(output) = grab.view_offset_output() {
                    return Some((output.clone(), true));
                } else if let Some(output) = grab.workspace_switch_output() {
                    return Some((output.clone(), false));
                }
            } else if let Some(grab) = grab.downcast_ref::<MoveGrab>() {
                if let Some(output) = grab.view_offset_output() {
                    return Some((output.clone(), true));
                }
            }
            None
        });
        if let Some((output, horizontal)) = spatial_grab.flatten() {
            if let Some(geo) = self.niri.global_space.output_geometry(&output) {
                let geo = geo.to_f64();
                if horizontal {
                    new_pos.x = (new_pos.x - geo.loc.x).rem_euclid(geo.size.w) + geo.loc.x;
                    new_pos.y = new_pos.y.clamp(geo.loc.y, geo.loc.y + geo.size.h - 1.);
                } else {
                    new_pos.x = new_pos.x.clamp(geo.loc.x, geo.loc.x + geo.size.w - 1.);
                    new_pos.y = (new_pos.y - geo.loc.y).rem_euclid(geo.size.h) + geo.loc.y;
                }
            }
        }

        if self
            .niri
            .global_space
            .output_under(new_pos)
            .next()
            .is_none()
        {
            // We ended up outside the outputs and need to clip the movement.
            if let Some(output) = self.niri.global_space.output_under(pos).next() {
                // The pointer was previously on some output. Clip the movement against its
                // boundaries.
                let geom = self.niri.global_space.output_geometry(output).unwrap();
                new_pos.x = new_pos
                    .x
                    .clamp(geom.loc.x as f64, (geom.loc.x + geom.size.w - 1) as f64);
                new_pos.y = new_pos
                    .y
                    .clamp(geom.loc.y as f64, (geom.loc.y + geom.size.h - 1) as f64);
            } else {
                // The pointer was not on any output in the first place. Find one for it.
                // Let's do the simple thing and just put it on the first output.
                let output = self.niri.global_space.outputs().next().unwrap();
                let geom = self.niri.global_space.output_geometry(output).unwrap();
                new_pos = center(geom).to_f64();
            }
        }

        if let Some(output) = self.niri.screenshot_ui.selection_output() {
            let geom = self.niri.global_space.output_geometry(output).unwrap();
            let point = (new_pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, None);
        }

        if let Some(mru_output) = self.niri.window_mru_ui.output() {
            if let Some((output, pos_within_output)) = self.niri.output_under(new_pos) {
                if mru_output == output {
                    self.niri.window_mru_ui.pointer_motion(pos_within_output);
                }
            }
        }

        if self.niri.end_session_dialog.is_open() {
            if let Some((output, pos_within_output)) = self.niri.output_under(new_pos) {
                let output_size = output_size(output);
                if self
                    .niri
                    .end_session_dialog
                    .pointer_motion(output_size, pos_within_output)
                {
                    self.niri.queue_redraw_all();
                }
            }
        }

        // Drag first: the dash's hover feedback is derived from the drag's unpin state,
        // so computing it the other way round would paint it one motion stale.
        self.update_app_drag(new_pos);
        self.update_panel_hover(new_pos);

        let under = self.niri.contents_under(new_pos);

        // Handle confined pointer.
        if let Some((focus_surface, region)) = pointer_confined {
            let mut prevent = false;

            // Prevent the pointer from leaving the focused surface.
            if Some(&focus_surface.0) != under.surface.as_ref().map(|(s, _)| s) {
                prevent = true;
            }

            // Prevent the pointer from leaving the confine region, if any.
            if let Some(region) = region {
                let new_pos_within_surface = new_pos - focus_surface.1;
                if !region.contains(new_pos_within_surface.to_i32_round()) {
                    prevent = true;
                }
            }

            if prevent {
                pointer.relative_motion(
                    self,
                    Some(focus_surface),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                return;
            }
        }

        self.niri.handle_focus_follows_mouse(&under);

        self.niri.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface.clone(),
            &MotionEvent {
                location: new_pos,
                serial,
                time: event.time_msec(),
            },
        );

        pointer.relative_motion(
            self,
            under.surface,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                utime: event.time(),
            },
        );

        pointer.frame(self);

        // contents_under() will return no surface when the hot corner should trigger, so
        // pointer.motion() will set the current focus to None.
        if under.hot_corner && pointer.current_focus().is_none() {
            if !was_inside_hot_corner
                && pointer
                    .with_grab(|_, grab| grab_allows_hot_corner(grab))
                    .unwrap_or(true)
            {
                self.niri.layout.toggle_overview();
            }
            self.niri.pointer_inside_hot_corner = true;
        }

        // Activate a new confinement if necessary.
        self.niri.maybe_activate_pointer_constraint();

        // Inform the layout of an ongoing DnD operation.
        let is_dnd_grab = pointer
            .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
            .unwrap_or(false);
        if is_dnd_grab {
            if let Some((output, pos_within_output)) = self.niri.output_under(new_pos) {
                let output = output.clone();
                self.niri.layout.dnd_update(output, pos_within_output);
            }
        }

        // Redraw to update the cursor position.
        // FIXME: redraw only outputs overlapping the cursor.
        self.niri.queue_redraw_all();
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        let was_inside_hot_corner = self.niri.pointer_inside_hot_corner;
        // Any of the early returns here mean that the pointer is not inside the hot corner.
        self.niri.pointer_inside_hot_corner = false;

        let Some(pos) = self.compute_absolute_location(&event, None).or_else(|| {
            self.global_bounding_rectangle().map(|output_geo| {
                event.position_transformed(output_geo.size) + output_geo.loc.to_f64()
            })
        }) else {
            return;
        };

        let serial = SERIAL_COUNTER.next_serial();

        let pointer = self.niri.seat.get_pointer().unwrap();

        if let Some(output) = self.niri.screenshot_ui.selection_output() {
            let geom = self.niri.global_space.output_geometry(output).unwrap();
            let point = (pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, None);
        }

        if let Some(mru_output) = self.niri.window_mru_ui.output() {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                if mru_output == output {
                    self.niri.window_mru_ui.pointer_motion(pos_within_output);
                }
            }
        }

        if self.niri.end_session_dialog.is_open() {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                let output_size = output_size(output);
                if self
                    .niri
                    .end_session_dialog
                    .pointer_motion(output_size, pos_within_output)
                {
                    self.niri.queue_redraw_all();
                }
            }
        }

        self.update_app_drag(pos);
        self.update_panel_hover(pos);

        let under = self.niri.contents_under(pos);

        self.niri.handle_focus_follows_mouse(&under);

        self.niri.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );

        pointer.frame(self);

        // contents_under() will return no surface when the hot corner should trigger, so
        // pointer.motion() will set the current focus to None.
        if under.hot_corner && pointer.current_focus().is_none() {
            if !was_inside_hot_corner
                && pointer
                    .with_grab(|_, grab| grab_allows_hot_corner(grab))
                    .unwrap_or(true)
            {
                self.niri.layout.toggle_overview();
            }
            self.niri.pointer_inside_hot_corner = true;
        }

        self.niri.maybe_activate_pointer_constraint();

        // We moved the pointer, show it.
        self.niri.pointer_visibility = PointerVisibility::Visible;

        // We moved the regular pointer, so show it now.
        self.niri.tablet_cursor_location = None;

        // Inform the layout of an ongoing DnD operation.
        let is_dnd_grab = pointer
            .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
            .unwrap_or(false);
        if is_dnd_grab {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                let output = output.clone();
                self.niri.layout.dnd_update(output, pos_within_output);
            }
        }

        // Redraw to update the cursor position.
        // FIXME: redraw only outputs overlapping the cursor.
        self.niri.queue_redraw_all();
    }

    /// Drive an app-icon drag: start one when a press on a dash or app-grid icon
    /// has been dragged past the threshold, and follow the pointer once it has.
    ///
    /// gnome-shell makes every app icon a DND source; `St.DndStartGesture` begins
    /// the drag once the pointer leaves a `drag-threshold` box around the press
    /// (`st-dnd-start-gesture.c:73-90`), which cancels the click — that is why the
    /// icon's own activation is on the release.
    fn update_app_drag(&mut self, pos: Point<f64, Logical>) {
        let Some((output, pos_within_output)) = self
            .niri
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p))
        else {
            return;
        };

        if let Some(drag) = &mut self.niri.app_drag {
            drag.output = output;
            drag.pos = pos_within_output;
            // While the folder dialog is up it owns the drag: the grid underneath it is
            // covered and takes no target (`AppDisplay._onDragMotion` returns CONTINUE
            // when `_currentDialog`, `appDisplay.js:1658-1663`).
            if self.niri.folder_dialog.is_open() {
                self.update_folder_dialog_drag();
            } else {
                self.update_dash_drop_slot();
                self.update_grid_drag();
            }
            self.niri.queue_redraw_all();
            return;
        }

        // A page drag on the grid background, once it has cleared the threshold. The
        // pages follow the pointer, so the travel is the *negation* of it —
        // `_getGestureDirFactor` is -1 for LTR (`swipeTracker.js:689-695`), which is the
        // opposite sign to the scroll path.
        if let Some(pan) = &mut self.niri.app_grid_pan {
            if pan.output == output {
                let dx = pos_within_output.x - pan.last.x;
                pan.last = pos_within_output;
                if !pan.dragging
                    && ((pos_within_output.x - pan.origin.x).abs() > DRAG_THRESHOLD
                        || (pos_within_output.y - pan.origin.y).abs() > DRAG_THRESHOLD)
                {
                    pan.dragging = true;
                    if let Some(area) = self
                        .niri
                        .layout
                        .controls_layout_for_output(&output)
                        .map(|c| c.app_display)
                    {
                        self.niri.app_grid.gesture_begin(SwipeSource::Pointer, area);
                    }
                }
                if self.niri.app_grid_pan.as_ref().is_some_and(|p| p.dragging) {
                    let area = self
                        .niri
                        .layout
                        .controls_layout_for_output(&output)
                        .map(|c| c.app_display);
                    if let Some(area) = area {
                        let now = self.niri.clock.now_unadjusted();
                        if self.niri.app_grid.gesture_update(-dx, now, area) {
                            self.niri.queue_redraw_all();
                        }
                    }
                    return;
                }
            }
        }

        let Some((_, hit, origin)) = &self.niri.overview_pressed else {
            return;
        };
        if (pos_within_output.x - origin.x).abs() <= DRAG_THRESHOLD
            && (pos_within_output.y - origin.y).abs() <= DRAG_THRESHOLD
        {
            return;
        }

        // Only the app icons are drag sources; the show-apps button, the page
        // controls and the search card are not.
        let controls = self.niri.layout.controls_layout_for_output(&output);
        // A dragged folder carries its whole composed tile, not one member's icon
        // (`FolderIcon.getDragActor`, `appDisplay.js:2368-2379`).
        let folder = match hit {
            OverviewHit::GridApp(i) => self
                .niri
                .app_grid
                .entry_folder(*i)
                .map(|members| members.iter().map(|m| m.icon.clone()).collect()),
            _ => None,
        };
        let (id, icon, tile_center) = match hit {
            OverviewHit::Dash(DashHit::App(i)) => (
                self.niri.dash.item_id(*i).map(str::to_owned),
                self.niri.dash.item_icon(*i).cloned(),
                controls.and_then(|c| self.niri.dash.tile_center(*i, c.dash)),
            ),
            OverviewHit::GridApp(i) => (
                self.niri.app_grid.entry_id(*i).map(str::to_owned),
                self.niri.app_grid.entry_icon(*i).cloned(),
                controls.and_then(|c| self.niri.app_grid.entry_center(*i, c.app_display)),
            ),
            // A search result is a `SearchResult` actor wrapping the same `AppIcon` the
            // grid uses, so it is draggable for the same reasons (`appDisplay.js`
            // `AppSearchProvider` results are `AppIcon`s).
            OverviewHit::Search(SearchHit::Result(i)) => (
                self.niri.overview_search.result_id(*i).map(str::to_owned),
                self.niri.overview_search.result_icon(*i).cloned(),
                controls.and_then(|c| self.niri.overview_search.result_center(*i, c.into())),
            ),
            // A member of the open folder: the same `AppIcon`, living in a `FolderView`.
            OverviewHit::Folder(DialogHit::App(i)) => {
                let view = Rectangle::from_size(output_size(&output));
                (
                    self.niri.folder_dialog.entry_id(*i).map(str::to_owned),
                    self.niri.folder_dialog.entry_icon(*i).cloned(),
                    self.niri.folder_dialog.entry_center(*i, view),
                )
            }
            _ => return,
        };
        // Dragging out of a folder: the app is not in the top-level grid at all, so GNOME
        // adds a placeholder icon for the reorder to work on — destroyed if the drag ends
        // nowhere, and the real thing if it is dropped (`_ensurePlaceholder`,
        // `appDisplay.js:1434-1448,1646-1656`).
        let from_folder = matches!(hit, OverviewHit::Folder(DialogHit::App(_)))
            .then(|| self.niri.folder_dialog.folder_id().map(str::to_owned))
            .flatten();
        let (Some(id), Some(icon)) = (id, icon) else {
            return;
        };
        // A **favourite** needs the same placeholder and for the same reason: the grid
        // excludes pinned apps, so a dash icon dragged into it has nothing there to
        // reorder either (`_onDragBegin`'s second arm, `appDisplay.js:1646-1656`).
        let from_dash = self.niri.app_system.is_favorite(&id);

        // The icon keeps the point it was grabbed by: gnome-shell positions its drag
        // actor at the pointer plus the offset the press had inside it
        // (`dnd.js:257-259`), so the icon doesn't jump under the cursor.
        let grab_offset = tile_center.map_or_else(Point::default, |center| center - *origin);
        self.niri.overview_pressed = None;
        if let Some(entry) = (from_folder.is_some() || from_dash)
            .then(|| AppGridEntry {
                id: id.clone(),
                name: self.niri.app_system.lookup(&id).map_or_else(
                    || id.clone(),
                    |e: crate::app_system::AppEntry| e.name.clone(),
                ),
                icon: icon.clone(),
                folder: None,
            })
            .filter(|e| self.niri.app_grid.index_of(&e.id).is_none())
        {
            self.niri.app_grid.add_placeholder(entry);
        }
        let drag_id = id.clone();
        self.niri.app_drag = Some(AppDrag {
            id,
            icon,
            folder,
            from_folder,
            output,
            pos: pos_within_output,
            grab_offset,
            unpin: false,
        });
        // An empty dash reserves its drop target for the duration of the drag, before
        // any slot is computed — with nothing in the run there would otherwise be
        // nothing to aim at (`_onItemDragBegin`, `dash.js:410-414`).
        self.niri.dash.set_drag_active(true);
        self.niri.app_grid.set_drag_active(true);
        self.niri.folder_dialog.set_drag_active(true);
        // The live grid reflow is provisional: remember where everything was, so a drag
        // that ends nowhere puts it back (`_onDragCancelled` → `_redisplay`). The open
        // folder's view reorders on the same terms, and takes the same snapshot.
        self.niri.app_grid.begin_reorder();
        self.niri.folder_dialog.begin_reorder();
        // The tile the drag picked up scales to half and fades away where it sits, so the
        // slot it still occupies reads as empty while the drag is in flight
        // (`_onDragBegin` → `scaleAndFade`, `appDisplay.js:1930-1934`). For a drag out of
        // a folder that is the placeholder just added (`:1446`), which is the same call.
        self.niri.app_grid.set_dragged(Some(&drag_id));
        self.niri.folder_dialog.set_dragged(Some(&drag_id));
        // The drag can begin with the pointer already over the dash (picking an icon up
        // off it, or crossing the threshold inside it), and the gap has to be open by
        // then: it is what the drop reads.
        //
        // Same gate as the motion path above: a drag that *starts* inside the open folder
        // belongs to the dialog. Driving the grid here reflowed the icons behind the
        // dialog under a pointer that never left it.
        if self.niri.folder_dialog.is_open() {
            self.update_folder_dialog_drag();
        } else {
            self.update_dash_drop_slot();
            self.update_grid_drag();
        }
        self.niri.queue_redraw_all();
    }

    /// Drive the open folder dialog's side of a drag out of it: the backdrop lightens and
    /// a `POPDOWN_DIALOG_TIMEOUT` countdown starts the moment the pointer leaves the
    /// panel, and both are undone if it comes back (`handleDragOver` +
    /// `_setupDragMonitor`, `appDisplay.js:2812-2853`).
    fn update_folder_dialog_drag(&mut self) {
        let Some(drag) = &self.niri.app_drag else {
            return;
        };
        let view = Rectangle::from_size(output_size(&drag.output));
        let outside = self.niri.folder_dialog.hit_test(drag.pos, view) == Some(DialogHit::Outside);
        self.niri.folder_dialog.set_drag_outside(outside);
        if !outside {
            self.clear_folder_popdown_timer();
            self.update_folder_drop_target();
            return;
        }
        // Out over the shade the members stop rearranging, but they stay where the drag
        // has already left them — the reorder is only undone by a cancel.
        self.clear_folder_pending_move();
        if self.niri.folder_popdown_timer.is_some() {
            return;
        }
        let timer = Timer::from_duration(Duration::from_millis(POPDOWN_DIALOG_MS));
        let token = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.niri.folder_popdown_timer = None;
                if state.niri.folder_dialog.popdown() {
                    // The drag carries on over the grid, which has been ignoring it.
                    state.update_dash_drop_slot();
                    state.update_grid_drag();
                    state.niri.queue_redraw_all();
                }
                TimeoutAction::Drop
            })
            .unwrap();
        self.niri.folder_popdown_timer = Some(token);
    }

    fn clear_folder_popdown_timer(&mut self) {
        if let Some(token) = self.niri.folder_popdown_timer.take() {
            self.niri.event_loop.remove(token);
        }
    }

    /// Track where a drag inside the open folder would land among its members and arm the
    /// delayed move. `FolderView` is a `BaseAppView` like the app display, so it inherits
    /// the same `_maybeMoveItem` — including the [`DELAYED_MOVE_MS`] wait, which is what
    /// keeps a drag that merely sweeps across the folder from shuffling it.
    /// Activating a workspace by clicking it in the overview (gnome-shell's `Workspace`
    /// click rules): clicking the *active* workspace's empty area leaves the overview,
    /// clicking another one switches to it and stays.
    fn activate_overview_workspace(&mut self, output: &Output, ws_id: WorkspaceId) {
        let Some((ws_idx, _)) = self.niri.layout.find_workspace_by_id(ws_id) else {
            return;
        };

        self.niri.layout.focus_output(output);

        let gnome_mode =
            self.niri.config.borrow().layout.windowing_mode == niri_config::WindowingMode::Floating;
        let is_active = self
            .niri
            .layout
            .active_workspace()
            .is_some_and(|active| active.id() == ws_id);
        if gnome_mode && !is_active {
            self.niri.layout.switch_workspace(ws_idx);
        } else {
            self.niri.layout.toggle_overview_to_workspace(ws_idx);
        }

        // FIXME: granular.
        self.niri.queue_redraw_all();
    }

    /// The same, addressed by position along the thumbnails strip — what a click on a
    /// thumbnail resolves to once [`ThumbGrab`] has decided it was not a drag.
    pub fn activate_overview_workspace_at(&mut self, output: &Output, idx: usize) {
        let Some(ws_id) = self
            .niri
            .layout
            .monitor_for_output(output)
            .and_then(|mon| mon.workspace_at(idx))
            .map(|ws| ws.id())
        else {
            return;
        };
        self.activate_overview_workspace(output, ws_id);
    }

    fn update_folder_drop_target(&mut self) {
        let Some(drag) = &self.niri.app_drag else {
            return;
        };
        let (id, pos, output) = (drag.id.clone(), drag.pos, drag.output.clone());
        // Only a member of *this* folder reorders it. An icon dragged in from elsewhere
        // cannot be here anyway — the dialog is modal and the grid is covered.
        if drag.from_folder.as_deref() != self.niri.folder_dialog.folder_id() {
            self.clear_folder_pending_move();
            return;
        }
        let view = Rectangle::from_size(output_size(&output));
        // Page switching first, then the drop target — the same order and the same two
        // mechanisms the grid has (`_onDragMotion`, `appDisplay.js:932-959`), because a
        // switch changes which page a target would even mean.
        let area = crate::ui::folder_dialog::FolderDialog::view_area(view);
        if !self.drag_maybe_switch_page_immediately(area, pos) {
            let hint = self.niri.folder_dialog.hint_at(pos, view);
            self.niri.folder_dialog.set_hint_hovered(hint);
            match hint {
                Some(direction) => self.arm_drag_page_switch(direction),
                None => self.reset_drag_page_switch(),
            }
        }
        let Some(per_page) = self.niri.folder_dialog.items_per_page(view) else {
            self.clear_folder_pending_move();
            return;
        };
        let target = self
            .niri
            .folder_dialog
            .drop_target_at(pos, view, &id)
            // Over the body of an icon, or over the dragged icon's own slot, there is
            // nothing to reflow around. Folding a folder into a folder is not a thing
            // (`FolderView` holds no `FolderIcon`s), so `ON_ICON` here is simply dead.
            .filter(|t| t.location != DragLocation::OnIcon);
        let Some(target) = target else {
            self.clear_folder_pending_move();
            return;
        };
        if self.niri.folder_pending_move == Some((target, per_page)) {
            return; // unchanged — let the armed timer run out
        }

        self.clear_folder_pending_move();
        self.niri.folder_pending_move = Some((target, per_page));
        let timer = Timer::from_duration(Duration::from_millis(DELAYED_MOVE_MS));
        let token = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.niri.folder_move_timer = None;
                state.apply_folder_pending_move();
                TimeoutAction::Drop
            })
            .unwrap();
        self.niri.folder_move_timer = Some(token);
    }

    /// Forget any armed move inside the folder.
    fn clear_folder_pending_move(&mut self) {
        self.niri.folder_pending_move = None;
        if let Some(token) = self.niri.folder_move_timer.take() {
            self.niri.event_loop.remove(token);
        }
    }

    /// Commit the armed move — from the timer, or from the drop when it beat the timer.
    ///
    /// Goes through [`Self::clear_folder_pending_move`] so the *source* is unregistered and
    /// not merely forgotten: a one-shot the drop beat still fires, and would then commit the
    /// next drag's armed move the moment it is armed — the delayed move's whole point being
    /// that it is not immediate. (The timer path nulls the token before calling in, so this
    /// never removes a source from inside its own callback.)
    fn apply_folder_pending_move(&mut self) {
        let Some((target, per_page)) = self.niri.folder_pending_move else {
            return;
        };
        self.clear_folder_pending_move();
        let Some(id) = self.niri.app_drag.as_ref().map(|d| d.id.clone()) else {
            return;
        };
        // Escape pops the dialog down without ending the drag, so a timer armed a moment
        // earlier can land on a folder that is already shrinking.
        if !self.niri.folder_dialog.is_open() {
            return;
        }
        if self.niri.folder_dialog.move_entry(&id, target, per_page) {
            self.niri.queue_redraw_all();
        }
    }

    /// Drive the app grid's side of a drag: page switching first, then the drop target
    /// (`_onDragMotion`, `appDisplay.js:932-959` — that order, because a switch changes
    /// which page a target would even mean).
    fn update_grid_drag(&mut self) {
        let Some(drag) = &self.niri.app_drag else {
            return;
        };
        let (output, pos) = (drag.output.clone(), drag.pos);
        let area = self
            .niri
            .layout
            .controls_layout_for_output(&output)
            .map(|c| c.app_display)
            .filter(|_| self.niri.layout.is_app_grid_open());

        if let Some(area) = area {
            // Two ways to switch pages mid-drag: bumping the screen edge switches at
            // once, and hovering a preview band switches after a beat.
            if !self.drag_maybe_switch_page_immediately(area, pos) {
                let hint = self.niri.app_grid.hint_at(pos, area);
                self.niri.app_grid.set_hint_hovered(hint);
                match hint {
                    Some(direction) => self.arm_drag_page_switch(direction),
                    None => self.reset_drag_page_switch(),
                }
            }
        } else {
            self.niri.app_grid.set_hint_hovered(None);
            self.reset_drag_page_switch();
        }

        self.update_grid_drop_target();
    }

    /// The edge bump (`_dragMaybeSwitchPageImmediately`, `appDisplay.js:854-904`): a
    /// pointer within [`EDGE_BUMP_PX`] of the grid's left/right edge flips the page
    /// straight away and then repeats, latched so that *leaning* on the edge is one
    /// switch — the pointer has to come back that far inside to arm another.
    ///
    /// Disabled with more than one output, where the gesture would fight dragging onto
    /// the adjacent monitor (`appDisplay.js:856-858`).
    fn drag_maybe_switch_page_immediately(
        &mut self,
        area: Rectangle<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> bool {
        if self.niri.global_space.outputs().count() > 1 {
            return false;
        }
        let start = area.loc.x + EDGE_BUMP_PX;
        let end = area.loc.x + area.size.w - EDGE_BUMP_PX;
        if pos.x > start && pos.x < end {
            let moved_back = self
                .niri
                .grid_page_switch_overshoot
                .is_some_and(|last| (last - pos.x).abs() > EDGE_BUMP_PX);
            if moved_back {
                self.reset_drag_page_switch();
            }
            return false;
        }
        // Still sitting in the overshoot region the last bump fired from.
        if self.niri.grid_page_switch_overshoot.is_some() {
            return false;
        }

        let direction = if pos.x <= start {
            PageArrow::Prev
        } else {
            PageArrow::Next
        };
        self.step_grid_page(direction);
        self.setup_drag_page_switch_repeat(direction);
        self.niri.grid_page_switch_overshoot = Some(pos.x);
        true
    }

    /// Start the initial hover delay over a preview band; when it fires, the page flips
    /// and the repeat takes over (`_maybeSetupDragPageSwitchInitialTimeout`,
    /// `appDisplay.js:906-921`). A timer already running is left alone.
    fn arm_drag_page_switch(&mut self, direction: PageArrow) {
        if self.niri.grid_page_switch_timer.is_some() {
            return;
        }
        let timer = Timer::from_duration(Duration::from_millis(PAGE_SWITCH_INITIAL_MS));
        let token = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                // Clear the slot first: this source is mid-dispatch and about to be
                // dropped, and the repeat setup would otherwise try to remove it.
                state.niri.grid_page_switch_timer = None;
                state.step_grid_page(direction);
                state.setup_drag_page_switch_repeat(direction);
                TimeoutAction::Drop
            })
            .unwrap();
        self.niri.grid_page_switch_timer = Some(token);
    }

    /// Keep flipping the page every [`PAGE_SWITCH_REPEAT_MS`] for as long as the drag
    /// stays where it is (`_setupDragPageSwitchRepeat`, `appDisplay.js:841-852`).
    fn setup_drag_page_switch_repeat(&mut self, direction: PageArrow) {
        self.reset_drag_page_switch();
        let repeat = Duration::from_millis(PAGE_SWITCH_REPEAT_MS);
        let token = self
            .niri
            .event_loop
            .insert_source(Timer::from_duration(repeat), move |_, _, state| {
                state.step_grid_page(direction);
                TimeoutAction::ToDuration(repeat)
            })
            .unwrap();
        self.niri.grid_page_switch_timer = Some(token);
    }

    /// Cancel any armed page switch and the edge-bump latch with it
    /// (`_resetDragPageSwitch`, `appDisplay.js:827-839`).
    fn reset_drag_page_switch(&mut self) {
        if let Some(token) = self.niri.grid_page_switch_timer.take() {
            self.niri.event_loop.remove(token);
        }
        self.niri.grid_page_switch_overshoot = None;
    }

    /// Step one page in `direction`, on the output the drag is over — of the open folder's
    /// view when the dialog has the drag, of the app grid otherwise. `FolderView` inherits
    /// the whole page-switch machinery from `BaseAppView`, so a member can be dragged onto
    /// the folder's other page exactly as an app can onto the grid's.
    fn step_grid_page(&mut self, direction: PageArrow) {
        let Some(output) = self.niri.app_drag.as_ref().map(|d| d.output.clone()) else {
            return;
        };
        if self.niri.folder_dialog.is_open() {
            let view = Rectangle::from_size(output_size(&output));
            let page = self.niri.folder_dialog.current_page();
            let page = match direction {
                PageArrow::Prev => page.saturating_sub(1),
                PageArrow::Next => page + 1,
            };
            if self.niri.folder_dialog.set_page(page, view) {
                self.clear_folder_pending_move();
                self.niri.queue_redraw_all();
            }
            return;
        }
        let Some(area) = self
            .niri
            .layout
            .controls_layout_for_output(&output)
            .map(|c| c.app_display)
        else {
            return;
        };
        let page = self.niri.app_grid.current_page();
        let page = match direction {
            PageArrow::Prev => page.saturating_sub(1),
            PageArrow::Next => page + 1,
        };
        if self.niri.app_grid.set_page(page, area) {
            // The target was resolved against the page that just left.
            self.clear_grid_pending_move();
            self.niri.queue_redraw_all();
        }
    }

    /// Track where a drag would land in the app grid and arm the delayed move
    /// (`_maybeMoveItem`, `appDisplay.js:768-810`).
    ///
    /// The move is deliberately *not* immediate: the target has to hold still for
    /// [`DELAYED_MOVE_MS`] before the grid reflows around it, so sweeping across the
    /// page does not shuffle every icon on the way. A pointer that stops still has to
    /// fire it, which is why this is a real timer and not a per-frame check.
    fn update_grid_drop_target(&mut self) {
        let Some(drag) = &self.niri.app_drag else {
            return;
        };
        // The dash speaks first: while its gap is open or the unpin target is armed,
        // the drop belongs to it, exactly as the drag monitor's ordering has it.
        if drag.unpin || self.niri.dash.drop_slot().is_some() {
            self.clear_grid_pending_move();
            self.clear_grid_drop_hover();
            return;
        }
        let (id, output, pos) = (drag.id.clone(), drag.output.clone(), drag.pos);
        let Some(area) = self
            .niri
            .layout
            .controls_layout_for_output(&output)
            .map(|c| c.app_display)
        else {
            self.clear_grid_pending_move();
            self.clear_grid_drop_hover();
            return;
        };
        if !self.niri.layout.is_app_grid_open() {
            self.clear_grid_pending_move();
            self.clear_grid_drop_hover();
            return;
        }

        let per_page = self.niri.app_grid.items_per_page(area);
        let target = self.niri.app_grid.drop_target_at(pos, area, &id);

        // Resting on the *body* of another icon is a drop of its own: onto an app it
        // offers to fold the two into a folder (`AppIcon._setHoveringByDnd`,
        // `appDisplay.js:3126-3149`), onto a folder it joins it
        // (`FolderIcon._setHoveringByDnd`, `:2350-2360`). Either way the dragged item has
        // to be a plain app — both `_canAccept`s take only another `AppIcon`.
        let source_is_app = self
            .niri
            .app_grid
            .index_of(&id)
            .is_some_and(|i| self.niri.app_grid.entry_folder(i).is_none());
        let over = target
            .filter(|t| source_is_app && t.location == DragLocation::OnIcon)
            .and_then(|t| {
                let over = self.niri.app_grid.entry_id_at(t, per_page)?;
                (over != id).then(|| self.niri.app_grid.index_of(over))?
            })
            // A folder that already holds the app takes no drop (`_canAccept`, `:2391-2394`).
            .filter(|&i| {
                self.niri
                    .app_grid
                    .entry_folder(i)
                    .is_none_or(|members| members.iter().all(|m| m.id != id))
            });
        self.arm_grid_drop_hover(over);

        // Over the body of an icon, or over the dragged icon's own slot, there is
        // nothing to reflow around.
        let target = target.filter(|t| {
            t.location != DragLocation::OnIcon
                && self.niri.app_grid.entry_id_at(*t, per_page) != Some(id.as_str())
        });
        let Some(target) = target else {
            self.clear_grid_pending_move();
            return;
        };
        if self.niri.grid_pending_move == Some((target, per_page)) {
            return; // unchanged — let the armed timer run out
        }

        self.clear_grid_pending_move();
        self.niri.grid_pending_move = Some((target, per_page));
        let timer = Timer::from_duration(Duration::from_millis(DELAYED_MOVE_MS));
        let token = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.niri.grid_move_timer = None;
                state.apply_grid_pending_move();
                TimeoutAction::Drop
            })
            .unwrap();
        self.niri.grid_move_timer = Some(token);
    }

    /// Point the `:drop` state at `target`, or take it away. Moving to a different icon
    /// starts over; leaving clears it, exactly as `_setHoveringByDnd(false)` does.
    ///
    /// A **folder** lights up straight away — the drop is unambiguous. An **app** only
    /// does after [`FOLDER_PREVIEW_MS`], because there the `:drop` state doubles as the
    /// offer to make a folder, and offering that to every icon a drag merely crosses
    /// would be noise (`GLib.timeout_add_once(…, 500, …)`, `appDisplay.js:3136-3142`).
    fn arm_grid_drop_hover(&mut self, target: Option<usize>) {
        if self.niri.grid_drop_target == target {
            return; // unchanged — let any armed timer run out
        }
        self.clear_grid_drop_hover();
        let Some(target) = target else { return };
        self.niri.grid_drop_target = Some(target);
        if self.niri.app_grid.entry_folder(target).is_some() {
            if self.niri.app_grid.set_drop_hover(Some(target)) {
                self.niri.queue_redraw_all();
            }
            return;
        }
        let timer = Timer::from_duration(Duration::from_millis(FOLDER_PREVIEW_MS));
        let token = self
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.niri.grid_drop_timer = None;
                if state.niri.app_grid.set_drop_hover(Some(target)) {
                    state.niri.queue_redraw_all();
                }
                TimeoutAction::Drop
            })
            .unwrap();
        self.niri.grid_drop_timer = Some(token);
    }

    /// Take a drop that landed on the body of another icon: onto an app it folds the two
    /// into a new folder (`AppIcon.acceptDrop` → `AppDisplay.createFolder`,
    /// `appDisplay.js:3152-3160`, `:1699-1751`), onto a folder it joins that folder
    /// (`FolderIcon.acceptDrop` → `FolderView.addApp`, `:2400-2408`, `:2223-2236`).
    /// Returns whether it took the drop; `false` leaves the reorder path to handle it.
    ///
    /// The armed target is the whole test — it is only set while the pointer rests on an
    /// icon this drag can drop into — so, as in GNOME, the 500 ms preview is an
    /// affordance and not a condition: a quick drop onto an app still makes the folder.
    fn accept_grid_icon_drop(&mut self) -> bool {
        let Some(target) = self.niri.grid_drop_target else {
            return false;
        };
        let Some(drag) = &self.niri.app_drag else {
            return false;
        };
        let (source_id, output) = (drag.id.clone(), drag.output.clone());
        // An app dragged out of a folder is a *move*, not a copy: it leaves the folder it
        // came from, as it does on every other drop the app display accepts
        // (`AppDisplay.acceptDrop`'s `view.removeApp`, `appDisplay.js:1688-1691`). A
        // favourite folded or joined from the dash is unpinned for the same reason, by the
        // `removeFavorite` two lines below it (`:1693-1694`).
        //
        // Divergence: in GNOME this drop cannot happen at all. Both `AppIcon._canAccept`
        // (`:3118-3123`) and `FolderIcon._canAccept` (`:2386-2392`) require the *source's*
        // view to be an `AppDisplay`, so an icon dragged out of a folder is refused by the
        // icon under it and falls through to a plain grid reorder. We let it fold and join,
        // because the icon offers to — it takes the `:drop` state either way — and a
        // visible offer that silently does something else is worse than the divergence.
        let from_folder = drag.from_folder.clone();

        if self.niri.app_grid.entry_folder(target).is_some() {
            let Some(folder) = self.niri.app_grid.join_folder(target, &source_id) else {
                return false;
            };
            if let Some(writer) = &self.niri.gnome_settings_writer {
                writer.add_to_app_folder(&folder, &source_id);
            }
            // No `_savePages` for the join itself: `addApp` writes only the folder's
            // `apps`, and every tile the joined app left behind keeps the position it
            // already had. Emptying the *source* folder does remove a tile, though.
            let mut save = from_folder.as_deref().map(|from| {
                self.leave_folder(from, &source_id);
                output.clone()
            });
            if self.niri.app_system.is_favorite(&source_id) {
                self.unpin_app(&source_id);
                save = Some(output.clone());
            }
            self.finish_grid_icon_drop(save.as_ref());
            return true;
        }

        let Some(over_id) = self.niri.app_grid.entry_id(target).map(str::to_owned) else {
            return false;
        };

        // The hovered icon is the folder's first app, and so the one whose category list
        // is walked for the name (`createFolder` passes `[this.id, source.id]`).
        let categories = |id: &str| {
            self.niri
                .app_system
                .lookup(id)
                .map(|e| e.categories)
                .unwrap_or_default()
        };
        let name = crate::gnome::best_folder_name(&[categories(&over_id), categories(&source_id)])
            .unwrap_or_else(|| "Unnamed Folder".to_owned());

        let id = crate::gnome::new_folder_id();
        // Place it first: nothing is written unless the pair really folds, and the local
        // order is what `_savePages` then persists — which is how the folder keeps the
        // hovered icon's slot across the reload that brings it back from gsettings.
        let Some(apps) =
            self.niri
                .app_grid
                .fold_into_folder(target, &source_id, id.clone(), name.clone())
        else {
            return false;
        };
        if let Some(writer) = &self.niri.gnome_settings_writer {
            writer.create_app_folder(&id, name, apps);
        }
        if let Some(from) = &from_folder {
            self.leave_folder(from, &source_id);
        }
        if self.niri.app_system.is_favorite(&source_id) {
            self.unpin_app(&source_id);
        }
        self.finish_grid_icon_drop(Some(&output));
        true
    }

    /// Wind the drag down after [`Self::accept_grid_icon_drop`] took it, persisting the
    /// arrangement when `save` names the output whose geometry paginates it.
    fn finish_grid_icon_drop(&mut self, save: Option<&Output>) {
        self.clear_grid_pending_move();
        self.clear_grid_drop_hover();
        self.reset_drag_page_switch();
        self.niri.app_drag = None;
        self.niri.dash.set_drop_slot(None);
        self.niri.dash.set_drag_active(false);
        self.niri.app_grid.set_drag_active(false);
        self.niri.folder_dialog.set_drag_active(false);
        // …and the tile it took eases back to full size (`undoScaleAndFade`).
        self.niri.app_grid.set_dragged(None);
        self.niri.folder_dialog.set_dragged(None);
        self.niri.app_grid.finish_reorder();
        if let Some(output) = save {
            self.save_app_picker_layout(output);
        }
        self.niri.queue_redraw_all();
    }

    /// Drop the armed target, its countdown, and any `:drop` state it produced.
    fn clear_grid_drop_hover(&mut self) {
        self.niri.grid_drop_target = None;
        if let Some(token) = self.niri.grid_drop_timer.take() {
            self.niri.event_loop.remove(token);
        }
        if self.niri.app_grid.set_drop_hover(None) {
            self.niri.queue_redraw_all();
        }
    }

    /// Forget any armed move (the target left the grid, or the drag ended).
    fn clear_grid_pending_move(&mut self) {
        self.niri.grid_pending_move = None;
        if let Some(token) = self.niri.grid_move_timer.take() {
            self.niri.event_loop.remove(token);
        }
    }

    /// Commit the armed move — from the timer, or from the drop when it beat the timer
    /// (`acceptDrop`, `appDisplay.js:1014-1020`).
    ///
    /// Clears rather than forgets, for the reason [`Self::apply_folder_pending_move`]
    /// spells out: a one-shot the drop beat is still registered.
    fn apply_grid_pending_move(&mut self) {
        let Some((target, per_page)) = self.niri.grid_pending_move else {
            return;
        };
        self.clear_grid_pending_move();
        let Some(id) = self.niri.app_drag.as_ref().map(|d| d.id.clone()) else {
            return;
        };
        if self.niri.app_grid.move_entry(&id, target, per_page) {
            self.niri.queue_redraw_all();
        }
    }

    /// Open, move or close the dash's drop gap for the drag in progress
    /// (`Dash.handleDragOver`, `dash.js:860-937`), and arm the unpin target.
    ///
    /// Order matters and is GNOME's: the drag monitor tests the show-apps button
    /// *first* and clears the placeholder when it is hovered (`dash.js:441-450`), so
    /// the dash never offers to pin and to unpin at the same time.
    fn update_dash_drop_slot(&mut self) {
        let Some(drag) = &self.niri.app_drag else {
            return;
        };
        let (id, output, pos) = (drag.id.clone(), drag.output.clone(), drag.pos);
        let Some(dash_area) = self
            .niri
            .layout
            .controls_layout_for_output(&output)
            .map(|c| c.dash)
        else {
            self.niri.dash.set_drop_slot(None);
            return;
        };

        let unpin = self.niri.dash.unpin_target_at(pos, dash_area, &id);
        let slot = (!unpin)
            .then(|| self.niri.dash.drop_slot_at(pos, dash_area, &id))
            .flatten();
        self.niri.dash.set_drop_slot(slot);
        if let Some(drag) = &mut self.niri.app_drag {
            drag.unpin = unpin;
        }
    }

    /// Abandon an item drag without a drop (Escape): everything the drag put in flight is
    /// undone — the live reorder, the placeholder a drag out of a folder added, the page
    /// previews, the `:drop` state — and the icon eases back to full size where it started
    /// (`_cancelDrag`, `dnd.js:501-540`).
    fn cancel_app_drag(&mut self) {
        self.clear_folder_popdown_timer();
        self.clear_folder_pending_move();
        self.clear_grid_pending_move();
        self.clear_grid_drop_hover();
        self.reset_drag_page_switch();
        self.niri.folder_dialog.set_drag_outside(false);
        self.niri.folder_dialog.set_hint_hovered(None);
        self.niri.app_grid.set_hint_hovered(None);

        let Some(drag) = self.niri.app_drag.take() else {
            return;
        };
        self.niri.dash.set_drop_slot(None);
        self.niri.dash.set_drag_active(false);
        self.niri.app_grid.set_drag_active(false);
        self.niri.folder_dialog.set_drag_active(false);
        self.niri.app_grid.set_dragged(None);
        self.niri.folder_dialog.set_dragged(None);

        self.niri.app_grid.cancel_reorder();
        self.niri.folder_dialog.cancel_reorder();
        // The placeholder the drag added — for a folder member or for a favourite — is
        // withdrawn, exactly as it is for a drop nobody took (`_removePlaceholder`,
        // `appDisplay.js:1450-1456`).
        if drag.from_folder.is_some() || self.niri.app_system.is_favorite(&drag.id) {
            self.niri.app_grid.remove_entry(&drag.id);
        }
        self.niri.queue_redraw_all();
    }

    /// Finish an app-icon drag. A drop on a workspace — in the picker or on a
    /// thumbnail — opens the app there (`Workspace.acceptDrop`,
    /// `workspace.js:1429-1434`); anywhere else it is simply dropped.
    ///
    /// Whoever takes the drop also decides the fate of the grid's live reorder: a drag
    /// that nobody accepted is *cancelled*, and gnome-shell redisplays the grid from
    /// the saved layout (`_onDragCancelled`, `appDisplay.js:979-984`).
    fn end_app_drag(&mut self) {
        self.clear_folder_popdown_timer();
        self.niri.folder_dialog.set_drag_outside(false);
        // A drag out of the open folder is the dialog's to take while it is still up: it
        // covers the monitor, so nothing underneath ever sees the drop.
        if self.niri.folder_dialog.is_open() && self.niri.app_drag.is_some() {
            self.end_folder_dialog_drag();
            return;
        }

        // A drop on an icon that has been hovered long enough to show the folder preview
        // makes a folder of the two (`AppIcon.acceptDrop`, `appDisplay.js:3152-3160`) —
        // and takes precedence over the reorder, whose target the same pointer position
        // also resolves.
        if self.accept_grid_icon_drop() {
            return;
        }

        // A drop that beat the delayed-move timer still commits the move
        // (`acceptDrop`, `appDisplay.js:1014-1020`). Before `app_drag` is taken — that
        // is where the id comes from.
        self.apply_grid_pending_move();
        self.clear_grid_pending_move();
        self.clear_grid_drop_hover();
        self.reset_drag_page_switch();

        let Some(drag) = self.niri.app_drag.take() else {
            self.niri.app_grid.cancel_reorder();
            return;
        };
        // Whether this app came out of a folder, and its id — `drag` is consumed by the
        // workspace-launch branch below.
        let (from_folder, drag_id) = (drag.from_folder.clone(), drag.id.clone());

        // A drop *on* a preview band sends the app to that page rather than reordering
        // within this one (`acceptDrop`, `appDisplay.js:1004-1013`). Read before the
        // previews are told to slide away, since that is what makes a band a target.
        let grid_area = self
            .niri
            .layout
            .controls_layout_for_output(&drag.output)
            .map(|c| c.app_display)
            .filter(|_| self.niri.layout.is_app_grid_open());
        let hint = grid_area.and_then(|area| self.niri.app_grid.hint_at(drag.pos, area));

        // A drop on the dash pins the app there, or moves it if it was already pinned
        // (`Dash.acceptDrop`, `dash.js:942-987`). The gap that was tracking the pointer
        // *is* the drop position, so take it before clearing.
        let slot = self.niri.dash.drop_slot();
        self.niri.dash.set_drop_slot(None);
        self.niri.dash.set_drag_active(false);
        self.niri.app_grid.set_drag_active(false);
        self.niri.folder_dialog.set_drag_active(false);
        // …and the tile it took eases back to full size (`undoScaleAndFade`).
        self.niri.app_grid.set_dragged(None);
        self.niri.folder_dialog.set_dragged(None);

        if let (Some(direction), Some(area)) = (hint, grid_area) {
            self.drop_onto_page(&drag.id, direction, area);
            if let Some(folder) = &from_folder {
                self.leave_folder(folder, &drag_id);
            }
            if self.niri.app_grid.finish_reorder() || from_folder.is_some() {
                self.save_app_picker_layout(&drag.output);
            }
            self.niri.queue_redraw_all();
            return;
        }

        // A drop on the show-apps button unpins instead (`ShowAppsIcon.acceptDrop`,
        // `dash.js:256-270`). `drag.unpin` already carries `_canRemoveApp`, so this is
        // only ever reached for an app that is currently a favourite.
        // Whether the *grid* took the drop, which is what decides an app dragged out of a
        // folder: only a landing in the grid takes it out of the folder. Pinned to the
        // dash or dropped on a workspace, it stays where it was.
        let mut grid_took = false;
        let accepted = if let Some(slot) = slot {
            self.pin_dragged_app(&drag.id, slot);
            true
        } else if drag.unpin {
            self.unpin_app(&drag.id);
            true
        } else {
            // The overview's own chrome is not a workspace: gnome-shell's dash and app
            // display take their own drops (favorites reordering, folders), so a drop
            // that never left them just goes back where it came from.
            let over_chrome = self.overview_hit(&drag.output, drag.pos).is_some();
            let target = (!over_chrome)
                .then(|| self.niri.layout.drop_workspace_at(&drag.output, drag.pos))
                .flatten();

            if let Some(workspace) = target {
                // `open_new_window`, not `activate`: a drop always asks for a window
                // *here*, even for an app that is already running elsewhere.
                self.launch_app(&drag.id, LaunchMode::NewWindow, Some(workspace), "drag");
                true
            } else {
                // The app display accepts any app icon dropped anywhere inside it
                // (`_canAccept` + `handleDragOver`, `appDisplay.js:986-995`) — that is
                // what makes a reorder stick even when the pointer ended between tiles.
                grid_took = self.niri.layout.is_app_grid_open()
                    && self
                        .niri
                        .layout
                        .controls_layout_for_output(&drag.output)
                        .is_some_and(|c| c.app_display.contains(drag.pos));
                grid_took
            }
        };

        if accepted {
            if self.niri.app_grid.finish_reorder() {
                self.save_app_picker_layout(&drag.output);
            }
        } else {
            self.niri.app_grid.cancel_reorder();
        }
        // The placeholder either becomes the real icon — and the app leaves wherever it
        // was excluded from the grid *by* — or it is withdrawn (`_removePlaceholder`,
        // `appDisplay.js:1450-1456`). A folder member leaves its folder; a favourite is
        // unpinned, which is the same `AppDisplay.acceptDrop` line for both
        // (`view.removeApp` / `removeFavorite`, `:1688-1694`).
        let was_favorite = self.niri.app_system.is_favorite(&drag_id);
        if from_folder.is_some() || was_favorite {
            if grid_took {
                if let Some(folder) = &from_folder {
                    self.leave_folder(folder, &drag_id);
                }
                // Persist *before* the unpin: `commit_favorites` re-derives the grid on
                // the spot, and what it sorts from is the layout — so the arrangement
                // has to be the one we are showing, drop position included.
                self.save_app_picker_layout(&drag.output);
                if was_favorite {
                    self.unpin_app(&drag_id);
                }
            } else {
                self.niri.app_grid.remove_entry(&drag_id);
            }
        }

        self.niri.queue_redraw_all();
    }

    /// Finish a drag that ended while the folder dialog was still up.
    ///
    /// The boundary is the folder's **view**, not the panel: a drop on it is
    /// `FolderView.acceptDrop` (`appDisplay.js:2213-2221`), which keeps the new order —
    /// reordering inside a folder. A drop anywhere else has no delegate that takes it and
    /// bubbles to the dialog actor, which covers the whole monitor: `AppFolderDialog`
    /// pops down, the app leaves the folder and the grid selects it (`:2857-2865`). So
    /// releasing over the folder's *name row* takes the app out, exactly as releasing over
    /// the shade does — the panel chrome is not a drop target.
    fn end_folder_dialog_drag(&mut self) {
        // A drop that beat the delayed-move timer still commits the move, as it does in
        // the grid. Before `app_drag` is taken — that is where the id comes from.
        let over_view = self.niri.app_drag.as_ref().is_some_and(|drag| {
            let view = Rectangle::from_size(output_size(&drag.output));
            crate::ui::folder_dialog::layout(view)
                .grid_area
                .contains(drag.pos)
        });
        // A drop *on* a preview band sends the member to that page rather than reordering
        // within this one (`acceptDrop`, `appDisplay.js:1004-1013`). Read before the
        // previews are told to slide away, since that is what makes a band a target.
        let hint = self.niri.app_drag.as_ref().and_then(|drag| {
            let view = Rectangle::from_size(output_size(&drag.output));
            self.niri.folder_dialog.hint_at(drag.pos, view)
        });
        if let (Some(direction), Some(drag)) = (hint, self.niri.app_drag.as_ref()) {
            let (id, output) = (drag.id.clone(), drag.output.clone());
            self.drop_member_onto_page(&id, direction, &output);
        } else if over_view {
            self.apply_folder_pending_move();
        }
        self.clear_folder_pending_move();
        self.niri.folder_dialog.set_hint_hovered(None);
        self.reset_drag_page_switch();

        let Some(drag) = self.niri.app_drag.take() else {
            return;
        };
        self.niri.dash.set_drop_slot(None);
        self.niri.dash.set_drag_active(false);
        self.niri.app_grid.set_drag_active(false);
        self.niri.folder_dialog.set_drag_active(false);
        // …and the tile it took eases back to full size (`undoScaleAndFade`).
        self.niri.app_grid.set_dragged(None);
        self.niri.folder_dialog.set_dragged(None);

        let from_folder = drag.from_folder.clone();
        match drag.from_folder.filter(|_| !over_view) {
            Some(folder) => {
                self.niri.folder_dialog.cancel_reorder();
                self.niri.folder_dialog.popdown();
                self.leave_folder(&folder, &drag.id);
                self.niri.app_grid.finish_reorder();
                // `selectApp(appId)`: the app the drop released takes the key focus in
                // the grid it just landed in.
                let i = self.niri.app_grid.index_of(&drag.id);
                self.niri.app_grid.set_focused(i);
                self.save_app_picker_layout(&drag.output);
            }
            None => {
                self.niri.app_grid.cancel_reorder();
                if from_folder.is_some() {
                    self.niri.app_grid.remove_entry(&drag.id);
                }
                // The folder keeps whatever order the drag left, and writes it back.
                if self.niri.folder_dialog.finish_reorder() {
                    if let Some(folder) = self.niri.folder_dialog.folder_id().map(str::to_owned) {
                        let apps = self.niri.folder_dialog.member_ids();
                        if let Some(writer) = &self.niri.gnome_settings_writer {
                            writer.set_app_folder_apps(&folder, apps);
                        }
                    }
                }
            }
        }
        self.niri.queue_redraw_all();
    }

    /// Write a renamed folder's `name`, with `translate` off — the name is now a literal
    /// to show, not a `.directory` basename to look up (`_maybeUpdateFolderName`,
    /// `appDisplay.js:2650-2657`).
    fn rename_folder(&mut self, folder: &str, name: &str) {
        if let Some(writer) = &self.niri.gnome_settings_writer {
            writer.rename_app_folder(folder, name.to_owned());
        }
    }

    /// Take the dragged app out of the folder it came from (`AppDisplay.acceptDrop`'s
    /// `view.removeApp`, `appDisplay.js:1688-1691`), deleting a folder the removal
    /// emptied.
    ///
    /// Divergence: GNOME deletes the folder when its **`apps` key** empties
    /// (`:2245-2262`), which for a categories-based folder means removing one swept-in
    /// member destroys the whole folder — its `apps` was empty to begin with. We delete
    /// when the folder has no *members* left, which is the same thing for every
    /// explicit-apps folder and is what the user is actually doing.
    fn leave_folder(&mut self, folder: &str, app: &str) {
        self.niri.folder_dialog.remove_member(app);
        let left = self.niri.app_grid.remove_folder_member(folder, app);
        let Some(writer) = &self.niri.gnome_settings_writer else {
            return;
        };
        if left == Some(0) {
            writer.delete_app_folder(folder);
        } else {
            writer.remove_from_app_folder(folder, app);
        }
    }

    /// Move `id` onto the adjacent page and follow it there (`acceptDrop`'s hint branch,
    /// `appDisplay.js:1004-1013`): it appends to an existing page, and stepping past the
    /// last one makes a new page with the app first on it.
    fn drop_onto_page(&mut self, id: &str, direction: PageArrow, area: Rectangle<f64, Logical>) {
        let per_page = self.niri.app_grid.items_per_page(area);
        let n_pages = self.niri.app_grid.page_count(area);
        let current = self.niri.app_grid.current_page();
        let page = match direction {
            PageArrow::Prev => current.saturating_sub(1),
            PageArrow::Next => (current + 1).min(n_pages),
        };
        let target = crate::ui::app_grid::GridDropTarget {
            page,
            position: if page < n_pages { None } else { Some(0) },
            location: DragLocation::EmptySpace,
        };
        self.niri.app_grid.move_entry(id, target, per_page);
        // Where it actually landed: a full page pushes it on to the next one.
        let page = page.min(self.niri.app_grid.page_count(area).saturating_sub(1));
        self.niri.app_grid.set_page(page, area);
    }

    /// Send a folder member to the page a preview band leads to, and follow it there —
    /// the folder's half of `acceptDrop`'s hint branch (`appDisplay.js:1004-1013`).
    fn drop_member_onto_page(&mut self, id: &str, direction: PageArrow, output: &Output) {
        let view = Rectangle::from_size(output_size(output));
        let Some(per_page) = self.niri.folder_dialog.items_per_page(view) else {
            return;
        };
        let n_pages = self.niri.folder_dialog.page_count(view);
        let current = self.niri.folder_dialog.current_page();
        let page = match direction {
            PageArrow::Prev => current.saturating_sub(1),
            PageArrow::Next => (current + 1).min(n_pages),
        };
        let target = crate::ui::app_grid::GridDropTarget {
            page,
            position: if page < n_pages { None } else { Some(0) },
            location: DragLocation::EmptySpace,
        };
        self.niri.folder_dialog.move_entry(id, target, per_page);
        // Where it actually landed: a full page pushes it on to the next one.
        let page = page.min(self.niri.folder_dialog.page_count(view).saturating_sub(1));
        self.niri.folder_dialog.set_page(page, view);
    }

    /// Persist the grid arrangement (`AppDisplay._savePages`, `appDisplay.js:1387-1404`).
    /// The page size comes from the output the drag ended on — pagination is per-output
    /// geometry, and that is the one the user was looking at.
    fn save_app_picker_layout(&mut self, output: &Output) {
        let Some(area) = self
            .niri
            .layout
            .controls_layout_for_output(output)
            .map(|c| c.app_display)
        else {
            return;
        };
        let per_page = self.niri.app_grid.items_per_page(area);
        let pages = self.niri.app_grid.pages(per_page);
        // Step the in-memory model too. The write hops to the settings thread and only
        // comes back through `changed`, but anything that re-derives the grid in between
        // sorts from *this* map — and an app missing from it falls in after every placed
        // app, by name (`_compareItems`, `appDisplay.js:1475-1490`). That is the tail an
        // unpinned dash favourite used to snap to, since `commit_favorites` re-syncs the
        // grid synchronously, one line after the drop placed it.
        self.niri.gnome_settings.app_picker_layout = pages
            .iter()
            .enumerate()
            .flat_map(|(page, ids)| {
                ids.iter()
                    .enumerate()
                    .map(move |(i, id)| (id.clone(), (page, i as i32)))
            })
            .collect();
        if let Some(writer) = &self.niri.gnome_settings_writer {
            writer.set_app_picker_layout(pages);
        }
    }

    /// Pin `id` into the favourites at `slot`, or move it there if it is already
    /// pinned, and persist the new order (`AppFavorites.addFavoriteAtPos` /
    /// `moveFavoriteToPos`, `appFavorites.js:98-116`).
    fn pin_dragged_app(&mut self, id: &str, slot: usize) {
        // `slot` indexes the favourites *as displayed*, gap included. Removing the app
        // first shifts everything after it down one, so a move to a later slot lands one
        // short — gnome-shell gets this by counting favourites before the placeholder and
        // skipping the dragged one (`dash.js:960-970`).
        let changed = match self.niri.dash.favorite_index(id) {
            Some(from) => {
                let to = if from < slot { slot - 1 } else { slot };
                if to == from {
                    false
                } else {
                    self.niri.app_system.move_favorite_to_pos(id, to);
                    true
                }
            }
            None => self.niri.app_system.add_favorite_at_pos(id, Some(slot)),
        };
        if changed {
            self.commit_favorites();
        }
    }

    /// Unpin `id` — dropped on the dash's show-apps button, or picked from an app
    /// menu (`AppFavorites.removeFavorite`, `appFavorites.js:127-137`).
    ///
    /// GNOME follows the removal with an undo notification offering to put it back
    /// (`appFavorites.js:106`); we have no undo surface for it yet, so the removal is
    /// final. Noted here rather than in the module docs because it is one line to add
    /// once the notification's action buttons can carry a callback.
    fn unpin_app(&mut self, id: &str) {
        if self.niri.app_system.remove_favorite(id) {
            self.commit_favorites();
        }
    }

    /// Persist the favourites and re-derive every surface that shows them: an app
    /// pinned or unpinned moves between the dash and the grid.
    fn commit_favorites(&mut self) {
        if let Some(writer) = &self.niri.gnome_settings_writer {
            writer.set_favorite_apps(self.niri.app_system.favorite_ids().to_vec());
        }
        self.niri.sync_dash_favorites();
        self.niri.sync_app_grid();
        self.niri.prewarm_app_icons();
    }

    /// Which of the overview's widgets is at `pos` on `output`, if the overview
    /// chrome is on screen and reactive there. Hit order follows the paint order:
    /// dash, then the search card, then the app grid (reactive only while open and
    /// not covered by a search — the same gate the hover tracking uses).
    fn overview_hit(&self, output: &Output, pos: Point<f64, Logical>) -> Option<OverviewHit> {
        if !self.niri.overview_ui_visible() {
            return None;
        }

        // The picker's close buttons come first: they overhang their preview and
        // sit above everything else in the row.
        if let Some(window) = self.preview_close_under(output, pos) {
            return Some(OverviewHit::PreviewClose(window));
        }

        // An open folder dialog is modal: it covers the monitor, so once it is up every
        // point on the output belongs to it (`grabHelper.grab`, `appDisplay.js:2879`).
        if let Some(hit) = self
            .niri
            .folder_dialog
            .hit_test(pos, Rectangle::from_size(output_size(output)))
        {
            return Some(OverviewHit::Folder(hit));
        }

        let controls = self.niri.layout.controls_layout_for_output(output)?;

        if let Some(hit) = self.niri.dash.hit_test(pos, controls.dash) {
            return Some(OverviewHit::Dash(hit));
        }
        if let Some(hit) = self.niri.overview_search.hit_test(pos, controls.into()) {
            return Some(OverviewHit::Search(hit));
        }
        if self.niri.layout.is_app_grid_open() && !self.niri.overview_search.is_active() {
            let area = controls.app_display;
            if let Some(i) = self.niri.app_grid.hit_test(pos, area) {
                return Some(OverviewHit::GridApp(i));
            }
            if let Some(page) = self.niri.app_grid.indicator_hit(pos, area) {
                return Some(OverviewHit::GridPage(page));
            }
            if let Some(arrow) = self.niri.app_grid.arrow_hit(pos, area) {
                return Some(OverviewHit::GridArrow(arrow));
            }
        }

        None
    }

    /// The window whose preview is *still* hovered at `pos` even though the picker slot
    /// under the pointer is not it — the pointer being on the part of its close button that
    /// overhangs the slot, or within [`window_preview::hover_rect`]'s slop of it.
    ///
    /// Only previews already showing an overlay are considered, so this can only ever *hold*
    /// a hover the slot hit test started; it never steals one from a neighbour, and it cannot
    /// arm a hover from outside (where the button is not drawn and so cannot be aimed at).
    fn preview_hover_under(
        &self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<smithay::desktop::Window> {
        let mon = self.niri.layout.monitor_for_output(output)?;
        mon.preview_overlays()
            .into_iter()
            .find(|(_, preview, _)| window_preview::hover_rect(*preview).contains(pos))
            .map(|(window, _, _)| window)
    }

    /// The window whose picker close button is at `pos`, if any. Only a preview
    /// showing its overlay has one, and the button is hit-tested at full size
    /// however far the overlay has faded — the fade is 200ms of easing, and a
    /// click on a button you can see should land.
    fn preview_close_under(
        &self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<smithay::desktop::Window> {
        let mon = self.niri.layout.monitor_for_output(output)?;
        mon.preview_overlays()
            .into_iter()
            .find(|(_, preview, _)| window_preview::close_rect(*preview).contains(pos))
            .map(|(window, _, _)| window)
    }

    /// Activate an overview widget: the release half of a click on the chrome.
    /// `button` is the button that was lifted; the backgrounds (dash pill, search
    /// card, entry body) are hit-tested so they consume the click, but do nothing.
    /// Keyboard navigation of the app grid, and of an open folder's grid (which is the
    /// same widget). Returns whether the key was consumed.
    ///
    /// GNOME has no app-grid keynav code of its own: the arrows are St's spatial focus
    /// navigation over the `can_focus` icons (`st-widget.c:1932-2030`, reproduced in
    /// [`AppGrid::focus_navigate`]), and Enter/space is `St.Button`'s activation — which
    /// here means routing the focused tile through the very same
    /// [`Self::activate_overview_hit`] a click takes, so launching, opening a folder and
    /// closing the overview cannot drift between mouse and keyboard.
    ///
    /// The caller has already given the search entry its shot at the key, so an active
    /// search never reaches this.
    fn overview_grid_key(&mut self, raw: Keysym, mods: ModifiersState) -> bool {
        let folder_open = self.niri.folder_dialog.is_open();
        let grid_open =
            self.niri.layout.is_app_grid_open() && !self.niri.overview_search.is_active();
        if !folder_open && !grid_open {
            return false;
        }
        let Some(output) = self.niri.layout.active_output().cloned() else {
            return false;
        };
        let view = Rectangle::from_size(output_size(&output));
        let area = self
            .niri
            .layout
            .controls_layout_for_output(&output)
            .map(|c| c.app_display);

        // Tab walks the icons in plain child order, wrapping — a *different* traversal
        // from the arrows' spatial one (`st_widget_get_focus_chain`, `st-widget.c:2086-2103`;
        // `wrap_around` is set for Tab at `st-focus-manager.c:96-106`). With nothing
        // focused it enters the grid instead, at the first icon or, backwards, the last
        // (`overviewControls.js:464-470` — the handler that gets Tab precisely when the
        // focus manager found no group to move within).
        if matches!(raw, Keysym::Tab | Keysym::ISO_Left_Tab) {
            let forward = !mods.shift && raw != Keysym::ISO_Left_Tab;
            if folder_open {
                return self.niri.folder_dialog.focus_tab(forward, view);
            }
            let Some(area) = area else { return false };
            self.niri.app_grid.focus_tab(forward, area);
            return true;
        }

        let dir = match raw {
            Keysym::Left => Some(FocusDir::Left),
            Keysym::Right => Some(FocusDir::Right),
            Keysym::Up => Some(FocusDir::Up),
            Keysym::Down => Some(FocusDir::Down),
            _ => None,
        };
        if let Some(dir) = dir {
            // While the dialog is up it is its own focus group, so the arrows stay inside
            // it and never reach the grid behind (`appDisplay.js:2516,2788-2789`).
            if folder_open {
                return self.niri.folder_dialog.focus_navigate(dir, view);
            }
            let Some(area) = area else { return false };
            // Consume the arrow either way: a move that finds no candidate (Down on the
            // last row) is still swallowed by the grid, not passed to the window binds.
            self.niri.app_grid.focus_navigate(dir, area);
            return true;
        }

        // The paging keys are `AppDisplay`'s own (`_onKeyPressEvent`,
        // `appDisplay.js:1599-1618`). They do not move the focus — a page reached this way
        // simply leaves the ring behind on the page it was on, which is what GNOME does too
        // (`goToPage` touches no focus). While a folder is up they are swallowed rather
        // than acted on: the dialog is a modal grab over the grid, and `AppDisplay` returns
        // `EVENT_STOP` for every key while `_displayingDialog`.
        let paging = matches!(
            raw,
            Keysym::Page_Up | Keysym::Page_Down | Keysym::Home | Keysym::End
        );
        if paging {
            if folder_open {
                return true;
            }
            let Some(area) = area else { return false };
            let cur = self.niri.app_grid.current_page();
            let last = self.niri.app_grid.page_count(area).saturating_sub(1);
            let page = match raw {
                Keysym::Page_Up => cur.saturating_sub(1),
                Keysym::Page_Down => cur + 1,
                Keysym::Home => 0,
                _ => last,
            };
            self.niri.app_grid.set_page(page, area);
            return true;
        }

        if !matches!(raw, Keysym::Return | Keysym::KP_Enter | Keysym::space) {
            return false;
        }
        let hit = if folder_open {
            let Some(i) = self.niri.folder_dialog.focused() else {
                return false;
            };
            OverviewHit::Folder(DialogHit::App(i))
        } else {
            let Some(i) = self.niri.app_grid.focused() else {
                return false;
            };
            OverviewHit::GridApp(i)
        };
        self.activate_overview_hit(&output, hit, Some(MouseButton::Left));
        true
    }

    fn activate_overview_hit(
        &mut self,
        output: &Output,
        hit: OverviewHit,
        button: Option<MouseButton>,
    ) {
        let primary = button == Some(MouseButton::Left);
        // GNOME's app icons take primary and middle (`button_mask`,
        // `appDisplay.js:1854`); middle is "open a new window", which for a stopped
        // app is the same launch.
        let launches = matches!(button, Some(MouseButton::Left | MouseButton::Middle));

        match hit {
            // The close button asks the window to close, like GNOME's
            // `_deleteAll` (`windowPreview.js:218`), and leaves the overview open.
            OverviewHit::PreviewClose(window) if primary => {
                if let Some((_, mapped)) = self
                    .niri
                    .layout
                    .windows()
                    .find(|(_, mapped)| mapped.window == window)
                {
                    mapped.toplevel().send_close();
                }
            }
            // A favorite launches and closes the overview. All our apps are stopped,
            // so this is a plain `Activate` — GNOME's dash icon does `open_new_window`
            // only for a *running* app (`appDisplay.js:3060`).
            OverviewHit::Dash(DashHit::App(i)) if launches => {
                if let Some(id) = self.niri.dash.item_id(i).map(str::to_owned) {
                    self.launch_app(&id, LaunchMode::Activate, None, "dash");
                    self.niri.layout.close_overview();
                }
            }
            // The show-apps button toggles the app grid (`ShowAppsIcon`,
            // `dash.js:189-213`).
            OverviewHit::Dash(DashHit::ShowApps) if primary => {
                self.niri.layout.toggle_app_grid();
            }
            OverviewHit::Search(SearchHit::Result(i)) if launches => {
                if let Some(id) = self.niri.overview_search.result_id(i).map(str::to_owned) {
                    self.launch_app(&id, LaunchMode::Activate, None, "search");
                    self.niri.overview_search.clear();
                    self.niri.layout.close_overview();
                }
            }
            OverviewHit::Search(SearchHit::Clear) if primary => {
                self.niri.overview_search.clear();
                self.niri.sync_overview_search();
            }
            // An app inside an open folder launches exactly like a top-level one, and
            // the dialog goes down with the overview.
            OverviewHit::Folder(DialogHit::App(i)) if launches => {
                if let Some(id) = self.niri.folder_dialog.entry_id(i).map(str::to_owned) {
                    self.launch_app(&id, LaunchMode::Activate, None, "folder");
                    // The overview is going with it, which is GNOME's source-unmapped
                    // path — no shrink to watch.
                    self.niri.folder_dialog.hide();
                    self.niri.layout.close_overview();
                }
            }
            // A click that misses the panel pops the dialog down (`clickGesture`,
            // `appDisplay.js:2480-2487`); one that lands on it and hits no control is
            // simply swallowed by the modal.
            OverviewHit::Folder(DialogHit::Outside) if primary => {
                self.niri.folder_dialog.popdown();
            }
            // The edit button toggles the rename entry (`notify::checked`,
            // `appDisplay.js:2591-2596`); turning it back off commits the name.
            OverviewHit::Folder(DialogHit::Edit) if primary => {
                if let Some((folder, name)) = self.niri.folder_dialog.toggle_rename() {
                    self.rename_folder(&folder, &name);
                }
            }
            OverviewHit::Folder(DialogHit::Page(page)) if primary => {
                let view = Rectangle::from_size(output_size(output));
                self.niri.folder_dialog.set_page(page, view);
            }
            OverviewHit::Folder(DialogHit::Arrow(arrow)) if primary => {
                let view = Rectangle::from_size(output_size(output));
                self.niri.folder_dialog.step_page(arrow, view);
            }
            // An app grid tile launches the app and closes the overview
            // (`AppIcon.activate`, `appDisplay.js:3060,3077`).
            // A folder tile opens its dialog instead of launching
            // (`FolderIcon.vfunc_clicked` → `open()`, `appDisplay.js:2334-2343,2456`).
            OverviewHit::GridApp(i) if launches && self.niri.app_grid.entry_folder(i).is_some() => {
                let Some(members) = self.niri.app_grid.entry_folder(i).map(<[_]>::to_vec) else {
                    return;
                };
                let Some(id) = self.niri.app_grid.entry_id(i).map(str::to_owned) else {
                    return;
                };
                let name = self
                    .niri
                    .app_grid
                    .entry_name(i)
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.clone());
                self.niri.folder_dialog.popup(&id, &name, members);
            }
            OverviewHit::GridApp(i) if launches => {
                if let Some(id) = self.niri.app_grid.entry_id(i).map(str::to_owned) {
                    self.launch_app(&id, LaunchMode::Activate, None, "app grid");
                    self.niri.layout.close_overview();
                }
            }
            // A page-indicator dot jumps to that page; a navigation arrow steps one.
            OverviewHit::GridPage(page) if primary => {
                if let Some(controls) = self.niri.layout.controls_layout_for_output(output) {
                    self.niri.app_grid.set_page(page, controls.app_display);
                }
            }
            OverviewHit::GridArrow(arrow) if primary => {
                if let Some(controls) = self.niri.layout.controls_layout_for_output(output) {
                    let cur = self.niri.app_grid.current_page();
                    let target = match arrow {
                        PageArrow::Prev => cur.saturating_sub(1),
                        PageArrow::Next => cur + 1,
                    };
                    self.niri.app_grid.set_page(target, controls.app_display);
                }
            }
            _ => {}
        }
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let pointer = self.niri.seat.get_pointer().unwrap();

        let serial = SERIAL_COUNTER.next_serial();

        let button = event.button();

        let button_code = event.button_code();

        let button_state = event.state();

        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        // End any quick-settings volume-slider drag on button release (the press that
        // started it is suppressed below, so handle it before that early return).
        if button_state == ButtonState::Released && self.niri.panel_popover.end_drag() {
            self.niri.queue_redraw_all();
        }

        // A drag beats a click: once an icon has left the press box, the release
        // drops it rather than activating anything.
        if button_state == ButtonState::Released && self.niri.app_drag.is_some() {
            self.niri.suppressed_buttons.remove(&button_code);
            self.end_app_drag();
            return;
        }

        // The release half of a click on the overview's chrome. St.Button's click
        // gesture completes on the *release*, and only if the pointer is still on
        // the widget it was pressed on (`clutter-click-gesture.c:68-81`) — lift off
        // it (or drag away) and nothing is activated. Runs before the suppression
        // check below, which is what keeps the release from reaching clients.
        // Releasing a grid page drag settles it on a page; a press that never moved is
        // simply dropped (a click on the grid background does nothing, as in GNOME).
        if button_state == ButtonState::Released {
            if let Some(pan) = self.niri.app_grid_pan.take() {
                if pan.button == button_code {
                    self.niri.suppressed_buttons.remove(&button_code);
                    if pan.dragging {
                        let area = self
                            .niri
                            .layout
                            .controls_layout_for_output(&pan.output)
                            .map(|c| c.app_display);
                        if let Some(area) = area {
                            self.niri.app_grid.gesture_end(area);
                        }
                    }
                    self.niri.queue_redraw_all();
                    return;
                }
                self.niri.app_grid_pan = Some(pan);
            }
        }

        if button_state == ButtonState::Released {
            if let Some((code, hit, origin)) = self.niri.overview_pressed.take() {
                if code == button_code {
                    self.niri.suppressed_buttons.remove(&button_code);

                    let location = pointer.current_location();
                    let under = self
                        .niri
                        .output_under(location)
                        .map(|(o, p)| (o.clone(), p));
                    if let Some((output, pos)) = under {
                        if self.overview_hit(&output, pos).as_ref() == Some(&hit) {
                            self.activate_overview_hit(&output, hit, button);
                        }
                    }

                    self.niri.queue_redraw_all();
                    return;
                }

                self.niri.overview_pressed = Some((code, hit, origin));
            }
        }

        // Ignore release events for mouse clicks that triggered a bind.
        if self.niri.suppressed_buttons.remove(&button_code) {
            return;
        }

        let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
        let modifiers = modifiers_from_state(mods);
        let mod_down = modifiers.contains(mod_key.to_modifiers());

        if ButtonState::Pressed == button_state {
            let mut is_mru_open = false;
            if let Some(mru_output) = self.niri.window_mru_ui.output() {
                is_mru_open = true;
                if let Some(MouseButton::Left) = button {
                    let location = pointer.current_location();
                    let (output, pos_within_output) = self.niri.output_under(location).unwrap();
                    if mru_output == output {
                        let id = self.niri.window_mru_ui.pointer_motion(pos_within_output);
                        if id.is_some() {
                            self.confirm_mru();
                        } else {
                            self.niri.cancel_mru();
                        }
                    } else {
                        self.niri.cancel_mru();
                    }

                    self.niri.suppressed_buttons.insert(button_code);
                    return;
                }
            }

            // The end-session dialog is modal: a left-click activates the button under the cursor,
            // and every button is swallowed so nothing reaches the windows behind it.
            if self.niri.end_session_dialog.is_open() {
                if button == Some(MouseButton::Left) {
                    let location = pointer.current_location();
                    if let Some((output, pos_within_output)) = self.niri.output_under(location) {
                        let output_size = output_size(output);
                        match self
                            .niri
                            .end_session_dialog
                            .pointer_click(output_size, pos_within_output)
                        {
                            DialogOutcome::Handled => {}
                            DialogOutcome::Confirm => self.niri.confirm_end_session(),
                            DialogOutcome::Cancel => self.niri.cancel_end_session(),
                        }
                    }
                }

                self.niri.suppressed_buttons.insert(button_code);
                return;
            }

            // GNOME top panel + its popovers.
            if self.niri.layout.is_gnome_mode() {
                let location = pointer.current_location();
                let under = self
                    .niri
                    .output_under(location)
                    .map(|(o, p)| (o.clone(), p));

                // An open popover (dateMenu calendar, quick settings, …) grabs pointer clicks: a
                // click inside routes to it (a quick-settings tile/button returns an action we
                // apply), anywhere else dismisses it. Either way the click is consumed.
                if self.niri.panel_popover.is_open() {
                    self.niri.suppressed_buttons.insert(button_code);
                    match under {
                        Some((output, pos)) => {
                            if let Some(action) =
                                self.niri.panel_popover.pointer_click(&output, pos)
                            {
                                self.apply_popover_action(action);
                            }
                        }
                        None => self.niri.panel_popover.close(),
                    }
                    // A click may have paged the calendar month (nav arrows) —
                    // reload the events for the now-shown grid, like GNOME's
                    // per-rebuild `requestRange` (`js/ui/calendar.js:748`).
                    self.niri.sync_calendar_range();
                    self.niri.refresh_popover_calendar_events();
                    self.niri.refresh_popover_world_clocks();
                    self.niri.queue_redraw_all();
                    return;
                }

                // A shown notification banner takes left clicks inside it (close button,
                // action buttons, body-activate); clicks elsewhere pass through — banners
                // never grab (`js/ui/messageList.js:730-736`, `js/ui/messageTray.js`).
                // DIVERGENCE: only left clicks are intercepted — middle/right clicks and
                // scrolls inside the banner still reach the window under it, where
                // GNOME's banner actor would swallow them. Recorded in the plan.
                if button == Some(MouseButton::Left) {
                    if let Some((output, pos)) = &under {
                        if let Some(hit) = self.niri.notification_banner.hit_test(output, *pos) {
                            self.niri.suppressed_buttons.insert(button_code);
                            self.on_banner_hit(hit);
                            self.niri.queue_redraw_all();
                            return;
                        }
                    }
                }

                // The overview's own widgets are St.Buttons — dash icons, the show-apps
                // button, app-grid tiles and page controls, search results — and an
                // St.Button acts on *release*: its click gesture completes only when the
                // button is lifted while the pointer is still on the widget
                // (`clutter-click-gesture.c:68-81` via `st-button.c:429-435`, which does
                // not set `recognize-on-press`). So the press only *records* the target;
                // the release path above re-tests the hit and activates. A press is also
                // what a drag starts from, which is the other reason GNOME can't act here.
                //
                // *Every* button is consumed on a hit so a right/middle press over the
                // dash can't fall through to the overview's right-drag / workspace grabs
                // beneath it.
                //
                // Only when the overview UI is actually visible (`overview_ui_visible`): a lock
                // surface or the screenshot UI can be raised over a still-open overview (neither
                // closes it) and the render path hides the dash/search behind them — so without
                // the guard they'd be invisible click-eaters (and, unlike the panel intercepts
                // which route through the lock-filtered `do_action`, these launch apps directly —
                // a lock-screen bypass). GNOME sidesteps this by dropping the overview from the
                // lock/unlock session modes (`sessionMode.js`).
                if let Some(hit) = under
                    .as_ref()
                    .and_then(|(output, pos)| self.overview_hit(output, *pos))
                {
                    self.niri.suppressed_buttons.insert(button_code);

                    // ...with one exception: the context menu is the one overview
                    // gesture that fires on the *press*. gnome-shell gives each
                    // `AppIcon` a secondary-button `Clutter.ClickGesture` built with
                    // `recognize_on_press: true` (`appDisplay.js:2981-2986`), unlike the
                    // activation click it leaves on the release.
                    if button == Some(MouseButton::Right) {
                        if let Some((output, _)) = &under {
                            let output = output.clone();
                            if self.open_app_menu_for(&output, hit.clone()) {
                                self.niri.queue_redraw_all();
                                return;
                            }
                        }
                    }

                    let origin = under.as_ref().map(|(_, pos)| *pos).unwrap_or_default();
                    self.niri.overview_pressed = Some((button_code, hit, origin));
                    self.niri.queue_redraw_all();
                    return;
                }

                // A press on the app grid's *background* — no tile, dot or arrow under it
                // — may become a page drag. gnome-shell's swipe tracker puts a
                // single-point `Clutter.PanGesture` on the grid's scroll view
                // (`swipeTracker.js:383-404`), and a press that lands on an icon is taken
                // by the icon's own DND instead, which is the case handled above.
                if button == Some(MouseButton::Left)
                    && self.niri.layout.is_app_grid_open()
                    && !self.niri.overview_search.is_active()
                    && !self.niri.folder_dialog.is_open()
                {
                    let over_grid = under.as_ref().and_then(|(output, pos)| {
                        let controls = self.niri.layout.controls_layout_for_output(output)?;
                        controls
                            .app_display
                            .contains(*pos)
                            .then(|| (output.clone(), *pos))
                    });
                    if let Some((output, pos)) = over_grid {
                        self.niri.suppressed_buttons.insert(button_code);
                        self.niri.app_grid_pan = Some(crate::niri::AppGridPan {
                            button: button_code,
                            output,
                            origin: pos,
                            last: pos,
                            dragging: false,
                        });
                        return;
                    }
                }

                // A left-click on a panel button: the workspace indicator toggles the overview
                // (the mouse counterpart of the Super-tap); the clock opens the calendar popover.
                if button == Some(MouseButton::Left) {
                    if let Some((output, pos)) = under {
                        let ws = self.niri.workspace_state_for(&output);
                        let output_w = output_size(&output).w;
                        match self.niri.panel.hit_test(pos, output_w, ws) {
                            Some(crate::ui::panel::ROLE_ACTIVITIES) => {
                                self.niri.suppressed_buttons.insert(button_code);
                                self.do_action(Action::ToggleOverview, false);
                                return;
                            }
                            Some(crate::ui::panel::ROLE_DATE_MENU) => {
                                let anchor = self.niri.panel.date_menu_rect(output_w);
                                let cal = self.niri.gnome_settings.calendar;
                                let accent = self.niri.gnome_settings.accent_color;
                                let now = self.niri.clock.now_unadjusted();
                                let cards = crate::ui::notification_card::message_list_groups(
                                    &self.niri.notifications,
                                    now,
                                );
                                let opened = self.niri.panel_popover.toggle_calendar(
                                    output,
                                    anchor,
                                    cal.week_start,
                                    cal.show_week_numbers,
                                    accent,
                                    cards,
                                );
                                // Opening the message list acknowledges everything
                                // in the store, exactly once per open — never on
                                // close (`js/ui/messageList.js:1193-1199`).
                                if opened {
                                    let effects = self.niri.notifications.acknowledge_all();
                                    self.niri.apply_notification_effects(effects);
                                    // Load events for the now-open calendar's grid
                                    // (`open-state-changed` → today, `js/ui/dateMenu.js:907-915`)
                                    // and populate the section from what's cached.
                                    self.niri.sync_calendar_range();
                                    self.niri.refresh_popover_calendar_events();
                                    self.niri.refresh_popover_world_clocks();
                                }
                                self.niri.suppressed_buttons.insert(button_code);
                                self.niri.queue_redraw_all();
                                return;
                            }
                            Some(crate::ui::panel::ROLE_QUICK_SETTINGS) => {
                                let toggles = self.niri.gnome_settings.quick_toggles;
                                let anchor = self.niri.panel.quick_settings_rect(output_w);
                                let network = self.niri.system_status.network;
                                let airplane = self.niri.system_status.airplane;
                                let power = self.niri.system_status.power.clone();
                                let battery = self.niri.system_status.battery.clone();
                                let audio = self.niri.audio;
                                let sink_list = self.niri.sink_list.clone();
                                let mic = self.niri.mic;
                                let source_list = self.niri.source_list.clone();
                                let accent = self.niri.gnome_settings.accent_color;
                                self.niri.panel_popover.toggle_quick_settings(
                                    output,
                                    anchor,
                                    toggles,
                                    network,
                                    airplane,
                                    power,
                                    battery,
                                    audio,
                                    sink_list,
                                    mic,
                                    source_list,
                                    accent,
                                );
                                self.niri.suppressed_buttons.insert(button_code);
                                self.niri.queue_redraw_all();
                                return;
                            }
                            Some(crate::ui::panel::ROLE_SCREEN_RECORDING) => {
                                // Recognize on press (GNOME's `ScreenRecordingIndicator`):
                                // clicking the indicator stops the recording(s).
                                self.niri.suppressed_buttons.insert(button_code);
                                self.niri.stop_screen_recordings();
                                self.niri.queue_redraw_all();
                                return;
                            }
                            Some(crate::ui::panel::ROLE_KEYBOARD) => {
                                // Open the input-source (keyboard-layout) menu, anchored on the
                                // indicator (gnome-shell's `InputSourceIndicator` popup).
                                if let Some(anchor) = self.niri.panel.keyboard_rect(output_w) {
                                    let (items, active) = self.input_source_menu_snapshot();
                                    self.niri
                                        .panel_popover
                                        .toggle_input_sources(output, anchor, items, active);
                                }
                                self.niri.suppressed_buttons.insert(button_code);
                                self.niri.queue_redraw_all();
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            }

            if is_mru_open || self.niri.mods_with_mouse_binds.contains(&modifiers) {
                if let Some(bind) = match button {
                    Some(MouseButton::Left) => Some(Trigger::MouseLeft),
                    Some(MouseButton::Right) => Some(Trigger::MouseRight),
                    Some(MouseButton::Middle) => Some(Trigger::MouseMiddle),
                    Some(MouseButton::Back) => Some(Trigger::MouseBack),
                    Some(MouseButton::Forward) => Some(Trigger::MouseForward),
                    _ => None,
                }
                .and_then(|trigger| {
                    let config = self.niri.config.borrow();
                    let bindings =
                        make_binds_iter(&config, &mut self.niri.window_mru_ui, modifiers);
                    find_configured_bind(bindings, mod_key, trigger, mods)
                })
                .filter(|bind| {
                    !self.niri.screenshot_ui.is_open() || allowed_during_screenshot(&bind.action)
                }) {
                    self.niri.suppressed_buttons.insert(button_code);
                    self.handle_bind(bind.clone());
                    return;
                };
            }

            // We received an event for the regular pointer, so show it now.
            self.niri.pointer_visibility = PointerVisibility::Visible;
            self.niri.tablet_cursor_location = None;

            let is_overview_open = self.niri.layout.is_overview_open();

            if is_overview_open && !pointer.is_grabbed() && button == Some(MouseButton::Right) {
                if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                    let ws_id = ws.id();
                    let ws_idx = self.niri.layout.find_workspace_by_id(ws_id).unwrap().0;

                    self.niri.layout.focus_output(&output);

                    let location = pointer.current_location();
                    let start_data = PointerGrabStartData {
                        focus: None,
                        button: button_code,
                        location,
                    };
                    self.niri
                        .layout
                        .view_offset_gesture_begin(&output, Some(ws_idx), false);
                    let grab = SpatialMovementGrab::new(start_data, output, ws_id, true);
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                    self.niri
                        .cursor_manager
                        .set_cursor_image(CursorImageStatus::Named(CursorIcon::AllScroll));

                    // FIXME: granular.
                    self.niri.queue_redraw_all();
                    return;
                }
            }

            if button == Some(MouseButton::Middle) && !pointer.is_grabbed() && mod_down {
                let output_ws = if is_overview_open {
                    self.niri.workspace_under_cursor(true)
                } else {
                    // We don't want to accidentally "catch" the wrong workspace during
                    // animations.
                    self.niri.output_under_cursor().and_then(|output| {
                        let mon = self.niri.layout.monitor_for_output(&output)?;
                        Some((output, mon.active_workspace_ref()))
                    })
                };

                if let Some((output, ws)) = output_ws {
                    let ws_id = ws.id();

                    self.niri.layout.focus_output(&output);

                    let location = pointer.current_location();
                    let start_data = PointerGrabStartData {
                        focus: None,
                        button: button_code,
                        location,
                    };
                    let grab = SpatialMovementGrab::new(start_data, output, ws_id, false);
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                    self.niri
                        .cursor_manager
                        .set_cursor_image(CursorImageStatus::Named(CursorIcon::AllScroll));

                    // FIXME: granular.
                    self.niri.queue_redraw_all();

                    // Don't activate the window under the cursor to avoid unnecessary
                    // scrolling when e.g. Mod+MMB clicking on a partially off-screen window.
                    return;
                }
            }

            // A press on a strip thumbnail takes a grab, so the release can tell a
            // workspace reorder from a plain click (divergence, see `ThumbGrab`). It comes
            // before the window check because a thumbnail is drawn over the picker, and
            // after the modifier gestures above, which stay in charge of their buttons.
            if button == Some(MouseButton::Left) && !pointer.is_grabbed() && is_overview_open {
                let hit = self
                    .niri
                    .thumbnail_under(pointer.current_location())
                    .map(|(output, _, idx)| (output, idx));
                if let Some((output, idx)) = hit {
                    let start_data = PointerGrabStartData {
                        focus: None,
                        button: button_code,
                        location: pointer.current_location(),
                    };
                    let grab = ThumbGrab::new(start_data, output, idx);
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                    return;
                }
            }

            if let Some(mapped) = self.niri.window_under_cursor() {
                let window = mapped.window.clone();

                // Check if we need to start an interactive move.
                if button == Some(MouseButton::Left) && !pointer.is_grabbed() {
                    if is_overview_open || mod_down {
                        let location = pointer.current_location();

                        if !is_overview_open {
                            self.niri.layout.activate_window(&window);
                        }

                        let start_data = PointerGrabStartData {
                            focus: None,
                            button: button_code,
                            location,
                        };
                        let start_data = PointerOrTouchStartData::Pointer(start_data);
                        let icon = CursorIcon::Grabbing;
                        if let Some(grab) =
                            MoveGrab::new(self, start_data, window.clone(), false, Some(icon))
                        {
                            pointer.set_grab(self, grab, serial, Focus::Clear);

                            // Set the cursor to Grabbing right away for Mod+LMB since it doesn't
                            // do any other gesture.
                            //
                            // In the overview, we click to activate window and close the overview,
                            // in this case setting the cursor right away would be distracting.
                            if !is_overview_open {
                                self.niri
                                    .cursor_manager
                                    .set_cursor_image(CursorImageStatus::Named(icon));
                            }
                        }
                    }
                }
                // Check if we need to start an interactive resize.
                else if button == Some(MouseButton::Right) && !pointer.is_grabbed() && mod_down {
                    let location = pointer.current_location();
                    let (output, pos_within_output) = self.niri.output_under(location).unwrap();
                    let edges = self
                        .niri
                        .layout
                        .resize_edges_under(output, pos_within_output)
                        .unwrap_or(ResizeEdge::empty());

                    if !edges.is_empty() {
                        // See if we got a double resize-click gesture.
                        // FIXME: deduplicate with resize_request in xdg-shell somehow.
                        let time = get_monotonic_time();
                        let last_cell = mapped.last_interactive_resize_start();
                        let mut last = last_cell.get();
                        last_cell.set(Some((time, edges)));

                        // Floating windows don't have either of the double-resize-click
                        // gestures, so just allow it to resize.
                        if mapped.is_floating() {
                            last = None;
                            last_cell.set(None);
                        }

                        if let Some((last_time, last_edges)) = last {
                            if time.saturating_sub(last_time) <= DOUBLE_CLICK_TIME {
                                // Allow quick resize after a triple click.
                                last_cell.set(None);

                                let intersection = edges.intersection(last_edges);
                                if intersection.intersects(ResizeEdge::LEFT_RIGHT) {
                                    // FIXME: don't activate once we can pass specific windows
                                    // to actions.
                                    self.niri.layout.activate_window(&window);
                                    self.niri.layout.toggle_full_width();
                                }
                                if intersection.intersects(ResizeEdge::TOP_BOTTOM) {
                                    self.niri.layout.activate_window(&window);
                                    self.niri.layout.reset_window_height(Some(&window));
                                }
                                // FIXME: granular.
                                self.niri.queue_redraw_all();
                                return;
                            }
                        }

                        self.niri.layout.activate_window(&window);

                        if self
                            .niri
                            .layout
                            .interactive_resize_begin(window.clone(), edges)
                        {
                            let start_data = PointerGrabStartData {
                                focus: None,
                                button: button_code,
                                location,
                            };
                            let grab = ResizeGrab::new(start_data, window.clone());
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            self.niri
                                .cursor_manager
                                .set_cursor_image(CursorImageStatus::Named(edges.cursor_icon()));
                        }
                    }
                }

                if !is_overview_open {
                    self.niri.layout.activate_window(&window);
                }

                // FIXME: granular.
                self.niri.queue_redraw_all();
            } else if let Some((output, ws)) = is_overview_open
                .then(|| {
                    // A strip thumbnail counts as its workspace: gnome-shell's
                    // WorkspaceThumbnail.activate has the same click rules.
                    self.niri
                        .thumbnail_workspace_under_cursor()
                        .or_else(|| self.niri.workspace_under_cursor(false))
                })
                .flatten()
            {
                let ws_id = ws.id();
                self.activate_overview_workspace(&output, ws_id);
            } else if let Some(output) = self.niri.output_under_cursor() {
                self.niri.layout.focus_output(&output);

                // FIXME: granular.
                self.niri.queue_redraw_all();
            }
        };

        self.update_pointer_contents();

        if ButtonState::Pressed == button_state {
            // A button press is a real user interaction: advance the
            // focus-stealing-prevention clocks. This runs after
            // click-to-focus, so the stamp lands on the newly focused window.
            let now = get_monotonic_time();
            self.niri.last_user_action_time = Some(now);
            if let Some(mapped) = self.niri.layout.focus_mut() {
                mapped.bump_user_time(now);
            }

            let layer_under = self.niri.pointer_contents.layer.clone();
            self.niri.focus_layer_surface_if_on_demand(layer_under);
        }

        if button == Some(MouseButton::Left) && self.niri.screenshot_ui.is_open() {
            if button_state == ButtonState::Pressed {
                let pos = pointer.current_location();

                // If we'll be moving the existing selection, use the selection output.
                let output = if mod_down {
                    self.niri.screenshot_ui.selection_output()
                } else {
                    self.niri.output_under(pos).map(|(out, _)| out)
                };

                if let Some(output) = output.cloned() {
                    let geom = self.niri.global_space.output_geometry(&output).unwrap();
                    let point = (pos - geom.loc.to_f64())
                        .to_physical(output.current_scale().fractional_scale())
                        .to_i32_round();

                    if self
                        .niri
                        .screenshot_ui
                        .pointer_down(output, point, None, mod_down)
                    {
                        self.niri.queue_redraw_all();
                    }
                }
            } else if let Some(capture) = self.niri.screenshot_ui.pointer_up(None) {
                if capture {
                    self.confirm_screenshot(true);
                } else {
                    self.niri.queue_redraw_all();
                }
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button: button_code,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let pointer = &self.niri.seat.get_pointer().unwrap();

        let source = event.source();

        let mod_key = self.backend.mod_key(&self.niri.config.borrow());

        // We received an event for the regular pointer, so show it now. This is also needed for
        // update_pointer_contents() below to return the real contents, necessary for the pointer
        // axis event to reach the window.
        self.niri.pointer_visibility = PointerVisibility::Visible;
        self.niri.tablet_cursor_location = None;

        let timestamp = Duration::from_micros(event.time());

        let horizontal_amount_v120 = event.amount_v120(Axis::Horizontal);
        let vertical_amount_v120 = event.amount_v120(Axis::Vertical);

        let is_overview_open = self.niri.layout.is_overview_open();

        // We should only handle scrolling in the overview if the pointer is not over a (top or
        // overlay) layer surface.
        let should_handle_in_overview = if is_overview_open {
            // FIXME: ideally this should happen after updating the pointer contents, which happens
            // below. However, our pointer actions are supposed to act on the old surface, before
            // updating the pointer contents.
            pointer
                .current_focus()
                .map(|surface| self.niri.find_root_shell_surface(&surface))
                .is_none_or(|root| {
                    !self
                        .niri
                        .mapped_layer_surfaces
                        .keys()
                        .any(|layer| *layer.wl_surface() == root)
                })
        } else {
            false
        };

        let is_mru_open = self.niri.window_mru_ui.is_open();

        // A scroll OVER an open panel popover is grabbed by it: the dateMenu
        // scrolls its message list (or pages the calendar month), gnome-shell's
        // `St.ScrollView` / `Calendar.vfunc_scroll_event`. Consume it so it
        // never leaks to workspace switching underneath. A scroll elsewhere
        // (the panel indicators, a window) still falls through to the handlers
        // below. Wheel notches move a fixed step; touchpad pixels pass as-is.
        if self.niri.panel_popover.is_open() {
            let location = pointer.current_location();
            let target = self
                .niri
                .output_under(location)
                .map(|(output, pos)| (output.clone(), pos));
            if let Some((output, pos)) = target {
                if self.niri.panel_popover.contains(&output, pos) {
                    // ~60 px per wheel notch (120 v120 units), else touchpad pixels.
                    let step = vertical_amount_v120
                        .map(|v| v / 120. * 60.)
                        .or_else(|| event.amount(Axis::Vertical))
                        .unwrap_or(0.);
                    if self.niri.panel_popover.pointer_scroll(&output, pos, step) {
                        // A scroll over the calendar column pages the month —
                        // reload events for the new grid (`js/ui/calendar.js:748`).
                        self.niri.sync_calendar_range();
                        self.niri.refresh_popover_calendar_events();
                        self.niri.refresh_popover_world_clocks();
                        self.niri.queue_redraw_all();
                    }
                    return;
                }
            }
        }

        // A scroll over the open app grid pages it (gnome-shell's grid
        // `scroll-event`, `appDisplay.js:658`). Consume every scroll over the grid so
        // it can't leak to workspace switching; only discrete wheel notches flip a
        // page, debounced so a fast spin doesn't fly through (`SCROLL_TIMEOUT_TIME`).
        // Touchpad/continuous scrolling is reserved for the deferred 1:1 swipe, so it
        // is consumed here but does nothing.
        if is_overview_open
            && should_handle_in_overview
            && self.niri.layout.is_app_grid_open()
            && !self.niri.overview_search.is_active()
        {
            let target = self
                .niri
                .output_under(pointer.current_location())
                .map(|(output, pos)| (output.clone(), pos));
            if let Some((output, pos)) = target {
                if let Some(controls) = self.niri.layout.controls_layout_for_output(&output) {
                    let area = controls.app_display;
                    if area.contains(pos) {
                        if source == AxisSource::Wheel {
                            let v = vertical_amount_v120
                                .or(horizontal_amount_v120)
                                .unwrap_or(0.);
                            let debounced = self.niri.app_grid_last_page_flip.is_some_and(|t| {
                                timestamp.saturating_sub(t) < Duration::from_millis(150)
                            });
                            if v != 0. && !debounced {
                                let n = self.niri.app_grid.page_count(area);
                                let cur = self.niri.app_grid.current_page();
                                let target_page = if v > 0. {
                                    (cur + 1).min(n.saturating_sub(1))
                                } else {
                                    cur.saturating_sub(1)
                                };
                                if self.niri.app_grid.set_page(target_page, area) {
                                    self.niri.app_grid_last_page_flip = Some(timestamp);
                                    self.niri.queue_redraw_all();
                                }
                            }
                        } else {
                            // Continuous scrolling is the 1:1 page swipe. GNOME's tracker
                            // is HORIZONTAL, so only sideways travel moves the pages — a
                            // two-finger vertical scroll over the grid does nothing, and
                            // is swallowed all the same.
                            let dx = event.amount(Axis::Horizontal).unwrap_or(0.);
                            let dy = event.amount(Axis::Vertical).unwrap_or(0.);
                            let action = self.niri.app_grid_scroll_swipe.update(dx, dy);
                            let mut redraw = false;
                            if action.end() {
                                redraw |= self.niri.app_grid.gesture_end(area);
                            } else {
                                if action.begin() {
                                    self.niri
                                        .app_grid
                                        .gesture_begin(SwipeSource::Touchpad, area);
                                }
                                redraw |= self.niri.app_grid.gesture_update(
                                    dx * crate::ui::app_grid::SWIPE_SCROLL_MULTIPLIER,
                                    timestamp,
                                    area,
                                );
                            }
                            if redraw {
                                self.niri.queue_redraw_all();
                            }
                        }
                        // Consume all scroll over the grid.
                        return;
                    }
                }
            }
        }

        // A scroll that did not reach the grid ends any swipe it had going — the pointer
        // left the band, or the grid closed under it.
        if self.niri.app_grid_scroll_swipe.reset() {
            let area = self
                .niri
                .layout
                .active_output()
                .cloned()
                .and_then(|o| self.niri.layout.controls_layout_for_output(&o))
                .map(|c| c.app_display);
            if let Some(area) = area {
                if self.niri.app_grid.gesture_cancel(area) {
                    self.niri.queue_redraw_all();
                }
            }
        }

        // GNOME top panel: a wheel scroll over the workspace indicator switches
        // workspaces (gnome-shell handleWorkspaceScroll). Handle it before the
        // generic wheel binds so it works with no modifier, and consume the event.
        if source == AxisSource::Wheel && self.niri.layout.is_gnome_mode() {
            let location = pointer.current_location();
            let over_indicator = self
                .niri
                .output_under(location)
                .map(|(output, pos)| {
                    let ws = self.niri.workspace_state_for(output);
                    let output_w = output_size(output).w;
                    self.niri.panel.hit_test(pos, output_w, ws)
                        == Some(crate::ui::panel::ROLE_ACTIVITIES)
                })
                .unwrap_or(false);
            if over_indicator {
                let vertical = vertical_amount_v120.unwrap_or(0.);
                let ticks = self.niri.vertical_wheel_tracker.accumulate(vertical);
                if ticks > 0 {
                    for _ in 0..ticks {
                        self.do_action(Action::FocusWorkspaceDownUnderMouse, false);
                    }
                } else if ticks < 0 {
                    for _ in ticks..0 {
                        self.do_action(Action::FocusWorkspaceUpUnderMouse, false);
                    }
                }
                return;
            }

            // Scroll over the quick-settings indicator adjusts the default sink's
            // volume, like gnome-shell's output indicator (±2% per tick, up = louder).
            #[cfg(feature = "pipewire")]
            {
                let over_qs = self
                    .niri
                    .output_under(location)
                    .map(|(output, pos)| {
                        let ws = self.niri.workspace_state_for(output);
                        let output_w = output_size(output).w;
                        self.niri.panel.hit_test(pos, output_w, ws)
                            == Some(crate::ui::panel::ROLE_QUICK_SETTINGS)
                    })
                    .unwrap_or(false);
                if over_qs {
                    let vertical = vertical_amount_v120.unwrap_or(0.);
                    let ticks = self.niri.vertical_wheel_tracker.accumulate(vertical);
                    if ticks != 0 {
                        let delta = -(ticks as f64) * crate::audio::SCROLL_STEP;
                        let new = self
                            .niri
                            .pw_audio
                            .as_ref()
                            .and_then(|pw| pw.adjust_volume(delta));
                        if let Some(status) = new {
                            self.on_audio_status(Some(status));
                        }
                    }
                    return;
                }
            }
        }

        // Handle wheel scroll bindings.
        if source == AxisSource::Wheel {
            // If we have a scroll bind with current modifiers, then accumulate and don't pass to
            // Wayland. If there's no bind, reset the accumulator.
            let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
            let modifiers = modifiers_from_state(mods);
            let should_handle = should_handle_in_overview
                || is_mru_open
                || self.niri.mods_with_wheel_binds.contains(&modifiers);
            if should_handle {
                let horizontal = horizontal_amount_v120.unwrap_or(0.);
                let ticks = self.niri.horizontal_wheel_tracker.accumulate(horizontal);
                if ticks != 0 {
                    let (bind_left, bind_right) =
                        if should_handle_in_overview && modifiers.is_empty() {
                            // In GNOME windowing mode the overview workspaces
                            // form a horizontal row: horizontal wheel scrolls
                            // through them (gnome-shell handleWorkspaceScroll).
                            let gnome_mode = self.niri.config.borrow().layout.windowing_mode
                                == niri_config::WindowingMode::Floating;
                            let (action_left, action_right, cooldown) = if gnome_mode {
                                (
                                    Action::FocusWorkspaceUpUnderMouse,
                                    Action::FocusWorkspaceDownUnderMouse,
                                    Some(Duration::from_millis(50)),
                                )
                            } else {
                                (
                                    Action::FocusColumnLeftUnderMouse,
                                    Action::FocusColumnRightUnderMouse,
                                    None,
                                )
                            };
                            let bind_left = Some(Bind {
                                key: Key {
                                    trigger: Trigger::WheelScrollLeft,
                                    modifiers: Modifiers::empty(),
                                },
                                action: action_left,
                                repeat: true,
                                cooldown,
                                allow_when_locked: false,
                                allow_inhibiting: false,
                                hotkey_overlay_title: None,
                            });
                            let bind_right = Some(Bind {
                                key: Key {
                                    trigger: Trigger::WheelScrollRight,
                                    modifiers: Modifiers::empty(),
                                },
                                action: action_right,
                                repeat: true,
                                cooldown,
                                allow_when_locked: false,
                                allow_inhibiting: false,
                                hotkey_overlay_title: None,
                            });
                            (bind_left, bind_right)
                        } else {
                            let config = self.niri.config.borrow();
                            let bindings =
                                make_binds_iter(&config, &mut self.niri.window_mru_ui, modifiers);
                            let bind_left = find_configured_bind(
                                bindings.clone(),
                                mod_key,
                                Trigger::WheelScrollLeft,
                                mods,
                            )
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                            let bind_right = find_configured_bind(
                                bindings,
                                mod_key,
                                Trigger::WheelScrollRight,
                                mods,
                            )
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                            (bind_left, bind_right)
                        };

                    if let Some(right) = bind_right {
                        for _ in 0..ticks {
                            self.handle_bind(right.clone());
                        }
                    }
                    if let Some(left) = bind_left {
                        for _ in ticks..0 {
                            self.handle_bind(left.clone());
                        }
                    }
                }

                let vertical = vertical_amount_v120.unwrap_or(0.);
                let ticks = self.niri.vertical_wheel_tracker.accumulate(vertical);
                if ticks != 0 {
                    let (bind_up, bind_down) = if should_handle_in_overview && modifiers.is_empty()
                    {
                        let bind_up = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollUp,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusWorkspaceUpUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            allow_inhibiting: false,
                            hotkey_overlay_title: None,
                        });
                        let bind_down = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollDown,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusWorkspaceDownUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            allow_inhibiting: false,
                            hotkey_overlay_title: None,
                        });
                        (bind_up, bind_down)
                    } else if should_handle_in_overview && modifiers == Modifiers::SHIFT {
                        let bind_up = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollUp,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusColumnLeftUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            allow_inhibiting: false,
                            hotkey_overlay_title: None,
                        });
                        let bind_down = Some(Bind {
                            key: Key {
                                trigger: Trigger::WheelScrollDown,
                                modifiers: Modifiers::empty(),
                            },
                            action: Action::FocusColumnRightUnderMouse,
                            repeat: true,
                            cooldown: Some(Duration::from_millis(50)),
                            allow_when_locked: false,
                            allow_inhibiting: false,
                            hotkey_overlay_title: None,
                        });
                        (bind_up, bind_down)
                    } else {
                        let config = self.niri.config.borrow();
                        let bindings =
                            make_binds_iter(&config, &mut self.niri.window_mru_ui, modifiers);
                        let bind_up = find_configured_bind(
                            bindings.clone(),
                            mod_key,
                            Trigger::WheelScrollUp,
                            mods,
                        )
                        .filter(|bind| {
                            !self.niri.screenshot_ui.is_open()
                                || allowed_during_screenshot(&bind.action)
                        });
                        let bind_down =
                            find_configured_bind(bindings, mod_key, Trigger::WheelScrollDown, mods)
                                .filter(|bind| {
                                    !self.niri.screenshot_ui.is_open()
                                        || allowed_during_screenshot(&bind.action)
                                });
                        (bind_up, bind_down)
                    };

                    if let Some(down) = bind_down {
                        for _ in 0..ticks {
                            self.handle_bind(down.clone());
                        }
                    }
                    if let Some(up) = bind_up {
                        for _ in ticks..0 {
                            self.handle_bind(up.clone());
                        }
                    }
                }

                return;
            } else {
                self.niri.horizontal_wheel_tracker.reset();
                self.niri.vertical_wheel_tracker.reset();
            }
        }

        let horizontal_amount = event.amount(Axis::Horizontal);
        let vertical_amount = event.amount(Axis::Vertical);

        // Handle touchpad and continuous scroll bindings.
        if source == AxisSource::Finger || source == AxisSource::Continuous {
            let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
            let modifiers = modifiers_from_state(mods);

            let horizontal = horizontal_amount.unwrap_or(0.);
            let vertical = vertical_amount.unwrap_or(0.);

            if should_handle_in_overview && modifiers.is_empty() {
                let mut redraw = false;

                let action = self
                    .niri
                    .overview_scroll_swipe_gesture
                    .update(horizontal, vertical);
                let is_vertical = self.niri.overview_scroll_swipe_gesture.is_vertical();

                if action.end() {
                    if is_vertical {
                        redraw |= self
                            .niri
                            .layout
                            .workspace_switch_gesture_end(Some(true))
                            .is_some();
                    } else {
                        redraw |= self
                            .niri
                            .layout
                            .view_offset_gesture_end(Some(true))
                            .is_some();
                    }
                } else {
                    // Maybe begin, then update.
                    if is_vertical {
                        if action.begin() {
                            if let Some(output) = self.niri.output_under_cursor() {
                                self.niri
                                    .layout
                                    .workspace_switch_gesture_begin(&output, true);
                                redraw = true;
                            }
                        }

                        let res = self
                            .niri
                            .layout
                            .workspace_switch_gesture_update(vertical, timestamp, true);
                        if let Some(Some(_)) = res {
                            redraw = true;
                        }
                    } else {
                        if action.begin() {
                            if let Some((output, ws)) = self.niri.workspace_under_cursor(true) {
                                let ws_id = ws.id();
                                let ws_idx =
                                    self.niri.layout.find_workspace_by_id(ws_id).unwrap().0;

                                self.niri.layout.view_offset_gesture_begin(
                                    &output,
                                    Some(ws_idx),
                                    true,
                                );
                                redraw = true;
                            }
                        }

                        let res = self
                            .niri
                            .layout
                            .view_offset_gesture_update(horizontal, timestamp, true);
                        if let Some(Some(_)) = res {
                            redraw = true;
                        }
                    }
                }

                if redraw {
                    self.niri.queue_redraw_all();
                }

                return;
            } else {
                let mut redraw = false;
                if self.niri.overview_scroll_swipe_gesture.reset() {
                    if self.niri.overview_scroll_swipe_gesture.is_vertical() {
                        redraw |= self
                            .niri
                            .layout
                            .workspace_switch_gesture_end(Some(true))
                            .is_some();
                    } else {
                        redraw |= self
                            .niri
                            .layout
                            .view_offset_gesture_end(Some(true))
                            .is_some();
                    }
                }
                if redraw {
                    self.niri.queue_redraw_all();
                }
            }

            if is_mru_open || self.niri.mods_with_finger_scroll_binds.contains(&modifiers) {
                let ticks = self
                    .niri
                    .horizontal_finger_scroll_tracker
                    .accumulate(horizontal);
                if ticks != 0 {
                    let config = self.niri.config.borrow();
                    let bindings =
                        make_binds_iter(&config, &mut self.niri.window_mru_ui, modifiers);
                    let bind_left = find_configured_bind(
                        bindings.clone(),
                        mod_key,
                        Trigger::TouchpadScrollLeft,
                        mods,
                    )
                    .filter(|bind| {
                        !self.niri.screenshot_ui.is_open()
                            || allowed_during_screenshot(&bind.action)
                    });
                    let bind_right =
                        find_configured_bind(bindings, mod_key, Trigger::TouchpadScrollRight, mods)
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                    drop(config);

                    if let Some(right) = bind_right {
                        for _ in 0..ticks {
                            self.handle_bind(right.clone());
                        }
                    }
                    if let Some(left) = bind_left {
                        for _ in ticks..0 {
                            self.handle_bind(left.clone());
                        }
                    }
                }

                let ticks = self
                    .niri
                    .vertical_finger_scroll_tracker
                    .accumulate(vertical);
                if ticks != 0 {
                    let config = self.niri.config.borrow();
                    let bindings =
                        make_binds_iter(&config, &mut self.niri.window_mru_ui, modifiers);
                    let bind_up = find_configured_bind(
                        bindings.clone(),
                        mod_key,
                        Trigger::TouchpadScrollUp,
                        mods,
                    )
                    .filter(|bind| {
                        !self.niri.screenshot_ui.is_open()
                            || allowed_during_screenshot(&bind.action)
                    });
                    let bind_down =
                        find_configured_bind(bindings, mod_key, Trigger::TouchpadScrollDown, mods)
                            .filter(|bind| {
                                !self.niri.screenshot_ui.is_open()
                                    || allowed_during_screenshot(&bind.action)
                            });
                    drop(config);

                    if let Some(down) = bind_down {
                        for _ in 0..ticks {
                            self.handle_bind(down.clone());
                        }
                    }
                    if let Some(up) = bind_up {
                        for _ in ticks..0 {
                            self.handle_bind(up.clone());
                        }
                    }
                }

                return;
            } else {
                self.niri.horizontal_finger_scroll_tracker.reset();
                self.niri.vertical_finger_scroll_tracker.reset();
            }
        }

        self.update_pointer_contents();

        let device_scroll_factor = {
            let config = self.niri.config.borrow();
            match source {
                AxisSource::Wheel => config.input.mouse.scroll_factor,
                AxisSource::Finger => config.input.touchpad.scroll_factor,
                _ => None,
            }
        };

        // Get window-specific scroll factor
        let window_scroll_factor = pointer
            .current_focus()
            .map(|focused| self.niri.find_root_shell_surface(&focused))
            .and_then(|root| self.niri.layout.find_window_and_output(&root).unzip().0)
            .and_then(|window| window.rules().scroll_factor)
            .unwrap_or(1.);

        // Determine final scroll factors based on configuration
        let (horizontal_factor, vertical_factor) = device_scroll_factor
            .map(|x| x.h_v_factors())
            .unwrap_or((1.0, 1.0));
        let (horizontal_factor, vertical_factor) = (
            horizontal_factor * window_scroll_factor,
            vertical_factor * window_scroll_factor,
        );

        let horizontal_amount = horizontal_amount.unwrap_or_else(|| {
            // Winit backend, discrete scrolling.
            horizontal_amount_v120.unwrap_or(0.0) / 120. * 15.
        }) * horizontal_factor;

        let vertical_amount = vertical_amount.unwrap_or_else(|| {
            // Winit backend, discrete scrolling.
            vertical_amount_v120.unwrap_or(0.0) / 120. * 15.
        }) * vertical_factor;

        let horizontal_amount_v120 = horizontal_amount_v120.map(|x| x * horizontal_factor);
        let vertical_amount_v120 = vertical_amount_v120.map(|x| x * vertical_factor);

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        if horizontal_amount != 0.0 {
            frame = frame
                .relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal));
            frame = frame.value(Axis::Horizontal, horizontal_amount);
            if let Some(v120) = horizontal_amount_v120 {
                frame = frame.v120(Axis::Horizontal, v120 as i32);
            }
        }
        if vertical_amount != 0.0 {
            frame =
                frame.relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical));
            frame = frame.value(Axis::Vertical, vertical_amount);
            if let Some(v120) = vertical_amount_v120 {
                frame = frame.v120(Axis::Vertical, v120 as i32);
            }
        }

        if source == AxisSource::Finger {
            if event.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if event.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        pointer.axis(self, frame);
        pointer.frame(self);
    }

    fn on_tablet_tool_axis<I: InputBackend>(&mut self, event: I::TabletToolAxisEvent)
    where
        I::Device: 'static, // Needed for downcasting.
    {
        let Some(pos) = self.compute_tablet_position(&event) else {
            return;
        };

        if let Some(output) = self.niri.screenshot_ui.selection_output() {
            let geom = self.niri.global_space.output_geometry(output).unwrap();
            let point = (pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, None);
        }

        if let Some(mru_output) = self.niri.window_mru_ui.output() {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                if mru_output == output {
                    self.niri.window_mru_ui.pointer_motion(pos_within_output);
                }
            }
        }

        if self.niri.end_session_dialog.is_open() {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                let output_size = output_size(output);
                if self
                    .niri
                    .end_session_dialog
                    .pointer_motion(output_size, pos_within_output)
                {
                    self.niri.queue_redraw_all();
                }
            }
        }

        let under = self.niri.contents_under(pos);

        let tablet_seat = self.niri.seat.tablet_seat();
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
        let tool = tablet_seat.get_tool(&event.tool());
        if let (Some(tablet), Some(tool)) = (tablet, tool) {
            if event.pressure_has_changed() {
                tool.pressure(event.pressure());
            }
            if event.distance_has_changed() {
                tool.distance(event.distance());
            }
            if event.tilt_has_changed() {
                tool.tilt(event.tilt());
            }
            if event.slider_has_changed() {
                tool.slider_position(event.slider_position());
            }
            if event.rotation_has_changed() {
                tool.rotation(event.rotation());
            }
            if event.wheel_has_changed() {
                tool.wheel(event.wheel_delta(), event.wheel_delta_discrete());
            }

            tool.motion(
                pos,
                under.surface,
                &tablet,
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );

            self.niri.pointer_visibility = PointerVisibility::Visible;
            self.niri.tablet_cursor_location = Some(pos);
        }

        // Redraw to update the cursor position.
        // FIXME: redraw only outputs overlapping the cursor.
        self.niri.queue_redraw_all();
    }

    fn on_tablet_tool_tip<I: InputBackend>(&mut self, event: I::TabletToolTipEvent) {
        let tool = self.niri.seat.tablet_seat().get_tool(&event.tool());

        let Some(tool) = tool else {
            return;
        };
        let tip_state = event.tip_state();

        let is_overview_open = self.niri.layout.is_overview_open();

        match tip_state {
            TabletToolTipState::Down => {
                let serial = SERIAL_COUNTER.next_serial();
                tool.tip_down(serial, event.time_msec());

                if let Some(pos) = self.niri.tablet_cursor_location {
                    let under = self.niri.contents_under(pos);

                    if self.niri.screenshot_ui.is_open() {
                        let mod_key = self.backend.mod_key(&self.niri.config.borrow());
                        let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
                        let modifiers = modifiers_from_state(mods);
                        let mod_down = modifiers.contains(mod_key.to_modifiers());

                        // If we'll be moving the existing selection, use the selection output.
                        let output = if mod_down {
                            self.niri.screenshot_ui.selection_output()
                        } else {
                            under.output.as_ref()
                        };

                        if let Some(output) = output.cloned() {
                            let geom = self.niri.global_space.output_geometry(&output).unwrap();
                            let point = (pos - geom.loc.to_f64())
                                .to_physical(output.current_scale().fractional_scale())
                                .to_i32_round();

                            if self
                                .niri
                                .screenshot_ui
                                .pointer_down(output, point, None, mod_down)
                            {
                                self.niri.queue_redraw_all();
                            }
                        }
                    } else if let Some(mru_output) = self.niri.window_mru_ui.output() {
                        if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                            if mru_output == output {
                                let id = self.niri.window_mru_ui.pointer_motion(pos_within_output);
                                if id.is_some() {
                                    self.confirm_mru();
                                } else {
                                    self.niri.cancel_mru();
                                }
                            } else {
                                self.niri.cancel_mru();
                            }
                        }
                    } else if let Some((window, _)) = under.window {
                        if let Some(output) = is_overview_open.then_some(under.output).flatten() {
                            let mut workspaces = self.niri.layout.workspaces();
                            if let Some(ws_idx) = workspaces.find_map(|(_, ws_idx, ws)| {
                                ws.windows().any(|w| w.window == window).then_some(ws_idx)
                            }) {
                                drop(workspaces);
                                self.niri.layout.focus_output(&output);
                                self.niri.layout.toggle_overview_to_workspace(ws_idx);
                            }
                        }

                        self.niri.layout.activate_window(&window);

                        // FIXME: granular.
                        self.niri.queue_redraw_all();
                    } else if let Some((output, ws)) = is_overview_open
                        .then(|| {
                            self.niri
                                .thumbnail_workspace_under(pos)
                                .or_else(|| self.niri.workspace_under(false, pos))
                        })
                        .flatten()
                    {
                        let ws_id = ws.id();
                        let ws_idx = self.niri.layout.find_workspace_by_id(ws_id).unwrap().0;

                        self.niri.layout.focus_output(&output);

                        // Same GNOME semantics as the pointer path: only the
                        // active workspace's empty area leaves the overview.
                        let gnome_mode = self.niri.config.borrow().layout.windowing_mode
                            == niri_config::WindowingMode::Floating;
                        let is_active = self
                            .niri
                            .layout
                            .active_workspace()
                            .is_some_and(|active| active.id() == ws_id);
                        if gnome_mode && !is_active {
                            self.niri.layout.switch_workspace(ws_idx);
                        } else {
                            self.niri.layout.toggle_overview_to_workspace(ws_idx);
                        }

                        // FIXME: granular.
                        self.niri.queue_redraw_all();
                    } else if let Some(output) = under.output {
                        self.niri.layout.focus_output(&output);

                        // FIXME: granular.
                        self.niri.queue_redraw_all();
                    }
                    self.niri.focus_layer_surface_if_on_demand(under.layer);
                }
            }
            TabletToolTipState::Up => {
                if let Some(capture) = self.niri.screenshot_ui.pointer_up(None) {
                    if capture {
                        self.confirm_screenshot(true);
                    } else {
                        self.niri.queue_redraw_all();
                    }
                }

                tool.tip_up(event.time_msec());
            }
        }
    }

    fn on_tablet_tool_proximity<I: InputBackend>(&mut self, event: I::TabletToolProximityEvent)
    where
        I::Device: 'static, // Needed for downcasting.
    {
        let Some(pos) = self.compute_tablet_position(&event) else {
            return;
        };

        let under = self.niri.contents_under(pos);

        let tablet_seat = self.niri.seat.tablet_seat();
        let display_handle = self.niri.display_handle.clone();
        let tool = tablet_seat.add_tool::<Self>(self, &display_handle, &event.tool());
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
        if let Some(tablet) = tablet {
            match event.state() {
                ProximityState::In => {
                    if let Some(under) = under.surface {
                        tool.proximity_in(
                            pos,
                            under,
                            &tablet,
                            SERIAL_COUNTER.next_serial(),
                            event.time_msec(),
                        );
                    }
                    self.niri.pointer_visibility = PointerVisibility::Visible;
                    self.niri.tablet_cursor_location = Some(pos);
                }
                ProximityState::Out => {
                    tool.proximity_out(event.time_msec());

                    // Move the mouse pointer here to avoid discontinuity.
                    //
                    // Plus, Wayland SDL2 currently warps the pointer into some weird
                    // location on proximity out, so this should help it a little.
                    if let Some(pos) = self.niri.tablet_cursor_location {
                        self.move_cursor(pos);
                    }

                    self.niri.pointer_visibility = PointerVisibility::Visible;
                    self.niri.tablet_cursor_location = None;
                }
            }

            // FIXME: granular.
            self.niri.queue_redraw_all();
        }
    }

    fn on_tablet_tool_button<I: InputBackend>(&mut self, event: I::TabletToolButtonEvent) {
        const BTN_STYLUS3: u32 = 0x149;
        const BTN_STYLUS: u32 = 0x14b;
        const BTN_STYLUS2: u32 = 0x14c;

        let tool = self.niri.seat.tablet_seat().get_tool(&event.tool());

        if let Some(tool) = tool {
            let button = event.button();

            if self.niri.suppressed_buttons.remove(&button) {
                return;
            }

            let trigger = match button {
                BTN_STYLUS => Some(Trigger::TabletStylusButton1),
                BTN_STYLUS2 => Some(Trigger::TabletStylusButton2),
                BTN_STYLUS3 => Some(Trigger::TabletStylusButton3),
                _ => None,
            };

            if let Some(trigger) = trigger {
                if event.button_state() == ButtonState::Pressed {
                    let mod_key = self.backend.mod_key(&self.niri.config.borrow());
                    let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
                    let modifiers = modifiers_from_state(mods);

                    if self.niri.mods_with_tablet_stylus_binds.contains(&modifiers) {
                        let bind = {
                            let config = self.niri.config.borrow();
                            let bindings = config.binds.0.iter();
                            find_configured_bind(bindings, mod_key, trigger, mods)
                        }
                        .filter(|bind| {
                            !self.niri.screenshot_ui.is_open()
                                || allowed_during_screenshot(&bind.action)
                        });
                        if let Some(bind) = bind {
                            self.niri.suppressed_buttons.insert(button);
                            self.handle_bind(bind.clone());
                            return;
                        }
                    }
                }
            }

            tool.button(
                button,
                event.button_state(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
        }
    }

    fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        if self.niri.window_mru_ui.is_open() {
            // Don't start swipe gestures while in the MRU.
            return;
        }

        if event.fingers() == 3 {
            self.niri.gesture_swipe_3f_cumulative = Some((0., 0.));

            // We handled this event.
            return;
        } else if event.fingers() == 4 {
            self.niri.layout.overview_gesture_begin();
            self.niri.queue_redraw_all();

            // We handled this event.
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_swipe_begin(
            self,
            &GestureSwipeBeginEvent {
                serial,
                time: event.time_msec(),
                fingers: event.fingers(),
            },
        );
    }

    fn on_gesture_swipe_update<I: InputBackend + 'static>(
        &mut self,
        event: I::GestureSwipeUpdateEvent,
    ) where
        I::Device: 'static,
    {
        let mut delta_x = event.delta_x();
        let mut delta_y = event.delta_y();

        if let Some(libinput_event) =
            (&event as &dyn Any).downcast_ref::<input::event::gesture::GestureSwipeUpdateEvent>()
        {
            delta_x = libinput_event.dx_unaccelerated();
            delta_y = libinput_event.dy_unaccelerated();
        }

        let uninverted_delta_y = delta_y;

        let device = event.device();
        if let Some(device) = (&device as &dyn Any).downcast_ref::<input::Device>() {
            if device.config_scroll_natural_scroll_enabled() {
                delta_x = -delta_x;
                delta_y = -delta_y;
            }
        }

        let is_overview_open = self.niri.layout.is_overview_open();

        if let Some((cx, cy)) = &mut self.niri.gesture_swipe_3f_cumulative {
            *cx += delta_x;
            *cy += delta_y;

            // Check if the gesture moved far enough to decide. Threshold copied from GNOME Shell.
            let (cx, cy) = (*cx, *cy);
            if cx * cx + cy * cy >= 16. * 16. {
                self.niri.gesture_swipe_3f_cumulative = None;

                if let Some(output) = self.niri.output_under_cursor() {
                    if cx.abs() > cy.abs() {
                        let output_ws = if is_overview_open {
                            self.niri.workspace_under_cursor(true)
                        } else {
                            // We don't want to accidentally "catch" the wrong workspace during
                            // animations.
                            self.niri.output_under_cursor().and_then(|output| {
                                let mon = self.niri.layout.monitor_for_output(&output)?;
                                Some((output, mon.active_workspace_ref()))
                            })
                        };

                        if let Some((output, ws)) = output_ws {
                            let ws_idx = self.niri.layout.find_workspace_by_id(ws.id()).unwrap().0;
                            self.niri
                                .layout
                                .view_offset_gesture_begin(&output, Some(ws_idx), true);
                        }
                    } else {
                        self.niri
                            .layout
                            .workspace_switch_gesture_begin(&output, true);
                    }
                }
            }
        }

        let timestamp = Duration::from_micros(event.time());

        let mut handled = false;
        let res = self
            .niri
            .layout
            .workspace_switch_gesture_update(delta_y, timestamp, true);
        if let Some(output) = res {
            if let Some(output) = output {
                self.niri.queue_redraw(&output);
            }
            handled = true;
        }

        let res = self
            .niri
            .layout
            .view_offset_gesture_update(delta_x, timestamp, true);
        if let Some(output) = res {
            if let Some(output) = output {
                self.niri.queue_redraw(&output);
            }
            handled = true;
        }

        let res = self
            .niri
            .layout
            .overview_gesture_update(-uninverted_delta_y, timestamp);
        if let Some(redraw) = res {
            if redraw {
                self.niri.queue_redraw_all();
            }
            handled = true;
        }

        if handled {
            // We handled this event.
            return;
        }

        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_swipe_update(
            self,
            &GestureSwipeUpdateEvent {
                time: event.time_msec(),
                delta: event.delta(),
            },
        );
    }

    fn on_gesture_swipe_end<I: InputBackend>(&mut self, event: I::GestureSwipeEndEvent) {
        self.niri.gesture_swipe_3f_cumulative = None;

        let mut handled = false;
        let res = self.niri.layout.workspace_switch_gesture_end(Some(true));
        if let Some(output) = res {
            self.niri.queue_redraw(&output);
            handled = true;
        }

        let res = self.niri.layout.view_offset_gesture_end(Some(true));
        if let Some(output) = res {
            self.niri.queue_redraw(&output);
            handled = true;
        }

        let res = self.niri.layout.overview_gesture_end();
        if res {
            self.niri.queue_redraw_all();
            handled = true;
        }

        if handled {
            // We handled this event.
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_swipe_end(
            self,
            &GestureSwipeEndEvent {
                serial,
                time: event.time_msec(),
                cancelled: event.cancelled(),
            },
        );
    }

    fn on_gesture_pinch_begin<I: InputBackend>(&mut self, event: I::GesturePinchBeginEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_pinch_begin(
            self,
            &GesturePinchBeginEvent {
                serial,
                time: event.time_msec(),
                fingers: event.fingers(),
            },
        );
    }

    fn on_gesture_pinch_update<I: InputBackend>(&mut self, event: I::GesturePinchUpdateEvent) {
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_pinch_update(
            self,
            &GesturePinchUpdateEvent {
                time: event.time_msec(),
                delta: event.delta(),
                scale: event.scale(),
                rotation: event.rotation(),
            },
        );
    }

    fn on_gesture_pinch_end<I: InputBackend>(&mut self, event: I::GesturePinchEndEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_pinch_end(
            self,
            &GesturePinchEndEvent {
                serial,
                time: event.time_msec(),
                cancelled: event.cancelled(),
            },
        );
    }

    fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_hold_begin(
            self,
            &GestureHoldBeginEvent {
                serial,
                time: event.time_msec(),
                fingers: event.fingers(),
            },
        );
    }

    fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.niri.seat.get_pointer().unwrap();

        if self.update_pointer_contents() {
            pointer.frame(self);
        }

        pointer.gesture_hold_end(
            self,
            &GestureHoldEndEvent {
                serial,
                time: event.time_msec(),
                cancelled: event.cancelled(),
            },
        );
    }

    fn compute_absolute_location<I: InputBackend>(
        &self,
        evt: &impl AbsolutePositionEvent<I>,
        fallback_output: Option<&Output>,
    ) -> Option<Point<f64, Logical>> {
        let output = evt.device().output(self);
        let output = output.filter(|output| self.niri.output_exists(output));
        let output = output.as_ref().or(fallback_output)?;
        let output_geo = self.niri.global_space.output_geometry(output).unwrap();
        let transform = output.current_transform();
        let size = transform.invert().transform_size(output_geo.size);
        Some(
            transform.transform_point_in(evt.position_transformed(size), &size.to_f64())
                + output_geo.loc.to_f64(),
        )
    }

    /// Computes the cursor position for the touch event.
    ///
    /// This function handles the touch output mapping, as well as coordinate transform
    fn compute_touch_location<I: InputBackend>(
        &self,
        evt: &impl AbsolutePositionEvent<I>,
    ) -> Option<Point<f64, Logical>> {
        self.compute_absolute_location(evt, self.niri.output_for_touch())
    }

    fn on_touch_down<I: InputBackend>(&mut self, evt: I::TouchDownEvent) {
        let Some(handle) = self.niri.seat.get_touch() else {
            return;
        };
        let Some(pos) = self.compute_touch_location(&evt) else {
            return;
        };
        let slot = evt.slot();

        let serial = SERIAL_COUNTER.next_serial();

        let under = self.niri.contents_under(pos);

        let mod_key = self.backend.mod_key(&self.niri.config.borrow());
        let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
        let mods = modifiers_from_state(mods);
        let mod_down = mods.contains(mod_key.to_modifiers());

        if self.niri.screenshot_ui.is_open() {
            // If we'll be moving the existing selection, use the selection output.
            let output = if mod_down {
                self.niri.screenshot_ui.selection_output()
            } else {
                under.output.as_ref()
            };

            if let Some(output) = output.cloned() {
                let geom = self.niri.global_space.output_geometry(&output).unwrap();
                let point = (pos - geom.loc.to_f64())
                    .to_physical(output.current_scale().fractional_scale())
                    .to_i32_round();

                if self
                    .niri
                    .screenshot_ui
                    .pointer_down(output, point, Some(slot), mod_down)
                {
                    self.niri.queue_redraw_all();
                }
            }
        } else if let Some(mru_output) = self.niri.window_mru_ui.output() {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                if mru_output == output {
                    let id = self.niri.window_mru_ui.pointer_motion(pos_within_output);
                    if id.is_some() {
                        self.confirm_mru();
                    } else {
                        self.niri.cancel_mru();
                    }
                } else {
                    self.niri.cancel_mru();
                }
            }
        } else if !handle.is_grabbed() {
            if self.niri.layout.is_overview_open()
                && !mod_down
                && under.layer.is_none()
                && under.output.is_some()
            {
                let (output, pos_within_output) = self.niri.output_under(pos).unwrap();
                let output = output.clone();

                let mut matched_narrow = true;
                let mut ws = self.niri.workspace_under(false, pos);
                if ws.is_none() {
                    matched_narrow = false;
                    ws = self.niri.workspace_under(true, pos);
                }
                let ws_id = ws.map(|(_, ws)| ws.id());

                let mapped = self.niri.window_under(pos);
                let window = mapped.map(|mapped| mapped.window.clone());

                let start_data = TouchGrabStartData {
                    focus: None,
                    slot,
                    location: pos,
                };
                let start_timestamp = Duration::from_micros(evt.time());
                let grab = TouchOverviewGrab::new(
                    start_data,
                    start_timestamp,
                    output,
                    pos_within_output,
                    ws_id,
                    matched_narrow,
                    window,
                );
                handle.set_grab(self, grab, serial);
            } else if let Some((window, _)) = under.window {
                self.niri.layout.activate_window(&window);

                // Check if we need to start a touch move grab.
                if mod_down {
                    let start_data = TouchGrabStartData {
                        focus: None,
                        slot,
                        location: pos,
                    };
                    let start_data = PointerOrTouchStartData::Touch(start_data);
                    if let Some(grab) = MoveGrab::new(self, start_data, window.clone(), true, None)
                    {
                        handle.set_grab(self, grab, serial);
                    }
                }

                // FIXME: granular.
                self.niri.queue_redraw_all();
            } else if let Some(output) = under.output {
                self.niri.layout.focus_output(&output);

                // FIXME: granular.
                self.niri.queue_redraw_all();
            }
            self.niri.focus_layer_surface_if_on_demand(under.layer);
        };

        handle.down(
            self,
            under.surface,
            &DownEvent {
                slot,
                location: pos,
                serial,
                time: evt.time_msec(),
            },
        );

        // We're using touch, hide the pointer.
        self.niri.pointer_visibility = PointerVisibility::Disabled;
    }
    fn on_touch_up<I: InputBackend>(&mut self, evt: I::TouchUpEvent) {
        let Some(handle) = self.niri.seat.get_touch() else {
            return;
        };
        let slot = evt.slot();

        if let Some(capture) = self.niri.screenshot_ui.pointer_up(Some(slot)) {
            if capture {
                self.confirm_screenshot(true);
            } else {
                self.niri.queue_redraw_all();
            }
        }

        let serial = SERIAL_COUNTER.next_serial();
        handle.up(
            self,
            &UpEvent {
                slot,
                serial,
                time: evt.time_msec(),
            },
        )
    }
    fn on_touch_motion<I: InputBackend>(&mut self, evt: I::TouchMotionEvent) {
        let Some(handle) = self.niri.seat.get_touch() else {
            return;
        };
        let Some(pos) = self.compute_touch_location(&evt) else {
            return;
        };
        let slot = evt.slot();

        if let Some(output) = self.niri.screenshot_ui.selection_output().cloned() {
            let geom = self.niri.global_space.output_geometry(&output).unwrap();
            let point = (pos - geom.loc.to_f64())
                .to_physical(output.current_scale().fractional_scale())
                .to_i32_round::<i32>();

            self.niri.screenshot_ui.pointer_motion(point, Some(slot));
            self.niri.queue_redraw(&output);
        }

        let under = self.niri.contents_under(pos);
        handle.motion(
            self,
            under.surface,
            &TouchMotionEvent {
                slot,
                location: pos,
                time: evt.time_msec(),
            },
        );

        // Inform the layout of an ongoing DnD operation.
        let is_dnd_grab = handle
            .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
            .unwrap_or(false);
        if is_dnd_grab {
            if let Some((output, pos_within_output)) = self.niri.output_under(pos) {
                let output = output.clone();
                self.niri.layout.dnd_update(output, pos_within_output);
            }
        }
    }
    fn on_touch_frame<I: InputBackend>(&mut self, _evt: I::TouchFrameEvent) {
        let Some(handle) = self.niri.seat.get_touch() else {
            return;
        };
        handle.frame(self);
    }
    fn on_touch_cancel<I: InputBackend>(&mut self, _evt: I::TouchCancelEvent) {
        let Some(handle) = self.niri.seat.get_touch() else {
            return;
        };
        handle.cancel(self);
    }

    fn on_switch_toggle<I: InputBackend>(&mut self, evt: I::SwitchToggleEvent) {
        let Some(switch) = evt.switch() else {
            return;
        };

        if switch == Switch::Lid {
            let is_closed = evt.state() == SwitchState::On;
            trace!("lid switch {}", if is_closed { "closed" } else { "opened" });
            self.set_lid_closed(is_closed);
        }

        let action = {
            let bindings = &self.niri.config.borrow().switch_events;
            find_configured_switch_action(bindings, switch, evt.state())
        };

        if let Some(action) = action {
            self.do_action(action, true);
        }
    }

    pub fn is_dnd_grab(grab: &dyn Any) -> bool {
        // Normal DnD
        grab.is::<DnDGrab<Self, WlDataSource, WlSurface>>()
            // Null-source DnD: weston-dnd --self-only
            || grab.is::<DnDGrab<Self, WlSurface, WlSurface>>()
    }

    fn grab_can_be_cancelled_with_esc(grab: &(dyn PointerGrab<State> + 'static)) -> bool {
        let grab = grab.as_any();

        grab.is::<PickWindowGrab>() || grab.is::<PickColorGrab>() || Self::is_dnd_grab(grab)
    }
}

/// Check whether the key should be intercepted and mark intercepted
/// pressed keys as `suppressed`, thus preventing `releases` corresponding
/// to them from being delivered.
#[allow(clippy::too_many_arguments)]
fn should_intercept_key<'a>(
    suppressed_keys: &mut HashSet<Keycode>,
    bindings: impl IntoIterator<Item = &'a Bind>,
    gnome_keybindings: &[GnomeKeybinding],
    accel_grabs: &[AccelGrab],
    mru_is_open: bool,
    mod_key: ModKey,
    key_code: Keycode,
    modified: Keysym,
    raw: Option<Keysym>,
    pressed: bool,
    mods: ModifiersState,
    screenshot_ui: &ScreenshotUi,
    disable_power_key_handling: bool,
    is_inhibiting_shortcuts: bool,
) -> FilterResult<Option<Bind>> {
    // Actions are only triggered on presses, release of the key
    // shouldn't try to intercept anything unless we have marked
    // the key to suppress.
    if !pressed && !suppressed_keys.contains(&key_code) {
        return FilterResult::Forward;
    }

    let mut final_bind = find_bind(
        bindings,
        gnome_keybindings,
        accel_grabs,
        mru_is_open,
        mod_key,
        key_code,
        modified,
        raw,
        mods,
        disable_power_key_handling,
    );

    // Allow only a subset of compositor actions while the screenshot UI is open, since the user
    // cannot see the screen.
    if screenshot_ui.is_open() {
        let mut use_screenshot_ui_action = true;

        if let Some(bind) = &final_bind {
            if allowed_during_screenshot(&bind.action) {
                use_screenshot_ui_action = false;
            }
        }

        if use_screenshot_ui_action {
            if let Some(raw) = raw {
                final_bind = screenshot_ui.action(raw, mods).map(|action| Bind {
                    key: Key {
                        trigger: Trigger::Keysym(raw),
                        // Not entirely correct but it doesn't matter in how we currently use it.
                        modifiers: Modifiers::empty(),
                    },
                    action,
                    repeat: true,
                    cooldown: None,
                    allow_when_locked: false,
                    // The screenshot UI owns the focus anyway, so this doesn't really matter.
                    // But logically, nothing can inhibit its actions. Only opening it can be
                    // inhibited.
                    allow_inhibiting: false,
                    hotkey_overlay_title: None,
                });
            }
        }
    }

    match (final_bind, pressed) {
        (Some(bind), true) => {
            if is_inhibiting_shortcuts && bind.allow_inhibiting {
                FilterResult::Forward
            } else {
                suppressed_keys.insert(key_code);
                FilterResult::Intercept(Some(bind))
            }
        }
        (_, false) => {
            // By this point, we know that the key was suppressed on press. Even if we're inhibiting
            // shortcuts, we should still suppress the release.
            // But we don't need to check for shortcuts inhibition here, because
            // if it was inhibited on press (forwarded to the client), it wouldn't be suppressed,
            // so the release would already have been forwarded at the start of this function.
            suppressed_keys.remove(&key_code);
            FilterResult::Intercept(None)
        }
        (None, true) => FilterResult::Forward,
    }
}

#[allow(clippy::too_many_arguments)]
fn find_bind<'a>(
    bindings: impl IntoIterator<Item = &'a Bind>,
    gnome_keybindings: &[GnomeKeybinding],
    accel_grabs: &[AccelGrab],
    mru_is_open: bool,
    mod_key: ModKey,
    key_code: Keycode,
    modified: Keysym,
    raw: Option<Keysym>,
    mods: ModifiersState,
    disable_power_key_handling: bool,
) -> Option<Bind> {
    use keysyms::*;

    // Handle hardcoded binds.
    #[allow(non_upper_case_globals)] // wat
    let hardcoded_action = match modified.raw() {
        modified @ KEY_XF86Switch_VT_1..=KEY_XF86Switch_VT_12 => {
            let vt = (modified - KEY_XF86Switch_VT_1 + 1) as i32;
            Some(Action::ChangeVt(vt))
        }
        KEY_XF86PowerOff if !disable_power_key_handling => Some(Action::Suspend),
        _ => None,
    };

    if let Some(action) = hardcoded_action {
        return Some(Bind {
            key: Key {
                // Not entirely correct but it doesn't matter in how we currently use it.
                trigger: Trigger::Keysym(modified),
                modifiers: Modifiers::empty(),
            },
            action,
            repeat: true,
            cooldown: None,
            allow_when_locked: false,
            // In a worst-case scenario, the user has no way to unlock the compositor and a
            // misbehaving client has a keyboard shortcuts inhibitor, "jailing" the user.
            // The user must always be able to change VTs to recover from such a situation.
            // It also makes no sense to inhibit the default power key handling.
            // Hardcoded binds must never be inhibited.
            allow_inhibiting: false,
            hotkey_overlay_title: None,
        });
    }

    // GNOME keybindings resolve before niri's configured binds: in a GNOME
    // session the GSettings store is the user's keybinding config, and mutter
    // processes it before anything else sees the key. The niri config stays
    // underneath as a fallback.
    if let Some(bind) = find_gnome_bind(gnome_keybindings, mru_is_open, key_code, raw, mods) {
        return Some(bind);
    }

    // External accelerator grabs live in the same table in mutter, after the
    // builtins (a conflicting grab is refused at grab time). They also don't
    // fire while the switcher's grab is up.
    if !mru_is_open {
        if let Some(bind) = find_accel_grab_bind(accel_grabs, key_code, raw, mods) {
            return Some(bind);
        }
    }

    let trigger = Trigger::Keysym(raw?);
    find_configured_bind(bindings, mod_key, trigger, mods)
}

/// Match an `org.gnome.Shell` accelerator grab (gsd-media-keys et al.),
/// synthesized into a bind whose action notifies the grabber over D-Bus.
fn find_accel_grab_bind(
    accel_grabs: &[AccelGrab],
    key_code: Keycode,
    raw: Option<Keysym>,
    mods: ModifiersState,
) -> Option<Bind> {
    let grab = accel_grabs
        .iter()
        .find(|g| accel_matches(&g.accel, key_code, raw, mods))?;

    Some(Bind {
        key: Key {
            // Not entirely correct but it doesn't matter in how we currently use it.
            trigger: Trigger::Keysym(raw.unwrap_or(Keysym::NoSymbol)),
            modifiers: Modifiers::empty(),
        },
        action: Action::ActivateAcceleratorGrab(grab.action),
        repeat: grab.grab_flags & AccelGrab::FLAG_IGNORE_AUTOREPEAT == 0,
        cooldown: None,
        // Volume/brightness keys are grabbed with lock-screen modes; honor
        // that so they keep working on the lock screen like in GNOME.
        allow_when_locked: grab.mode_flags
            & (AccelGrab::MODE_LOCK_SCREEN | AccelGrab::MODE_UNLOCK_SCREEN)
            != 0,
        allow_inhibiting: grab.grab_flags & AccelGrab::FLAG_NON_MASKABLE == 0,
        hotkey_overlay_title: None,
    })
}

/// Find a GNOME keybinding (`org.gnome.desktop.wm.keybindings`, read through
/// the live `gnome_settings` model) matching this key event, synthesized into
/// the equivalent niri bind.
fn find_gnome_bind(
    keybindings: &[GnomeKeybinding],
    mru_is_open: bool,
    key_code: Keycode,
    raw: Option<Keysym>,
    mods: ModifiersState,
) -> Option<Bind> {
    let keybinding = keybindings.iter().find(|kb| {
        kb.accels
            .iter()
            .any(|accel| accel_matches(accel, key_code, raw, mods))
    })?;

    // The window switcher is modal (GNOME holds a grab while it's up; niri
    // disables the general binds): only the switch actions themselves keep
    // resolving so further taps continue cycling.
    if mru_is_open
        && !matches!(
            keybinding.action,
            GnomeKeyAction::SwitchWindows { .. } | GnomeKeyAction::SwitchApplications { .. }
        )
    {
        return None;
    }

    let action = action_for_gnome(keybinding.action)?;

    // Mutter flags the workspace switches META_KEY_BINDING_IGNORE_AUTOREPEAT.
    let repeat = !matches!(
        keybinding.action,
        GnomeKeyAction::SwitchToWorkspace(_)
            | GnomeKeyAction::SwitchToWorkspacePrevious
            | GnomeKeyAction::SwitchToWorkspaceNext
            | GnomeKeyAction::MoveToWorkspace(_)
            | GnomeKeyAction::MoveToWorkspacePrevious
            | GnomeKeyAction::MoveToWorkspaceNext
    );

    Some(Bind {
        key: Key {
            // Not entirely correct but it doesn't matter in how we currently use it.
            trigger: Trigger::Keysym(raw.unwrap_or(Keysym::NoSymbol)),
            modifiers: Modifiers::empty(),
        },
        action,
        repeat,
        cooldown: None,
        allow_when_locked: false,
        // GNOME bindings are maskable: mutter suppresses everything not
        // NON_MASKABLE while the focused window inhibits shortcuts.
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    })
}

/// Whether an accelerator matches this key event. Mirrors mutter's matching:
/// Caps/Num/Scroll Lock never participate (`ModifiersState` already keeps
/// locks out of ctrl/alt/shift/logo), and the virtual META/HYPER modifiers
/// match their conventional homes (the Alt and Super keys) rather than going
/// through the keymap's modmap. Accelerators demanding the raw MOD2/MOD3
/// masks never match — we don't track those as modifiers.
fn accel_matches(
    accel: &Accel,
    key_code: Keycode,
    raw: Option<Keysym>,
    mods: ModifiersState,
) -> bool {
    let trigger_matches = match accel.trigger {
        AccelTrigger::Keysym(keysym) => raw == Some(keysym),
        AccelTrigger::Keycode(keycode) => key_code.raw() == keycode,
    };
    if !trigger_matches {
        return false;
    }

    if accel.mods.intersects(AccelMods::MOD2 | AccelMods::MOD3) {
        return false;
    }

    let want = |m: AccelMods| accel.mods.intersects(m);
    mods.ctrl == want(AccelMods::CONTROL)
        && mods.shift == want(AccelMods::SHIFT)
        && mods.alt == want(AccelMods::MOD1 | AccelMods::META)
        && mods.logo == want(AccelMods::SUPER | AccelMods::HYPER | AccelMods::MOD4)
        && mods.iso_level3_shift == want(AccelMods::MOD5)
        && !mods.iso_level5_shift
}

/// The niri action implementing a GNOME keybinding action, or `None` for
/// actions adopted in the settings model but not implemented yet (their keys
/// stay with the client). Workspace indices are 1-based on both sides.
fn action_for_gnome(action: GnomeKeyAction) -> Option<Action> {
    Some(match action {
        GnomeKeyAction::PanelRunDialog => Action::ShowRunDialog,
        GnomeKeyAction::Maximize => Action::Maximize,
        GnomeKeyAction::Unmaximize => Action::Unmaximize,
        GnomeKeyAction::ToggleTiled(TileSide::Left) => Action::ToggleTiledLeft,
        GnomeKeyAction::ToggleTiled(TileSide::Right) => Action::ToggleTiledRight,
        GnomeKeyAction::Close => Action::CloseWindow,
        GnomeKeyAction::ToggleFullscreen => Action::FullscreenWindow,
        GnomeKeyAction::SwitchToWorkspace(n) => {
            Action::FocusWorkspace(WorkspaceReference::Index(n))
        }
        GnomeKeyAction::SwitchToWorkspacePrevious => Action::FocusWorkspaceUp,
        GnomeKeyAction::SwitchToWorkspaceNext => Action::FocusWorkspaceDown,
        GnomeKeyAction::MoveToWorkspace(n) => {
            Action::MoveWindowToWorkspace(WorkspaceReference::Index(n), true)
        }
        GnomeKeyAction::MoveToWorkspacePrevious => Action::MoveWindowToWorkspaceUp(true),
        GnomeKeyAction::MoveToWorkspaceNext => Action::MoveWindowToWorkspaceDown(true),
        GnomeKeyAction::SwitchWindows { backward } => Action::MruAdvance {
            direction: mru_direction(backward),
            scope: Some(MruScope::Workspace),
            filter: Some(MruFilter::All),
        },
        GnomeKeyAction::SwitchApplications { backward } => Action::MruAdvance {
            direction: mru_direction(backward),
            scope: Some(MruScope::All),
            filter: Some(MruFilter::All),
        },
    })
}

fn mru_direction(backward: bool) -> MruDirection {
    if backward {
        MruDirection::Backward
    } else {
        MruDirection::Forward
    }
}

fn find_configured_bind<'a>(
    bindings: impl IntoIterator<Item = &'a Bind>,
    mod_key: ModKey,
    trigger: Trigger,
    mods: ModifiersState,
) -> Option<Bind> {
    // Handle configured binds.
    let mut modifiers = modifiers_from_state(mods);

    let mod_down = modifiers_from_state(mods).contains(mod_key.to_modifiers());
    if mod_down {
        modifiers |= Modifiers::COMPOSITOR;
    }

    for bind in bindings {
        if bind.key.trigger != trigger {
            continue;
        }

        let mut bind_modifiers = bind.key.modifiers;
        if bind_modifiers.contains(Modifiers::COMPOSITOR) {
            bind_modifiers |= mod_key.to_modifiers();
        } else if bind_modifiers.contains(mod_key.to_modifiers()) {
            bind_modifiers |= Modifiers::COMPOSITOR;
        }

        if bind_modifiers == modifiers {
            return Some(bind.clone());
        }
    }

    None
}

fn find_configured_switch_action(
    bindings: &SwitchBinds,
    switch: Switch,
    state: SwitchState,
) -> Option<Action> {
    let switch_action = match (switch, state) {
        (Switch::Lid, SwitchState::Off) => &bindings.lid_open,
        (Switch::Lid, SwitchState::On) => &bindings.lid_close,
        (Switch::TabletMode, SwitchState::Off) => &bindings.tablet_mode_off,
        (Switch::TabletMode, SwitchState::On) => &bindings.tablet_mode_on,
        _ => unreachable!(),
    };
    switch_action
        .as_ref()
        .map(|switch_action| Action::Spawn(switch_action.spawn.clone()))
}

fn modifiers_from_state(mods: ModifiersState) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    if mods.ctrl {
        modifiers |= Modifiers::CTRL;
    }
    if mods.shift {
        modifiers |= Modifiers::SHIFT;
    }
    if mods.alt {
        modifiers |= Modifiers::ALT;
    }
    if mods.logo {
        modifiers |= Modifiers::SUPER;
    }
    if mods.iso_level3_shift {
        modifiers |= Modifiers::ISO_LEVEL3_SHIFT;
    }
    if mods.iso_level5_shift {
        modifiers |= Modifiers::ISO_LEVEL5_SHIFT;
    }
    modifiers
}

/// Whether this event cancels a pending GNOME overlay-key (Super) tap. Mirrors
/// the event types mutter resets `overlay_key_only_pressed` on: pointer button,
/// scroll, and touch begin/end (not motion).
fn event_cancels_overlay_key<I: InputBackend>(event: &InputEvent<I>) -> bool {
    matches!(
        event,
        InputEvent::PointerButton { .. }
            | InputEvent::PointerAxis { .. }
            | InputEvent::TouchDown { .. }
            | InputEvent::TouchUp { .. }
    )
}

fn should_activate_monitors<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerMotionAbsolute { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::GestureHoldBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TabletToolAxis { .. }
        | InputEvent::TabletToolProximity { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => true,
        // Ignore events like device additions and removals, key releases, gesture ends.
        _ => false,
    }
}

fn should_hide_hotkey_overlay<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => true,
        _ => false,
    }
}

fn should_hide_exit_confirm_dialog<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => true,
        _ => false,
    }
}

fn should_notify_activity<I: InputBackend>(event: &InputEvent<I>) -> bool {
    !matches!(
        event,
        InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
    )
}

fn should_reset_pointer_inactivity_timer<I: InputBackend>(event: &InputEvent<I>) -> bool {
    matches!(
        event,
        InputEvent::PointerAxis { .. }
            | InputEvent::PointerButton { .. }
            | InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::TabletToolAxis { .. }
            | InputEvent::TabletToolButton { .. }
            | InputEvent::TabletToolProximity { .. }
            | InputEvent::TabletToolTip { .. }
    )
}

fn allowed_when_locked(action: &Action) -> bool {
    matches!(
        action,
        Action::Quit(_)
            | Action::ChangeVt(_)
            | Action::Suspend
            | Action::PowerOffMonitors
            | Action::PowerOnMonitors
            | Action::Logout
            | Action::PowerOff
            | Action::Reboot
            | Action::SwitchLayout(_)
            | Action::ToggleKeyboardShortcutsInhibit
    )
}

fn allowed_during_screenshot(action: &Action) -> bool {
    matches!(
        action,
        Action::Quit(_)
            | Action::ChangeVt(_)
            | Action::Suspend
            | Action::PowerOffMonitors
            | Action::PowerOnMonitors
            // Intended for binds such as volume up/down, lock the screen, etc.
            | Action::Spawn(_)
            | Action::SpawnSh(_)
            // The screenshot UI can handle these.
            | Action::MoveColumnLeft
            | Action::MoveColumnLeftOrToMonitorLeft
            | Action::MoveColumnRight
            | Action::MoveColumnRightOrToMonitorRight
            | Action::MoveWindowUp
            | Action::MoveWindowUpOrToWorkspaceUp
            | Action::MoveWindowDown
            | Action::MoveWindowDownOrToWorkspaceDown
            | Action::MoveColumnToMonitorLeft
            | Action::MoveColumnToMonitorRight
            | Action::MoveColumnToMonitorUp
            | Action::MoveColumnToMonitorDown
            | Action::MoveColumnToMonitorPrevious
            | Action::MoveColumnToMonitorNext
            | Action::MoveColumnToMonitor(_)
            | Action::MoveWindowToMonitorLeft
            | Action::MoveWindowToMonitorRight
            | Action::MoveWindowToMonitorUp
            | Action::MoveWindowToMonitorDown
            | Action::MoveWindowToMonitorPrevious
            | Action::MoveWindowToMonitorNext
            | Action::MoveWindowToMonitor(_)
            | Action::SetWindowWidth(_)
            | Action::SetWindowHeight(_)
            | Action::SetColumnWidth(_)
    )
}

fn hardcoded_overview_bind(raw: Keysym, mods: ModifiersState) -> Option<Bind> {
    let mods = modifiers_from_state(mods);
    if !mods.is_empty() {
        return None;
    }

    let mut repeat = true;
    let action = match raw {
        Keysym::Escape | Keysym::Return => {
            repeat = false;
            Action::ToggleOverview
        }
        Keysym::Left => Action::FocusColumnLeft,
        Keysym::Right => Action::FocusColumnRight,
        Keysym::Up => Action::FocusWindowOrWorkspaceUp,
        Keysym::Down => Action::FocusWindowOrWorkspaceDown,
        _ => {
            return None;
        }
    };

    Some(Bind {
        key: Key {
            trigger: Trigger::Keysym(raw),
            modifiers: Modifiers::empty(),
        },
        action,
        repeat,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: false,
        hotkey_overlay_title: None,
    })
}

pub fn apply_libinput_settings(config: &niri_config::Input, device: &mut input::Device) {
    // According to Mutter code, this setting is specific to touchpads.
    let is_touchpad = device.config_tap_finger_count() > 0;
    if is_touchpad {
        let c = &config.touchpad;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else if c.disabled_on_external_mouse {
            input::SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE
        } else {
            input::SendEventsMode::ENABLED
        });
        let _ = device.config_tap_set_enabled(c.tap);
        let _ = device.config_dwt_set_enabled(c.dwt);
        let _ = device.config_dwtp_set_enabled(c.dwtp);
        let _ = device.config_tap_set_drag_lock_enabled(if c.drag_lock {
            input::DragLockState::EnabledTimeout
        } else {
            input::DragLockState::Disabled
        });
        let _ = device.config_scroll_set_natural_scroll_enabled(c.natural_scroll);
        let _ = device.config_accel_set_speed(c.accel_speed.0);
        let _ = device.config_left_handed_set(c.left_handed);
        let _ = device.config_middle_emulation_set_enabled(c.middle_emulation);

        if let Some(drag) = c.drag {
            let _ = device.config_tap_set_drag_enabled(drag);
        } else {
            let default = device.config_tap_default_drag_enabled();
            let _ = device.config_tap_set_drag_enabled(default);
        }

        if let Some(accel_profile) = c.accel_profile {
            let _ = device.config_accel_set_profile(accel_profile.into());
        } else if let Some(default) = device.config_accel_default_profile() {
            let _ = device.config_accel_set_profile(default);
        }

        if let Some(method) = c.scroll_method {
            let _ = device.config_scroll_set_method(method.into());

            if method == niri_config::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        } else if let Some(default) = device.config_scroll_default_method() {
            let _ = device.config_scroll_set_method(default);

            if default == input::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        }

        if let Some(tap_button_map) = c.tap_button_map {
            let _ = device.config_tap_set_button_map(tap_button_map.into());
        } else if let Some(default) = device.config_tap_default_button_map() {
            let _ = device.config_tap_set_button_map(default);
        }

        if let Some(method) = c.click_method {
            let _ = device.config_click_set_method(method.into());
        } else if let Some(default) = device.config_click_default_method() {
            let _ = device.config_click_set_method(default);
        }
    }

    // This is how Mutter tells apart mice.
    let mut is_trackball = false;
    let mut is_trackpoint = false;
    if let Some(udev_device) = unsafe { device.udev_device() } {
        if udev_device.property_value("ID_INPUT_TRACKBALL").is_some() {
            is_trackball = true;
        }
        if udev_device
            .property_value("ID_INPUT_POINTINGSTICK")
            .is_some()
        {
            is_trackpoint = true;
        }
    }

    let is_mouse = device.has_capability(input::DeviceCapability::Pointer)
        && !is_touchpad
        && !is_trackball
        && !is_trackpoint;
    if is_mouse {
        let c = &config.mouse;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });
        let _ = device.config_scroll_set_natural_scroll_enabled(c.natural_scroll);
        let _ = device.config_accel_set_speed(c.accel_speed.0);
        let _ = device.config_left_handed_set(c.left_handed);
        let _ = device.config_middle_emulation_set_enabled(c.middle_emulation);

        if let Some(accel_profile) = c.accel_profile {
            let _ = device.config_accel_set_profile(accel_profile.into());
        } else if let Some(default) = device.config_accel_default_profile() {
            let _ = device.config_accel_set_profile(default);
        }

        if let Some(method) = c.scroll_method {
            let _ = device.config_scroll_set_method(method.into());

            if method == niri_config::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        } else if let Some(default) = device.config_scroll_default_method() {
            let _ = device.config_scroll_set_method(default);

            if default == input::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        }
    }

    if is_trackball {
        let c = &config.trackball;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });
        let _ = device.config_scroll_set_natural_scroll_enabled(c.natural_scroll);
        let _ = device.config_accel_set_speed(c.accel_speed.0);
        let _ = device.config_middle_emulation_set_enabled(c.middle_emulation);
        let _ = device.config_left_handed_set(c.left_handed);

        if let Some(accel_profile) = c.accel_profile {
            let _ = device.config_accel_set_profile(accel_profile.into());
        } else if let Some(default) = device.config_accel_default_profile() {
            let _ = device.config_accel_set_profile(default);
        }

        if let Some(method) = c.scroll_method {
            let _ = device.config_scroll_set_method(method.into());

            if method == niri_config::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        } else if let Some(default) = device.config_scroll_default_method() {
            let _ = device.config_scroll_set_method(default);

            if default == input::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        }
    }

    if is_trackpoint {
        let c = &config.trackpoint;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });
        let _ = device.config_scroll_set_natural_scroll_enabled(c.natural_scroll);
        let _ = device.config_accel_set_speed(c.accel_speed.0);
        let _ = device.config_left_handed_set(c.left_handed);
        let _ = device.config_middle_emulation_set_enabled(c.middle_emulation);

        if let Some(accel_profile) = c.accel_profile {
            let _ = device.config_accel_set_profile(accel_profile.into());
        } else if let Some(default) = device.config_accel_default_profile() {
            let _ = device.config_accel_set_profile(default);
        }

        if let Some(method) = c.scroll_method {
            let _ = device.config_scroll_set_method(method.into());

            if method == niri_config::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        } else if let Some(default) = device.config_scroll_default_method() {
            let _ = device.config_scroll_set_method(default);

            if default == input::ScrollMethod::OnButtonDown {
                if let Some(button) = c.scroll_button {
                    let _ = device.config_scroll_set_button(button);
                }
                let _ = device.config_scroll_set_button_lock(if c.scroll_button_lock {
                    input::ScrollButtonLockState::Enabled
                } else {
                    input::ScrollButtonLockState::Disabled
                });
            }
        }
    }

    let is_tablet = device.has_capability(input::DeviceCapability::TabletTool);
    if is_tablet {
        let c = &config.tablet;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });

        #[rustfmt::skip]
        const IDENTITY_MATRIX: [f32; 6] = [
            1., 0., 0.,
            0., 1., 0.,
        ];

        let _ = device.config_calibration_set_matrix(
            c.calibration_matrix
                .as_deref()
                .and_then(|m| m.try_into().ok())
                .or(device.config_calibration_default_matrix())
                .unwrap_or(IDENTITY_MATRIX),
        );

        let _ = device.config_left_handed_set(c.left_handed);
    }

    let is_touch = device.has_capability(input::DeviceCapability::Touch);
    if is_touch {
        let c = &config.touch;
        let _ = device.config_send_events_set_mode(if c.off {
            input::SendEventsMode::DISABLED
        } else {
            input::SendEventsMode::ENABLED
        });

        #[rustfmt::skip]
        const IDENTITY_MATRIX: [f32; 6] = [
            1., 0., 0.,
            0., 1., 0.,
        ];

        let _ = device.config_calibration_set_matrix(
            c.calibration_matrix
                .as_deref()
                .and_then(|m| m.try_into().ok())
                .or(device.config_calibration_default_matrix())
                .unwrap_or(IDENTITY_MATRIX),
        );
    }
}

pub fn mods_with_binds(mod_key: ModKey, binds: &Binds, triggers: &[Trigger]) -> HashSet<Modifiers> {
    let mut rv = HashSet::new();
    for bind in &binds.0 {
        if !triggers.contains(&bind.key.trigger) {
            continue;
        }

        let mut mods = bind.key.modifiers;
        if mods.contains(Modifiers::COMPOSITOR) {
            mods.remove(Modifiers::COMPOSITOR);
            mods.insert(mod_key.to_modifiers());
        }

        rv.insert(mods);
    }

    rv
}

pub fn mods_with_mouse_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::MouseLeft,
            Trigger::MouseRight,
            Trigger::MouseMiddle,
            Trigger::MouseBack,
            Trigger::MouseForward,
        ],
    )
}

pub fn mods_with_wheel_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::WheelScrollUp,
            Trigger::WheelScrollDown,
            Trigger::WheelScrollLeft,
            Trigger::WheelScrollRight,
        ],
    )
}

pub fn mods_with_finger_scroll_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::TouchpadScrollUp,
            Trigger::TouchpadScrollDown,
            Trigger::TouchpadScrollLeft,
            Trigger::TouchpadScrollRight,
        ],
    )
}

pub fn mods_with_tablet_stylus_binds(mod_key: ModKey, binds: &Binds) -> HashSet<Modifiers> {
    mods_with_binds(
        mod_key,
        binds,
        &[
            Trigger::TabletStylusButton1,
            Trigger::TabletStylusButton2,
            Trigger::TabletStylusButton3,
        ],
    )
}

fn grab_allows_hot_corner(grab: &(dyn PointerGrab<State> + 'static)) -> bool {
    let grab = grab.as_any();

    // We lean on the blocklist approach here since it's not a terribly big deal if hot corner
    // works where it shouldn't, but it could prevent some workflows if the hot corner doesn't work
    // when it should.
    //
    // Some notable grabs not mentioned here:
    // - DnDGrab allows hot corner to DnD across workspaces.
    // - ClickGrab keeps pointer focus on the window, so the hot corner doesn't trigger.
    // - Touch grabs: touch doesn't trigger the hot corner.
    if grab.is::<ResizeGrab>() || grab.is::<SpatialMovementGrab>() {
        return false;
    }

    if let Some(grab) = grab.downcast_ref::<MoveGrab>() {
        // Window move allows hot corner to DnD across workspaces.
        if !grab.is_move() {
            return false;
        }
    }

    true
}

/// Returns an iterator over bindings.
///
/// Includes dynamically populated bindings like the MRU UI.
fn make_binds_iter<'a>(
    config: &'a Config,
    mru: &'a mut WindowMruUi,
    mods: Modifiers,
) -> impl Iterator<Item = &'a Bind> + Clone {
    // Figure out the binds to use depending on whether the MRU is enabled and/or open.
    let general_binds = (!mru.is_open()).then_some(config.binds.0.iter());
    let general_binds = general_binds.into_iter().flatten();

    let mru_binds =
        (config.recent_windows.on || mru.is_open()).then_some(config.recent_windows.binds.iter());
    let mru_binds = mru_binds.into_iter().flatten();

    let mru_open_binds = mru.is_open().then(|| mru.opened_bindings(mods));
    let mru_open_binds = mru_open_binds.into_iter().flatten();

    // General binds take precedence over the MRU binds.
    general_binds.chain(mru_binds).chain(mru_open_binds)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::animation::Clock;

    #[test]
    fn bindings_suppress_keys() {
        let close_keysym = Keysym::q;
        let bindings = Binds(vec![Bind {
            key: Key {
                trigger: Trigger::Keysym(close_keysym),
                modifiers: Modifiers::COMPOSITOR | Modifiers::CTRL,
            },
            action: Action::CloseWindow,
            repeat: true,
            cooldown: None,
            allow_when_locked: false,
            allow_inhibiting: true,
            hotkey_overlay_title: None,
        }]);

        let comp_mod = ModKey::Super;
        let mut suppressed_keys = HashSet::new();

        let screenshot_ui = ScreenshotUi::new(Clock::default(), Default::default());
        let disable_power_key_handling = false;
        let is_inhibiting_shortcuts = Cell::new(false);

        // The key_code we pick is arbitrary, the only thing
        // that matters is that they are different between cases.

        let close_key_code = Keycode::from(close_keysym.raw() + 8u32);
        let close_key_event = |suppr: &mut HashSet<Keycode>, mods: ModifiersState, pressed| {
            should_intercept_key(
                suppr,
                &bindings.0,
                &[],
                &[],
                false,
                comp_mod,
                close_key_code,
                close_keysym,
                Some(close_keysym),
                pressed,
                mods,
                &screenshot_ui,
                disable_power_key_handling,
                is_inhibiting_shortcuts.get(),
            )
        };

        // Key event with the code which can't trigger any action.
        let none_key_event = |suppr: &mut HashSet<Keycode>, mods: ModifiersState, pressed| {
            should_intercept_key(
                suppr,
                &bindings.0,
                &[],
                &[],
                false,
                comp_mod,
                Keycode::from(Keysym::l.raw() + 8),
                Keysym::l,
                Some(Keysym::l),
                pressed,
                mods,
                &screenshot_ui,
                disable_power_key_handling,
                is_inhibiting_shortcuts.get(),
            )
        };

        let mut mods = ModifiersState {
            logo: true,
            ctrl: true,
            ..Default::default()
        };

        // Action press/release.

        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(
            filter,
            FilterResult::Intercept(Some(Bind {
                action: Action::CloseWindow,
                ..
            }))
        ));
        assert!(suppressed_keys.contains(&close_key_code));

        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Intercept(None)));
        assert!(suppressed_keys.is_empty());

        // Remove mod to make it for a binding.

        mods.shift = true;
        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(filter, FilterResult::Forward));

        mods.shift = false;
        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Forward));

        // Just none press/release.

        let filter = none_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(filter, FilterResult::Forward));

        let filter = none_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Forward));

        // Press action, press arbitrary, release action, release arbitrary.

        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(
            filter,
            FilterResult::Intercept(Some(Bind {
                action: Action::CloseWindow,
                ..
            }))
        ));

        let filter = none_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(filter, FilterResult::Forward));

        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Intercept(None)));

        let filter = none_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Forward));

        // Trigger and remove all mods.

        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(
            filter,
            FilterResult::Intercept(Some(Bind {
                action: Action::CloseWindow,
                ..
            }))
        ));

        mods = Default::default();
        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Intercept(None)));

        // Ensure that no keys are being suppressed.
        assert!(suppressed_keys.is_empty());

        // Now test shortcut inhibiting.

        // With inhibited shortcuts, we don't intercept our shortcut.
        is_inhibiting_shortcuts.set(true);

        mods = ModifiersState {
            logo: true,
            ctrl: true,
            ..Default::default()
        };

        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(filter, FilterResult::Forward));
        assert!(suppressed_keys.is_empty());

        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Forward));
        assert!(suppressed_keys.is_empty());

        // Toggle it off after pressing the shortcut.
        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(filter, FilterResult::Forward));
        assert!(suppressed_keys.is_empty());

        is_inhibiting_shortcuts.set(false);

        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Forward));
        assert!(suppressed_keys.is_empty());

        // Toggle it on after pressing the shortcut.
        let filter = close_key_event(&mut suppressed_keys, mods, true);
        assert!(matches!(
            filter,
            FilterResult::Intercept(Some(Bind {
                action: Action::CloseWindow,
                ..
            }))
        ));
        assert!(suppressed_keys.contains(&close_key_code));

        is_inhibiting_shortcuts.set(true);

        let filter = close_key_event(&mut suppressed_keys, mods, false);
        assert!(matches!(filter, FilterResult::Intercept(None)));
        assert!(suppressed_keys.is_empty());
    }

    #[test]
    fn comp_mod_handling() {
        let bindings = Binds(vec![
            Bind {
                key: Key {
                    trigger: Trigger::Keysym(Keysym::q),
                    modifiers: Modifiers::COMPOSITOR,
                },
                action: Action::CloseWindow,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                allow_inhibiting: true,
                hotkey_overlay_title: None,
            },
            Bind {
                key: Key {
                    trigger: Trigger::Keysym(Keysym::h),
                    modifiers: Modifiers::SUPER,
                },
                action: Action::FocusColumnLeft,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                allow_inhibiting: true,
                hotkey_overlay_title: None,
            },
            Bind {
                key: Key {
                    trigger: Trigger::Keysym(Keysym::j),
                    modifiers: Modifiers::empty(),
                },
                action: Action::FocusWindowDown,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                allow_inhibiting: true,
                hotkey_overlay_title: None,
            },
            Bind {
                key: Key {
                    trigger: Trigger::Keysym(Keysym::k),
                    modifiers: Modifiers::COMPOSITOR | Modifiers::SUPER,
                },
                action: Action::FocusWindowUp,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                allow_inhibiting: true,
                hotkey_overlay_title: None,
            },
            Bind {
                key: Key {
                    trigger: Trigger::Keysym(Keysym::l),
                    modifiers: Modifiers::SUPER | Modifiers::ALT,
                },
                action: Action::FocusColumnRight,
                repeat: true,
                cooldown: None,
                allow_when_locked: false,
                allow_inhibiting: true,
                hotkey_overlay_title: None,
            },
        ]);

        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::q),
                ModifiersState {
                    logo: true,
                    ..Default::default()
                }
            )
            .as_ref(),
            Some(&bindings.0[0])
        );
        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::q),
                ModifiersState::default(),
            ),
            None,
        );

        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::h),
                ModifiersState {
                    logo: true,
                    ..Default::default()
                }
            )
            .as_ref(),
            Some(&bindings.0[1])
        );
        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::h),
                ModifiersState::default(),
            ),
            None,
        );

        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::j),
                ModifiersState {
                    logo: true,
                    ..Default::default()
                }
            ),
            None,
        );
        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::j),
                ModifiersState::default(),
            )
            .as_ref(),
            Some(&bindings.0[2])
        );

        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::k),
                ModifiersState {
                    logo: true,
                    ..Default::default()
                }
            )
            .as_ref(),
            Some(&bindings.0[3])
        );
        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::k),
                ModifiersState::default(),
            ),
            None,
        );

        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::l),
                ModifiersState {
                    logo: true,
                    alt: true,
                    ..Default::default()
                }
            )
            .as_ref(),
            Some(&bindings.0[4])
        );
        assert_eq!(
            find_configured_bind(
                &bindings.0,
                ModKey::Super,
                Trigger::Keysym(Keysym::l),
                ModifiersState {
                    logo: true,
                    ..Default::default()
                },
            ),
            None,
        );
    }
}
