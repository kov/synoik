// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::renderer::element::{Element, Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::Color32F;
use smithay::utils::{Physical, Rectangle, Scale};

use super::solid_color::SolidColorRenderElement;
use crate::synoik::OutputRenderElements;

pub fn push_opaque_regions(
    elem: &OutputRenderElements,
    scale: Scale<f64>,
    push: &mut dyn FnMut(OutputRenderElements),
) {
    // HACK
    if format!("{elem:?}").contains("ExtraDamage") {
        return;
    }

    let geo = elem.geometry(scale);
    let mut opaque = elem.opaque_regions(scale).to_vec();

    for rect in &mut opaque {
        rect.loc += geo.loc;
    }

    let semitransparent = geo.subtract_rects(opaque.iter().copied());

    for rect in opaque {
        let color = SolidColorRenderElement::new(
            Id::new(),
            rect.to_f64().to_logical(scale),
            CommitCounter::default(),
            Color32F::from([0., 0., 0.2, 0.2]),
            Kind::Unspecified,
        );
        push(color.into());
    }

    for rect in semitransparent {
        let color = SolidColorRenderElement::new(
            Id::new(),
            rect.to_f64().to_logical(scale),
            CommitCounter::default(),
            Color32F::from([0.3, 0., 0., 0.3]),
            Kind::Unspecified,
        );
        push(color.into());
    }
}

/// Insert the damage overlay's tint for `rects` (physical, output-relative).
///
/// `ids` is a persistent pool: slot *i* keeps its `Id` across frames, so a rect that does not move
/// contributes no damage of its own. Minting a fresh `Id` per rect per frame makes every tinted
/// region repaint every frame for as long as the overlay is on, and the overlay then perturbs the
/// thing it is there to show.
pub fn draw_damage(
    ids: &mut Vec<Id>,
    rects: &[Rectangle<i32, Physical>],
    scale: Scale<f64>,
    elements: &mut Vec<OutputRenderElements>,
) {
    let _span = tracy_client::span!("draw_damage");

    while ids.len() < rects.len() {
        ids.push(Id::new());
    }

    for (id, rect) in ids.iter().zip(rects) {
        let color = SolidColorRenderElement::new(
            id.clone(),
            rect.to_f64().to_logical(scale),
            CommitCounter::default(),
            Color32F::from([0.3, 0., 0., 0.3]),
            Kind::Unspecified,
        );
        elements.insert(0, OutputRenderElements::SolidColor(color));
    }
}

/// The damage the overlay itself contributed to the frame it was just drawn into, given the tint
/// rects of the previous frame and of this one.
///
/// smithay damages an element's old *and* new geometry when it moves, so a pool slot whose rect is
/// unchanged contributes nothing and one that changed contributes both. Subtracting exactly this
/// from the composed frame's damage is what keeps the overlay out of its own input: the tint is
/// composited into the primary plane, so its pixels are inside that plane's recorded damage and no
/// element-id filter can remove them. Left in, the churn reads as damage, gets tinted, and never
/// drains.
///
/// The cost is that real damage coinciding with tint churn is masked for one frame. That is the
/// right trade for a locator.
pub fn tint_damage(
    prev: &[Rectangle<i32, Physical>],
    now: &[Rectangle<i32, Physical>],
) -> Vec<Rectangle<i32, Physical>> {
    let mut contributed = Vec::new();
    for i in 0..prev.len().max(now.len()) {
        let (before, after) = (prev.get(i), now.get(i));
        if before == after {
            continue;
        }
        contributed.extend(before.copied());
        contributed.extend(after.copied());
    }
    contributed
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Size};

    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    /// A tint that did not move damages nothing, so nothing comes back off the composed damage.
    /// This is what makes the overlay converge: a still screen tints the same rects frame after
    /// frame and the reported damage drains to empty instead of feeding itself.
    #[test]
    fn an_unmoved_tint_contributes_no_damage() {
        let same = [rect(0, 0, 100, 100), rect(200, 200, 50, 50)];
        assert!(tint_damage(&same, &same).is_empty());
    }

    /// A slot that moved damages both where it was and where it is — smithay's rule for any
    /// element — and both have to be subtracted, or the region the tint just vacated reads as
    /// fresh damage next frame.
    #[test]
    fn a_moved_tint_contributes_both_of_its_rects() {
        let before = [rect(0, 0, 100, 100), rect(200, 200, 50, 50)];
        let after = [rect(0, 0, 100, 100), rect(300, 200, 50, 50)];

        let contributed = tint_damage(&before, &after);

        assert_eq!(
            contributed,
            vec![rect(200, 200, 50, 50), rect(300, 200, 50, 50)]
        );
    }

    /// Slots appearing and disappearing are the same case with one side missing.
    #[test]
    fn appearing_and_departing_tints_count_once() {
        assert_eq!(
            tint_damage(&[], &[rect(5, 5, 10, 10)]),
            vec![rect(5, 5, 10, 10)]
        );
        assert_eq!(
            tint_damage(&[rect(5, 5, 10, 10)], &[]),
            vec![rect(5, 5, 10, 10)]
        );
    }
}
