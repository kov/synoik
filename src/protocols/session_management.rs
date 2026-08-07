// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! `xdg_session_management_v1` — toplevel session restore.
//!
//! See `docs/fork/session-management-port.md`. This is the protocol skeleton: sessions live only
//! for as long as a client holds them, nothing is written to disk, and `restore_toplevel` always
//! degrades to `add_toplevel`. Persistence and actual restore arrive in later slices.
//!
//! The behavioural reference is mutter 50.3's `src/wayland/meta-wayland-xdg-session*.c`, which
//! implements the same design under the pre-merge `xx_session_manager_v1` names.
//!
//! **Inertness is not a flag.** A session object is inert exactly when it is no longer the object
//! registered for its id — which covers all three ways it happens (another client took the id over,
//! the session was destroyed, the session was removed) without a separate bit that can go stale.
//! Toplevel session objects follow the same rule one level down: inert when their session is, or
//! when their name no longer resolves back to them.

use std::collections::HashMap;
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_backend::server::ClientId;

use super::raw::xdg_session_management::v1::server::xdg_session_manager_v1::{
    self, XdgSessionManagerV1,
};
use super::raw::xdg_session_management::v1::server::xdg_session_v1::{self, XdgSessionV1};
use super::raw::xdg_session_management::v1::server::xdg_toplevel_session_v1::{
    self, XdgToplevelSessionV1,
};

const VERSION: u32 = 1;

pub trait SessionManagerHandler {
    fn session_manager_state(&mut self) -> &mut SessionManagerState;

    /// Whether the client has already performed the initial commit on this toplevel's surface.
    ///
    /// `restore_toplevel` is only meaningful before that commit, since restoring means changing
    /// the very first configure the toplevel receives.
    fn toplevel_had_initial_commit(&mut self, toplevel: &XdgToplevel) -> bool;
}

/// A session currently held by a client.
#[derive(Debug)]
struct LiveSession {
    /// The object managing this session. Any other [`XdgSessionV1`] carrying the same id is inert.
    resource: XdgSessionV1,

    /// Toplevel names registered in this session.
    ///
    /// A name stays taken for the lifetime of the session even after its toplevel goes away —
    /// that is the whole point of the protocol, and it is what `name_in_use` protects.
    toplevels: HashMap<String, RegisteredToplevel>,
}

#[derive(Debug)]
struct RegisteredToplevel {
    /// The toplevel registered under this name. May be dead; the name survives it.
    toplevel: XdgToplevel,

    /// The handle handed to the client. Also may be dead — `destroy` on it is a no-op by spec.
    handle: XdgToplevelSessionV1,
}

#[derive(Debug, Default)]
pub struct SessionManagerState {
    live: HashMap<String, LiveSession>,
}

/// User data on an [`XdgSessionV1`].
#[derive(Debug)]
pub struct SessionData {
    id: String,
}

/// User data on an [`XdgToplevelSessionV1`].
#[derive(Debug)]
pub struct ToplevelSessionData {
    session_id: String,
    /// The name this handle currently answers to. `rename` moves it.
    name: Mutex<String>,
}

pub struct SessionManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl SessionManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<XdgSessionManagerV1, SessionManagerGlobalData>,
        D: Dispatch<XdgSessionManagerV1, ()>,
        D: Dispatch<XdgSessionV1, SessionData>,
        D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
        D: SessionManagerHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = SessionManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, XdgSessionManagerV1, _>(VERSION, global_data);

        Self::default()
    }

    /// The live session this object manages, or `None` if the object is inert.
    fn session_of(&mut self, resource: &XdgSessionV1) -> Option<&mut LiveSession> {
        let id = &resource.data::<SessionData>()?.id;
        let live = self.live.get_mut(id)?;
        (&live.resource == resource).then_some(live)
    }

    /// Whether this toplevel is already registered in any session held by `client`.
    ///
    /// The spec scopes `already_added` to the client, not to the session, so this walks every
    /// session the client holds rather than just the one being added to.
    fn client_already_added(&self, client: &Client, toplevel: &XdgToplevel) -> bool {
        self.live.values().any(|live| {
            live.resource.client().as_ref() == Some(client)
                && live.toplevels.values().any(|reg| &reg.toplevel == toplevel)
        })
    }

    /// Hands `id` to `resource`, replacing whoever held it.
    ///
    /// The previous holder is told it was `replaced` and everything it owned becomes inert. Its
    /// live registrations do not carry over: what a takeover inherits is the *saved* state, which
    /// this slice does not have yet.
    fn take_over(&mut self, id: String, resource: XdgSessionV1) {
        if let Some(previous) = self.live.remove(&id) {
            if previous.resource.is_alive() {
                previous.resource.replaced();
            }
        }

        self.live.insert(
            id,
            LiveSession {
                resource,
                toplevels: HashMap::new(),
            },
        );
    }
}

impl<D> GlobalDispatch<XdgSessionManagerV1, SessionManagerGlobalData, D> for SessionManagerState
where
    D: GlobalDispatch<XdgSessionManagerV1, SessionManagerGlobalData>,
    D: Dispatch<XdgSessionManagerV1, ()>,
    D: Dispatch<XdgSessionV1, SessionData>,
    D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
    D: SessionManagerHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<XdgSessionManagerV1>,
        _global_data: &SessionManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &SessionManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<XdgSessionManagerV1, (), D> for SessionManagerState
where
    D: Dispatch<XdgSessionManagerV1, ()>,
    D: Dispatch<XdgSessionV1, SessionData>,
    D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
    D: SessionManagerHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        manager: &XdgSessionManagerV1,
        request: <XdgSessionManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_session_manager_v1::Request::Destroy => (),
            xdg_session_manager_v1::Request::GetSession {
                id,
                reason,
                session_id,
            } => {
                if reason.into_result().is_err() {
                    // Still initialise the object: the client is about to be disconnected, but
                    // leaving a `New` uninitialised is a panic in wayland-rs.
                    data_init.init(
                        id,
                        SessionData {
                            id: String::from("<invalid>"),
                        },
                    );
                    manager.post_error(
                        xdg_session_manager_v1::Error::InvalidReason,
                        "unknown session reason",
                    );
                    return;
                }

                // An id we do not know about is treated as if NULL had been passed, so both cases
                // fall through to "make a fresh session".
                let known = session_id
                    .filter(|sid| state.session_manager_state().live.contains_key(sid))
                    .map(|sid| {
                        let holder = state.session_manager_state().live[&sid]
                            .resource
                            .client()
                            .as_ref()
                            == Some(client);
                        (sid, holder)
                    });

                match known {
                    Some((sid, true)) => {
                        data_init.init(id, SessionData { id: sid });
                        manager.post_error(
                            xdg_session_manager_v1::Error::InUse,
                            "session is already in use by this client",
                        );
                    }
                    Some((sid, false)) => {
                        let session = data_init.init(id, SessionData { id: sid.clone() });
                        state
                            .session_manager_state()
                            .take_over(sid, session.clone());
                        session.restored();
                    }
                    None => {
                        let sid = uuid::Uuid::new_v4().to_string();
                        let session = data_init.init(id, SessionData { id: sid.clone() });
                        state
                            .session_manager_state()
                            .take_over(sid.clone(), session.clone());
                        session.created(sid);
                    }
                }
            }
        }
    }
}

impl<D> Dispatch<XdgSessionV1, SessionData, D> for SessionManagerState
where
    D: Dispatch<XdgSessionV1, SessionData>,
    D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
    D: SessionManagerHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        session: &XdgSessionV1,
        request: <XdgSessionV1 as Resource>::Request,
        data: &SessionData,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            // Both are destructors, so `destroyed` below drops the live session either way —
            // doing it here as well would be dead code. They differ only in what happens to the
            // *stored* state, which slice 2 adds: `remove` deletes it, `destroy` keeps it. That
            // is why the two arms stay separate even though they read alike today.
            xdg_session_v1::Request::Destroy => (),
            xdg_session_v1::Request::Remove => (),
            xdg_session_v1::Request::AddToplevel { id, toplevel, name } => {
                add_toplevel(state, client, session, data, id, toplevel, name, data_init);
            }
            xdg_session_v1::Request::RestoreToplevel { id, toplevel, name } => {
                // Restoring is only meaningful before the toplevel's first commit, since it
                // changes the very first configure the client sees.
                if state.toplevel_had_initial_commit(&toplevel) {
                    data_init.init(id, ToplevelSessionData::inert(name));
                    session.post_error(
                        xdg_session_v1::Error::AlreadyMapped,
                        "restore_toplevel after the toplevel's initial commit",
                    );
                    return;
                }

                // With no persistent store yet there is never anything to restore, so this
                // degrades to `add_toplevel` and sends no `restored` event — which is exactly
                // what the spec prescribes for an unknown name. Slice 4 makes it real.
                add_toplevel(state, client, session, data, id, toplevel, name, data_init);
            }
            xdg_session_v1::Request::RemoveToplevel { name } => {
                if let Some(live) = state.session_manager_state().session_of(session) {
                    live.toplevels.remove(&name);
                }
            }
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, session: &XdgSessionV1, data: &SessionData) {
        // Drops the session when the client goes away, too. Only if this object still owns the
        // id: a takeover already moved it to somebody else.
        let manager = state.session_manager_state();
        if manager.live.get(&data.id).map(|live| &live.resource) == Some(session) {
            manager.live.remove(&data.id);
        }
    }
}

impl ToplevelSessionData {
    fn new(session_id: String, name: String) -> Self {
        Self {
            session_id,
            name: Mutex::new(name),
        }
    }

    /// Data for a handle that is inert from birth: no session resolves the empty id.
    fn inert(name: String) -> Self {
        Self::new(String::new(), name)
    }
}

#[allow(clippy::too_many_arguments)]
fn add_toplevel<D>(
    state: &mut D,
    client: &Client,
    session: &XdgSessionV1,
    data: &SessionData,
    id: New<XdgToplevelSessionV1>,
    toplevel: XdgToplevel,
    name: String,
    data_init: &mut DataInit<'_, D>,
) where
    D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
    D: SessionManagerHandler,
    D: 'static,
{
    let manager = state.session_manager_state();

    // An inert session accepts the request but does nothing with it.
    if manager.session_of(session).is_none() {
        data_init.init(id, ToplevelSessionData::inert(name));
        return;
    }

    let already_added = manager.client_already_added(client, &toplevel);
    let name_taken = manager
        .session_of(session)
        .is_some_and(|live| live.toplevels.contains_key(&name));

    if already_added || name_taken {
        let (code, msg) = if already_added {
            (
                xdg_session_v1::Error::AlreadyAdded,
                "toplevel is already in a session held by this client",
            )
        } else {
            (
                xdg_session_v1::Error::NameInUse,
                "a toplevel with this name is already in the session",
            )
        };
        data_init.init(id, ToplevelSessionData::inert(name));
        session.post_error(code, msg);
        return;
    }

    let handle = data_init.init(id, ToplevelSessionData::new(data.id.clone(), name.clone()));
    let live = state
        .session_manager_state()
        .session_of(session)
        .expect("checked above");
    live.toplevels.insert(
        name,
        RegisteredToplevel {
            toplevel,
            handle: handle.clone(),
        },
    );
}

impl<D> Dispatch<XdgToplevelSessionV1, ToplevelSessionData, D> for SessionManagerState
where
    D: Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
    D: SessionManagerHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        handle: &XdgToplevelSessionV1,
        request: <XdgToplevelSessionV1 as Resource>::Request,
        data: &ToplevelSessionData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            // By spec this has no effect on window management, so the registration — and with it
            // the name and its state — outlives the handle.
            xdg_toplevel_session_v1::Request::Destroy => (),
            xdg_toplevel_session_v1::Request::Rename { name: new_name } => {
                let manager = state.session_manager_state();
                let Some(live) = manager.live.get_mut(&data.session_id) else {
                    return;
                };

                let mut current = data.name.lock().unwrap();

                // Inert once the name no longer resolves back to this handle.
                if live.toplevels.get(&*current).map(|reg| &reg.handle) != Some(handle) {
                    return;
                }

                if *current == new_name {
                    return;
                }

                if live.toplevels.contains_key(&new_name) {
                    live.resource.post_error(
                        xdg_session_v1::Error::NameInUse,
                        "a toplevel with this name is already in the session",
                    );
                    return;
                }

                let registration = live.toplevels.remove(&*current).expect("checked above");
                live.toplevels.insert(new_name.clone(), registration);
                *current = new_name;
            }
        }
    }
}

#[macro_export]
macro_rules! delegate_session_management {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::xdg_session_management::v1::server::xdg_session_manager_v1::XdgSessionManagerV1: $crate::protocols::session_management::SessionManagerGlobalData
        ] => $crate::protocols::session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::xdg_session_management::v1::server::xdg_session_manager_v1::XdgSessionManagerV1: ()
        ] => $crate::protocols::session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::xdg_session_management::v1::server::xdg_session_v1::XdgSessionV1: $crate::protocols::session_management::SessionData
        ] => $crate::protocols::session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            $crate::protocols::raw::xdg_session_management::v1::server::xdg_toplevel_session_v1::XdgToplevelSessionV1: $crate::protocols::session_management::ToplevelSessionData
        ] => $crate::protocols::session_management::SessionManagerState);
    };
}
