// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The window menu — gnome-shell's `WindowMenu` (`js/ui/windowMenu.js`), the menu a right-click
//! on a titlebar pops up.
//!
//! On Wayland the titlebar belongs to the *client*: a CSD toolkit recognizes the right-click
//! itself and asks the compositor for the menu with `xdg_toplevel.show_window_menu`, which mutter
//! turns into `meta_window_show_menu(WM, …)` (`meta-wayland-xdg-shell.c:293-315`) and the shell
//! answers by popping a `PopupMenu` anchored at the requested point
//! (`WindowMenuManager.showWindowMenuForWindow`, `windowMenu.js:204-250`). The other two ways in
//! are `activate-window-menu` (`<Alt>space`, `keybindings.c:1999-2021`) and mutter's Mod+RMB
//! passive button grab (`window.c:7743-7844`).
//!
//! **Scope.** gnome-shell builds, in order: Take Screenshot, Hide, Maximize/Restore, Move, Resize,
//! Always on Top, Always on Visible Workspace, Move to Workspace {Left,Right,Up,Down}, a
//! separator, Move to Monitor {Up,Down,Left,Right}, a separator, and Close. All of them are
//! built:
//!
//! - **Take Screenshot** — the window's own pixels, saved and put on the clipboard with a
//!   notification, and without the pointer (`windowMenu.js:26-36`, whose
//!   `captureScreenshot(texture, null, 1, null)` passes a null cursor).
//! - **Hide** — minimize (`windowMenu.js:38-42`). GNOME dims it when `can_minimize()` is false,
//!   which for a Wayland toplevel it never is: `has_minimize_func` only goes false for a non-NORMAL
//!   window type or a skip-taskbar window (`window.c:6014-6018`, `:6079-6082`), and neither has an
//!   xdg-shell equivalent.
//! - **Maximize / Restore** — one row, whichever the window is not (`windowMenu.js:44-55`).
//! - **Move / Resize** — the keyboard move and resize grabs (`windowMenu.js:58-84`), insensitive
//!   when the window does not own its own geometry.
//! - **Always on Top** — `make_above` / `unmake_above` (`windowMenu.js:86-98`), checked when set
//!   and insensitive while maximized.
//! - **Always on Visible Workspace** — `stick` / `unstick` (`windowMenu.js:105-114`), checked when
//!   set. See `docs/fork/window-menu-port.md`.
//! - **Move to Workspace Left / Right** — only when the neighbour in that direction is a
//!   *different* workspace, which is what `workspace.get_neighbor(dir) !== workspace` tests
//!   (`windowMenu.js:110-135`). GNOME's horizontal workspace axis is our vertical one, the same
//!   mapping `move-to-workspace-left` → `MoveWindowToWorkspaceUp` already uses.
//! - **Move to Monitor Up / Down / Left / Right** — only the directions with a neighbouring
//!   monitor, in GNOME's order (`windowMenu.js:143-181`).
//! - **Close** (`windowMenu.js:185-189`).
//!
//! `docs/fork/window-menu-port.md` carries the stacking, keyboard-grab and sticky models the
//! rows sit on.

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Size};

use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Menu, MenuEntry, MenuHit, MenuItem, Ornament};
use crate::window::mapped::MappedId;

/// A neighbouring monitor's direction, in the order gnome-shell lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Which way along the workspace axis a row moves the window. GNOME labels these Left/Right
/// because its workspaces run horizontally; ours run vertically, so `Left` is the workspace
/// *before* this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDirection {
    Left,
    Right,
}

/// Where the menu hangs from — the three ways a window menu is summoned each name the point
/// differently.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowMenuAnchor {
    /// The window's top-left. `activate-window-menu` anchors on the frame plus the client area's
    /// origin (`keybindings.c:2011-2019`), which for an undecorated window is the window itself.
    Window,
    /// A point in the window's *surface* (buffer) coordinates, as `xdg_toplevel.show_window_menu`
    /// sends it — mutter adds it to `buffer_rect`, not to the geometry rect
    /// (`meta-wayland-xdg-shell.c:311-314`), so a CSD window's invisible shadow margin counts.
    Surface(Point<i32, Logical>),
    /// A point in output-local logical coordinates: where the pointer was, for mutter's Mod+RMB
    /// passive button grab (`window.c:7743-7844`).
    Output(Point<f64, Logical>),
}

/// What activating a row does. The menu widget knows rows only by `u64`, so the mapping from row
/// id to action lives here — the id is this table's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    TakeScreenshot,
    Minimize,
    SetMaximized(bool),
    BeginMove,
    BeginResize,
    SetAlwaysOnTop(bool),
    SetSticky(bool),
    MoveToWorkspace(WorkspaceDirection),
    MoveToMonitor(MonitorDirection),
    Close,
}

/// What the menu needs to know about the window, snapshotted at open time — activating any row
/// closes the menu, so nothing here has to stay live.
#[derive(Debug, Clone, Copy)]
pub struct WindowMenuContext {
    pub window: MappedId,
    /// Drawn as Restore when true, Maximize when false (`window.is_maximized()`).
    pub is_maximized: bool,
    /// Whether the window still owns its own geometry — `allows_move` / `allows_resize`, which
    /// are `has_move_func`/`has_resize_func` and false for a maximized or fullscreen window.
    pub is_normal_size: bool,
    /// Ticks the Always on Top row (`window.is_above()`).
    pub is_above: bool,
    /// Ticks Always on Visible Workspace, and hides the Move to Workspace rows
    /// (`window.is_on_all_workspaces()`, `windowMenu.js:103-142`).
    pub is_sticky: bool,
    /// Whether a workspace exists before / after this one on its monitor.
    pub workspace_left: bool,
    pub workspace_right: bool,
    /// Whether a monitor neighbours this window's, per direction: up, down, left, right.
    pub monitor_up: bool,
    pub monitor_down: bool,
    pub monitor_left: bool,
    pub monitor_right: bool,
}

/// The window menu's content.
pub struct WindowMenu {
    /// The window this menu is for — every action carries it back out, so a menu left open while
    /// the focus moves still acts on the window it was summoned on.
    window: MappedId,
    menu: Menu,
    /// Row id → what it does. Indexed by the id the widget hands back.
    actions: Vec<RowAction>,
}

impl WindowMenu {
    /// Build the menu from a snapshot of the window.
    pub fn new(ctx: &WindowMenuContext) -> Self {
        let mut actions: Vec<RowAction> = Vec::new();
        let row = |label: &str, action: RowAction, actions: &mut Vec<RowAction>| {
            let id = actions.len() as u64;
            actions.push(action);
            MenuEntry::Item(MenuItem::new(id, label))
        };

        // Group 0: the capture, the size verbs and the workspace moves — one flat group, as in
        // gnome-shell, which puts no separator between them. Take Screenshot leads, and has no
        // `can_*` gate: every window can be photographed.
        let mut state = vec![row(
            "Take Screenshot",
            RowAction::TakeScreenshot,
            &mut actions,
        )];
        state.push(row("Hide", RowAction::Minimize, &mut actions));
        state.push(if ctx.is_maximized {
            row("Restore", RowAction::SetMaximized(false), &mut actions)
        } else {
            row("Maximize", RowAction::SetMaximized(true), &mut actions)
        });
        // Move and Resize start the keyboard grabs (`windowMenu.js:58-84`), dimmed when the
        // window does not own its own geometry — which is the same gate
        // `begin_keyboard_window_grab` applies, so an enabled row always does something.
        for (label, action) in [
            ("Move", RowAction::BeginMove),
            ("Resize", RowAction::BeginResize),
        ] {
            state.push({
                let id = actions.len() as u64;
                actions.push(action);
                let mut item = MenuItem::new(id, label);
                if !ctx.is_normal_size {
                    item = item.disabled();
                }
                MenuEntry::Item(item)
            });
        }
        // Checked when set, and **insensitive while maximized**, which is not a UI nicety: a
        // maximized window is in the normal layer even with the flag set
        // (`meta_window_get_default_layer`), so the row would claim an effect it does not have.
        // gnome-shell's other three disabling cases are X11 window types that xdg-shell has no
        // equivalent of.
        state.push({
            let id = actions.len() as u64;
            actions.push(RowAction::SetAlwaysOnTop(!ctx.is_above));
            let mut item = MenuItem::new(id, "Always on Top");
            if ctx.is_above {
                item = item.with_ornament(Ornament::Check(true));
            }
            if ctx.is_maximized {
                item = item.disabled();
            }
            MenuEntry::Item(item)
        });
        // Always on Visible Workspace, ticked when set. GNOME's `is_always_on_all_workspaces()`
        // disabling case is a window type xdg-shell has no equivalent of, so the row is never
        // insensitive here.
        state.push({
            let id = actions.len() as u64;
            actions.push(RowAction::SetSticky(!ctx.is_sticky));
            let mut item = MenuItem::new(id, "Always on Visible Workspace");
            if ctx.is_sticky {
                item = item.with_ornament(Ornament::Check(true));
            }
            MenuEntry::Item(item)
        });
        // A sticky window is on every workspace already, so there is nowhere to move it to:
        // gnome-shell builds these rows inside `if (!isSticky)` (`windowMenu.js:116`).
        if ctx.workspace_left && !ctx.is_sticky {
            state.push(row(
                "Move to Workspace Left",
                RowAction::MoveToWorkspace(WorkspaceDirection::Left),
                &mut actions,
            ));
        }
        if ctx.workspace_right && !ctx.is_sticky {
            state.push(row(
                "Move to Workspace Right",
                RowAction::MoveToWorkspace(WorkspaceDirection::Right),
                &mut actions,
            ));
        }

        // Group 1: the monitor moves, in gnome-shell's up/down/left/right order.
        let mut monitors = Vec::new();
        for (present, dir, label) in [
            (ctx.monitor_up, MonitorDirection::Up, "Move to Monitor Up"),
            (
                ctx.monitor_down,
                MonitorDirection::Down,
                "Move to Monitor Down",
            ),
            (
                ctx.monitor_left,
                MonitorDirection::Left,
                "Move to Monitor Left",
            ),
            (
                ctx.monitor_right,
                MonitorDirection::Right,
                "Move to Monitor Right",
            ),
        ] {
            if present {
                monitors.push(row(label, RowAction::MoveToMonitor(dir), &mut actions));
            }
        }

        // Group 2: Close, always last and always behind a separator.
        let close = vec![row("Close", RowAction::Close, &mut actions)];

        let mut entries = Vec::new();
        for group in [state, monitors, close] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::Separator);
            }
            entries.extend(group);
        }

        Self {
            window: ctx.window,
            menu: Menu::new(entries),
            actions,
        }
    }

    /// The window this menu acts on.
    pub fn window(&self) -> MappedId {
        self.window
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

    /// The menu-local centre of the row labelled `label`, so the corpus can click a row by name
    /// rather than by arithmetic that would drift with the box model.
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

    /// Activate the keyboard-focused row (Enter/Space).
    pub fn activate_focused(&mut self) -> PopoverAction {
        let hit = self.menu.activate_focused();
        self.resolve(hit)
    }

    /// Turn a widget hit into the action it means. A hit on a separator or on the content padding
    /// is consumed without doing anything, like gnome-shell's non-reactive separator items.
    fn resolve(&self, hit: MenuHit) -> PopoverAction {
        let MenuHit::Activated(id) = hit else {
            // This menu has no submenus, so `Toggled` cannot occur.
            return PopoverAction::Consumed;
        };
        let Some(action) = self.actions.get(id as usize) else {
            return PopoverAction::Consumed;
        };
        let window = self.window;
        match *action {
            RowAction::TakeScreenshot => PopoverAction::WindowTakeScreenshot(window),
            RowAction::Minimize => PopoverAction::WindowMinimize(window),
            RowAction::BeginMove => PopoverAction::WindowBeginMove(window),
            RowAction::BeginResize => PopoverAction::WindowBeginResize(window),
            RowAction::SetSticky(sticky) => PopoverAction::WindowSetSticky { window, sticky },
            RowAction::SetAlwaysOnTop(above) => {
                PopoverAction::WindowSetAlwaysOnTop { window, above }
            }
            RowAction::SetMaximized(maximized) => {
                PopoverAction::WindowSetMaximized { window, maximized }
            }
            RowAction::MoveToWorkspace(dir) => PopoverAction::WindowMoveToWorkspace { window, dir },
            RowAction::MoveToMonitor(dir) => PopoverAction::WindowMoveToMonitor { window, dir },
            RowAction::Close => PopoverAction::WindowClose(window),
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
                tracing::error!("error baking the window menu: {err:#}");
                Vec::new()
            }
        }
    }
}
