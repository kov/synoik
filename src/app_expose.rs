// SPDX-License-Identifier: GPL-3.0-only

//! App Exposé's app half: which windows it shows (`docs/fork/app-expose.md`).
//!
//! The layout lays out and draws whatever windows it is handed; deciding them needs the app
//! system, which the layout does not know about.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use crate::synoik::{State, Synoik};
use crate::ui::app_grid::FocusDir;
use crate::ui::switcher::app_switcher::{app_items, AppItem};
use crate::window::mapped::MappedId;

impl Synoik {
    /// Every running app with a window, in the app switcher's order (most recently used first),
    /// each with its windows across every workspace and display.
    fn app_expose_apps(&self) -> Vec<AppItem> {
        let tab_list = self.switcher_tab_list(false);
        app_items(self.app_system.running(), &tab_list)
    }

    /// The layout's handles for `ids`.
    fn app_expose_handles(&self, ids: &[MappedId]) -> Vec<Window> {
        self.layout
            .windows()
            .filter(|(_, mapped)| ids.contains(&mapped.id()))
            .map(|(_, mapped)| mapped.window.clone())
            .collect()
    }

    /// The focused window's app's windows — what App Exposé opens over. `None` with no focused
    /// window, or one no running app owns.
    pub fn app_expose_windows_for_focus(&self) -> Option<Vec<Window>> {
        let focused = self.layout.focus()?.id();
        let item = self
            .app_expose_apps()
            .into_iter()
            .find(|item| item.windows.contains(&focused))?;
        Some(self.app_expose_handles(&item.windows))
    }

    /// The next (or previous) app's windows after the one App Exposé shows, wrapping.
    fn app_expose_cycled_windows(&self, backward: bool) -> Option<Vec<Window>> {
        let current = self.layout.app_expose_windows()?;
        let apps = self.app_expose_apps();
        if apps.is_empty() {
            return None;
        }
        // The app showing is the one owning any window shown; App Exposé holds one app's.
        let at = apps.iter().position(|item| {
            self.app_expose_handles(&item.windows)
                .iter()
                .any(|window| current.contains(window))
        });
        let next = match (at, backward) {
            (None, _) => 0,
            (Some(at), false) => (at + 1) % apps.len(),
            (Some(at), true) => (at + apps.len() - 1) % apps.len(),
        };
        Some(self.app_expose_handles(&apps[next].windows))
    }
}

impl State {
    /// Bring App Exposé up over the focused app, or put it away. Nothing happens while locked, or
    /// with nothing focused.
    pub fn toggle_app_expose(&mut self) {
        self.synoik.app_expose_key_selection = None;
        if self.synoik.layout.is_app_expose_open() {
            if self.synoik.layout.close_app_expose() {
                self.synoik.queue_redraw_all();
            }
            return;
        }
        // GNOME windowing mode only, like the overview's picker it borrows from.
        if self.synoik.is_locked() || !self.synoik.layout.is_gnome_mode() {
            return;
        }
        let Some(windows) = self.synoik.app_expose_windows_for_focus() else {
            return;
        };
        if self.synoik.layout.open_app_expose(windows) {
            self.synoik.queue_redraw_all();
        }
    }

    /// Show the next (or previous) running app's windows instead.
    pub fn cycle_app_expose(&mut self, backward: bool) {
        let Some(windows) = self.synoik.app_expose_cycled_windows(backward) else {
            return;
        };
        if self.synoik.layout.set_app_expose_windows(windows) {
            self.synoik.app_expose_key_selection = None;
            self.synoik.queue_redraw_all();
        }
    }

    /// Move the arrow keys' pick to the nearest preview `dir` of it, on the active display. With
    /// nothing picked yet, the first move picks the focused window, or the first preview.
    pub fn move_app_expose_selection(&mut self, dir: FocusDir) {
        let Some(output) = self.synoik.layout.active_output().cloned() else {
            return;
        };
        let slots = self.synoik.layout.app_expose_slots(&output);
        let current = self
            .synoik
            .app_expose_key_selection
            .as_ref()
            .and_then(|sel| slots.iter().find(|(w, _)| w == sel));
        let next = match current {
            Some((_, from)) => nearest_in_direction(*from, dir, &slots),
            None => {
                let focused = self.synoik.layout.focus().map(|m| m.window.clone());
                slots
                    .iter()
                    .find(|(w, _)| Some(w) == focused.as_ref())
                    .or(slots.first())
                    .map(|(w, _)| w.clone())
            }
        };
        if let Some(next) = next {
            self.synoik.app_expose_key_selection = Some(next);
            let pointer = self.synoik.seat.get_pointer().unwrap().current_location();
            self.update_expose_hover(pointer);
            self.synoik.queue_redraw_all();
        }
    }

    /// Go to the window the arrow keys picked, or failing that the one the pointer is on.
    pub fn activate_app_expose_selection(&mut self) {
        let picked = self.synoik.app_expose_key_selection.take().or_else(|| {
            let pointer = self.synoik.seat.get_pointer().unwrap().current_location();
            let (output, pos) = self.synoik.output_under(pointer)?;
            let output = output.clone();
            self.synoik
                .layout
                .window_under_app_expose(&output, pos)
                .map(|mapped| mapped.window.clone())
        });
        if let Some(window) = picked {
            if self.synoik.layout.activate_app_expose_window(&window) {
                self.synoik.queue_redraw_all();
            }
        }
    }
}

/// The preview nearest `from` in `dir`: among the slots whose centre lies that way, the one
/// closest along it, with straying off the axis counted double.
fn nearest_in_direction(
    from: Rectangle<f64, Logical>,
    dir: FocusDir,
    slots: &[(Window, Rectangle<f64, Logical>)],
) -> Option<Window> {
    let center = |r: Rectangle<f64, Logical>| -> Point<f64, Logical> {
        r.loc + r.size.downscale(2.).to_point()
    };
    let origin = center(from);
    slots
        .iter()
        .filter_map(|(window, slot)| {
            let d = center(*slot) - origin;
            let (along, across) = match dir {
                FocusDir::Left => (-d.x, d.y),
                FocusDir::Right => (d.x, d.y),
                FocusDir::Up => (-d.y, d.x),
                FocusDir::Down => (d.y, d.x),
            };
            (along > 0.).then(|| (window, along + 2. * across.abs()))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(window, _)| window.clone())
}
