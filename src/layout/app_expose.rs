// SPDX-License-Identifier: GPL-3.0-only

//! App Exposé: one app's windows, from every workspace of a display, in one grid over the
//! blurred wallpaper (`docs/fork/app-expose.md`).
//!
//! A state parallel to the overview, not a mode of it. The overview's progress gates nearly
//! everything the overview draws, and App Exposé turns off more of that than it shares, so it
//! keeps a progress of its own that only the backdrop reads alongside the overview's.
//!
//! `Layout` knows nothing about apps: the windows are decided by the caller and handed in.

use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle};

use super::expose::ExposeInput;
use super::monitor::Monitor;
use super::tile::Tile;
use super::workspace::{expose_tile_render, AppExposeTile};
use super::{Layout, LayoutElement, OVERVIEW_GESTURE_MOVEMENT};
use crate::animation::Animation;
use crate::input::swipe_tracker::{self, SwipeTracker};

/// How large a window from another workspace starts, relative to its slot. It has no on-screen
/// origin to fly from, so it grows into the slot from here while it fades in.
const OFF_WORKSPACE_FROM_SCALE: f64 = 0.85;

/// App Exposé's state on the layout. `None` once it has finished going away, so a closed one
/// costs nothing to check.
#[derive(Debug)]
pub(super) struct AppExpose<W: LayoutElement> {
    /// The windows it shows, as the caller decided them. A window that has since closed simply
    /// finds no tile; one of the app that maps while this is up is not added.
    windows: Vec<W::Id>,
    /// Whether it is up — what input goes by. True from the moment a swipe begins, as the
    /// overview's flag is.
    open: bool,
    progress: AppExposeProgress,
    /// The window that had the focus when it came up. Focus moving off it to a window it does
    /// not show — the switcher, an activation — puts it away (see
    /// [`Layout::close_app_expose_if_focus_left`]).
    focus: Option<W::Id>,
}

#[derive(Debug)]
enum AppExposeProgress {
    Animation(Animation),
    Gesture(AppExposeGesture),
}

/// A touchpad swipe along App Exposé's one leg, 0 the desktop and 1 fully up. The overview's
/// swipe with a single snap point either side.
#[derive(Debug)]
struct AppExposeGesture {
    tracker: SwipeTracker,
    start: f64,
    state: f64,
}

impl AppExposeProgress {
    fn value(&self) -> f64 {
        match self {
            AppExposeProgress::Animation(anim) => anim.clamped_value(),
            AppExposeProgress::Gesture(gesture) => gesture.state,
        }
    }
}

/// What a monitor is told about App Exposé: the windows, and how far it is up.
#[derive(Debug)]
pub(super) struct MonitorAppExpose<W: LayoutElement> {
    windows: Vec<W::Id>,
    progress: f64,
    /// Whether it is up for input, as opposed to on its way out.
    open: bool,
}

/// One preview in a display's App Exposé grid.
#[derive(Debug)]
pub(super) struct AppExposeEntry<'a, W: LayoutElement> {
    pub tile: &'a Tile<W>,
    /// The rect it interpolates from, in output coordinates. Its size is the tile's natural size,
    /// always — see `ExposeLayout` in `workspace.rs`, whose contract this shares.
    pub rect: Rectangle<f64, Logical>,
    /// Its slot in the grid, in output coordinates.
    pub slot: Rectangle<f64, Logical>,
    /// Its scale at progress 0.
    pub from_scale: f64,
    /// Whether the window is on screen when App Exposé is down — on its display's active
    /// workspace. One that is not has nowhere to fly from, and fades in instead.
    pub on_screen: bool,
    /// How far the picker overlay is up on it — the hover growth.
    pub hover: f64,
    /// Its place in its workspace's stack, front-to-back — what the previews draw in, so a
    /// window raised on activation is on top for the whole way back.
    stack: usize,
}

impl<W: LayoutElement> AppExposeEntry<'_, W> {
    /// Where the preview draws at `progress`, and at what scale — the picker's own geometry,
    /// hover growth included, at zoom 1.
    pub fn placement(&self, progress: f64) -> (Point<f64, Logical>, f64) {
        expose_tile_render(
            self.rect,
            self.slot,
            self.from_scale,
            self.hover,
            progress,
            1.,
        )
    }

    /// The rect the preview draws into at `progress`.
    pub fn drawn_rect(&self, progress: f64) -> Rectangle<f64, Logical> {
        let (pos, scale) = self.placement(progress);
        Rectangle::new(pos, self.rect.size.upscale(scale))
    }
}

impl<W: LayoutElement> Monitor<W> {
    pub(super) fn set_app_expose(&mut self, state: Option<(&[W::Id], f64, bool)>) {
        self.app_expose = state.map(|(windows, progress, open)| MonitorAppExpose {
            windows: windows.to_vec(),
            progress,
            open,
        });
    }

    /// Whether App Exposé is up for input here. On its way out its previews are scenery: they
    /// take no hits and grow no overlay, and the windows beneath take the pointer back.
    pub(super) fn app_expose_takes_input(&self) -> bool {
        self.app_expose.as_ref().is_some_and(|state| state.open)
    }

    /// How far App Exposé is up on this display, `None` when it is not there at all.
    pub fn app_expose_progress(&self) -> Option<f64> {
        self.app_expose.as_ref().map(|state| state.progress)
    }

    /// The windows the live desktop must not draw while App Exposé is anywhere on screen: they
    /// are drawn at their slots instead, and would otherwise show twice on the way.
    pub(super) fn app_expose_hidden(&self) -> &[W::Id] {
        self.app_expose
            .as_ref()
            .map_or(&[], |state| &state.windows[..])
    }

    /// Decide this display's grid afresh at the next query.
    pub(super) fn forget_app_expose_layout(&self) {
        self.app_expose_held.forget();
    }

    /// This display's App Exposé grid: the windows it was handed that live on any of its
    /// workspaces, minimized ones included, each with its slot.
    ///
    /// One decision over all the workspaces at once. They share the display's view size, so
    /// their workspace-local rects are already in one frame, and at zoom 1 that frame is the
    /// output's.
    pub(super) fn app_expose_layout(&self) -> Vec<AppExposeEntry<'_, W>> {
        let Some(state) = &self.app_expose else {
            return Vec::new();
        };

        let active = self.active_workspace_idx;
        let active_loc = self
            .workspaces_render_geo()
            .nth(active)
            .map_or_else(Point::default, |geo| geo.loc);

        let mut found: Vec<(usize, usize, AppExposeTile<'_, W>)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.app_expose_tiles(&state.windows)
                    .into_iter()
                    .enumerate()
                    .map(move |(stack, found)| (ws_idx, stack, found))
            })
            .collect();
        // Stable creation order, for the reason the picker uses it — see
        // `Workspace::expose_live_inputs`.
        found.sort_by_key(|(_, _, (_, (seq, _), _, _))| *seq);

        let inputs: Vec<ExposeInput> = found
            .iter()
            .map(|(_, _, (_, input, _, _))| *input)
            .collect();
        let area = self.workspaces[active].expose_area();
        let slots = self.app_expose_held.slots(inputs, self.view_size, area);

        found
            .into_iter()
            .zip(slots)
            .map(|((ws_idx, stack, (tile, _, rect, from_scale)), slot)| {
                let hover = self.workspaces[ws_idx].expose_hover_value(tile.window().id());
                if ws_idx == active {
                    AppExposeEntry {
                        tile,
                        rect: Rectangle::new(rect.loc + active_loc, rect.size),
                        slot,
                        from_scale,
                        on_screen: true,
                        hover,
                        stack,
                    }
                } else {
                    // Nowhere on screen to come from: start a little smaller than the slot,
                    // centred on it, and fade in.
                    let natural = rect.size;
                    let from_scale = if natural.w > 0. {
                        slot.size.w / natural.w * OFF_WORKSPACE_FROM_SCALE
                    } else {
                        1.
                    };
                    let from_size = natural.upscale(from_scale);
                    let center = slot.loc + slot.size.downscale(2.).to_point();
                    let loc = center - from_size.downscale(2.).to_point();
                    AppExposeEntry {
                        tile,
                        rect: Rectangle::new(loc, natural),
                        slot,
                        from_scale,
                        on_screen: false,
                        hover,
                        stack,
                    }
                }
            })
            .collect()
    }

    /// The grid in draw order, first topmost: the hovered preview above its neighbours, as in the
    /// picker (`render_expose`), and otherwise each workspace's own stack. Previews of different
    /// workspaces never meet — those of other workspaces draw as a group of their own.
    pub(super) fn app_expose_draw_order(&self) -> Vec<AppExposeEntry<'_, W>> {
        let mut layout = self.app_expose_layout();
        layout.sort_by(|a, b| b.hover.total_cmp(&a.hover).then(a.stack.cmp(&b.stack)));
        layout
    }

    /// The window whose slot is under `pos` (output coordinates) — the picker's hit test,
    /// which is the slot and not the drawn rect, so what a click picks is what hover grows.
    pub(super) fn window_under_app_expose(&self, pos: Point<f64, Logical>) -> Option<&W> {
        self.app_expose_progress()?;
        self.app_expose_layout()
            .into_iter()
            .find(|entry| entry.slot.contains(pos))
            .map(|entry| entry.tile.window())
    }

    /// Every preview as drawn, with its overlay's alpha — what the preview chrome is built from,
    /// as [`Monitor::preview_rects`] is for the picker.
    pub(super) fn app_expose_preview_rects(&self) -> Vec<(W::Id, Rectangle<f64, Logical>, f64)> {
        let Some(progress) = self.app_expose_progress() else {
            return Vec::new();
        };
        self.app_expose_layout()
            .into_iter()
            .map(|entry| {
                (
                    entry.tile.window().id().clone(),
                    entry.drawn_rect(progress),
                    entry.hover * progress,
                )
            })
            .collect()
    }
}

impl<W: LayoutElement> Layout<W> {
    /// Whether App Exposé is up — what input goes by.
    pub fn is_app_expose_open(&self) -> bool {
        self.app_expose.as_ref().is_some_and(|state| state.open)
    }

    /// The windows App Exposé shows, while it is up.
    pub fn app_expose_windows(&self) -> Option<&[W::Id]> {
        self.app_expose
            .as_ref()
            .filter(|state| state.open)
            .map(|state| &state.windows[..])
    }

    fn app_expose_value(&self) -> f64 {
        self.app_expose
            .as_ref()
            .map_or(0., |state| state.progress.value())
    }

    /// Bring App Exposé up over `windows`. Refused while the overview is open — the two are
    /// never up together — and over no windows at all.
    pub fn open_app_expose(&mut self, windows: Vec<W::Id>) -> bool {
        if self.overview_open || windows.is_empty() || self.is_app_expose_open() {
            return false;
        }

        let from = self.app_expose_value();
        let focus = self.focus().map(|win| win.id().clone());
        self.app_expose = Some(AppExpose {
            windows,
            open: true,
            progress: AppExposeProgress::Animation(self.app_expose_animation(from, 1., 0.)),
            focus,
        });
        self.forget_app_expose_layouts();
        self.set_monitors_app_expose();
        true
    }

    /// Put App Exposé away if the focus has moved to a window it does not show — the switcher
    /// committing, an activation, another app's window mapping with focus. That window would
    /// otherwise sit focused and invisible under the backdrop, with App Exposé taking its keys.
    ///
    /// A change of focus, not a level test: cycling to another app leaves the focus on the first
    /// app's window, which the grid then does not show, and must not close anything.
    pub fn close_app_expose_if_focus_left(&mut self) -> bool {
        let Some(state) = self.app_expose.as_ref().filter(|state| state.open) else {
            return false;
        };
        let Some(now) = self.focus().map(|win| win.id()) else {
            return false;
        };
        if state.focus.as_ref() == Some(now) || state.windows.contains(now) {
            return false;
        }
        self.close_app_expose()
    }

    /// Put App Exposé away, leaving the desktop as it was.
    pub fn close_app_expose(&mut self) -> bool {
        if !self.is_app_expose_open() {
            return false;
        }

        // Leaving drops the overlay at once, as the overview's exit does (`toggle_overview`).
        for ws in self.workspaces_mut() {
            ws.clear_expose_hover();
        }

        let from = self.app_expose_value();
        let anim = self.app_expose_animation(from, 0., 0.);
        let state = self.app_expose.as_mut().unwrap();
        state.open = false;
        state.progress = AppExposeProgress::Animation(anim);
        self.set_monitors_app_expose();
        true
    }

    /// Show `windows` instead — cycling to another app. Every display decides its grid afresh.
    pub fn set_app_expose_windows(&mut self, windows: Vec<W::Id>) -> bool {
        let Some(state) = self.app_expose.as_mut().filter(|state| state.open) else {
            return false;
        };
        if windows.is_empty() || state.windows == windows {
            return false;
        }
        state.windows = windows;
        self.forget_app_expose_layouts();
        self.set_monitors_app_expose();
        true
    }

    /// Activate `window` from App Exposé: its workspace becomes its display's active one, it
    /// takes focus, and App Exposé goes away. The workspace switch runs on App Exposé's own
    /// timing, so the preview lands where the window now is as the backdrop clears.
    pub fn activate_app_expose_window(&mut self, window: &W::Id) -> bool {
        if !self.is_app_expose_open() {
            return false;
        }
        let config = self.options.animations.overview_open_close.0;
        let found = self.monitors_mut().find_map(|mon| {
            let ws_idx = mon
                .workspaces
                .iter()
                .position(|ws| ws.holds_window(window))?;
            mon.activate_workspace_with_anim_config(ws_idx, Some(config));
            Some(())
        });
        if found.is_none() {
            return false;
        }
        self.activate_window(window);
        self.close_app_expose();
        true
    }

    /// Begin a touchpad swipe towards App Exposé over `windows`, or back from it. A swipe that
    /// catches App Exposé on its way out keeps the windows it was showing. Refused while the
    /// overview is open: a swipe never crosses from one to the other.
    pub fn app_expose_gesture_begin(&mut self, windows: Vec<W::Id>) -> bool {
        if self.overview_open {
            return false;
        }

        let start = self.app_expose_value();
        let gesture = AppExposeProgress::Gesture(AppExposeGesture {
            tracker: SwipeTracker::new(),
            start,
            state: start,
        });
        match &mut self.app_expose {
            Some(state) => {
                state.open = true;
                state.progress = gesture;
            }
            None => {
                if windows.is_empty() {
                    return false;
                }
                let focus = self.focus().map(|win| win.id().clone());
                self.app_expose = Some(AppExpose {
                    windows,
                    open: true,
                    progress: gesture,
                    focus,
                });
                self.forget_app_expose_layouts();
            }
        }
        self.set_monitors_app_expose();
        true
    }

    /// Move the swipe by `delta` touchpad pixels, positive towards App Exposé. Clamped to its one
    /// leg. Returns `None` when no App Exposé swipe is running, otherwise whether anything moved.
    pub fn app_expose_gesture_update(&mut self, delta: f64, timestamp: Duration) -> Option<bool> {
        let Some(AppExpose {
            progress: AppExposeProgress::Gesture(gesture),
            ..
        }) = &mut self.app_expose
        else {
            return None;
        };

        gesture.tracker.push(delta, timestamp);
        let state =
            (gesture.start + gesture.tracker.pos() / OVERVIEW_GESTURE_MOVEMENT).clamp(0., 1.);
        if gesture.state == state {
            return Some(false);
        }
        gesture.state = state;
        self.set_monitors_app_expose();
        Some(true)
    }

    /// Release the swipe, easing to where the overview's release rule projects it.
    pub fn app_expose_gesture_end(&mut self, timestamp: Duration) -> bool {
        let Some(AppExpose {
            progress: AppExposeProgress::Gesture(gesture),
            ..
        }) = &mut self.app_expose
        else {
            return false;
        };

        gesture.tracker.release(timestamp);
        let velocity = gesture.tracker.velocity() / 1000.;
        let target =
            swipe_tracker::end_progress(gesture.start, gesture.state, (0., 1.), velocity, true);
        gesture
            .tracker
            .log_release("app-expose", gesture.start, gesture.state, target);
        let from = gesture.state;
        let velocity = gesture.tracker.velocity() / OVERVIEW_GESTURE_MOVEMENT;
        let velocity = if (target - from) * velocity > 0. {
            velocity
        } else {
            0.
        };

        let anim = self.app_expose_animation(from, target, velocity);
        let state = self.app_expose.as_mut().unwrap();
        state.open = target >= 1.;
        state.progress = AppExposeProgress::Animation(anim);
        self.set_monitors_app_expose();
        true
    }

    /// Whether a swipe is driving App Exposé.
    pub fn is_app_expose_gesture_ongoing(&self) -> bool {
        matches!(
            self.app_expose,
            Some(AppExpose {
                progress: AppExposeProgress::Gesture(_),
                ..
            })
        )
    }

    pub(super) fn is_app_expose_animating(&self) -> bool {
        matches!(
            &self.app_expose,
            Some(AppExpose { progress: AppExposeProgress::Animation(anim), .. }) if !anim.is_done()
        )
    }

    /// Retire a finished exit, and keep the monitors' copy current.
    pub(super) fn advance_app_expose(&mut self) {
        if let Some(AppExpose {
            open: false,
            progress: AppExposeProgress::Animation(anim),
            ..
        }) = &self.app_expose
        {
            if anim.is_done() {
                self.app_expose = None;
            }
        }
        self.set_monitors_app_expose();
    }

    fn app_expose_animation(&self, from: f64, to: f64, velocity: f64) -> Animation {
        Animation::new(
            self.clock.clone(),
            from,
            to,
            velocity,
            self.options.animations.overview_open_close.0,
        )
    }

    fn forget_app_expose_layouts(&mut self) {
        for mon in self.monitors_mut() {
            mon.forget_app_expose_layout();
        }
    }

    pub(super) fn set_monitors_app_expose(&mut self) {
        let state = self
            .app_expose
            .as_ref()
            .map(|state| (state.windows.clone(), state.progress.value(), state.open));
        for mon in self.monitors_mut() {
            mon.set_app_expose(state.as_ref().map(|(w, p, o)| (&w[..], *p, *o)));
        }
    }

    /// Every display's App Exposé grid, as `(window, slot)` in output coordinates.
    pub fn app_expose_slots(
        &self,
        output: &smithay::output::Output,
    ) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        self.monitors()
            .find(|mon| mon.output() == output)
            .map(|mon| {
                mon.app_expose_layout()
                    .into_iter()
                    .map(|entry| (entry.tile.window().id().clone(), entry.slot))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The windows of `output`'s App Exposé in the order their previews draw, topmost first.
    pub fn app_expose_draw_order(&self, output: &smithay::output::Output) -> Vec<W::Id> {
        self.monitors()
            .find(|mon| mon.output() == output)
            .map(|mon| {
                mon.app_expose_draw_order()
                    .into_iter()
                    .map(|entry| entry.tile.window().id().clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The window whose App Exposé preview is under `pos` on `output`.
    pub fn window_under_app_expose(
        &self,
        output: &smithay::output::Output,
        pos: Point<f64, Logical>,
    ) -> Option<&W> {
        self.monitors()
            .find(|mon| mon.output() == output)?
            .window_under_app_expose(pos)
    }
}
