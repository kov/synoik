// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! An app indicator's menu — the popover content behind a click on a tray icon.
//!
//! Thin, like [`crate::ui::app_menu`]: the box model, hit-testing and painting are
//! [`crate::ui::widget::Menu`]'s, and what is left here is the *identity* — which item this menu
//! belongs to, and the fact that its rows are named by a **remote** client's ids rather than by an
//! index into a table we built.
//!
//! The rows arrive after the menu opens, not before. A client's `GetLayout` is a round trip, and
//! `AboutToShow` may be another, so this opens **empty** and fills in when the layout lands
//! ([`Self::set_layout`]) — which is also what happens on every change the client announces while
//! the menu is up. GNOME's own menus are built before they are shown; a remote menu cannot be.

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Size};

use crate::dbusmenu::{to_entries, MenuNode};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Menu, MenuHit};

pub struct IndicatorMenu {
    /// The indicator this menu belongs to — every action carries it back out, because the
    /// compositor addresses the client by item id.
    item_id: String,
    menu: Menu,
    /// The height cap, kept so a layout arriving after the menu opened is capped too.
    max_height: Option<f64>,
}

impl IndicatorMenu {
    /// An empty menu for `item_id`, waiting for its layout.
    pub fn new(item_id: String) -> Self {
        Self {
            item_id,
            menu: Menu::new(Vec::new()),
            max_height: None,
        }
    }

    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    /// Replace the rows with a client's layout. Returns whether anything changed — a client that
    /// announces a change that turns out to be none costs no redraw.
    pub fn set_layout(&mut self, root: &MenuNode) -> bool {
        let entries = to_entries(&root.children);
        let changed = self.menu.set_entries(entries);
        // A layout that arrives *after* the menu is on screen must still obey the screen.
        self.menu.set_max_height(self.max_height);
        changed
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.menu.logical_size()
    }

    /// Cap the menu's height to what the screen can show.
    pub fn set_max_height(&mut self, max_height: Option<f64>) -> bool {
        self.max_height = max_height;
        self.menu.set_max_height(max_height)
    }

    pub fn scroll(&mut self, delta: f64) -> bool {
        self.menu.scroll_by(delta)
    }

    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    /// Every row's label, top to bottom — the menu as a person reads it. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    /// The menu-local centre of the row labelled `label`, so a test can click a row by name.
    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        self.menu.row_center(label)
    }

    /// Route a menu-local click.
    ///
    /// Both outcomes are calls to the client, not just the activating one: expanding a submenu
    /// sends `AboutToShow`, which is the client's chance to fill it in
    /// (`dbusMenu.js:664-673`). The row ids are the client's own node ids, carried through
    /// [`crate::dbusmenu`] as `u64` and named back to it as `i32`.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let item_id = self.item_id.clone();
        match self.menu.pointer_click(pos) {
            MenuHit::Activated(id) => PopoverAction::IndicatorMenuActivate {
                item_id,
                node_id: id as i32,
            },
            MenuHit::Toggled(id) => PopoverAction::IndicatorMenuExpand {
                item_id,
                node_id: id as i32,
            },
            MenuHit::Nothing => PopoverAction::Consumed,
        }
    }

    /// Update the hovered row (`None` clears). Returns whether it changed.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        self.menu.pointer_hover(pos)
    }

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
                tracing::error!("error baking an indicator menu: {err:#}");
                Vec::new()
            }
        }
    }
}
