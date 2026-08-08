// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::collections::hash_map::Entry;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::calloop::Interest;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Point, Rectangle, Size};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, remove_pre_commit_hook,
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;
use smithay::wayland::shell::xdg::ToplevelCachedState;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_shm};
use synoik_config::WindowingMode;
use synoik_ipc::PositionChange;

use super::xdg_shell::add_mapped_toplevel_pre_commit_hook;
use crate::gnome::FocusNewWindows;
use crate::handlers::XDG_ACTIVATION_TOKEN_TIMEOUT;
use crate::layout::{ActivateWindow, AddWindowTarget, LayoutElement as _};
use crate::synoik::{CastTarget, ClientState, LockState, State};
use crate::utils::transaction::Transaction;
use crate::utils::{get_monotonic_time, is_mapped, send_scale_transform, with_toplevel_role};
use crate::window::{InitialConfigureState, Mapped, ResolvedWindowRules, Unmapped};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.synoik.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let mut root = parent.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(output) = self.synoik.output_for_root(&root) {
            let scale = output.current_scale();
            let transform = output.current_transform();
            with_states(surface, |data| {
                send_scale_transform(surface, data, scale, transform);
            });
        }
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        self.add_default_dmabuf_pre_commit_hook(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        let _span = tracy_client::span!("CompositorHandler::commit");
        let _span = trace_span!("commit", surface = %surface.id()).entered();
        trace!("commit");

        on_commit_buffer_handler::<Self>(surface);

        let mut root_surface = surface.clone();
        while let Some(parent) = get_parent(&root_surface) {
            root_surface = parent;
        }

        // Update the cached root surface.
        self.synoik
            .root_surface
            .insert(surface.clone(), root_surface.clone());

        if is_sync_subsurface(surface) {
            return;
        }

        if surface == &root_surface {
            // This is a root surface commit. It might have mapped a previously-unmapped toplevel.
            if let Entry::Occupied(entry) = self.synoik.unmapped_windows.entry(surface.clone()) {
                if is_mapped(surface) {
                    // The toplevel got mapped.
                    let Unmapped {
                        window,
                        state,
                        activation_token_data,
                        activation_token,
                        had_initial_commit: _,
                        wants_session_restore: _,
                    } = entry.remove();

                    window.on_commit();

                    let toplevel = window.toplevel().expect("no X11 support");
                    let app_id = with_toplevel_role(toplevel, |role| role.app_id.clone());

                    let (
                        rules,
                        width,
                        height,
                        is_full_width,
                        output,
                        workspace_id,
                        is_pending_maximized,
                        was_restored,
                        unmaximize_to,
                    ) = if let InitialConfigureState::Configured {
                        rules,
                        width,
                        height,
                        floating_width: _,
                        floating_height: _,
                        is_full_width,
                        output,
                        workspace_name,
                        is_pending_maximized,
                        restore,
                    } = state
                    {
                        // Check that the output is still connected.
                        let output =
                            output.filter(|o| self.synoik.layout.monitor_for_output(o).is_some());

                        // Check that the workspace still exists.
                        let workspace_id = workspace_name
                            .as_deref()
                            .and_then(|n| self.synoik.layout.find_workspace_by_name(n))
                            .map(|(_, ws)| ws.id());

                        // A restored window goes back to its saved workspace *index*, resolved
                        // now rather than at configure time: the monitor's workspaces can have
                        // changed in between. Clamped to the last one, since with dynamic
                        // workspaces the index may no longer exist and clamping beats creating
                        // workspaces up to it.
                        let workspace_id = workspace_id.or_else(|| {
                            let idx = restore.as_ref()?.workspace_idx?;
                            let mon = self.synoik.layout.monitor_for_output(output.as_ref()?)?;
                            let workspaces = mon.workspaces_ref();
                            let last = workspaces.len().checked_sub(1)?;
                            Some(workspaces[idx.min(last)].id())
                        });

                        let was_restored = restore.is_some();
                        let unmaximize_to = restore.and_then(|restore| restore.unmaximize_to);

                        (
                            rules,
                            width,
                            height,
                            is_full_width,
                            output,
                            workspace_id,
                            is_pending_maximized,
                            was_restored,
                            unmaximize_to,
                        )
                    } else {
                        // Can happen when a surface unmaps by attaching a null buffer while
                        // there are in-flight pending configures.
                        debug!("window mapped without proper initial configure");
                        (
                            ResolvedWindowRules::default(),
                            None,
                            None,
                            false,
                            None,
                            None,
                            false,
                            false,
                            None,
                        )
                    };

                    let windowing_mode = self.synoik.config.borrow().layout.windowing_mode;

                    // The GTK about dialog sets min/max size after the initial configure but
                    // before mapping, so we need to compute open_floating at the last possible
                    // moment, that is here.
                    let is_floating = rules.compute_open_floating(toplevel, windowing_mode);

                    // Figure out if we should activate the window.
                    let activate = rules.open_focused.map(|focus| {
                        if focus {
                            ActivateWindow::Yes
                        } else {
                            ActivateWindow::No
                        }
                    });

                    // Whether GNOME focus-stealing prevention denied the focus; the
                    // window is marked urgent after mapping.
                    let mut denied_focus_steal = false;

                    let activate = match activate {
                        Some(activate) => activate,
                        None if windowing_mode == WindowingMode::Floating => {
                            // GNOME focus-stealing prevention (mutter's
                            // window_state_on_map + intervening_user_event_occurred).
                            //
                            // Transients of the focused window always take focus. In
                            // strict mode nothing else does. In smart mode (the GNOME
                            // default), the window takes focus unless its launch — the
                            // activation token's mint time, which mutter carries as the
                            // startup-sequence timestamp — predates the last user
                            // interaction with the focused window.
                            let transient_of_focus = {
                                let focus_surface = self
                                    .synoik
                                    .layout
                                    .focus()
                                    .map(|mapped| mapped.toplevel().wl_surface().clone());
                                let mut found = false;
                                if let Some(focus_surface) = focus_surface {
                                    let mut parent = toplevel.parent();
                                    while let Some(p) = parent {
                                        if p == focus_surface {
                                            found = true;
                                            break;
                                        }
                                        parent = self
                                            .synoik
                                            .layout
                                            .find_window_and_output(&p)
                                            .and_then(|(mapped, _)| mapped.toplevel().parent());
                                    }
                                }
                                found
                            };

                            let focus_user_time = self
                                .synoik
                                .layout
                                .focus()
                                .and_then(|mapped| mapped.user_time());

                            if transient_of_focus {
                                ActivateWindow::Yes
                            } else if self.synoik.gnome_settings.focus_new_windows
                                == FocusNewWindows::Strict
                            {
                                denied_focus_steal = self.synoik.layout.focus().is_some();
                                ActivateWindow::No
                            } else {
                                let launch_time = activation_token_data.as_ref().map(|token| {
                                    get_monotonic_time().saturating_sub(token.timestamp.elapsed())
                                });
                                match (launch_time, focus_user_time) {
                                    // The launch predates the last interaction with the
                                    // focused window: taking focus would be a steal.
                                    (Some(launch), Some(user_time)) if launch < user_time => {
                                        denied_focus_steal = true;
                                        ActivateWindow::No
                                    }
                                    // No launch time, no interaction with the focused
                                    // window, or a fresh launch: focus is fine.
                                    _ => ActivateWindow::Yes,
                                }
                            }
                        }
                        None => {
                            // niri's policy for the scrolling mode.
                            //
                            // Check the token timestamp again in case the window took a
                            // while between requesting activation and mapping.
                            let token = activation_token_data.filter(|token| {
                                token.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TIMEOUT
                            });
                            if token.is_some() {
                                ActivateWindow::Yes
                            } else {
                                let config = self.synoik.config.borrow();
                                if config.debug.strict_new_window_focus_policy {
                                    ActivateWindow::No
                                } else {
                                    ActivateWindow::Smart
                                }
                            }
                        }
                    };

                    let parent = toplevel
                        .parent()
                        .and_then(|parent| self.synoik.layout.find_window_and_output(&parent))
                        // Only consider the parent if we configured the window for the same
                        // output.
                        //
                        // Normally when we're following the parent, the configured output will be
                        // None. If the configured output is set, that means it was set explicitly
                        // by a window rule or a fullscreen request.
                        .filter(|(_, parent_output)| {
                            parent_output.is_none()
                                || output.is_none()
                                || output.as_ref() == *parent_output
                        })
                        .map(|(mapped, _)| mapped.window.clone());

                    // The mapped pre-commit hook deals with dma-bufs on its own.
                    self.remove_default_dmabuf_pre_commit_hook(surface);
                    let hook = add_mapped_toplevel_pre_commit_hook(toplevel);
                    let mapped = {
                        let config = self.synoik.config.borrow();
                        Mapped::new(window, rules, hook, &config)
                    };
                    let window = mapped.window.clone();

                    // A window launched onto a workspace (an app icon dropped on it
                    // in the overview) opens there, unless a window rule already
                    // pinned one. This is `meta_display_apply_startup_properties`
                    // (`mutter/src/core/display.c:2661-2731`): the window completes
                    // the startup sequence it belongs to — matched by its activation
                    // token, else by app id — and inherits that sequence's workspace.
                    let workspace_id = workspace_id.or_else(|| {
                        let target = self.synoik.app_system.complete_startup(
                            app_id.as_deref(),
                            activation_token.as_deref(),
                            get_monotonic_time(),
                        )?;
                        // The workspace may be gone by the time the app got around
                        // to mapping.
                        self.synoik
                            .layout
                            .workspaces()
                            .any(|(_, _, ws)| ws.id() == target)
                            .then_some(target)
                    });

                    let target = if let Some(p) = &parent {
                        // Open dialogs next to their parent window.
                        AddWindowTarget::NextTo(p)
                    } else if let Some(id) = workspace_id {
                        AddWindowTarget::Workspace(id)
                    } else if let Some(output) = &output {
                        AddWindowTarget::Output(output)
                    } else {
                        AddWindowTarget::Auto
                    };
                    let output = self.synoik.layout.add_window(
                        mapped,
                        target,
                        width,
                        height,
                        is_full_width,
                        is_floating,
                        activate,
                    );
                    let output = output.cloned();

                    // The window state cannot contain Fullscreen and Maximized at once. Therefore,
                    // if the window ended up fullscreen, then we only know that it is also
                    // maximized from the is_pending_maximized variable. Tell the layout about it
                    // here so that unfullscreening the window makes it maximized.
                    // A window a tray icon opened belongs under that icon; nothing else can put
                    // it there. Before the fullscreen fix-up, so a window that is going fullscreen
                    // is not first moved and then ignored.
                    self.place_indicator_window(surface, activation_token.as_deref());

                    if let Some((mapped, _)) = self.synoik.layout.find_window_and_output(surface) {
                        if mapped.pending_sizing_mode().is_fullscreen() && is_pending_maximized {
                            self.synoik.layout.set_maximized(&window, true);
                        }
                    } else {
                        error!("layout is missing the window that we just added");
                    }

                    if denied_focus_steal {
                        // The window wanted focus but taking it would have been a
                        // steal: mark it demands-attention (urgent) so the shell can
                        // surface it, like mutter's meta_window_show. (mutter only
                        // marks when the denied window overlaps the focus window; we
                        // skip that test.)
                        if let Some((mapped, _)) =
                            self.synoik.layout.find_window_and_output_mut(surface)
                        {
                            mapped.set_urgent(true);
                        }
                    }

                    // A window that was denied focus must not cover the window
                    // that kept it (mutter's place.c step H, which runs just
                    // before the auto-maximize below).
                    if denied_focus_steal
                        && windowing_mode == WindowingMode::Floating
                        && is_floating
                        && parent.is_none()
                    {
                        self.synoik.layout.avoid_focus_window(&window);
                    }

                    // GNOME auto-maximize (mutter place.c): a first-shown window
                    // covering more than 80% of the work area opens maximized.
                    // Transients are left alone.
                    //
                    // A session-restored window is not first-shown in the sense that matters: its
                    // size is remembered rather than guessed, and auto-maximize would both
                    // override the remembered state and overwrite the rect to return to with its
                    // own shrunken guess.
                    if windowing_mode == WindowingMode::Floating
                        && is_floating
                        && parent.is_none()
                        && !was_restored
                    {
                        self.synoik.layout.auto_maximize_if_too_big(&window);
                    }

                    // A restored window that maps straight into maximized or fullscreen has no
                    // floating incarnation this run, so tell the layout the rect it came from —
                    // otherwise un-maximizing lands on a default size rather than the saved one.
                    // After auto-maximize, which is the other writer of this field.
                    if let Some([x, y, w, h]) = unmaximize_to {
                        let origin = output
                            .as_ref()
                            .and_then(|output| self.synoik.global_space.output_geometry(output))
                            .map_or_else(Point::default, |geo| geo.loc.to_f64());
                        let rect = Rectangle::new(
                            Point::from((f64::from(x), f64::from(y))),
                            Size::from((f64::from(w), f64::from(h))),
                        );
                        self.synoik
                            .layout
                            .seed_unmaximize_geometry(&window, rect, origin);
                    }

                    if let Some(output) = output {
                        self.synoik.layout.start_open_animation_for_window(&window);

                        let new_focus = self.synoik.layout.focus().map(|m| &m.window);
                        if new_focus == Some(&window) {
                            // We activated the newly opened window.
                            self.maybe_warp_cursor_to_focus();
                            self.synoik.layer_shell_on_demand_focus = None;
                        }

                        self.synoik.queue_redraw(&output);
                    }
                    return;
                }

                // The toplevel remains unmapped.
                trace!("toplevel remains unmapped");
                let unmapped = entry.into_mut();
                unmapped.had_initial_commit = true;
                if unmapped.needs_initial_configure() {
                    let toplevel = unmapped.window.toplevel().expect("no x11 support").clone();
                    self.queue_initial_configure(toplevel);
                }
                return;
            }

            // This is a commit of a previously-mapped root or a non-toplevel root.
            if let Some((mapped, output)) = self.synoik.layout.find_window_and_output(surface) {
                let window = mapped.window.clone();
                let output = output.cloned();

                let id = mapped.id();

                // This is a commit of a previously-mapped toplevel.
                let is_mapped = is_mapped(surface);

                // Must start the close animation before window.on_commit().
                let transaction = Transaction::new();
                if !is_mapped {
                    let blocker = transaction.blocker();
                    // The snapshot is renderer-neutral, so starting the animation needs no
                    // renderer — and must not depend on one being available.
                    self.synoik
                        .layout
                        .start_close_animation_for_window(&window, blocker);

                    // Before `on_commit`, for the same reason the close snapshot is: the commit
                    // being processed is the one that unmaps, so afterwards the window's size is
                    // already zero and there is nothing left worth remembering.
                    self.save_session_toplevel(&window);
                }

                window.on_commit();

                if !is_mapped {
                    // The toplevel got unmapped.
                    //
                    // Test client: wleird-unmap.
                    trace!("toplevel got unmapped");

                    let active_window = self.synoik.layout.focus().map(|m| &m.window);
                    let was_active = active_window == Some(&window);

                    self.synoik
                        .stop_casts_for_target(CastTarget::Window { id: id.get() });

                    // A window vanishing under an open switcher removes its item -- or, for an
                    // app switcher, only that app's chevron unless it was the app's last window
                    // (`_itemRemoved`, `switcherPopup.js:269-284`).
                    let outcome = self.synoik.switcher.window_removed(id);
                    self.finish_switcher(outcome);
                    self.synoik
                        .layout
                        .remove_window(&window, transaction.clone());
                    self.add_default_dmabuf_pre_commit_hook(surface);

                    // If this is the only instance, then this transaction will complete
                    // immediately, so no need to set the timer.
                    if !transaction.is_last() {
                        transaction.register_deadline_timer(&self.synoik.event_loop);
                    }

                    if was_active {
                        self.maybe_warp_cursor_to_focus();
                    }

                    // Newly-unmapped toplevels must perform the initial commit-configure sequence
                    // afresh. The *toplevel object* is not new though, so its initial commit is
                    // long past: `already_mapped` is pinned to the first commit after the toplevel
                    // was created, not after it was last unmapped.
                    let mut unmapped = Unmapped::new(window);
                    unmapped.had_initial_commit = true;
                    self.synoik
                        .unmapped_windows
                        .insert(surface.clone(), unmapped);

                    if let Some(output) = output {
                        self.synoik.queue_redraw(&output);
                        self.synoik.queue_redraw_switcher_output();
                    }
                    return;
                }

                let (serial, buffer_delta) = with_states(surface, |states| {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take();

                    let serial = states
                        .cached_state
                        .get::<ToplevelCachedState>()
                        .current()
                        .last_acked
                        .as_ref()
                        .map(|c| c.serial);
                    (serial, buffer_delta)
                });
                if serial.is_none() {
                    error!("commit on a mapped surface without a configured serial");
                }

                // The toplevel remains mapped.
                self.synoik.layout.update_window(&window, serial);

                // Move the toplevel according to the attach offset.
                if let Some(delta) = buffer_delta {
                    if delta.x != 0 || delta.y != 0 {
                        let (x, y) = delta.to_f64().into();
                        self.synoik.layout.move_floating_window(
                            Some(&window),
                            PositionChange::AdjustFixed(x),
                            PositionChange::AdjustFixed(y),
                            false,
                        );
                    }
                }

                // Popup placement depends on window size which might have changed.
                self.update_reactive_popups(&window);

                if let Some(output) = output {
                    self.synoik.queue_redraw(&output);
                    self.synoik.queue_redraw_switcher_output();
                }
                return;
            }

            // This is a commit of a non-toplevel root.
        }

        // This is a commit of a non-root or a non-toplevel root.
        let root_window_output = self.synoik.layout.find_window_and_output(&root_surface);
        if let Some((mapped, output)) = root_window_output {
            let window = mapped.window.clone();
            let output = output.cloned();
            window.on_commit();
            self.synoik.layout.update_window(&window, None);
            if let Some(output) = output {
                self.synoik.queue_redraw(&output);
                self.synoik.queue_redraw_switcher_output();
            }
            return;
        }

        // This might be a popup.
        self.popups_handle_commit(surface);
        if let Some(popup) = self.synoik.popups.find_popup(surface) {
            if let Some(output) = self.output_for_popup(&popup) {
                self.synoik.queue_redraw(&output.clone());
            }
            return;
        }

        // This might be a layer-shell surface.
        if self.layer_shell_handle_commit(surface) {
            return;
        }

        // This might be a cursor surface.
        if matches!(
            &self.synoik.cursor_manager.cursor_image(),
            CursorImageStatus::Surface(s) if s == &root_surface
        ) {
            // In case the cursor surface has been committed handle the role specific
            // buffer offset by applying the offset on the cursor image hotspot
            if surface == &root_surface {
                with_states(surface, |states| {
                    let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                    if let Some(mut cursor_image_attributes) =
                        cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                    {
                        let buffer_delta = states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take();
                        if let Some(buffer_delta) = buffer_delta {
                            cursor_image_attributes.hotspot -= buffer_delta;
                        }
                    }
                });
            }

            // FIXME: granular redraws for cursors.
            self.synoik.queue_redraw_all();
            return;
        }

        // This might be a DnD icon surface.
        if matches!(&self.synoik.dnd_icon, Some(icon) if icon.surface == root_surface) {
            let dnd_icon = self.synoik.dnd_icon.as_mut().unwrap();

            // In case the dnd surface has been committed handle the role specific
            // buffer offset by applying the offset on the dnd icon offset
            if surface == &dnd_icon.surface {
                with_states(&dnd_icon.surface, |states| {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take()
                        .unwrap_or_default();
                    dnd_icon.offset += buffer_delta;
                });
            }

            // FIXME: granular redraws for cursors.
            self.synoik.queue_redraw_all();
            return;
        }

        // This might be a lock surface.
        for (output, state) in &self.synoik.output_state {
            if let Some(lock_surface) = &state.lock_surface {
                if lock_surface.wl_surface() == &root_surface {
                    if matches!(self.synoik.lock_state, LockState::WaitingForSurfaces { .. }) {
                        self.synoik.maybe_continue_to_locking();
                    } else {
                        self.synoik.queue_redraw(&output.clone());
                    }

                    return;
                }
            }
        }

        // This message can trigger for lock surfaces that had a commit right after we unlocked
        // the session, but that's ok, we don't need to handle them.
        trace!("commit on an unrecognized surface: {surface:?}, root: {root_surface:?}");
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // Clients may destroy their subsurfaces before the main surface. Ensure we have a snapshot
        // when that happens, so that the closing animation includes all these subsurfaces.
        //
        // Test client: alacritty with CSD <= 0.13 (it was fixed in winit afterwards:
        // https://github.com/rust-windowing/winit/pull/3625).
        //
        // This is still not perfect, as this function is called already after the (first)
        // subsurface is destroyed; in the case of alacritty, this is the top CSD shadow. But, it
        // gets most of the job done.
        if let Some(root) = self.synoik.root_surface.get(surface) {
            if let Some((mapped, output)) = self.synoik.layout.find_window_and_output(root) {
                let window = mapped.window.clone();
                let output = output.cloned();
                self.store_unmap_snapshot(&window, output.as_ref());
            }
        }

        self.synoik
            .root_surface
            .retain(|k, v| k != surface && v != surface);

        // The object destruction order is not guaranteed to follow the logical role order. So for
        // example when a client disconnects unexpectedly, WlSurface::destroyed() may be called
        // before XdgShellHandler::toplevel_destroyed(). In this case, the surface will *not* have
        // the default dmabuf pre-commit hook: it will still have the toplevel pre-commit hook.
        //
        // So, this may come out empty, and then the toplevel pre-commit hook will be removed in the
        // subsequent toplevel_destroyed() call.
        if let Some(hook) = self.synoik.dmabuf_pre_commit_hook.remove(surface) {
            remove_pre_commit_hook(surface, &hook);
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.synoik.shm_state
    }
}

delegate_compositor!(State);
delegate_shm!(State);

impl State {
    pub fn add_default_dmabuf_pre_commit_hook(&mut self, surface: &WlSurface) {
        if !surface.is_alive() {
            error!("tried to add dmabuf pre-commit hook for a dead surface");
            return;
        }

        let hook = add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                // Explicit-sync acquire timeline point, if the client set one this commit.
                acquire_point.clone_from(
                    &surface_data
                        .cached_state
                        .get::<DrmSyncobjCachedState>()
                        .pending()
                        .acquire_point,
                );
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            let Some(dmabuf) = maybe_dmabuf else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };

            // Prefer the explicit-sync acquire point (linux-drm-syncobj-v1): hold the commit
            // until the client's acquire timeline point signals. Falls back to the buffer's
            // implicit fence when the client doesn't use explicit sync (or the blocker can't be
            // built). Either way the buffer is producer-complete before it is ever sampled.
            if let Some((blocker, source)) = acquire_point.and_then(|p| p.generate_blocker().ok()) {
                let res = state.synoik.event_loop.insert_source(source, {
                    let client = client.clone();
                    move |_, _, state| {
                        let display_handle = state.synoik.display_handle.clone();
                        state
                            .client_compositor_state(&client)
                            .blocker_cleared(state, &display_handle);
                        Ok(())
                    }
                });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                    trace!("added explicit-sync acquire blocker");
                    return;
                }
            }

            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                let res = state
                    .synoik
                    .event_loop
                    .insert_source(source, move |_, _, state| {
                        let display_handle = state.synoik.display_handle.clone();
                        state
                            .client_compositor_state(&client)
                            .blocker_cleared(state, &display_handle);
                        Ok(())
                    });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                    trace!("added default dmabuf blocker");
                }
            }
        });

        let s = surface.clone();
        if let Some(prev) = self.synoik.dmabuf_pre_commit_hook.insert(s, hook) {
            error!("tried to add dmabuf pre-commit hook when there was already one");
            remove_pre_commit_hook(surface, &prev);
        }
    }

    pub fn remove_default_dmabuf_pre_commit_hook(&mut self, surface: &WlSurface) {
        if let Some(hook) = self.synoik.dmabuf_pre_commit_hook.remove(surface) {
            remove_pre_commit_hook(surface, &hook);
        } else {
            error!("tried to remove dmabuf pre-commit hook but there was none");
        }
    }
}
