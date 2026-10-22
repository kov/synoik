// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `gtk_shell1` — mutter's `meta-wayland-gtk-shell.c`, served for one request: `system_bell`,
//! the only way a GTK3 app rings the bell (GTK4 prefers `xdg_system_bell_v1` when it is there,
//! `gdkdisplay-wayland.c:gdk_wayland_display_system_bell`).
//!
//! **Version 1, not mutter's 7.** What a client does with `gtk_shell1` depends on the version it
//! binds, and every later version hands the compositor a duty we do not perform:
//! - v2: GTK3 and GTK4 take window edge constraints from `gtk_surface1.configure_edges` instead of
//!   assuming every untiled edge resizes (`supports_native_edge_constraints`), so a compositor that
//!   never sends it leaves windows unresizable.
//! - v5: GTK delegates titlebar double/middle/right clicks to `titlebar_gesture`.
//!
//! At v1 every `gtk_surface1` request is accepted and ignored: GTK only sends `present` and
//! `request_focus` when `xdg_activation_v1` is missing, which we serve, and the rest
//! (`set_dbus_properties`, `set_modal`) have no consumer here yet.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use super::raw::gtk_shell::v1::server::gtk_shell1::{self, GtkShell1};
use super::raw::gtk_shell::v1::server::gtk_surface1::GtkSurface1;

const VERSION: u32 = 1;

pub struct GtkShellState {}

pub trait GtkShellHandler {
    /// `gtk_shell1.system_bell`: `surface` is the `wl_surface` of the `gtk_surface1` that rang,
    /// or `None` for a bell from no window in particular.
    fn system_bell(&mut self, surface: Option<WlSurface>);
}

impl GtkShellState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<GtkShell1, ()>,
        D: Dispatch<GtkShell1, ()>,
        D: Dispatch<GtkSurface1, WlSurface>,
        D: GtkShellHandler,
        D: 'static,
    {
        display.create_global::<D, GtkShell1, _>(VERSION, ());
        Self {}
    }
}

impl<D> GlobalDispatch<GtkShell1, (), D> for GtkShellState
where
    D: GlobalDispatch<GtkShell1, ()>,
    D: Dispatch<GtkShell1, ()>,
    D: Dispatch<GtkSurface1, WlSurface>,
    D: GtkShellHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GtkShell1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        // `capabilities` stays unsent: we offer none of the global menu / desktop icons ones.
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GtkShell1, (), D> for GtkShellState
where
    D: Dispatch<GtkShell1, ()>,
    D: Dispatch<GtkSurface1, WlSurface>,
    D: GtkShellHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &GtkShell1,
        request: <GtkShell1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            gtk_shell1::Request::GetGtkSurface {
                gtk_surface,
                surface,
            } => {
                data_init.init(gtk_surface, surface);
            }
            gtk_shell1::Request::SystemBell { surface } => {
                let surface = surface.and_then(|s| s.data::<WlSurface>().cloned());
                state.system_bell(surface);
            }
            // Startup notification rides `xdg_activation_v1`; there is no sequence to complete.
            gtk_shell1::Request::SetStartupId { .. } | gtk_shell1::Request::NotifyLaunch { .. } => {
            }
        }
    }
}

impl<D> Dispatch<GtkSurface1, WlSurface, D> for GtkShellState
where
    D: Dispatch<GtkSurface1, WlSurface>,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &GtkSurface1,
        _request: <GtkSurface1 as Resource>::Request,
        _data: &WlSurface,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

#[macro_export]
macro_rules! delegate_gtk_shell {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::gtk_shell::v1::server::gtk_shell1::GtkShell1: ()
        ] => $crate::protocols::gtk_shell::GtkShellState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::gtk_shell::v1::server::gtk_shell1::GtkShell1: ()
        ] => $crate::protocols::gtk_shell::GtkShellState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::gtk_shell::v1::server::gtk_surface1::GtkSurface1: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface
        ] => $crate::protocols::gtk_shell::GtkShellState);
    };
}
