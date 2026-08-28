// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The desktop context menu — gnome-shell's `BackgroundMenu` (`js/ui/backgroundMenu.js`), the
//! menu a right-click on the wallpaper raises.
//!
//! Three rows, in GNOME's order and grouping: "Change Background…", a separator, "Display
//! Settings" and "Settings" (`backgroundMenu.js:13-16`). gnome-shell attaches it to every
//! background actor (`layout.js:496-508`), which is the bottom-most thing on the stage — so the
//! menu appears only where nothing else took the press.
//!
//! **Divergence — no long-press.** GNOME also raises it on a primary-button long press, for
//! touch (`backgroundMenu.js:40-50`). We have no long-press gesture anywhere yet (touch is
//! deferred), so the right-click is the only way in.
//!
//! The first two rows open a Settings *panel* rather than launching that panel's desktop file,
//! which is the divergence [`PopoverAction::LaunchSettingsPanel`] documents.

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Size};

use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::{PopoverAction, SETTINGS_DESKTOP_ID};
use crate::ui::widget::{Dir, Menu, MenuEntry, MenuHit, MenuItem};

/// The desktop context menu's content.
pub struct BackgroundMenu {
    menu: Menu,
    /// Row id → what it does, indexed by the id the widget hands back.
    actions: Vec<PopoverAction>,
}

impl Default for BackgroundMenu {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundMenu {
    pub fn new() -> Self {
        let rows: [(&str, PopoverAction); 3] = [
            (
                "Change Background…",
                PopoverAction::LaunchSettingsPanel {
                    panel: "background".to_owned(),
                    args: Vec::new(),
                },
            ),
            (
                "Display Settings",
                PopoverAction::LaunchSettingsPanel {
                    panel: "display".to_owned(),
                    args: Vec::new(),
                },
            ),
            (
                "Settings",
                PopoverAction::ActivateApp(SETTINGS_DESKTOP_ID.to_owned()),
            ),
        ];

        let mut actions = Vec::new();
        let mut entries = Vec::new();
        for (index, (label, action)) in rows.into_iter().enumerate() {
            // The separator sits after the first row only: gnome-shell adds it between "Change
            // Background…" and the two settings rows (`backgroundMenu.js:14`).
            if index == 1 {
                entries.push(MenuEntry::Separator);
            }
            let id = actions.len() as u64;
            actions.push(action);
            entries.push(MenuEntry::Item(MenuItem::new(id, label)));
        }

        Self {
            menu: Menu::new(entries),
            actions,
        }
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.menu.logical_size()
    }

    /// Cap the menu's height to what the screen can show.
    pub fn set_max_height(&mut self, max_height: Option<f64>) -> bool {
        self.menu.set_max_height(max_height)
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    /// Every item's label, top to bottom, separators excluded. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    /// The menu-local centre of the row labelled `label`, so the corpus can click a row by name.
    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        self.menu.row_center(label)
    }

    /// Route a menu-local click to its action.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let hit = self.menu.pointer_click(pos);
        self.resolve(hit)
    }

    /// Move the keyboard focus by `delta` rows. Returns whether it moved.
    pub fn focus_step(&mut self, delta: isize) -> bool {
        self.menu.focus_step(delta)
    }

    /// The keyboard-focused row's label, if any.
    pub fn focused_label(&self) -> Option<String> {
        self.menu.focused_label()
    }

    /// Take one keyboard navigation step. Returns whether the key was consumed.
    pub fn nav(&mut self, dir: Dir) -> bool {
        self.menu.nav(dir)
    }

    /// Activate the keyboard-focused row (Enter/Space).
    pub fn activate_focused(&mut self) -> PopoverAction {
        let hit = self.menu.activate_focused();
        self.resolve(hit)
    }

    /// Update the hovered row (`None` clears). Returns whether it changed.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        self.menu.pointer_hover(pos)
    }

    /// Turn a widget hit into the action it means. A hit on the separator or on the content
    /// padding is consumed without doing anything.
    fn resolve(&self, hit: MenuHit) -> PopoverAction {
        let MenuHit::Activated(id) = hit else {
            // This menu has no submenus, so `Toggled` cannot occur.
            return PopoverAction::Consumed;
        };
        self.actions
            .get(id as usize)
            .cloned()
            .unwrap_or(PopoverAction::Consumed)
    }

    /// The menu's render elements at `origin` — one baked card.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        match self.menu.bake(renderer, scale) {
            Ok(buffer) => {
                vec![TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                )]
            }
            Err(err) => {
                tracing::error!("error baking the background menu: {err:#}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_menu_is_gnomes_three_rows() {
        let menu = BackgroundMenu::new();
        assert_eq!(
            menu.labels(),
            ["Change Background…", "Display Settings", "Settings"]
        );
    }

    #[test]
    fn every_row_opens_what_it_names() {
        for (label, expected) in [
            ("Change Background…", "background"),
            ("Display Settings", "display"),
        ] {
            let mut menu = BackgroundMenu::new();
            let at = menu.row_center(label).unwrap();
            match menu.pointer_click(at) {
                PopoverAction::LaunchSettingsPanel { panel, args } => {
                    assert_eq!(panel, expected);
                    assert!(args.is_empty());
                }
                other => panic!("{label} gave {other:?}"),
            }
        }

        let mut menu = BackgroundMenu::new();
        let at = menu.row_center("Settings").unwrap();
        match menu.pointer_click(at) {
            PopoverAction::ActivateApp(id) => assert_eq!(id, SETTINGS_DESKTOP_ID),
            other => panic!("Settings gave {other:?}"),
        }
    }
}
