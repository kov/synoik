// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The app-icon context menu — gnome-shell's `AppMenu` (`js/ui/appMenu.js`), the
//! menu a right-click on a dash / app-grid / search-result icon pops up
//! (`AppIcon.popupMenu`, `appDisplay.js:3027-3052`).
//!
//! The box model, layout, hit-testing and painting all live in [`widget::Menu`] now — this file
//! is what it always should have been: the *policy* for which rows an app gets, and what each one
//! does. (Its previous header promised the metrics would be lifted out for the next menu; the app
//! indicators' remote menus were that next menu.)
//!
//! **Scope.** gnome-shell's menu has, in order: an "Open Windows" section, "New
//! Window", the `.desktop` action section, a discrete-GPU launch item, the favourite
//! toggle, "App Details", and "Quit". We build all but the GPU item:
//!
//! - **Open Windows** — a labelled separator heading one row per window of a running app, labelled
//!   with its title (`_updateWindowsSection`, `appMenu.js:262-291`). Shown from one window, which
//!   is what an app-grid / dash icon asks for (`showSingleWindows: true`, `appDisplay.js:3033`).
//! - **New Window** — shown only when the app has no `new-window` desktop action, since that action
//!   would be a duplicate of it (`_updateNewWindowItem`, `appMenu.js:140-144`).
//! - **The action section** — one row per `.desktop` action, labelled with its display name
//!   (`setApp`, `appMenu.js:229-242`).
//! - **Pin to Dash / Unpin** — the favourite toggle (`_updateFavoriteItem`, `appMenu.js:146-162`).
//! - **App Details** — an `org.gtk.Actions` call into `org.gnome.Software`, shown only when
//!   Software is installed (`_updateDetailsVisibility`, `appMenu.js:182-185`).
//! - **Quit** — shown only for a RUNNING app (`_updateQuitItem`, `appMenu.js:136-138`).
//!
//! The **GPU** item stays deferred: it needs `switcheroo-control`, which this hardware
//! does not have.
//!
//! Separators are inserted between *populated* groups only, which is what
//! gnome-shell's `_updateSeparatorVisibility` (`popupMenu.js`) arrives at by hiding
//! the ones that end up adjacent to nothing.

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Size, Transform};

use crate::app_system::{AppEntry, AppState, RunningWindow};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{Menu, MenuEntry, MenuHit, MenuItem};
use crate::window::mapped::MappedId;

/// What activating a row does. The menu widget knows rows only by `u64`, so the mapping from row
/// id to action lives here — the id is this table's index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowAction {
    NewWindow,
    LaunchAction(String),
    ToggleFavorite,
    /// Raise one of the app's open windows — `Main.activateWindow(window)`
    /// (`appMenu.js:285`).
    ActivateWindow(MappedId),
    AppDetails,
    Quit,
}

/// The app context menu's content.
pub struct AppMenu {
    /// The app this menu is for — every action carries it back out.
    app_id: String,
    menu: Menu,
    /// Row id → what it does. Indexed by the id the widget hands back.
    actions: Vec<RowAction>,
}

/// What the menu needs to know about the app beyond its catalog entry — the model
/// reads gnome-shell's `AppMenu` does at open time (app state, window list, whether
/// Software is installed). Snapshotted once, because activating any row closes the
/// menu.
pub struct AppMenuContext<'a> {
    pub entry: &'a AppEntry,
    pub is_favorite: bool,
    /// `shell_app_get_state()` — gates Quit and the new-window verb.
    pub state: AppState,
    /// `shell_app_get_windows()`, most recently used first.
    pub windows: &'a [RunningWindow],
    /// Whether `org.gnome.Software.desktop` resolves — gates App Details.
    pub has_software: bool,
}

impl AppMenu {
    /// Build the menu from a snapshot of the app.
    pub fn new(ctx: &AppMenuContext<'_>) -> Self {
        let entry = ctx.entry;
        let mut actions: Vec<RowAction> = Vec::new();
        let row = |label: String, action: RowAction, actions: &mut Vec<RowAction>| {
            let id = actions.len() as u64;
            actions.push(action);
            MenuEntry::Item(MenuItem::new(id, label))
        };

        // Group 0: the open windows. gnome-shell labels each row with the window
        // title and falls back to the app's name for an untitled one
        // (`appMenu.js:283`), and heads the section with a labelled separator.
        let mut windows = Vec::new();
        if !ctx.windows.is_empty() {
            windows.push(MenuEntry::SectionHeader("Open Windows".to_owned()));
            for w in ctx.windows {
                let label = w
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| entry.name.clone());
                windows.push(row(label, RowAction::ActivateWindow(w.id), &mut actions));
            }
        }

        // Group 1: the launch verbs. "New Window" is suppressed when a `new-window`
        // action would restate it (`appMenu.js:140-144`), and when the app can't be
        // asked for one at all (`_updateNewWindowItem`, `:142-143`).
        let mut launch = Vec::new();
        if !entry.actions.iter().any(|a| a.id == "new-window") && ctx.state != AppState::Starting {
            launch.push(row(
                "New Window".to_owned(),
                RowAction::NewWindow,
                &mut actions,
            ));
        }
        for a in &entry.actions {
            launch.push(row(
                a.name.clone(),
                RowAction::LaunchAction(a.id.clone()),
                &mut actions,
            ));
        }

        // Group 2: the favourite toggle.
        let favorite = vec![row(
            if ctx.is_favorite {
                "Unpin"
            } else {
                "Pin to Dash"
            }
            .to_owned(),
            RowAction::ToggleFavorite,
            &mut actions,
        )];

        // Group 3: App Details, only with Software installed.
        let details = if ctx.has_software {
            vec![row(
                "App Details".to_owned(),
                RowAction::AppDetails,
                &mut actions,
            )]
        } else {
            Vec::new()
        };

        // Group 4: Quit, only for a running app.
        let quit = if ctx.state == AppState::Running {
            vec![row("Quit".to_owned(), RowAction::Quit, &mut actions)]
        } else {
            Vec::new()
        };

        let mut entries = Vec::new();
        for group in [windows, launch, favorite, details, quit] {
            if group.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(MenuEntry::Separator);
            }
            entries.extend(group);
        }

        Self {
            app_id: entry.id.clone(),
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

    /// A wheel notch over the menu. Returns whether it scrolled — a menu that fits does not take
    /// the event.
    pub fn scroll(&mut self, delta: f64) -> bool {
        self.menu.scroll_by(delta)
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        self.menu.corner_radius()
    }

    /// Every item's label, top to bottom, separators excluded — the menu's contents
    /// as a person reads them. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.menu.labels()
    }

    /// The menu-local centre of the row labelled `label`, so the corpus can click a
    /// row by name rather than by arithmetic that would drift with the box model.
    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        self.menu.row_center(label)
    }

    /// Route a menu-local click to its action. A click on a separator or on the
    /// content padding is consumed without doing anything, like gnome-shell's
    /// non-reactive separator items.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let MenuHit::Activated(id) = self.menu.pointer_click(pos) else {
            // This menu has no submenus, so `Toggled` cannot occur.
            return PopoverAction::Consumed;
        };
        let Some(action) = self.actions.get(id as usize) else {
            return PopoverAction::Consumed;
        };
        let app = self.app_id.clone();
        match action {
            RowAction::NewWindow => PopoverAction::AppNewWindow(app),
            RowAction::LaunchAction(a) => PopoverAction::AppLaunchAction {
                id: app,
                action: a.clone(),
            },
            RowAction::ToggleFavorite => PopoverAction::AppToggleFavorite(app),
            RowAction::ActivateWindow(w) => PopoverAction::AppActivateWindow(*w),
            RowAction::AppDetails => PopoverAction::AppDetails(app),
            RowAction::Quit => PopoverAction::AppQuit(app),
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
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
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
                tracing::error!("error baking the app menu: {err:#}");
                Vec::new()
            }
        }
    }
}
