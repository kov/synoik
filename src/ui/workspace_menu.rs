// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The workspace context menu — rename, close, and send to another display.
//!
//! **Divergence** (`docs/fork/multi-display.md` §6). gnome-shell has no such menu: its
//! workspaces are unnamed, reaped when they empty, and belong to every monitor at once, so
//! there is nothing to rename, nothing to dismiss and nowhere to send. All three exist here
//! ([`docs/fork/dynamic-workspaces-divergence.md`] for the first two, per-monitor workspaces
//! for the third), and only the drag expressed the third — which the keyboard cannot do. The
//! menu is the reachable way to say the same things.
//!
//! Built on [`Menu`], the same widget the window menu uses, so the rows, the keyboard
//! navigation and the card are the shell's one menu.

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Size};

use crate::layout::workspace::WorkspaceId;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Dir, Menu, MenuEntry, MenuHit, MenuItem};

/// What activating a row does. The menu widget knows rows only by `u64`, so the mapping from
/// row id to action lives here — the id is this table's index.
#[derive(Debug, Clone)]
enum RowAction {
    Rename,
    Close,
    /// Send the workspace to this display. Carried as the output itself: the menu outlives the
    /// press that opened it, and a connector name would have to be resolved again at activation.
    SendToDisplay(Output),
}

/// One display the workspace could be sent to.
#[derive(Debug, Clone)]
pub struct DisplayTarget {
    pub output: Output,
    /// What the row is labelled — the display's human name, connector as the fallback.
    pub label: String,
}

/// What the menu needs to know about the workspace, snapshotted at open time.
#[derive(Debug, Clone)]
pub struct WorkspaceMenuContext {
    pub workspace: WorkspaceId,
    /// Whether it already has a name, which is what makes the row a rename rather than a naming.
    pub is_named: bool,
    /// Whether the workspace can be dismissed at all — `Monitor::workspace_is_closable`, which
    /// refuses the trailing empty, the last few, and anything holding windows or a name.
    pub is_closable: bool,
    /// Every display but the one the workspace is on, in the order the strips are laid out.
    pub displays: Vec<DisplayTarget>,
}

/// The workspace menu's content.
pub struct WorkspaceMenu {
    /// The workspace this menu is for — every action carries it back out, so a menu left open
    /// while the active workspace moves still acts on the one it was summoned on.
    workspace: WorkspaceId,
    menu: Menu,
    /// Row id → what it does. Indexed by the id the widget hands back.
    actions: Vec<RowAction>,
}

impl WorkspaceMenu {
    /// Build the menu from a snapshot of the workspace.
    pub fn new(ctx: &WorkspaceMenuContext) -> Self {
        let mut actions: Vec<RowAction> = Vec::new();

        // Group 0: what the workspace itself is. Rename first — it is the row that exists on
        // every workspace, closable or not.
        let mut own = Vec::new();
        own.push({
            let id = actions.len() as u64;
            actions.push(RowAction::Rename);
            MenuEntry::Item(MenuItem::new(
                id,
                if ctx.is_named { "Rename…" } else { "Name…" },
            ))
        });
        own.push({
            let id = actions.len() as u64;
            actions.push(RowAction::Close);
            let mut item = MenuItem::new(id, "Close");
            // Insensitive rather than absent: a workspace that cannot be closed is the common
            // case (anything with a window in it), and a row that comes and goes reads as a bug.
            if !ctx.is_closable {
                item = item.disabled();
            }
            MenuEntry::Item(item)
        });

        // Group 1: the displays, one row each. A single-display seat gets no group and no
        // separator — there is nowhere to send anything.
        let mut displays = Vec::new();
        for target in &ctx.displays {
            let id = actions.len() as u64;
            actions.push(RowAction::SendToDisplay(target.output.clone()));
            displays.push(MenuEntry::Item(MenuItem::new(
                id,
                format!("Send to {}", target.label),
            )));
        }

        let mut entries = Vec::new();
        for group in [own, displays] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::Separator);
            }
            entries.extend(group);
        }

        Self {
            workspace: ctx.workspace,
            menu: Menu::new(entries),
            actions,
        }
    }

    /// The workspace this menu acts on.
    pub fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.menu.logical_size()
    }

    /// Cap the menu's height to what the screen can show.
    pub fn set_max_height(&mut self, max_height: Option<f64>) -> bool {
        self.menu.set_max_height(max_height)
    }

    /// A wheel notch over the menu. Returns whether it scrolled — a menu that fits does not take
    /// the event.
    pub fn scroll(&mut self, delta: f64) -> bool {
        self.menu.scroll_by(delta)
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    /// Every item's label, top to bottom, separators excluded. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    /// The labels of the rows drawn insensitive — see [`Menu::disabled_labels`].
    pub fn disabled_labels(&self) -> Vec<&str> {
        self.menu.disabled_labels()
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

    /// Turn a widget hit into the action it means. A hit on a separator or on the content padding
    /// is consumed without doing anything.
    fn resolve(&self, hit: MenuHit) -> PopoverAction {
        let MenuHit::Activated(id) = hit else {
            // This menu has no submenus, so `Toggled` cannot occur.
            return PopoverAction::Consumed;
        };
        let Some(action) = self.actions.get(id as usize) else {
            return PopoverAction::Consumed;
        };
        let workspace = self.workspace;
        match action {
            RowAction::Rename => PopoverAction::WorkspaceRename(workspace),
            RowAction::Close => PopoverAction::WorkspaceClose(workspace),
            RowAction::SendToDisplay(output) => PopoverAction::WorkspaceSendToDisplay {
                workspace,
                output: output.clone(),
            },
        }
    }

    /// Update the hovered row (`None` clears). Returns whether it changed.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        self.menu.pointer_hover(pos)
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
                tracing::error!("error baking the workspace menu: {err:#}");
                Vec::new()
            }
        }
    }
}
