// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::Cell;

use calloop::Interest;
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output, utils, LayerSurface,
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window,
    WindowSurfaceType,
};
use smithay::input::pointer::Focus;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner::ConstraintAdjustment;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::{self};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration;
use smithay::reexports::wayland_server::protocol::wl_output;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{self, Resource, WEnum};
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial};
use smithay::wayland::compositor::{
    add_blocker, add_pre_commit_hook, with_states, BufferAssignment, CompositorHandler as _,
    HookId, SurfaceAttributes,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;
use smithay::wayland::input_method::InputMethodSeat;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::wlr_layer::{self, Layer};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::{
    delegate_kde_decoration, delegate_xdg_decoration, delegate_xdg_foreign, delegate_xdg_shell,
};
use synoik_config::window_rule::{FloatingPosition, RelativeTo};
use synoik_config::{FloatOrInt, PresetSize, WindowingMode};
use tracing::field::Empty;

use crate::input::move_grab::MoveGrab;
use crate::input::resize_grab::ResizeGrab;
use crate::input::touch_resize_grab::TouchResizeGrab;
use crate::input::{PointerOrTouchStartData, DOUBLE_CLICK_TIME};
use crate::layout::placement::PlacementSeeds;
use crate::layout::workspace::WorkspaceId;
use crate::layout::ActivateWindow;
use crate::output_identity::OutputIdentity;
use crate::protocols::raw::xdg_session_management::v1::server::xdg_session_manager_v1::Reason;
use crate::protocols::raw::xdg_session_management::v1::server::xdg_toplevel_session_v1::XdgToplevelSessionV1;
use crate::protocols::session_management::RestoreSlot;
use crate::session_state::{ToplevelRecord, WindowState};
use crate::synoik::{CastTarget, PopupGrabState, State};
use crate::ui::window_menu::WindowMenuAnchor;
use crate::utils::transaction::Transaction;
use crate::utils::{
    get_monotonic_time, output_matches_name, send_scale_transform, update_tiled_state, ResizeEdge,
};
use crate::window::{
    InitialConfigureState, ResolvedWindowRules, RestoreOnMap, RestoreRuleSeeds, Unmapped, WindowRef,
};

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.synoik.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let unmapped = Unmapped::new(Window::new_wayland_window(surface));
        let existing = self.synoik.unmapped_windows.insert(wl_surface, unmapped);
        assert!(existing.is_none());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let popup = PopupKind::Xdg(surface);
        self.unconstrain_popup(&popup);

        if let Err(err) = self.synoik.popups.track_popup(popup) {
            warn!("error tracking popup: {err:?}");
        }
    }

    /// `xdg_toplevel.set_minimized` — a client asking for its own window to be hidden, which is
    /// what a CSD titlebar's minimize button sends (`xdg_toplevel_set_minimized`,
    /// `meta-wayland-xdg-shell.c:535-549`, straight into `meta_window_minimize`).
    ///
    /// Unlike `show_window_menu` this carries no serial and no seat, so there is nothing to
    /// validate: the request is a window acting on itself, which is always its own business.
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        let Some(window) = self
            .synoik
            .layout
            .find_window_and_output(surface.wl_surface())
            .map(|(mapped, _)| mapped.window.clone())
        else {
            return;
        };
        self.minimize_window(&window);
    }

    /// `xdg_toplevel.show_window_menu` — a CSD client's titlebar right-click asking the
    /// compositor for the window menu (`xdg_toplevel_show_window_menu`,
    /// `meta-wayland-xdg-shell.c:293-315`).
    ///
    /// mutter validates the request against the seat's recorded grab serial and drops it
    /// otherwise. smithay hands the serial straight through without checking it, so the gate
    /// here is focus: only the window the keyboard is on may summon its menu, which is the same
    /// rule `grab` above applies to a toplevel popup grab. A background client asking for a menu
    /// would otherwise steal the modal grab from under the user.
    fn show_window_menu(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        location: Point<i32, Logical>,
    ) {
        let focused = self
            .synoik
            .layout
            .focus()
            .map(|win| win.toplevel().wl_surface().clone());
        if focused.as_ref() != Some(surface.wl_surface()) {
            trace!("ignoring show_window_menu because another window has focus");
            return;
        }
        let Some((window, _)) = self
            .synoik
            .layout
            .find_window_and_output(surface.wl_surface())
            .map(|(mapped, output)| (mapped.window.clone(), output))
        else {
            return;
        };
        self.show_window_menu(&window, WindowMenuAnchor::Surface(location));
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface();

        let mut grab_start_data = None;

        // See if this comes from a pointer grab.
        let pointer = self.synoik.seat.get_pointer().unwrap();
        pointer.with_grab(|grab_serial, grab| {
            if grab_serial == serial {
                let start_data = grab.start_data();
                if let Some((focus, _)) = &start_data.focus {
                    if focus.id().same_client_as(&wl_surface.id()) {
                        // Deny move requests from DnD grabs to work around
                        // https://gitlab.gnome.org/GNOME/gtk/-/issues/7113
                        let is_dnd_grab = Self::is_dnd_grab(grab.as_any());

                        if !is_dnd_grab {
                            grab_start_data =
                                Some(PointerOrTouchStartData::Pointer(start_data.clone()));
                        }
                    }
                }
            }
        });

        // See if this comes from a touch grab.
        if let Some(touch) = self.synoik.seat.get_touch() {
            touch.with_grab(|grab_serial, grab| {
                if grab_serial == serial {
                    let start_data = grab.start_data();
                    if let Some((focus, _)) = &start_data.focus {
                        if focus.id().same_client_as(&wl_surface.id()) {
                            // Deny move requests from DnD grabs to work around
                            // https://gitlab.gnome.org/GNOME/gtk/-/issues/7113
                            let is_dnd_grab = Self::is_dnd_grab(grab.as_any());

                            if !is_dnd_grab {
                                grab_start_data =
                                    Some(PointerOrTouchStartData::Touch(start_data.clone()));
                            }
                        }
                    }
                }
            });
        }

        let Some(start_data) = grab_start_data else {
            return;
        };

        let Some((mapped, output)) = self.synoik.layout.find_window_and_output(wl_surface) else {
            return;
        };

        let Some(output) = output else {
            return;
        };

        let window = mapped.window.clone();
        let output = output.clone();

        match &start_data {
            PointerOrTouchStartData::Pointer(_) => {
                if let Some(grab) = MoveGrab::new(self, start_data, window.clone(), true, None) {
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                }
            }
            PointerOrTouchStartData::Touch(_) => {
                let touch = self.synoik.seat.get_touch().unwrap();
                if let Some(grab) = MoveGrab::new(self, start_data, window.clone(), true, None) {
                    touch.set_grab(self, grab, serial);
                }
            }
        }

        self.synoik.queue_redraw(&output);
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let wl_surface = surface.wl_surface();

        let mut grab_start_data = None;

        // See if this comes from a pointer grab.
        let pointer = self.synoik.seat.get_pointer().unwrap();
        if pointer.has_grab(serial) {
            if let Some(start_data) = pointer.grab_start_data() {
                if let Some((focus, _)) = &start_data.focus {
                    if focus.id().same_client_as(&wl_surface.id()) {
                        grab_start_data = Some(PointerOrTouchStartData::Pointer(start_data));
                    }
                }
            }
        }

        // See if this comes from a touch grab.
        if let Some(touch) = self.synoik.seat.get_touch() {
            if touch.has_grab(serial) {
                if let Some(start_data) = touch.grab_start_data() {
                    if let Some((focus, _)) = &start_data.focus {
                        if focus.id().same_client_as(&wl_surface.id()) {
                            grab_start_data = Some(PointerOrTouchStartData::Touch(start_data));
                        }
                    }
                }
            }
        }

        let Some(start_data) = grab_start_data else {
            return;
        };

        let Some((mapped, _)) = self.synoik.layout.find_window_and_output(wl_surface) else {
            return;
        };

        let edges = ResizeEdge::from(edges);
        let window = mapped.window.clone();

        // See if we got a double resize-click gesture.
        let time = get_monotonic_time();
        let last_cell = mapped.last_interactive_resize_start();
        let mut last = last_cell.get();
        last_cell.set(Some((time, edges)));

        // Floating windows don't have either of the double-resize-click gestures, so just allow it
        // to resize.
        if mapped.is_floating() {
            last = None;
            last_cell.set(None);
        }

        if let Some((last_time, last_edges)) = last {
            if time.saturating_sub(last_time) <= DOUBLE_CLICK_TIME {
                // Allow quick resize after a triple click.
                last_cell.set(None);

                let intersection = edges.intersection(last_edges);
                if intersection.intersects(ResizeEdge::LEFT_RIGHT) {
                    // FIXME: don't activate once we can pass specific windows to actions.
                    self.synoik.layout.activate_window(&window);
                    self.synoik.layer_shell_on_demand_focus = None;
                    self.synoik.layout.toggle_full_width();
                }
                if intersection.intersects(ResizeEdge::TOP_BOTTOM) {
                    self.synoik.layer_shell_on_demand_focus = None;
                    self.synoik.layout.reset_window_height(Some(&window));
                }
                // FIXME: granular.
                self.synoik.queue_redraw_all();
                return;
            }
        }

        if !self
            .synoik
            .layout
            .interactive_resize_begin(window.clone(), edges)
        {
            return;
        }

        match start_data {
            PointerOrTouchStartData::Pointer(start_data) => {
                let grab = ResizeGrab::new(start_data, window);
                pointer.set_grab(self, grab, serial, Focus::Clear);
            }
            PointerOrTouchStartData::Touch(start_data) => {
                let touch = self.synoik.seat.get_touch().unwrap();
                let grab = TouchResizeGrab::new(start_data, window);
                touch.set_grab(self, grab, serial);
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&PopupKind::Xdg(surface.clone()));
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, serial: Serial) {
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else {
            trace!("ignoring popup grab because no root surface");
            return;
        };

        // We need to hand out the grab in a way consistent with what update_keyboard_focus()
        // thinks the current focus is, otherwise it will desync and cause weird issues with
        // keyboard focus being at the wrong place.
        if self.synoik.exit_confirm_dialog.is_open() {
            trace!("ignoring popup grab because the exit confirm dialog is open");
            let _ = PopupManager::dismiss_popup(&root, &popup);
            return;
        } else if self.synoik.is_locked() {
            if Some(&root) != self.synoik.lock_surface_focus().as_ref() {
                trace!("ignoring popup grab because the session is locked");
                let _ = PopupManager::dismiss_popup(&root, &popup);
                return;
            }
        } else if self.synoik.screenshot_ui.is_open() {
            trace!("ignoring popup grab because the screenshot UI is open");
            let _ = PopupManager::dismiss_popup(&root, &popup);
            return;
        } else if let Some(output) = self.synoik.layout.active_output() {
            let layers = layer_map_for_output(output);

            // FIXME: somewhere here we probably need to check is_overview_open to match the logic
            // in update_keyboard_focus().

            if let Some(layer) = layers.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL) {
                // This is a grab for a layer surface.

                if let Some(mapped) = self.synoik.mapped_layer_surfaces.get(layer) {
                    if mapped.place_within_backdrop() {
                        trace!("ignoring popup grab for a layer surface within overview backdrop");
                        let _ = PopupManager::dismiss_popup(&root, &popup);
                        return;
                    }
                }
            } else {
                // This is a grab for a regular window; check that there's no layer surface with a
                // higher input priority.

                if layers.layers_on(Layer::Overlay).any(|l| {
                    (l.cached_state().keyboard_interactivity
                        == wlr_layer::KeyboardInteractivity::Exclusive
                        || Some(l) == self.synoik.layer_shell_on_demand_focus.as_ref())
                        && self.synoik.mapped_layer_surfaces.contains_key(l)
                }) {
                    trace!("ignoring toplevel popup grab because the overlay layer has focus");
                    let _ = PopupManager::dismiss_popup(&root, &popup);
                    return;
                }

                let mon = self.synoik.layout.monitor_for_output(output).unwrap();
                if !mon.render_above_top_layer()
                    && layers.layers_on(Layer::Top).any(|l| {
                        (l.cached_state().keyboard_interactivity
                            == wlr_layer::KeyboardInteractivity::Exclusive
                            || Some(l) == self.synoik.layer_shell_on_demand_focus.as_ref())
                            && self.synoik.mapped_layer_surfaces.contains_key(l)
                    })
                {
                    trace!("ignoring toplevel popup grab because the top layer has focus");
                    let _ = PopupManager::dismiss_popup(&root, &popup);
                    return;
                }

                let layout_focus = self.synoik.layout.focus();
                if Some(&root) != layout_focus.map(|win| win.toplevel().wl_surface()) {
                    trace!("ignoring toplevel popup grab because another window has focus");
                    let _ = PopupManager::dismiss_popup(&root, &popup);
                    return;
                }
            }
        } else {
            trace!("ignoring popup grab because no output is active");
            let _ = PopupManager::dismiss_popup(&root, &popup);
            return;
        }

        let seat = &self.synoik.seat;
        let mut grab = match self
            .synoik
            .popups
            .grab_popup(root.clone(), popup, seat, serial)
        {
            Ok(grab) => grab,
            Err(err) => {
                trace!("ignoring popup grab: {err:?}");
                return;
            }
        };

        let keyboard = seat.get_keyboard().unwrap();
        let pointer = seat.get_pointer().unwrap();

        // Smithay cannot do overlapping grabs, so if we have an IME keyboard grab, don't overwrite
        // it with a popup keyboard grab. This makes the popup menu work in Telegram while an IME
        // is active (otherwise it hits the grab mismatch check below).
        //
        // The second check is for layer surfaces that can't receive keyboard focus, without it
        // popups don't work properly in Waybar (GTK 3).
        let can_receive_keyboard_focus = !self.synoik.seat.input_method().keyboard_grabbed()
            && self
                .synoik
                .layout
                .active_output()
                .and_then(|output| {
                    layer_map_for_output(output)
                        .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                        .map(|layer_surface| layer_surface.can_receive_keyboard_focus())
                })
                .unwrap_or(true);

        let keyboard_grab_mismatches = keyboard.is_grabbed()
            && !(keyboard.has_grab(serial)
                || grab.previous_serial().is_none_or(|s| keyboard.has_grab(s)));
        let pointer_grab_mismatches = pointer.is_grabbed()
            && !(pointer.has_grab(serial)
                || grab.previous_serial().is_none_or(|s| pointer.has_grab(s)));
        if (can_receive_keyboard_focus && keyboard_grab_mismatches) || pointer_grab_mismatches {
            trace!("ignoring popup grab because of current grab mismatch");
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        trace!("new grab for root {:?}", root);
        if can_receive_keyboard_focus {
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        self.synoik.popup_grab = Some(PopupGrabState {
            root,
            grab,
            has_keyboard_grab: can_receive_keyboard_focus,
        });
    }

    fn maximize_request(&mut self, toplevel: ToplevelSurface) {
        if let Some((mapped, _)) = self
            .synoik
            .layout
            .find_window_and_output_mut(toplevel.wl_surface())
        {
            // A configure is required in response to this event regardless if there are pending
            // changes.
            mapped.set_needs_configure();

            let window = mapped.window.clone();
            self.synoik.layout.set_maximized(&window, true);
        } else if let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) {
            match &mut unmapped.state {
                InitialConfigureState::NotConfigured {
                    wants_maximized, ..
                } => {
                    *wants_maximized = true;

                    // The required configure will be the initial configure.
                }
                InitialConfigureState::Configured {
                    restore: _,
                    rules,
                    output,
                    workspace_name,
                    is_pending_maximized,
                    ..
                } => {
                    let parent = toplevel.parent();
                    let target = self.synoik.layout.resolve_placement(PlacementSeeds {
                        workspace_name: workspace_name.as_deref(),
                        output: output.as_ref(),
                        parent: parent.as_ref(),
                        ..Default::default()
                    });

                    *output = target.output_to_store();
                    let ws = target.workspace;

                    if let Some(ws) = ws {
                        // If the window is pending fullscreen, then this will do nothing. But
                        // that's expected: the window remains fullscreen, and we simply remember
                        // that it is now pending maximized.
                        *is_pending_maximized = true;
                        toplevel.with_pending_state(|state| {
                            if !state.states.contains(xdg_toplevel::State::Fullscreen) {
                                state.states.set(xdg_toplevel::State::Maximized);
                            }
                        });
                        ws.configure_new_window(&unmapped.window, None, None, false, rules);
                    }

                    // We already sent the initial configure, so we need to reconfigure.
                    toplevel.send_configure();
                }
            }
        } else {
            error!("couldn't find the toplevel in maximize_request()");
            toplevel.send_configure();
        }
    }

    fn unmaximize_request(&mut self, toplevel: ToplevelSurface) {
        if let Some((mapped, _)) = self
            .synoik
            .layout
            .find_window_and_output_mut(toplevel.wl_surface())
        {
            // A configure is required in response to this event regardless if there are pending
            // changes.
            mapped.set_needs_configure();

            let window = mapped.window.clone();
            self.synoik.layout.set_maximized(&window, false);
        } else if let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) {
            match &mut unmapped.state {
                InitialConfigureState::NotConfigured {
                    wants_maximized, ..
                } => {
                    *wants_maximized = false;

                    // The required configure will be the initial configure.
                }
                InitialConfigureState::Configured {
                    restore: _,
                    rules,
                    width,
                    height,
                    floating_width,
                    floating_height,
                    is_full_width,
                    output,
                    workspace_name,
                    is_pending_maximized,
                } => {
                    let parent = toplevel.parent();
                    let target = self.synoik.layout.resolve_placement(PlacementSeeds {
                        workspace_name: workspace_name.as_deref(),
                        output: output.as_ref(),
                        parent: parent.as_ref(),
                        ..Default::default()
                    });

                    *output = target.output_to_store();
                    let ws = target.workspace;

                    if let Some(ws) = ws {
                        // If the window is pending fullscreen, then this will do nothing since
                        // then the Maximized state is already unset. But that's expected: the
                        // window remains fullscreen, and we simply remember that it is no
                        // longer pending maximized.
                        *is_pending_maximized = false;
                        toplevel.with_pending_state(|state| {
                            state.states.unset(xdg_toplevel::State::Maximized);
                        });

                        let is_floating = rules.compute_open_floating(
                            &toplevel,
                            self.synoik.config.borrow().layout.windowing_mode,
                        );
                        let configure_width = if is_floating {
                            *floating_width
                        } else if *is_full_width {
                            Some(PresetSize::Proportion(1.))
                        } else {
                            *width
                        };
                        let configure_height = if is_floating {
                            *floating_height
                        } else {
                            *height
                        };
                        ws.configure_new_window(
                            &unmapped.window,
                            configure_width,
                            configure_height,
                            is_floating,
                            rules,
                        );
                    }

                    // We already sent the initial configure, so we need to reconfigure.
                    toplevel.send_configure();
                }
            }
        } else {
            error!("couldn't find the toplevel in unmaximize_request()");
            toplevel.send_configure();
        }
    }

    fn fullscreen_request(
        &mut self,
        toplevel: ToplevelSurface,
        wl_output: Option<wl_output::WlOutput>,
    ) {
        let requested_output = wl_output.and_then(|o| self.synoik.output_from_resource(&o));

        if let Some((mapped, current_output)) = self
            .synoik
            .layout
            .find_window_and_output_mut(toplevel.wl_surface())
        {
            // A configure is required in response to this event regardless if there are pending
            // changes.
            mapped.set_needs_configure();

            let window = mapped.window.clone();

            if let Some(requested_output) = requested_output {
                if Some(&requested_output) != current_output {
                    self.synoik.layout.move_to_output(
                        Some(&window),
                        &requested_output,
                        None,
                        ActivateWindow::Smart,
                    );
                }
            }

            self.synoik.layout.set_fullscreen(&window, true);
        } else if let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) {
            match &mut unmapped.state {
                InitialConfigureState::NotConfigured {
                    wants_fullscreen, ..
                } => {
                    *wants_fullscreen = Some(requested_output);

                    // The required configure will be the initial configure.
                }
                InitialConfigureState::Configured {
                    restore: _,
                    rules,
                    output,
                    workspace_name,
                    ..
                } => {
                    let parent = toplevel.parent();
                    let target = self.synoik.layout.resolve_placement(PlacementSeeds {
                        // Only when the client did not name an output: a named workspace pins
                        // the monitor, so seeding both would let the workspace we resolved
                        // earlier veto the output the client just asked for. The mapped path
                        // above honours `requested_output` unconditionally; so do we.
                        workspace_name: if requested_output.is_some() {
                            None
                        } else {
                            workspace_name.as_deref()
                        },
                        // A requested output wins; otherwise the one we resolved before.
                        output: requested_output.as_ref().or(output.as_ref()),
                        parent: parent.as_ref(),
                        ..Default::default()
                    });

                    *output = target.output_to_store();
                    let ws = target.workspace;

                    if let Some(ws) = ws {
                        toplevel.with_pending_state(|state| {
                            state.states.set(xdg_toplevel::State::Fullscreen);
                            state.states.unset(xdg_toplevel::State::Maximized);
                        });
                        ws.configure_new_window(&unmapped.window, None, None, false, rules);
                    }

                    // We already sent the initial configure, so we need to reconfigure.
                    toplevel.send_configure();
                }
            }
        } else {
            error!("couldn't find the toplevel in fullscreen_request()");
            toplevel.send_configure();
        }
    }

    fn unfullscreen_request(&mut self, toplevel: ToplevelSurface) {
        if let Some((mapped, _)) = self
            .synoik
            .layout
            .find_window_and_output_mut(toplevel.wl_surface())
        {
            // A configure is required in response to this event regardless if there are pending
            // changes.
            mapped.set_needs_configure();

            let window = mapped.window.clone();
            self.synoik.layout.set_fullscreen(&window, false);
        } else if let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) {
            match &mut unmapped.state {
                InitialConfigureState::NotConfigured {
                    wants_fullscreen, ..
                } => {
                    *wants_fullscreen = None;

                    // The required configure will be the initial configure.
                }
                InitialConfigureState::Configured {
                    restore: _,
                    rules,
                    width,
                    height,
                    floating_width,
                    floating_height,
                    is_full_width,
                    output,
                    workspace_name,
                    is_pending_maximized,
                } => {
                    let parent = toplevel.parent();
                    let target = self.synoik.layout.resolve_placement(PlacementSeeds {
                        workspace_name: workspace_name.as_deref(),
                        output: output.as_ref(),
                        parent: parent.as_ref(),
                        ..Default::default()
                    });

                    *output = target.output_to_store();
                    let ws = target.workspace;

                    if let Some(ws) = ws {
                        toplevel.with_pending_state(|state| {
                            state.states.unset(xdg_toplevel::State::Fullscreen);

                            if *is_pending_maximized {
                                state.states.set(xdg_toplevel::State::Maximized);
                            }
                        });

                        let is_floating = rules.compute_open_floating(
                            &toplevel,
                            self.synoik.config.borrow().layout.windowing_mode,
                        );
                        let configure_width = if is_floating {
                            *floating_width
                        } else if *is_full_width {
                            Some(PresetSize::Proportion(1.))
                        } else {
                            *width
                        };
                        let configure_height = if is_floating {
                            *floating_height
                        } else {
                            *height
                        };
                        ws.configure_new_window(
                            &unmapped.window,
                            configure_width,
                            configure_height,
                            is_floating,
                            rules,
                        );
                    }

                    // We already sent the initial configure, so we need to reconfigure.
                    toplevel.send_configure();
                }
            }
        } else {
            error!("couldn't find the toplevel in unfullscreen_request()");
            toplevel.send_configure();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self
            .synoik
            .unmapped_windows
            .remove(surface.wl_surface())
            .is_some()
        {
            // An unmapped toplevel got destroyed.
            return;
        }

        let win_out = self
            .synoik
            .layout
            .find_window_and_output(surface.wl_surface());

        let Some((mapped, output)) = win_out else {
            // I have no idea how this can happen, but I saw it happen once, in a weird interaction
            // involving laptop going to sleep and resuming.
            error!("toplevel missing from both unmapped_windows and layout");
            return;
        };
        let window = mapped.window.clone();
        let output = output.cloned();

        let id = mapped.id();
        self.synoik
            .stop_casts_for_target(CastTarget::Window { id: id.get() });

        self.save_session_toplevel(&window);
        self.store_unmap_snapshot(&window);

        let transaction = Transaction::new();
        let blocker = transaction.blocker();
        // As in `CompositorHandler::commit`: the close snapshot is renderer-neutral, so it starts
        // the animation without a renderer rather than through one it ignores.
        self.synoik
            .layout
            .start_close_animation_for_window(&window, blocker);

        let active_window = self.synoik.layout.focus().map(|m| &m.window);
        let was_active = active_window == Some(&window);

        self.window_removed(id);
        self.synoik
            .layout
            .remove_window(&window, transaction.clone());

        let surface = surface.wl_surface();
        // This check is necessary because implicit resource destruction is done with
        // undefined order, so the surface might get destroyed before toplevel_destroyed() is
        // called. In this case, adding the default pre-commit hook here would leak it, since the
        // place that removes it is WlSurface::destroyed(), which had already been called by now.
        if surface.is_alive() {
            self.add_default_dmabuf_pre_commit_hook(surface);
        }

        // If this is the only instance, then this transaction will complete immediately, so no
        // need to set the timer.
        if !transaction.is_last() {
            transaction.register_deadline_timer(&self.synoik.event_loop);
        }

        if was_active {
            self.maybe_warp_cursor_to_focus();
        }

        if let Some(output) = output {
            self.synoik.queue_redraw(&output);
            self.synoik.queue_redraw_switcher_output();
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(output) = self.output_for_popup(&PopupKind::Xdg(surface)) {
            self.synoik.queue_redraw(&output.clone());
        }
    }

    fn app_id_changed(&mut self, toplevel: ToplevelSurface) {
        self.update_window_rules(&toplevel);
    }

    fn title_changed(&mut self, toplevel: ToplevelSurface) {
        self.update_window_rules(&toplevel);
    }

    fn parent_changed(&mut self, toplevel: ToplevelSurface) {
        let Some(parent) = toplevel.parent() else {
            return;
        };

        if let Some((mapped, output)) = self.synoik.layout.find_window_and_output_mut(&parent) {
            let output = output.cloned();
            let window = mapped.window.clone();
            if self.synoik.layout.descendants_added(&window) {
                if let Some(output) = output {
                    self.synoik.queue_redraw(&output);
                }
            }
        }
    }
}

delegate_xdg_shell!(State);

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // If we want CSD, we hide this global altogether.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: zxdg_toplevel_decoration_v1::Mode) {
        // Set whatever the client wants, rather than our preferred mode. This especially matters
        // for SDL2 which has a bug where forcing a different (client-side) decoration mode during
        // their window creation sequence would leave the window permanently hidden.
        //
        // https://github.com/libsdl-org/SDL/issues/8173
        //
        // The bug has been fixed, but there's a ton of apps which will use the buggy version for a
        // long while...
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });

        // A configure is required in response to this event. However, if an initial configure
        // wasn't sent, then we will send this as part of the initial configure later.
        if toplevel.is_initial_configure_sent() {
            // If this is a mapped window, flag it as needs configure to avoid duplicate configures.
            let surface = toplevel.wl_surface();
            if let Some((mapped, _)) = self.synoik.layout.find_window_and_output_mut(surface) {
                mapped.set_needs_configure();
            } else {
                toplevel.send_configure();
            }
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // If we want CSD, we hide this global altogether.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        });

        // A configure is required in response to this event. However, if an initial configure
        // wasn't sent, then we will send this as part of the initial configure later.
        if toplevel.is_initial_configure_sent() {
            // If this is a mapped window, flag it as needs configure to avoid duplicate configures.
            let surface = toplevel.wl_surface();
            if let Some((mapped, _)) = self.synoik.layout.find_window_and_output_mut(surface) {
                mapped.set_needs_configure();
            } else {
                toplevel.send_configure();
            }
        }
    }
}
delegate_xdg_decoration!(State);

/// Whether KDE server decorations are in use.
#[derive(Default, Clone)]
pub struct KdeDecorationsModeState {
    server: Cell<bool>,
}

impl KdeDecorationsModeState {
    pub fn is_server(&self) -> bool {
        self.server.get()
    }
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.synoik.kde_decoration_state
    }

    fn request_mode(
        &mut self,
        surface: &WlSurface,
        decoration: &org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        mode: wayland_server::WEnum<org_kde_kwin_server_decoration::Mode>,
    ) {
        let WEnum::Value(mode) = mode else {
            return;
        };

        decoration.mode(mode);

        with_states(surface, |states| {
            let state = states
                .data_map
                .get_or_insert(KdeDecorationsModeState::default);
            state
                .server
                .set(mode == org_kde_kwin_server_decoration::Mode::Server);
        });
    }
}
delegate_kde_decoration!(State);

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.synoik.xdg_foreign_state
    }
}
delegate_xdg_foreign!(State);

/// A resolved session restore, from [`State::resolve_session_restore`].
#[derive(Debug)]
struct SessionRestore {
    /// The handle to send `restored` on, immediately before the first configure.
    handle: XdgToplevelSessionV1,
    /// Decides activation, and nothing else.
    reason: Reason,
    /// The record as it will be replayed, exactly as it was saved.
    record: ToplevelRecord,
    /// Which slot of an absent display's strip this record asks for, when its display is gone.
    ///
    /// The record's own index means nothing on the strip it is landing on, so it is ranked
    /// against every other absent slot in the store instead — see [`State::absent_restore_slots`].
    slot: Option<RestoreSlot>,
    /// The output the record names, or `None` if that display is not connected.
    ///
    /// Gates the position: an output-local rect means nothing without its output.
    output: Option<Output>,
    /// Where a record whose display is gone goes instead: the primary, matching what a live
    /// unplug does with that display's workspaces. Deliberately *not* `output`, which would
    /// unlock the position replay along with it.
    fallback_output: Option<Output>,
}

impl State {
    /// What a `restore_toplevel` request resolves to, once the initial configure actually fires.
    ///
    /// Nothing here is trusted from when the request was made: the session can have been taken
    /// over, the record removed, or the handle destroyed in between. A takeover empties the
    /// previous holder's registrations, so it simply fails to resolve.
    fn resolve_session_restore(&self, toplevel: &ToplevelSurface) -> Option<SessionRestore> {
        let target = self
            .synoik
            .session_manager_state
            .restore_target_for(toplevel.xdg_toplevel())?;
        if !target.handle.is_alive() {
            return None;
        }

        // Both misses are logged rather than swallowed: a client asking under a session id or a
        // name the store has never seen is indistinguishable, from the outside, from a restore
        // that replayed a stale record — and it is the likelier of the two.
        let Some(session) = self
            .synoik
            .session_manager_state
            .store
            .get(&target.session_id)
        else {
            info!(
                "session restore: no session {} in the store (toplevel {})",
                target.session_id, target.name
            );
            return None;
        };
        let Some(record) = session.toplevels.get(&target.name).cloned() else {
            info!(
                "session restore: session {} has no toplevel {} ({} known: {:?})",
                target.session_id,
                target.name,
                session.toplevels.len(),
                session.toplevels.keys().collect::<Vec<_>>()
            );
            return None;
        };

        let output = record
            .output
            .as_ref()
            .and_then(|saved| self.output_matching_identity(saved));

        // A display that did not come back sends its windows to the primary, where a live unplug
        // would have appended its workspaces — so they land there as a block too, below what the
        // primary's strip holds on its own account, rather than interleaving with it.
        // Only a record that *names* a display can be homeless: one with no display recorded (a
        // v1 leftover, or a window that was never on an output) has no group to keep together and
        // no strip to be appended after, so it takes the ordinary placement path unchanged.
        let mut slot = None;
        let fallback_output = if output.is_some() || record.output.is_none() {
            None
        } else {
            let primary = self.synoik.layout.primary_output().cloned();
            if primary.is_some() {
                if let Some((identity, workspace)) = record.output.clone().zip(record.workspace) {
                    slot = Some(RestoreSlot {
                        identity,
                        workspace,
                    });
                }
            }
            primary
        };

        info!(
            "session restore: {}/{} replaying {:?} state {:?} workspace {:?}/{:?} onto {:?} (saved on {:?})",
            target.session_id,
            target.name,
            record.floating_rect,
            record.state,
            record.workspace,
            record.workspace_name,
            output.as_ref().map(|o| o.name()),
            record.output.as_ref().map(|o| &o.connector)
        );

        Some(SessionRestore {
            handle: target.handle,
            reason: target.reason,
            record,
            slot,
            output,
            fallback_output,
        })
    }

    /// Every workspace the store asks for on a display that is not connected, in one order.
    ///
    /// A live unplug appends the removed display's whole stack to the primary, before the trailing
    /// empty workspace, with its internal order intact (`Monitor::append_workspaces`). A restore
    /// that finds the display missing lands the same way: the absent displays' workspaces form one
    /// block below what the primary's strip holds on its own account, displays in connector order
    /// and each display's workspaces in its own, so two of them do not interleave.
    ///
    /// Read from the *whole store*, not from the session that happens to be restoring. Two apps
    /// coming back at once name slots on the same absent display, and a per-session answer had
    /// each of them counting only its own records — so their windows interleaved with each other's
    /// (`docs/fork/multi-display.md` §3).
    ///
    /// Distinct slots, so what a session that never comes back costs is one rank per workspace it
    /// names rather than the number it names. Nothing here reserves anything either: a slot
    /// becomes a workspace only when a window actually restores into it, so a stale record naming
    /// desktop 30 grows no strip.
    fn absent_restore_slots(&self) -> Vec<RestoreSlot> {
        let mut slots: Vec<RestoreSlot> = self
            .synoik
            .session_manager_state
            .store
            .records()
            .filter_map(|record| Some((record.output.as_ref()?, record.workspace?)))
            .filter(|(saved, _)| self.output_matching_identity(saved).is_none())
            .map(|(saved, workspace)| RestoreSlot {
                identity: saved.clone(),
                workspace,
            })
            .collect();
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    /// The workspaces materialized for absent displays so far, forgetting any that has gone away.
    ///
    /// A workspace under a slot is not forever: its display coming back takes it along. Pruning
    /// here rather than at the removal keeps the block a derived fact, with one place to be wrong.
    pub(super) fn materialized_restore_block(&mut self) -> Vec<WorkspaceId> {
        let alive: Vec<WorkspaceId> = self
            .synoik
            .session_manager_state
            .materialized_workspaces()
            .filter(|id| self.synoik.layout.find_workspace_by_id(*id).is_some())
            .collect();
        self.synoik
            .session_manager_state
            .retain_materialized_workspaces(&alive);
        alive
    }

    /// The workspace a slot means on `output`, making it if this is the first window to ask.
    ///
    /// Placed against the block's other workspaces rather than at an index, so the answer does not
    /// depend on which window restored first: a slot goes above every materialized slot that ranks
    /// after it, and at the bottom of the strip when none of them is here yet.
    pub(super) fn materialize_restore_slot(
        &mut self,
        slot: &RestoreSlot,
        output: &Output,
    ) -> Option<WorkspaceId> {
        let block = self.materialized_restore_block();
        if let Some(id) = self
            .synoik
            .session_manager_state
            .materialized_workspace(slot)
        {
            return Some(id);
        }

        let order = self.absent_restore_slots();
        let after = order
            .iter()
            .position(|known| known == slot)
            .map_or(order.len(), |rank| rank + 1);
        let before = order[after.min(order.len())..].iter().find_map(|known| {
            self.synoik
                .session_manager_state
                .materialized_workspace(known)
        });

        let id = self.synoik.layout.insert_restore_workspace(
            output,
            before,
            &block,
            slot.identity.clone(),
            slot.workspace as usize,
        )?;
        self.synoik
            .session_manager_state
            .set_materialized_workspace(slot.clone(), id);
        Some(id)
    }

    /// The connected output a record was saved on, by identity.
    ///
    /// Mutter asks which monitor a *global* rect lands in
    /// (`meta_monitor_manager_get_logical_monitor_from_rect`), which answers a different question:
    /// where the window would be if the layout had not changed. A record names its display, so the
    /// answer does not depend on the layout at all — which is what lets a session come back under
    /// a configuration that shares no origin with the one that saved it.
    ///
    /// `None` when that display is not connected. The normal placement chain then decides, rather
    /// than a remembered position being replayed onto a monitor that never held the window.
    fn output_matching_identity(&self, saved: &OutputIdentity) -> Option<Output> {
        self.synoik
            .global_space
            .outputs()
            .find(|output| saved.matches_output(output))
            .cloned()
    }

    pub fn send_initial_configure(&mut self, toplevel: &ToplevelSurface) {
        let _span = tracy_client::span!("State::send_initial_configure");

        // Resolved up here because `output_under_cursor` borrows all of
        // `self.synoik`, which the `unmapped_windows` borrow below rules out.
        let pointer_output = (self.synoik.config.borrow().layout.windowing_mode
            == WindowingMode::Floating)
            .then(|| self.synoik.output_under_cursor())
            .flatten();

        // Resolved before the `unmapped_windows` borrow below, like `pointer_output`. `None` for
        // every window that did not ask to be restored, which is what keeps this whole branch
        // additive: without it, everything below runs exactly as it did before.
        let wants_restore = self
            .synoik
            .unmapped_windows
            .get(toplevel.wl_surface())
            .is_some_and(|unmapped| unmapped.wants_session_restore);
        let restore = wants_restore
            .then(|| self.resolve_session_restore(toplevel))
            .flatten();
        let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) else {
            error!("window must be present in unmapped_windows in send_initial_configure()");
            return;
        };

        let config = self.synoik.config.borrow();
        let mut rules = ResolvedWindowRules::compute(
            &config.window_rules,
            WindowRef::Unmapped(unmapped),
            self.synoik.is_at_startup,
        );

        // Restore writes itself into the *rules*, so the placement, sizing and state code below
        // stays one path. It overrides any config window rule: the saved state is both more
        // specific and more recent than a static rule. The seeds are kept as well as applied, so
        // that a later recompute of the rules — a title change, a config reload — can put them
        // back instead of dropping them; see `RestoreRuleSeeds`.
        let mut rule_seeds = RestoreRuleSeeds::default();
        let restored_state = restore.as_ref().and_then(|r| r.record.restorable_state());
        if let Some(restore) = &restore {
            match restored_state {
                Some(WindowState::Fullscreen) => rule_seeds.open_fullscreen = Some(true),
                Some(WindowState::Maximized) => rule_seeds.open_maximized_to_edges = Some(true),
                // Edge tiling is applied on the map, once there is a tile to tile — see
                // `RestoreOnMap::edge_tiled`. What the configure owes it is the *size*, seeded
                // below out of the saved tiled rect, so the client's first buffer is already the
                // one the tiled slot wants instead of a floating size it will resize away from.
                _ => (),
            }

            // Whichever of the record's two rects this state's geometry lives in: the live rect
            // for anything the compositor sizes, the floating one otherwise.
            if let Some([_, _, w, h]) = restored_state.and_then(|s| s.saved_rect(&restore.record)) {
                rule_seeds.default_width = Some(Some(PresetSize::Fixed(w)));
                rule_seeds.default_height = Some(Some(PresetSize::Fixed(h)));
            }
            rule_seeds.apply(&mut rules);
        }

        let Unmapped { window, state, .. } = unmapped;

        let InitialConfigureState::NotConfigured {
            wants_fullscreen,
            wants_maximized,
        } = state
        else {
            error!("window must not be already configured in send_initial_configure()");
            return;
        };

        // Two things can name an output here: the window rules, and a fullscreen request that
        // came in before the initial configure. Take the first that actually corresponds to a
        // monitor, so a rule naming a disconnected output doesn't mask the fullscreen request.
        let rule_output = rules.open_on_output.as_deref().and_then(|name| {
            self.synoik
                .global_space
                .outputs()
                .find(|output| output_matches_name(output, name))
        });
        let fullscreen_output = wants_fullscreen.as_ref().and_then(|x| x.as_ref());
        // A restore's saved rect names the output ahead of a rule, since it *is* where the window
        // was; the rule only ever described where it should open the first time.
        // The recorded display when it is here, else the primary — a homeless *group* has to land
        // together for its appended workspace numbering to mean anything, and per-window fallback
        // through the pointer or the active monitor would scatter it.
        let restore_output = restore
            .as_ref()
            .and_then(|r| r.output.as_ref().or(r.fallback_output.as_ref()));
        let seed_output = [restore_output, rule_output, fullscreen_output]
            .into_iter()
            .flatten()
            .find(|o| self.synoik.layout.monitor_for_output(o).is_some());

        let parent = toplevel.parent();
        // A record whose display is gone contributes no index here: its saved one is a position in
        // *that* display's strip, and what it lands on is decided at the map, once the block it
        // belongs to has somewhere to be. The configure loses nothing by it — a working area is
        // per-monitor, so every workspace of the fallback output answers the same.
        let restore_workspace_idx = restore
            .as_ref()
            .filter(|restore| restore.slot.is_none())
            .and_then(|restore| restore.record.workspace)
            .map(|idx| idx as usize);

        // A saved workspace *name* is asked for ahead of the index, and only when some monitor
        // still has it: names survive a restart, indices into a dynamic stack only approximate
        // one. A name that is gone falls back to the index rather than to nothing — unlike the
        // `open-on-workspace` rule, which is a standing instruction, this is a memory, and a
        // memory that no longer resolves should degrade rather than cancel the placement.
        // Matching by name also lets a window follow a workspace the user moved to another
        // display, which is the whole reason a name outranks the record's output here.
        let restore_workspace_name = restore
            .as_ref()
            .and_then(|restore| restore.record.workspace_name.as_deref())
            .filter(|name| self.synoik.layout.monitor_for_workspace(name).is_some());

        let target = self.synoik.layout.resolve_placement(PlacementSeeds {
            workspace_name: restore_workspace_name.or(rules.open_on_workspace.as_deref()),
            workspace_idx: restore_workspace_idx,
            output: seed_output,
            // A dialog with a parent follows it, and `output_to_store` then declines to pin the
            // output so that mapping re-fetches the parent's, in case it moved in between.
            parent: parent.as_ref(),
            // Only the initial configure seeds the pointer. This is where mutter seeds
            // `window->monitor` for a window that gave no position hint (`window.c:1245-1259`),
            // and the same monitor placement later picks up
            // (`meta_backend_get_current_logical_monitor`, `place.c:951-955`) — the pointer's,
            // not the one the keyboard focus last landed on. niri's scrolling mode keeps the
            // active monitor. Later requests must not re-consult it, or a window would hop
            // monitors because the mouse moved.
            pointer_output: pointer_output.as_ref(),
        });

        let output = target.output_to_store();
        let ws = target.workspace;

        // The saved position, converted into the frame `default_floating_position` speaks: the
        // working area of the workspace the window is actually landing on. That is per-workspace
        // (struts differ), which is why it waits until placement has resolved rather than being
        // folded in with the size above. Going in, a floating rule is what makes the placement
        // cascade leave the window alone (`FloatingSpace::avoid_focus_window`), which is exactly
        // what restoring a remembered position wants.
        // A record whose display is gone has a rect that means nothing: it was output-local, and
        // there is no output to be local to. Placement decides in that case, so the position is
        // only replayed when the identity actually resolved.
        let restored_rect = restore
            .as_ref()
            .filter(|r| r.output.is_some())
            .and_then(|r| restored_state?.saved_rect(&r.record));
        if let Some(([x, y, w, h], ws)) = restored_rect.zip(ws) {
            // The rect comes off disk output-local; `default_floating_position` speaks the working
            // area of the workspace the window is actually landing on, which is per-workspace
            // (struts differ) and so has to wait until placement has resolved. No origin enters
            // here: adding one on save and subtracting a *different* one on restore is exactly the
            // ratchet that moved windows a monitor's width per session.
            let area = ws.working_area();

            // The display it lands on may be smaller than the one it was saved on — a different
            // configuration entirely is the case this schema exists for. Keep the window's
            // top-left inside the working area and its body from hanging off the far edge, in the
            // spirit of mutter's `META_MOVE_RESIZE_CONSTRAIN`.
            let clamp = |pos: f64, size: f64, extent: f64| pos.min(extent - size).max(0.);

            rule_seeds.default_floating_position = Some(FloatingPosition {
                x: FloatOrInt(clamp(f64::from(x) - area.loc.x, f64::from(w), area.size.w)),
                y: FloatOrInt(clamp(f64::from(y) - area.loc.y, f64::from(h), area.size.h)),
                relative_to: RelativeTo::TopLeft,
            });
            rule_seeds.apply(&mut rules);
        }

        let restore_on_map = restore.as_ref().map(|restore| RestoreOnMap {
            reason: restore.reason,
            workspace_idx: restore_workspace_idx,
            restore_slot: restore.slot.clone(),
            // Only for a window that maps straight into maximized or fullscreen — anything else
            // gets its floating geometry from the configure and the position rule.
            unmaximize_to: restored_state
                .filter(|state| *state != WindowState::Floating)
                // Output-local, and only meaningful when the display it is local to came back.
                .filter(|_| restore.output.is_some())
                .and(restore.record.floating_rect),
            displaced_rect: restore
                .output
                .is_some()
                .then_some(restore.record.displaced_rect)
                .flatten(),
            auto_maximized: restore.record.auto_maximized,
            // Not gated on the display coming back, unlike the rects: an edge-tiled window is
            // sized and placed from the work area it lands on, so the state survives a restore
            // onto a display the record never saw — the same way maximize and fullscreen do.
            edge_tiled: restored_state.and_then(WindowState::tile_side),
            minimized: restore.record.is_minimized,
            rule_seeds,
        });

        let mut width = None;
        let mut floating_width = None;
        let mut height = None;
        let mut floating_height = None;
        let is_full_width = rules.open_maximized.unwrap_or(false);
        let is_floating = rules
            .compute_open_floating(toplevel, self.synoik.config.borrow().layout.windowing_mode);

        let mut is_pending_maximized = false;
        if let Some(ws) = ws {
            // Set a fullscreen and maximized state based on window request and window rule.
            is_pending_maximized = (*wants_maximized && rules.open_maximized_to_edges.is_none())
                || rules.open_maximized_to_edges == Some(true);

            if (wants_fullscreen.is_some() && rules.open_fullscreen.is_none())
                || rules.open_fullscreen == Some(true)
            {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                });
            } else if is_pending_maximized {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Maximized);
                });
            }

            width = ws.resolve_default_width(rules.default_width, false);
            floating_width = ws.resolve_default_width(rules.default_width, true);
            height = ws.resolve_default_height(rules.default_height, false);
            floating_height = ws.resolve_default_height(rules.default_height, true);

            let configure_width = if is_floating {
                floating_width
            } else if is_full_width {
                Some(PresetSize::Proportion(1.))
            } else {
                width
            };
            let configure_height = if is_floating { floating_height } else { height };
            ws.configure_new_window(
                window,
                configure_width,
                configure_height,
                is_floating,
                &rules,
            );
        }

        // Set the tiled state for the initial configure.
        update_tiled_state(toplevel, config.prefer_no_csd, rules.tiled_state);

        // Set the configured settings.
        *state = InitialConfigureState::Configured {
            rules,
            width,
            height,
            floating_width,
            floating_height,
            is_full_width,
            output,
            workspace_name: ws.and_then(|w| w.name().cloned()),
            is_pending_maximized,
            restore: restore_on_map,
        };

        // "prior to the first xdg_toplevel.configure" — the spec pins the ordering, and the
        // client needs it to know that the configure it is about to see is a restored geometry
        // rather than a fresh placement.
        if let Some(restore) = &restore {
            if restore.handle.is_alive() {
                restore.handle.restored();
            }
        }

        trace!(surface = %toplevel.wl_surface().id(), "sending initial configure");
        toplevel.send_configure();
    }

    pub fn queue_initial_configure(&self, toplevel: ToplevelSurface) {
        // Send the initial configure in an idle, in case the client sent some more info after the
        // initial commit.
        self.synoik.event_loop.insert_idle(move |state| {
            if !toplevel.alive() {
                return;
            }

            if let Some(unmapped) = state.synoik.unmapped_windows.get(toplevel.wl_surface()) {
                if unmapped.needs_initial_configure() {
                    state.send_initial_configure(&toplevel);
                }
            }
        });
    }

    /// Should be called on `WlSurface::commit`
    pub fn popups_handle_commit(&mut self, surface: &WlSurface) {
        self.synoik.popups.commit(surface);

        if let Some(popup) = self.synoik.popups.find_popup(surface) {
            match popup {
                PopupKind::Xdg(ref popup) => {
                    if !popup.is_initial_configure_sent() {
                        if let Some(output) = self.output_for_popup(&PopupKind::Xdg(popup.clone()))
                        {
                            let scale = output.current_scale();
                            let transform = output.current_transform();
                            with_states(surface, |data| {
                                send_scale_transform(surface, data, scale, transform);
                            });
                        }
                        popup.send_configure().expect("initial configure failed");
                    }
                }
                // Input method popup can arbitrary change its geometry, so we need to unconstrain
                // it on commit.
                PopupKind::InputMethod(_) => {
                    self.unconstrain_popup(&popup);
                }
            }
        }
    }

    pub fn output_for_popup(&self, popup: &PopupKind) -> Option<&Output> {
        let root = find_popup_root_surface(popup).ok()?;
        self.synoik.output_for_root(&root)
    }

    pub fn unconstrain_popup(&self, popup: &PopupKind) {
        let _span = tracy_client::span!("Synoik::unconstrain_popup");

        // Popups with a NULL parent will get repositioned in their respective protocol handlers
        // (i.e. layer-shell).
        let Ok(root) = find_popup_root_surface(popup) else {
            return;
        };

        // Figure out if the root is a window or a layer surface.
        if let Some((mapped, _)) = self.synoik.layout.find_window_and_output(&root) {
            self.unconstrain_window_popup(popup, &mapped.window);
        } else if let Some((layer_surface, output)) = self.synoik.layout.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer_surface = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
            Some((layer_surface.clone(), o))
        }) {
            self.unconstrain_layer_shell_popup(popup, &layer_surface, output);
        }
    }

    fn unconstrain_window_popup(&self, popup: &PopupKind, window: &Window) {
        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = self.synoik.layout.popup_target_rect(window);
        target.loc -= get_popup_toplevel_coords(popup).to_f64();

        self.position_popup_within_rect(popup, target, true);
    }

    pub fn unconstrain_layer_shell_popup(
        &self,
        popup: &PopupKind,
        layer_surface: &LayerSurface,
        output: &Output,
    ) {
        let output_geo = self.synoik.global_space.output_geometry(output).unwrap();
        let map = layer_map_for_output(output);
        let Some(layer_geo) = map.layer_geometry(layer_surface) else {
            return;
        };

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = Rectangle::from_size(output_geo.size);

        // Background and bottom layer popups render below the top and the overlay layer, so let's
        // put them into the non-exclusive zone.
        //
        // FIXME: ideally this should use the "top and overlay layer" non-exclusive zone, but
        // Smithay only computes the "all layers" non-exclusive zone atm.
        //
        // FIXME: related to the above, top layer popups should use the "overlay layer"
        // non-exclusive zone.
        if matches!(layer_surface.layer(), Layer::Background | Layer::Bottom) {
            target = map.non_exclusive_zone();
        }

        target.loc -= layer_geo.loc;
        target.loc -= get_popup_toplevel_coords(popup);

        // Don't add padding to layer-shell popups. It's not really needed, and it's unexpected.
        self.position_popup_within_rect(popup, target.to_f64(), false);
    }

    fn position_popup_within_rect(
        &self,
        popup: &PopupKind,
        target: Rectangle<f64, Logical>,
        padding: bool,
    ) {
        match popup {
            PopupKind::Xdg(popup) => {
                popup.with_pending_state(|state| {
                    state.geometry = if padding {
                        unconstrain_with_padding(state.positioner, target)
                    } else {
                        state
                            .positioner
                            .get_unconstrained_geometry(target.to_i32_round())
                    };
                });
            }
            PopupKind::InputMethod(popup) => {
                let text_input_rectangle = popup.text_input_rectangle();
                let mut bbox =
                    utils::bbox_from_surface_tree(popup.wl_surface(), text_input_rectangle.loc)
                        .to_f64();

                // Position bbox horizontally first.
                let overflow_x = (bbox.loc.x + bbox.size.w) - (target.loc.x + target.size.w);
                if overflow_x > 0. {
                    bbox.loc.x -= overflow_x;
                }

                // Ensure that the popup starts within the window.
                bbox.loc.x = f64::max(bbox.loc.x, target.loc.x);

                // Try to position IME popup below the text input rectangle.
                let mut below = bbox;
                below.loc.y += f64::from(text_input_rectangle.size.h);

                let mut above = bbox;
                above.loc.y -= bbox.size.h;

                if target.loc.y + target.size.h >= below.loc.y + below.size.h {
                    popup.set_location(below.loc.to_i32_round());
                } else {
                    popup.set_location(above.loc.to_i32_round());
                }
            }
        }
    }

    pub fn update_reactive_popups(&self, window: &Window) {
        let _span = tracy_client::span!("Synoik::update_reactive_popups");

        for (popup, _) in PopupManager::popups_for_surface(
            window.toplevel().expect("no x11 support").wl_surface(),
        ) {
            match &popup {
                xdg_popup @ PopupKind::Xdg(popup) => {
                    if popup.with_pending_state(|state| state.positioner.reactive) {
                        self.unconstrain_window_popup(xdg_popup, window);
                        if let Err(err) = popup.send_pending_configure() {
                            warn!("error re-configuring reactive popup: {err:?}");
                        }
                    }
                }
                PopupKind::InputMethod(_) => (),
            }
        }
    }

    pub fn update_window_rules(&mut self, toplevel: &ToplevelSurface) {
        let config = self.synoik.config.borrow();
        let window_rules = &config.window_rules;

        if let Some(unmapped) = self.synoik.unmapped_windows.get_mut(toplevel.wl_surface()) {
            let new_rules = ResolvedWindowRules::compute(
                window_rules,
                WindowRef::Unmapped(unmapped),
                self.synoik.is_at_startup,
            );
            if let InitialConfigureState::Configured { rules, restore, .. } = &mut unmapped.state {
                *rules = new_rules;
                // Recomputing from the config alone would drop everything a session restore
                // seeded. Overlay it back — the window is still unmapped, so its saved position
                // and size have not been applied yet.
                if let Some(restore) = restore {
                    restore.rule_seeds.apply(rules);
                }
            }
        } else if let Some((mapped, output)) = self
            .synoik
            .layout
            .find_window_and_output_mut(toplevel.wl_surface())
        {
            if mapped.recompute_window_rules(window_rules, self.synoik.is_at_startup) {
                drop(config);
                let output = output.cloned();
                let window = mapped.window.clone();
                self.synoik.layout.update_window(&window, None);

                if let Some(output) = output {
                    self.synoik.queue_redraw(&output);
                }
            }
        }
    }
}

fn unconstrain_with_padding(
    positioner: PositionerState,
    target: Rectangle<f64, Logical>,
) -> Rectangle<i32, Logical> {
    // Try unconstraining with a small padding first which looks nicer, then if it doesn't fit try
    // unconstraining without padding.
    const PADDING: f64 = 8.;

    let mut padded = target;
    if PADDING * 2. < padded.size.w {
        padded.loc.x += PADDING;
        padded.size.w -= PADDING * 2.;
    }
    if PADDING * 2. < padded.size.h {
        padded.loc.y += PADDING;
        padded.size.h -= PADDING * 2.;
    }

    // No padding, so just unconstrain with the original target.
    if padded == target {
        return positioner.get_unconstrained_geometry(target.to_i32_round());
    }

    // Do not try to resize to fit the padded target rectangle.
    let mut no_resize = positioner;
    no_resize
        .constraint_adjustment
        .remove(ConstraintAdjustment::ResizeX);
    no_resize
        .constraint_adjustment
        .remove(ConstraintAdjustment::ResizeY);

    let geo = no_resize.get_unconstrained_geometry(padded.to_i32_round());
    if padded.contains_rect(geo.to_f64()) {
        return geo;
    }

    // Could not unconstrain into the padded target, so resort to the regular one.
    positioner.get_unconstrained_geometry(target.to_i32_round())
}

pub fn add_mapped_toplevel_pre_commit_hook(toplevel: &ToplevelSurface) -> HookId {
    add_pre_commit_hook::<State, _>(toplevel.wl_surface(), move |state, _dh, surface| {
        let _span = tracy_client::span!("mapped toplevel pre-commit");
        let span =
            trace_span!("toplevel pre-commit", surface = %surface.id(), serial = Empty).entered();

        let Some((mapped, output)) = state.synoik.layout.find_window_and_output_mut(surface) else {
            error!("pre-commit hook for mapped surfaces must be removed upon unmapping");
            return;
        };

        let (got_unmapped, dmabuf, acquire_point, commit_serial) = with_states(surface, |states| {
            let (got_unmapped, dmabuf) = {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                match guard.pending().buffer.as_ref() {
                    Some(BufferAssignment::NewBuffer(buffer)) => {
                        let dmabuf = get_dmabuf(buffer).cloned().ok();
                        (false, dmabuf)
                    }
                    Some(BufferAssignment::Removed) => (true, None),
                    None => (false, None),
                }
            };

            // Explicit-sync acquire timeline point, if the client set one this commit.
            let acquire_point = states
                .cached_state
                .get::<DrmSyncobjCachedState>()
                .pending()
                .acquire_point
                .clone();

            let role = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            let serial = role.last_acked.as_ref().map(|c| c.serial);

            (got_unmapped, dmabuf, acquire_point, serial)
        });

        let mut transaction_for_dmabuf = None;
        let mut animate = false;
        if let Some(serial) = commit_serial {
            if !span.is_disabled() {
                span.record("serial", format!("{serial:?}"));
            }

            // trace!("taking pending transaction");
            if let Some(transaction) = mapped.take_pending_transaction(serial) {
                // Transaction can be already completed if it ran past the deadline.
                let disable = state.synoik.config.borrow().debug.disable_transactions;
                if !transaction.is_completed() && !disable {
                    // Register the deadline even if this is the last pending, since dmabuf
                    // rendering can still run over the deadline.
                    transaction.register_deadline_timer(&state.synoik.event_loop);

                    let is_last = transaction.is_last();

                    // If this is the last transaction, we don't need to add a separate
                    // notification, because the transaction will complete in our dmabuf blocker
                    // callback, which already calls blocker_cleared(), or by the end of this
                    // function, in which case there would be no blocker in the first place.
                    if !is_last {
                        // Waiting for some other surface; register a notification and add a
                        // transaction blocker.
                        if let Some(client) = surface.client() {
                            transaction.add_notification(
                                state.synoik.blocker_cleared_tx.clone(),
                                client.clone(),
                            );
                            add_blocker(surface, transaction.blocker());
                        }
                    }

                    // Delay dropping (and completing) the transaction until the dmabuf is ready.
                    // If there's no dmabuf, this will be dropped by the end of this pre-commit
                    // hook.
                    transaction_for_dmabuf = Some(transaction);
                }
            }

            animate = mapped.should_animate_commit(serial);
        } else if !got_unmapped {
            error!("commit on a mapped surface without a configured serial");
        };

        if let (Some(dmabuf), Some(client)) = (dmabuf, surface.client()) {
            // Prefer the explicit-sync acquire point (linux-drm-syncobj-v1); fall back to the
            // buffer's implicit fence. Either blocker also releases the held layout transaction
            // once the buffer is producer-complete.
            if let Some((blocker, source)) = acquire_point.and_then(|p| p.generate_blocker().ok()) {
                let res = state
                    .synoik
                    .event_loop
                    .insert_source(source, move |_, _, state| {
                        // This surface is now ready for the transaction.
                        drop(transaction_for_dmabuf.take());

                        let display_handle = state.synoik.display_handle.clone();
                        state
                            .client_compositor_state(&client)
                            .blocker_cleared(state, &display_handle);

                        Ok(())
                    });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                    trace!("added explicit-sync acquire blocker");
                }
            } else if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                let res = state
                    .synoik
                    .event_loop
                    .insert_source(source, move |_, _, state| {
                        // This surface is now ready for the transaction.
                        drop(transaction_for_dmabuf.take());

                        let display_handle = state.synoik.display_handle.clone();
                        state
                            .client_compositor_state(&client)
                            .blocker_cleared(state, &display_handle);

                        Ok(())
                    });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                    trace!("added dmabuf blocker");
                }
            }
        }

        let window = mapped.window.clone();
        if got_unmapped {
            state.store_unmap_snapshot(&window);
        } else {
            if animate {
                // The snapshot struct is still stored: the capture below needs it to exist.
                mapped.store_animation_snapshot_neutral();

                let scale = Scale::from(
                    output
                        .map(|o| o.current_scale().fractional_scale())
                        .unwrap_or(1.),
                );
                state
                    .backend
                    .with_vulkan_renderer(|vk| mapped.capture_neutral_vulkan(vk, scale));
                // A failed capture leaves `neutral` unset, which costs this resize its crossfade —
                // the plain window renders instead. That is the same fail-closed trade the
                // blocked-out target already makes in `Tile::render`.
            }

            // The toplevel remains mapped; clear any stored unmap snapshot.
            state.synoik.layout.clear_unmap_snapshot(&window);
        }
    })
}
