//! Dragging a workspace thumbnail along the overview strip to reorder the workspaces.
//!
//! **Divergence (approved 2026-07-28).** gnome-shell's thumbnails never reorder: a drag on
//! that strip is only ever a *window* being moved to another workspace, which we keep. This
//! adds macOS Mission Control's other gesture on top, and the two are told apart by what
//! the press landed on — a thumbnail reorders, a window preview still moves the window.
//!
//! Like [`MoveGrab`](super::move_grab::MoveGrab), the grab starts out only *recognizing*:
//! under the movement threshold the release is a plain click, and the workspace is
//! activated exactly as pressing a thumbnail always did.

use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::utils::{Logical, Point};

use crate::niri::State;

/// How far the pointer must travel before a press on a thumbnail is a reorder rather than
/// a click that switches workspace. The same threshold interactive window moves use.
const DRAG_THRESHOLD: f64 = 8.;

pub struct ThumbGrab {
    start_data: PointerGrabStartData<State>,
    output: Output,
    /// The workspace the press landed on, by index at press time.
    idx: usize,
    /// Whether the pointer has moved far enough for this to be a drag.
    armed: bool,
}

impl ThumbGrab {
    pub fn new(start_data: PointerGrabStartData<State>, output: Output, idx: usize) -> Self {
        Self {
            start_data,
            output,
            idx,
            armed: false,
        }
    }

    /// Feeds the pointer position in, arming the drag once it has moved far enough.
    fn update(&mut self, data: &mut State, location: Point<f64, Logical>) {
        let Some((output, pos_within_output)) = data.niri.output_under(location) else {
            return;
        };
        if *output != self.output {
            return;
        }
        let output = output.clone();

        if !self.armed {
            let c = location - self.start_data.location;
            if c.x * c.x + c.y * c.y < DRAG_THRESHOLD * DRAG_THRESHOLD {
                return;
            }
            let Some(mon) = data.niri.layout.monitor_for_output_mut(&output) else {
                return;
            };
            // The press position, not the current one: the thumbnail must keep the grab
            // offset it was picked up with, or it jumps by the threshold on arming.
            if !mon.begin_thumb_drag(self.idx, pos_within_output - c) {
                return;
            }
            self.armed = true;
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Grabbing));
        }

        if let Some(mon) = data.niri.layout.monitor_for_output_mut(&output) {
            mon.update_thumb_drag(pos_within_output);
        }
        data.niri.queue_redraw(&output);
    }

    fn on_ungrab(&mut self, data: &mut State) {
        if self.armed {
            if let Some(mon) = data.niri.layout.monitor_for_output_mut(&self.output) {
                mon.finish_thumb_drag();
            }
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
        } else {
            // Never became a drag, so it was a click on the thumbnail.
            data.activate_overview_workspace_at(&self.output, self.idx);
        }

        // FIXME: only redraw this output.
        data.niri.queue_redraw_all();
    }
}

impl PointerGrab<State> for ThumbGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);
        self.update(data, event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button) {
            // The button that initiated the grab was released.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}
