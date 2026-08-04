// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The dash as a dock — **a deliberate divergence from GNOME**, see
//! `docs/fork/dock-divergence.md`.
//!
//! gnome-shell's dash exists only inside the overview. Here it also slides up from the bottom
//! edge when the pointer *pushes* into it, the way the hot corner opens the overview: same
//! [`Barrier`](crate::input::pressure::Barrier), same reason — an edge that triggers on contact
//! is an edge you trip over. It is the same [`Dash`](crate::ui::dash::Dash) the overview draws,
//! so favorites, running dots, hover and drag behave identically in both places; only where it
//! is drawn differs.
//!
//! Pure overlay: it reserves no space and relayouts nothing, so a window under it is unaware.

use std::time::Duration;

use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock, Curve};
use crate::input::pressure::{Barrier, Edge, Segment};
use crate::utils::output_size;

/// How far the pointer must push into the bottom edge, and within how long. The hot corner's
/// numbers (`layout.js:24-25`): a corner is harder to hit by accident than a full edge, but the
/// barrier already discounts travel *along* an edge, which is what a bottom edge mostly sees.
const THRESHOLD: f64 = crate::input::pressure::HOT_CORNER_THRESHOLD;
const TIMEOUT: Duration = crate::input::pressure::HOT_CORNER_TIMEOUT;

/// The slide, matching the dash's own settle animation (`dash.js` uses 200 ms eases).
const SLIDE_MS: u64 = 200;

/// How long the dock waits after the pointer leaves before sliding away. Long enough to cross a
/// gap between two icons, or to overshoot the dock's top edge and come back.
const HIDE_DELAY: Duration = Duration::from_millis(300);

enum State {
    Hidden,
    Showing(Animation),
    Shown,
    Hiding(Animation),
}

pub struct Dock {
    state: State,
    /// The output it is showing on — the one whose bottom edge was pushed.
    output: Option<Output>,
    /// Whether the pointer is over the dock's area, and when it last left.
    hovered: bool,
    leave_at: Option<Duration>,
    /// Held open regardless of the pointer while an icon is being dragged out of it.
    dragging: bool,
    barrier: Barrier,
    clock: Clock,
}

impl Dock {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: State::Hidden,
            output: None,
            hovered: false,
            leave_at: None,
            dragging: false,
            barrier: Barrier::new(THRESHOLD, TIMEOUT),
            clock,
        }
    }

    /// The bottom edge of `output`, as the barrier's one segment.
    fn segment(output: &Output) -> Segment {
        Segment {
            edge: Edge::Bottom,
            from: 0.,
            to: output_size(output).w,
        }
    }

    /// Feed one clamped motion to the dock's barrier; `true` means it just tripped.
    ///
    /// Same shape as the hot corner's ([`Synoik::push_hot_corner`](crate::synoik::Synoik)): the
    /// pressure is the motion the output clamp discarded.
    pub fn push(
        &mut self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
        discarded: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        time: Duration,
    ) -> bool {
        // Pressure built against one output's edge doesn't carry to another's.
        if self.output.as_ref().is_some_and(|o| o != output) && !self.is_visible() {
            self.barrier.leave();
        }

        let size = output_size(output);
        let segment = Self::segment(output);
        if !segment.contains(size, pos_within_output) {
            self.barrier.leave();
            return false;
        }

        match segment.push(size, pos_within_output, discarded, delta, time) {
            Some(push) => self.barrier.push(push),
            None => false,
        }
    }

    /// Slide the dock out on `output`, or restart its hide timer if it is already out.
    pub fn show(&mut self, output: &Output) {
        self.leave_at = None;

        if self.output.as_ref() != Some(output) {
            self.output = Some(output.clone());
            self.state = State::Hidden;
        }

        let from = match &self.state {
            State::Shown | State::Showing(_) => return,
            // Reverse out of a hide rather than snapping to the bottom and starting over.
            State::Hiding(anim) => anim.clamped_value(),
            State::Hidden => 0.,
        };
        self.state = State::Showing(self.slide(from, 1.));
    }

    /// Start the slide away. A no-op if it is already going or gone.
    pub fn hide(&mut self) {
        let from = match &self.state {
            State::Hidden | State::Hiding(_) => return,
            State::Shown => 1.,
            State::Showing(anim) => anim.clamped_value(),
        };
        self.leave_at = None;
        self.state = State::Hiding(self.slide(from, 0.));
    }

    /// Hide without animating — for the cases where the dock must simply not be there
    /// (the session locking, the output going away).
    pub fn dismiss(&mut self) {
        self.state = State::Hidden;
        self.output = None;
        self.hovered = false;
        self.leave_at = None;
        self.dragging = false;
        self.barrier.leave();
    }

    fn slide(&self, from: f64, to: f64) -> Animation {
        Animation::ease(
            self.clock.clone(),
            from,
            to,
            0.,
            SLIDE_MS,
            Curve::EaseOutQuad,
        )
    }

    pub fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    /// Whether the dock is on screen at all (including mid-slide, in either direction).
    pub fn is_visible(&self) -> bool {
        !matches!(self.state, State::Hidden)
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    /// How far out the dock is: 0 fully hidden, 1 fully out.
    fn progress(&self) -> f64 {
        match &self.state {
            State::Hidden => 0.,
            State::Shown => 1.,
            State::Showing(anim) | State::Hiding(anim) => anim.clamped_value(),
        }
    }

    /// The dash's area on `output` — full width, dash-height, bottom-anchored, slid down by
    /// however much of the animation is left. The [`Dash`](crate::ui::dash::Dash) centres its
    /// own pill inside whatever area it is given, exactly as it does in the overview.
    ///
    /// `None` when the dock isn't on this output, or isn't out at all.
    pub fn area(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        if !self.is_visible() || self.output.as_ref() != Some(output) {
            return None;
        }

        let size = output_size(output);
        let h = crate::ui::dash::preferred_height(size);
        let hidden_by = (1. - self.progress()) * h;
        Some(Rectangle::new(
            Point::from((0., size.h - h + hidden_by)),
            Size::from((size.w, h)),
        ))
    }

    /// The area a *pointer* counts as being on the dock, which is the resting area — using the
    /// sliding one would make the dock chase the pointer away from itself mid-animation.
    fn hover_area(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        if !self.is_visible() || self.output.as_ref() != Some(output) {
            return None;
        }
        let size = output_size(output);
        let h = crate::ui::dash::preferred_height(size);
        Some(Rectangle::new(
            Point::from((0., size.h - h)),
            Size::from((size.w, h)),
        ))
    }

    /// Track the pointer. Returns `true` if the dock's state changed enough to want a redraw.
    pub fn pointer_motion(&mut self, output: &Output, pos_within_output: Point<f64, Logical>) {
        let over = self
            .hover_area(output)
            .is_some_and(|area| area.contains(pos_within_output));

        if over == self.hovered {
            return;
        }
        self.hovered = over;
        self.leave_at = (!over).then(|| self.clock.now_unadjusted() + HIDE_DELAY);
    }

    /// The pointer left the dock's output entirely.
    pub fn pointer_left(&mut self) {
        if self.hovered {
            self.hovered = false;
            self.leave_at = Some(self.clock.now_unadjusted() + HIDE_DELAY);
        }
    }

    /// An icon is being dragged out of the dock: hold it open until the drag ends, however far
    /// the pointer wanders.
    ///
    /// Idempotent, and it has to be: the caller re-states the drag every frame (it is derived
    /// from `app_drag`, so no drag-ending path can forget it). Acting on the *value* rather than
    /// the change re-armed the hide deadline on every frame, which pushed it forward forever and
    /// left the dock on screen for good — invisible to a test that calls this once.
    pub fn set_dragging(&mut self, dragging: bool) {
        if dragging == self.dragging {
            return;
        }
        self.dragging = dragging;
        if dragging {
            self.leave_at = None;
        } else if !self.hovered && self.is_visible() {
            self.leave_at = Some(self.clock.now_unadjusted() + HIDE_DELAY);
        }
    }

    /// Advance the state machine: settle finished slides, and hide once the grace period after
    /// the pointer left has run out.
    pub fn advance_animations(&mut self) {
        match &self.state {
            State::Showing(anim) if anim.is_clamped_done() => self.state = State::Shown,
            State::Hiding(anim) if anim.is_clamped_done() => {
                self.state = State::Hidden;
                self.output = None;
                self.hovered = false;
                self.leave_at = None;
            }
            _ => (),
        }

        if self.dragging {
            return;
        }
        if let Some(at) = self.leave_at {
            if self.clock.now_unadjusted() >= at {
                self.hide();
            }
        }
    }

    /// When the hide timer wants a frame, so an idle session still slides it away.
    pub fn next_wakeup(&self) -> Option<Duration> {
        (!self.dragging).then_some(self.leave_at).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushing into the bottom edge builds pressure the same way the hot corner does: contact
    /// alone is not enough.
    #[test]
    fn the_bottom_edge_needs_pressure() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock);
        let output = crate::utils::test_output(1920, 1080);

        let at_edge = Point::from((960., 1079.));
        let push = Point::from((0., 20.));

        // Sliding along the edge builds nothing, however far it travels.
        for i in 0..40 {
            assert!(!dock.push(
                &output,
                at_edge,
                Point::from((0., 4.)),
                Point::from((40., 4.)),
                Duration::from_millis(i * 16)
            ));
        }

        // Pushing into it does.
        let mut tripped = false;
        for i in 0..10 {
            tripped |= dock.push(
                &output,
                at_edge,
                push,
                push,
                Duration::from_millis(1000 + i * 16),
            );
        }
        assert!(tripped, "sustained pushing must trip the dock's barrier");
    }

    /// Away from the edge there is no barrier to push against.
    #[test]
    fn the_middle_of_the_screen_is_not_the_edge() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock);
        let output = crate::utils::test_output(1920, 1080);

        for i in 0..40 {
            assert!(!dock.push(
                &output,
                Point::from((960., 540.)),
                Point::from((0., 20.)),
                Point::from((0., 20.)),
                Duration::from_millis(i * 16)
            ));
        }
    }

    /// The dock slides up from below its resting place and settles there.
    #[test]
    fn it_slides_up_into_place() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        assert!(dock.area(&output).is_none(), "hidden: no area at all");

        dock.show(&output);
        let start = dock.area(&output).expect("showing").loc.y;

        clock.set_unadjusted(Duration::from_millis(SLIDE_MS / 2));
        let mid = dock.area(&output).expect("mid-slide").loc.y;

        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        let end = dock.area(&output).expect("shown").loc.y;

        assert!(mid < start, "it must travel upward, not down");
        assert!(end < mid, "and keep going");

        let h = crate::ui::dash::preferred_height(output_size(&output));
        assert_eq!(
            end,
            output_size(&output).h - h,
            "it settles flush with the bottom edge"
        );
        assert!(!dock.are_animations_ongoing(), "and stops animating");
    }

    /// The pointer leaving starts a grace period, not an immediate hide — and coming back
    /// inside it cancels the hide entirely.
    #[test]
    fn leaving_hides_after_a_grace_period() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);
        let inside = Point::from((960., 1050.));
        let outside = Point::from((960., 400.));

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        dock.pointer_motion(&output, inside);

        dock.pointer_motion(&output, outside);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS) + HIDE_DELAY / 2);
        dock.advance_animations();
        assert!(
            !dock.are_animations_ongoing(),
            "it must not start hiding inside the grace period"
        );
        // A dock just sitting there asks for no frames, so the grace period has nothing to
        // elapse on unless it also asks the event loop to wake — the trap that left it on
        // screen forever the first time it ran live.
        assert!(
            dock.next_wakeup().is_some(),
            "leaving must arm a wake-up, or an idle session never hides it"
        );

        // Back on the dock: the hide is called off.
        dock.pointer_motion(&output, inside);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS) + HIDE_DELAY * 2);
        dock.advance_animations();
        assert!(dock.is_visible(), "returning must cancel the hide");
        assert!(!dock.are_animations_ongoing());

        // Leaving for good does hide it.
        dock.pointer_motion(&output, outside);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS) + HIDE_DELAY * 4);
        dock.advance_animations();
        assert!(dock.are_animations_ongoing(), "now it slides away");
    }

    /// The compositor re-states the drag every frame, so a *steady* "not dragging" must leave
    /// the hide deadline alone. Re-arming it per frame pushed it forward forever and the dock
    /// never went away — caught on the seat, not here, the first time round.
    #[test]
    fn restating_the_drag_does_not_postpone_the_hide() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        dock.pointer_motion(&output, Point::from((960., 1050.)));
        dock.pointer_motion(&output, Point::from((960., 300.)));

        let deadline = dock.next_wakeup().expect("leaving arms a hide");

        // Frames keep coming, each re-stating "no drag in progress".
        for ms in [210, 220, 230, 240] {
            clock.set_unadjusted(Duration::from_millis(ms));
            dock.set_dragging(false);
            dock.advance_animations();
        }
        assert_eq!(
            dock.next_wakeup(),
            Some(deadline),
            "the hide deadline must not move just because frames happened"
        );

        clock.set_unadjusted(Duration::from_millis(SLIDE_MS) + HIDE_DELAY * 2);
        dock.set_dragging(false);
        dock.advance_animations();
        assert!(dock.are_animations_ongoing(), "and it does eventually hide");
    }

    /// A drag holds the dock open however far the pointer wanders, and releasing re-arms the
    /// hide (`set_dragging(false)` with the pointer away).
    #[test]
    fn a_drag_holds_the_dock_open() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        dock.pointer_motion(&output, Point::from((960., 1050.)));
        dock.set_dragging(true);

        dock.pointer_motion(&output, Point::from((960., 200.)));
        clock.set_unadjusted(Duration::from_secs(5));
        dock.advance_animations();
        assert!(!dock.are_animations_ongoing(), "a drag pins it open");
        assert!(dock.next_wakeup().is_none(), "and arms no hide timer");

        dock.set_dragging(false);
        clock.set_unadjusted(Duration::from_secs(5) + HIDE_DELAY * 2);
        dock.advance_animations();
        assert!(dock.are_animations_ongoing(), "dropping releases it");
    }
}
