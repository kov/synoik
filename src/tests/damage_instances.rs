// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! What the damage tracker owes an element that is drawn in more than one place.
//!
//! One cached texture drawn several times in a frame — the same `Id` at several geometries — is
//! supported by design (`ElementState.last_instances`), and we lean on it: a window appears in the
//! workspace *and* in every thumbnail of the peek strip that shows its workspace, from one buffer.
//!
//! The tracker decides per instance: an instance whose geometry matches any remembered one takes
//! the cheap branch and reports only the element's own damage; one that matches none damages its
//! new geometry *plus every remembered instance*, so a moved instance heals the rect it left. That
//! leaves exactly one hole, and it is the reason this file exists — see the test.
//!
//! Pinned here rather than through the compositor because the tracker is what is being asserted
//! about: no renderer, no GPU, no scene. `damage_output` is renderer-free.

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{Element, Id, Kind};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet};
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Size, Transform};

/// An element that is nothing but an id and a rect, which is all the tracker reads.
#[derive(Debug, Clone)]
struct Patch {
    id: Id,
    geometry: Rectangle<i32, Physical>,
    commit: CommitCounter,
}

impl Patch {
    fn new(id: &Id, x: i32, y: i32, w: i32, h: i32) -> Self {
        Patch {
            id: id.clone(),
            geometry: Rectangle::new(Point::from((x, y)), Size::from((w, h))),
            commit: CommitCounter::default(),
        }
    }
}

impl Element for Patch {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        Rectangle::from_size(Size::from((
            self.geometry.size.w as f64,
            self.geometry.size.h as f64,
        )))
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)])
        }
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

fn tracker() -> OutputDamageTracker {
    OutputDamageTracker::new(Size::from((800, 600)), 1., Transform::Normal)
}

/// The damage reported over `frames`, unioned — what a screen `frames` buffers deep is told to
/// repaint before it comes back to the buffer it started on.
fn covered(damage: &[Vec<Rectangle<i32, Physical>>], rect: Rectangle<i32, Physical>) -> bool {
    let mut left = vec![rect];
    for frame in damage {
        left = Rectangle::subtract_rects_many(left, frame.iter().copied());
    }
    left.is_empty()
}

/// A vacated instance leaves no damage when the count shrinks, and nothing later heals it.
///
/// The tracker matches an instance against *any* remembered one (`instance_matches` is an `.any()`)
/// and `elements_gone` is keyed on an Id being absent altogether. So when one of an Id's instances
/// goes away while another stays put, the survivor takes the cheap branch and the Id is still
/// present: the rect the departed instance covered is asked for by nobody, in that frame or any
/// after it. On a screen that cycles buffers the pixels sit in the buffer that missed the repaint
/// and surface every time it comes round — which is what a workspace thumbnail alternating between
/// two positions at frame parity looks like.
#[test]
#[ignore = "the gap is real and unfixed: the vacated rect is asked for by nobody, ever. The fix is \
            in the smithay fork (damage every remembered instance when the count shrinks) and \
            shipping it means pushing the fork, so it waits for that. Un-ignore with the fix"]
fn a_departed_instance_leaves_its_rect_behind() {
    let id = Id::new();
    let other = Id::new();
    let mut tracker = tracker();

    let backdrop = Patch::new(&other, 0, 0, 800, 600);
    let stays = Patch::new(&id, 10, 10, 100, 100);
    let leaves = Patch::new(&id, 400, 300, 100, 100);
    let vacated = leaves.geometry;

    // Both instances on screen.
    tracker
        .damage_output(0, &[backdrop.clone(), stays.clone(), leaves])
        .unwrap();

    // One of them goes, and later something else changes elsewhere — the frames a two-buffer
    // screen gets before it shows the buffer that was current when both were up.
    let mut damage = Vec::new();
    for frame in [
        vec![backdrop.clone(), stays.clone()],
        vec![backdrop.clone(), stays.clone()],
    ] {
        let (rects, _) = tracker.damage_output(1, &frame).unwrap();
        damage.push(rects.cloned().unwrap_or_default());
    }

    assert!(
        covered(&damage, vacated),
        "the rect an instance vacated was never repainted: asked for {damage:?}"
    );
}

/// The same shape, with the instance coming back somewhere else: the arrival damages its new
/// geometry and every remembered instance, so the vacated rect is healed after all — as long as it
/// happens in a frame the missing buffer still sees.
#[test]
fn an_instance_that_moves_in_one_frame_heals_where_it_was() {
    let id = Id::new();
    let other = Id::new();
    let mut tracker = tracker();

    let backdrop = Patch::new(&other, 0, 0, 800, 600);
    let stays = Patch::new(&id, 10, 10, 100, 100);
    let before = Patch::new(&id, 400, 300, 100, 100);
    let after = Patch::new(&id, 430, 300, 100, 100);
    let vacated = before.geometry;

    tracker
        .damage_output(0, &[backdrop.clone(), stays.clone(), before])
        .unwrap();

    let (rects, _) = tracker.damage_output(1, &[backdrop, stays, after]).unwrap();
    let damage = vec![rects.cloned().unwrap_or_default()];

    assert!(
        covered(&damage, vacated),
        "a moved instance did not heal the rect it left: asked for {damage:?}"
    );
}
