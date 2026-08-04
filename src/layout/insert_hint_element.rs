// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::utils::{Logical, Point, Rectangle, Size};
use synoik_config::CornerRadius;

use super::focus_ring::{FocusRing, FocusRingRenderElement};

#[derive(Debug)]
pub struct InsertHintElement {
    inner: FocusRing,
}

pub type InsertHintRenderElement = FocusRingRenderElement;

impl InsertHintElement {
    pub fn new(config: synoik_config::InsertHint) -> Self {
        Self {
            inner: FocusRing::new(synoik_config::FocusRing {
                off: config.off,
                width: 0.,
                active_color: config.color,
                inactive_color: config.color,
                urgent_color: config.color,
                active_gradient: config.gradient,
                inactive_gradient: config.gradient,
                urgent_gradient: config.gradient,
            }),
        }
    }

    pub fn update_config(&mut self, config: synoik_config::InsertHint) {
        self.inner.update_config(synoik_config::FocusRing {
            off: config.off,
            width: 0.,
            active_color: config.color,
            inactive_color: config.color,
            urgent_color: config.color,
            active_gradient: config.gradient,
            inactive_gradient: config.gradient,
            urgent_gradient: config.gradient,
        });
    }

    pub fn update_shaders(&mut self) {
        self.inner.update_shaders();
    }

    pub fn update_render_elements(
        &mut self,
        size: Size<f64, Logical>,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
    ) {
        self.inner
            .update_render_elements(size, true, false, false, view_rect, radius, scale, 1.);
    }

    pub fn render(
        &self,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        self.inner.render(location, push)
    }
}
