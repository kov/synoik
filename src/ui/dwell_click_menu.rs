// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The dwell-click indicator's menu — gnome-shell's `DwellClickIndicator`
//! (`js/ui/status/dwellClick.js`): four actions, each the click a window-mode dwell makes next.
//! The indicator's icon names the one in force, and follows it when a one-shot click falls back
//! to a single one (`ptr-a11y-dwell-click-type-changed`).

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Size};

use crate::input::pointer_a11y::DwellClickType;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Dir, Menu, MenuEntry, MenuHit, MenuItem};

/// `DWELL_CLICK_MODES`, in the order `_init` adds them (`dwellClick.js:11-32, 56-59`).
pub const MODES: [(DwellClickType, &str, &str); 4] = [
    (
        DwellClickType::Primary,
        "Single Click",
        "pointer-primary-click-symbolic",
    ),
    (
        DwellClickType::Double,
        "Double Click",
        "pointer-double-click-symbolic",
    ),
    (DwellClickType::Drag, "Drag", "pointer-drag-symbolic"),
    (
        DwellClickType::Secondary,
        "Secondary Click",
        "pointer-secondary-click-symbolic",
    ),
];

/// The indicator's icon for `click_type` (`_updateClickType`). A type no row offers — middle,
/// which nothing in the shell picks — keeps the single click's.
pub fn icon(click_type: DwellClickType) -> &'static str {
    MODES
        .iter()
        .find(|(t, _, _)| *t == click_type)
        .map_or(MODES[0].2, |(_, _, icon)| icon)
}

pub struct DwellClickMenu {
    menu: Menu,
}

impl Default for DwellClickMenu {
    fn default() -> Self {
        Self::new()
    }
}

impl DwellClickMenu {
    pub fn new() -> Self {
        let entries = MODES
            .iter()
            .enumerate()
            .map(|(id, (_, label, icon))| {
                MenuEntry::Item(MenuItem::new(id as u64, *label).with_icon(*icon))
            })
            .collect();
        Self {
            menu: Menu::new(entries),
        }
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.menu.logical_size()
    }

    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        self.menu.row_center(label)
    }

    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let hit = self.menu.pointer_click(pos);
        Self::resolve(hit)
    }

    pub fn focused_label(&self) -> Option<String> {
        self.menu.focused_label()
    }

    pub fn nav(&mut self, dir: Dir) -> bool {
        self.menu.nav(dir)
    }

    pub fn activate_focused(&mut self) -> PopoverAction {
        let hit = self.menu.activate_focused();
        Self::resolve(hit)
    }

    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        self.menu.pointer_hover(pos)
    }

    fn resolve(hit: MenuHit) -> PopoverAction {
        let MenuHit::Activated(id) = hit else {
            return PopoverAction::Consumed;
        };
        MODES
            .get(id as usize)
            .map_or(PopoverAction::Consumed, |(t, _, _)| {
                PopoverAction::SetDwellClickType(*t)
            })
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        match self.menu.bake(renderer, scale) {
            Ok(buffer) => vec![TextureRenderElement::from_texture_buffer(
                buffer,
                origin,
                1.,
                None,
                None,
                Kind::Unspecified,
            )],
            Err(err) => {
                tracing::error!("error baking the dwell-click menu: {err:#}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_picks_the_click_it_names() {
        for (click_type, label, _) in MODES {
            let mut menu = DwellClickMenu::new();
            let at = menu.row_center(label).unwrap();
            assert!(
                matches!(menu.pointer_click(at), PopoverAction::SetDwellClickType(t) if t == click_type),
                "{label}"
            );
        }
        assert_eq!(
            DwellClickMenu::new().labels(),
            ["Single Click", "Double Click", "Drag", "Secondary Click"]
        );
    }
}
