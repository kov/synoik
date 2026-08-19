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

/// How far out the dock rests while an app is demanding attention: enough of the icon to read
/// which app it is, not so much that it becomes a dock you did not ask for.
///
/// **Divergence.** gnome-shell's only urgency affordance is a notification
/// (`windowAttentionHandler.js`) and its dash shows nothing at all. The poke is deliberately
/// louder, and it costs no screen space — the dock reserves none.
const POKE_PROGRESS: f64 = 0.45;

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
    /// Held open the same way while a workspace peek is up — see [`Dock::set_peeked`].
    peeked: bool,
    /// Whether an app is demanding attention, which gives the dock a *resting* position above
    /// the edge instead of fully hidden. Not a hold: the dock still slides freely between the
    /// poke and fully out, the poke is just where "away" now means.
    poking: bool,
    /// Held open the same way while one of its icons has its context menu up — the menu pops
    /// *upward* out of the dash, so reaching it takes the pointer off the dock, and without
    /// this the dock slides away from under the menu a moment later.
    menu_open: bool,
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
            peeked: false,
            poking: false,
            menu_open: false,
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
            // Losing the edge forgets the pressure — which is correct, and is also the prime
            // suspect whenever pushing "does nothing": anything that moves the pointer off the
            // last row between pushes (a second, absolute pointing device re-stating the host
            // cursor's position, say) resets the barrier every time it happens.
            if self.barrier.pressure() > 0. {
                trace!(
                    "dock: left the bottom edge at y={:.1} (of {:.1}), dropping {:.0} px of pressure",
                    pos_within_output.y,
                    size.h,
                    self.barrier.pressure(),
                );
            }
            self.barrier.leave();
            return false;
        }

        let push = segment.push(size, pos_within_output, discarded, delta, time);
        let tripped = match push {
            Some(push) => self.barrier.push(push),
            None => false,
        };
        trace!(
            "dock: on the bottom edge at y={:.1}, delta=({:.1},{:.1}) discarded=({:.1},{:.1}) \
             push={:?} pressure={:.0}/{:.0}{}",
            pos_within_output.y,
            delta.x,
            delta.y,
            discarded.x,
            discarded.y,
            push.map(|p| (p.across, p.along)),
            self.barrier.pressure(),
            self.barrier.threshold(),
            if tripped { " TRIPPED" } else { "" },
        );
        tripped
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
        // To the floor, not to nothing: an app still wanting attention keeps its poke.
        self.state = State::Hiding(self.slide(from, self.floor()));
    }

    /// Drop any pressure accumulated on the bottom edge, and re-arm the barrier.
    ///
    /// For a caller that is refusing the edge for a reason the barrier cannot see — a fullscreen
    /// window owning it. Without this the pressure would sit there and the dock would spring out
    /// on the first motion after the window left fullscreen.
    pub fn forget_pressure(&mut self) {
        self.barrier.leave();
    }

    /// Hide without animating — for the cases where the dock must simply not be there
    /// (the session locking, the output going away).
    pub fn dismiss(&mut self) {
        self.state = State::Hidden;
        self.output = None;
        self.hovered = false;
        self.leave_at = None;
        self.dragging = false;
        self.peeked = false;
        self.poking = false;
        self.menu_open = false;
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
        !matches!(self.state, State::Hidden) || self.poking
    }

    /// Whether the dock is only out because an app wants attention — the state where it draws
    /// the urgent icons alone rather than the whole dash.
    ///
    /// True while *sliding* into (or back down to) the poke as well as at rest on it, and false
    /// whenever the dock is out for its own reasons. Keying this on `Hidden` alone meant the
    /// rise into a poke was drawn as the full dash and only snapped to icons when the slide
    /// finished — chrome flashing up the screen edge on every urgent window.
    pub fn is_poking(&self) -> bool {
        self.poking && !matches!(self.state, State::Shown | State::Showing(_))
    }

    /// How far out "away" is: the bottom edge normally, the poke while an app wants attention.
    fn floor(&self) -> f64 {
        if self.poking {
            POKE_PROGRESS
        } else {
            0.
        }
    }

    /// An app started or stopped demanding attention on `output`.
    ///
    /// Idempotent, and it has to be: the dash snapshot is re-stated on every sync, and re-arming
    /// the slide from a value it already holds would restart the animation every frame — the
    /// same shape as the hide deadline that once got pushed forward forever.
    pub fn set_poking(&mut self, output: Option<&Output>, poking: bool) {
        if self.poking == poking {
            return;
        }
        self.poking = poking;

        if poking {
            if self.output.is_none() {
                self.output = output.cloned();
            }
            // Rise into the poke only from rest; mid-slide or fully out, the floor change is
            // enough and the current animation still lands somewhere sensible.
            if matches!(self.state, State::Hidden) {
                self.state = State::Hiding(self.slide(0., POKE_PROGRESS));
            }
        } else if matches!(self.state, State::Hidden) {
            // Resting on the poke with the urgency gone: slide the rest of the way away.
            self.state = State::Hiding(self.slide(POKE_PROGRESS, 0.));
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    /// How far out the dock is: 0 fully hidden, 1 fully out.
    fn progress(&self) -> f64 {
        match &self.state {
            State::Hidden => self.floor(),
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

    /// Track the pointer.
    pub fn pointer_motion(&mut self, output: &Output, pos_within_output: Point<f64, Logical>) {
        // Re-arming the barrier belongs *here*, on the path every pointing device reaches — not
        // in `push`, which only relative motion calls.
        //
        // A barrier latches when it fires and stays latched until the pointer leaves, so that
        // resting against a triggered edge doesn't re-fire it every frame. If only the relative
        // path can release that latch, then on a seat where some *other* device delivers the
        // motion that carries the pointer away, the latch is never released at all: the dock
        // fires once and then every later push arrives against a barrier that refuses to
        // accumulate. This VM is exactly that seat — its pointer sends absolute events for
        // ordinary movement and switches to relative deltas only while the host cursor is pinned
        // against a screen edge, so *pushing* is relative and *leaving* is absolute.
        if !Self::segment(output).contains(output_size(output), pos_within_output) {
            self.barrier.leave();
        }

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
        // Off the output is off the edge — re-arm, for the reason in [`Self::pointer_motion`].
        self.barrier.leave();
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
        let was = self.held();
        self.dragging = dragging;
        self.held_changed(was);
    }

    /// An icon's context menu is up: hold the dock open until it closes, for the reason in
    /// [`Self::menu_open`].
    ///
    /// Re-stated every frame from the popover's own state like [`Self::set_dragging`], so no
    /// close path can forget it — and idempotent for the same reason: acting on the value
    /// rather than the change would re-arm the hide deadline every frame and leave the dock out
    /// for good.
    pub fn set_menu_open(&mut self, open: bool) {
        let was = self.held();
        self.menu_open = open;
        self.held_changed(was);
    }

    /// A workspace peek is up: hold the dock out alongside the thumbnail strip
    /// (`docs/fork/workspace-peek.md`). The peek shows both pieces of the overview's furniture,
    /// and the dock is what a drag onto a thumbnail starts from.
    ///
    /// Re-stated every frame from the peek's own state like [`Self::set_dragging`], and
    /// idempotent for the same reason.
    pub fn set_peeked(&mut self, peeked: bool) {
        let was = self.held();
        self.peeked = peeked;
        self.held_changed(was);
    }

    /// Whether something other than the pointer is keeping the dock out.
    fn held(&self) -> bool {
        self.dragging || self.menu_open || self.peeked
    }

    /// Re-arm (or cancel) the hide deadline when the last hold is released or the first taken.
    fn held_changed(&mut self, was: bool) {
        let held = self.held();
        if held == was {
            return;
        }
        if held {
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
                self.hovered = false;
                self.leave_at = None;
                // A poking dock is still on screen and still belongs to its output.
                if !self.poking {
                    self.output = None;
                }
            }
            _ => (),
        }

        if self.held() {
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
        (!self.held()).then_some(self.leave_at).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app demanding attention pokes its icon above the bottom edge while the dock is away.
    ///
    /// **Divergence.** gnome-shell's only urgency affordance is a notification
    /// (`windowAttentionHandler.js`); the dash shows nothing. A poke is louder on purpose, and it
    /// costs no screen space — the dock is an overlay that reserves none.
    #[test]
    fn urgency_pokes_the_dock_above_the_edge() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        assert!(dock.area(&output).is_none(), "nothing to show at rest");

        dock.set_poking(Some(&output), true);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        let poked = dock.area(&output).expect("urgency must put it on screen");

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS * 3));
        dock.advance_animations();
        let full = dock.area(&output).expect("shown").loc.y;
        assert!(
            poked.loc.y > full,
            "the poke sits lower than the shown dock ({} vs {full})",
            poked.loc.y
        );
        assert!(
            poked.loc.y < 1080.,
            "but still on screen, or there is nothing to see"
        );
    }

    /// The poke is a resting position, not a transient: it survives the hide the pointer leaving
    /// would otherwise complete, and only clearing the urgency puts the dock away.
    #[test]
    fn the_poke_outlives_a_hide_and_ends_with_the_urgency() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        dock.set_poking(Some(&output), true);
        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();

        dock.hide();
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS * 2));
        dock.advance_animations();
        assert!(
            dock.area(&output).is_some(),
            "hiding must fall back to the poke while the app still wants attention"
        );

        dock.set_poking(Some(&output), false);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS * 4));
        dock.advance_animations();
        assert!(
            dock.area(&output).is_none(),
            "and clearing the urgency puts it away"
        );
    }

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

    /// [`Dock::forget_pressure`] empties the edge, so pressure a caller *refused* — the bottom
    /// edge of a fullscreen window — cannot be banked and spent the moment it stops refusing.
    ///
    /// The pointer never leaves the edge here, which is the whole point: leaving re-arms the
    /// barrier by itself, so a test that moves away would pass with no `forget_pressure` at all.
    #[test]
    fn refused_pressure_is_forgotten_rather_than_banked() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock);
        let output = crate::utils::test_output(1920, 1080);

        let at_edge = Point::from((960., 1079.));
        let push = Point::from((0., 20.));
        let shove = |dock: &mut Dock, at: u64| {
            (0..6).any(|i| {
                dock.push(
                    &output,
                    at_edge,
                    push,
                    push,
                    Duration::from_millis(at + i * 8),
                )
            })
        };

        // Just under the trip: six capped pushes is 90 of the 100 needed.
        assert!(!shove(&mut dock, 0), "the setup must not trip on its own");
        dock.forget_pressure();

        // The same again would trip a barrier that had kept the first round — all twelve pushes
        // land inside the one-second window, so nothing was refunded by time.
        assert!(
            !shove(&mut dock, 48),
            "forgotten pressure must not be spendable later"
        );
    }

    /// The barrier is re-armed by the pointer *moving away*, whichever device moved it — not
    /// only by the relative path that feeds `push`.
    ///
    /// Seat regression (2026-08-04): this VM's pointer sends absolute events for ordinary
    /// movement and relative deltas only while the host cursor is pinned against a screen edge.
    /// So pushing is relative and leaving is absolute — and with the release living in `push`,
    /// the dock fired exactly once and then never again, every later push landing on a latched
    /// barrier with `pressure=0`. Leaving here goes through `pointer_motion` alone, with no
    /// `push` call at all, which is what the seat actually did.
    #[test]
    fn leaving_by_any_device_re_arms_the_barrier() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock);
        let output = crate::utils::test_output(1920, 1080);

        let at_edge = Point::from((960., 1079.));
        let push = Point::from((0., 20.));
        let shove = |dock: &mut Dock, at: u64| {
            let mut tripped = false;
            for i in 0..10 {
                tripped |= dock.push(
                    &output,
                    at_edge,
                    push,
                    push,
                    Duration::from_millis(at + i * 16),
                );
            }
            tripped
        };

        assert!(shove(&mut dock, 0), "the first push trips it");

        // The pointer is carried away by a device that never calls `push`.
        dock.pointer_motion(&output, Point::from((960., 400.)));

        assert!(
            shove(&mut dock, 2000),
            "a second push must trip it too — the barrier stayed latched from the first"
        );
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

    /// A context menu pins the dock the same way a drag does — the menu opens *upward* out of
    /// the dash, so reaching it takes the pointer off the dock and the hide timer would
    /// otherwise pull the dock out from under it.
    #[test]
    fn a_menu_holds_the_dock_open() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        dock.pointer_motion(&output, Point::from((960., 1050.)));
        dock.set_menu_open(true);

        // Up to the menu, well off the dock.
        dock.pointer_motion(&output, Point::from((960., 700.)));
        clock.set_unadjusted(Duration::from_secs(5));
        dock.advance_animations();
        assert!(dock.area(&output).is_some(), "an open menu pins it");
        assert!(dock.next_wakeup().is_none(), "and arms no hide timer");

        dock.set_menu_open(false);
        clock.set_unadjusted(Duration::from_secs(5) + HIDE_DELAY * 2);
        dock.advance_animations();
        assert!(
            dock.are_animations_ongoing(),
            "closing it releases the dock"
        );
    }

    /// Re-stated every frame, so it must be idempotent: acting on the value rather than the
    /// change would push the hide deadline forward forever (the bug `set_dragging` already
    /// carries a comment about).
    #[test]
    fn re_stating_the_menu_hold_does_not_re_arm_the_deadline() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut dock = Dock::new(clock.clone());
        let output = crate::utils::test_output(1920, 1080);

        dock.show(&output);
        clock.set_unadjusted(Duration::from_millis(SLIDE_MS));
        dock.advance_animations();
        dock.pointer_motion(&output, Point::from((960., 1050.)));
        dock.pointer_motion(&output, Point::from((960., 200.)));
        let armed = dock.next_wakeup().expect("leaving arms the hide");

        for _ in 0..10 {
            clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(16));
            dock.set_menu_open(false);
        }
        assert_eq!(
            dock.next_wakeup(),
            Some(armed),
            "re-stating `false` must not push the deadline out"
        );
    }
}
