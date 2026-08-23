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
    self, Reason, XdgSessionManagerV1,
};
use super::raw::xdg_session_management::v1::server::xdg_session_v1::{self, XdgSessionV1};
use super::raw::xdg_session_management::v1::server::xdg_toplevel_session_v1::{
    self, XdgToplevelSessionV1,
};
use crate::layout::workspace::WorkspaceId;
use crate::output_identity::OutputIdentity;
use crate::session_state::SessionStore;

const VERSION: u32 = 1;

pub trait SessionManagerHandler {
    fn session_manager_state(&mut self) -> &mut SessionManagerState;

    /// Whether the client has already performed the initial commit on this toplevel's surface.
    ///
    /// `restore_toplevel` is only meaningful before that commit, since restoring means changing
    /// the very first configure the toplevel receives.
    fn toplevel_had_initial_commit(&mut self, toplevel: &XdgToplevel) -> bool;

    /// Marks a not-yet-configured toplevel as wanting session restore.
    ///
    /// A no-op if the toplevel is not in `unmapped_windows` — a client can register a toplevel it
    /// never commits.
    fn note_session_restore_requested(&mut self, toplevel: &XdgToplevel);

    /// Snapshots the state of every still-mapped toplevel registered under `session_id`.
    ///
    /// `xdg_session_v1.destroy` is specified as "preserving the current state", so the state has to
    /// be frozen as of the request. Our only other save trigger is unmap, and a client is entitled
    /// to destroy the session while its windows are still up — which would otherwise leave those
    /// windows holding whatever record their *previous* run wrote.
    fn save_live_session_toplevels(&mut self, session_id: &str);

    /// Arms the debounced write of the session store.
    ///
    /// The protocol code owns *what* changed; the timer lives with the event loop, so the two are
    /// kept apart by this hook rather than by handing the loop into the protocol.
    fn schedule_session_save(&mut self);
}

/// A session currently held by a client.
#[derive(Debug)]
struct LiveSession {
    /// The object managing this session. Any other [`XdgSessionV1`] carrying the same id is inert.
    resource: XdgSessionV1,

    /// Why it was asked for; decides whether a restored window takes focus.
    reason: Reason,

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

    /// Where each [`RestoreSlot`] materialized on the strip it landed on.
    ///
    /// Session-lived, and deliberately not persisted: the *ordering* of the slots is derived from
    /// the store every time it is asked, but which workspace a slot became is a fact about this
    /// run's strips, and a saved one would be an arrangement nobody could check.
    ///
    /// A `Vec`, scanned rather than a map: the key holds a connector, which is compared without
    /// case, and a restore burst has a handful of slots at most.
    restore_slots: Vec<(RestoreSlot, WorkspaceId)>,

    /// Everything we remember across restarts. A session id is "known" if it is in here, whether
    /// or not any client currently holds it.
    pub store: SessionStore,
}

/// One workspace of a display that is not connected, as the restore ordering sees it.
///
/// The saved index is an index into *that display's* strip, never into the one the window is
/// landing on, so it is a slot to be ranked rather than a position to be used. Slots are ordered
/// by connector and then by index, which is what lets every window in a restore burst agree on
/// the arrangement whatever order the clients ask in.
#[derive(Debug, Clone)]
pub struct RestoreSlot {
    /// The display the record named. Carried whole so the workspace this becomes can be tagged as
    /// that display's, and reclaimed when it is plugged back in.
    pub identity: OutputIdentity,
    /// The workspace index the record saved.
    pub workspace: u32,
}

impl RestoreSlot {
    /// Identity and order in one: the connector without case, then the index.
    ///
    /// Only the connector, not the whole [`OutputIdentity`] — `matches` is not an equivalence
    /// relation (a missing EDID field is not a mismatch), so it cannot order or de-duplicate
    /// anything. Grouping absent displays by connector is what the offset it replaces did too.
    fn key(&self) -> (String, u32) {
        (self.identity.connector.to_ascii_lowercase(), self.workspace)
    }
}

impl PartialEq for RestoreSlot {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for RestoreSlot {}

impl PartialOrd for RestoreSlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RestoreSlot {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// User data on an [`XdgSessionV1`].
#[derive(Debug)]
pub struct SessionData {
    id: String,
}

/// Everything [`State::send_initial_configure`] needs to restore a toplevel, resolved fresh at
/// configure time rather than trusted from when `restore_toplevel` was made: in between, the
/// session can have been taken over, the record removed, or the handle destroyed.
#[derive(Debug)]
pub struct RestoreTarget {
    pub session_id: String,
    pub name: String,
    pub reason: Reason,
    /// The handle to send `restored` on, immediately before the first configure.
    pub handle: XdgToplevelSessionV1,
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
    pub fn new<D, F>(display: &DisplayHandle, store: SessionStore, filter: F) -> Self
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

        Self {
            store,
            ..Self::default()
        }
    }

    /// The workspace a slot has already materialized into, if it has.
    pub fn materialized_workspace(&self, slot: &RestoreSlot) -> Option<WorkspaceId> {
        self.restore_slots
            .iter()
            .find(|(known, _)| known == slot)
            .map(|(_, id)| *id)
    }

    /// Every workspace materialized for a restore so far, in no particular order.
    pub fn materialized_workspaces(&self) -> impl Iterator<Item = WorkspaceId> + '_ {
        self.restore_slots.iter().map(|(_, id)| *id)
    }

    /// Records where a slot materialized.
    pub fn set_materialized_workspace(&mut self, slot: RestoreSlot, id: WorkspaceId) {
        self.restore_slots.retain(|(known, _)| known != &slot);
        self.restore_slots.push((slot, id));
    }

    /// Forgets every materialized workspace that is not in `alive`.
    ///
    /// A workspace can go away under a slot — its display coming back takes it along — and a stale
    /// id would then place the rest of the block against a workspace that is not there.
    pub fn retain_materialized_workspaces(&mut self, alive: &[WorkspaceId]) {
        self.restore_slots.retain(|(_, id)| alive.contains(id));
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

    /// The `(session id, name)` this toplevel is registered under, if any.
    ///
    /// A toplevel belongs to one client and `already_added` is client-scoped, so there is at most
    /// one registration to find.
    pub fn registration_for(&self, toplevel: &XdgToplevel) -> Option<(String, String)> {
        self.live.iter().find_map(|(id, live)| {
            let (name, _) = live
                .toplevels
                .iter()
                .find(|(_, reg)| &reg.toplevel == toplevel)?;
            Some((id.clone(), name.clone()))
        })
    }

    /// Resolves what it takes to restore `toplevel`, or `None` if it is no longer restorable.
    ///
    /// A takeover empties the previous holder's registrations, so a session that changed hands
    /// between the request and the configure simply fails to resolve here — the inertness rule
    /// doing the work rather than a staleness check.
    pub fn restore_target_for(&self, toplevel: &XdgToplevel) -> Option<RestoreTarget> {
        self.live.iter().find_map(|(session_id, live)| {
            let (name, reg) = live
                .toplevels
                .iter()
                .find(|(_, reg)| &reg.toplevel == toplevel)?;
            Some(RestoreTarget {
                session_id: session_id.clone(),
                name: name.clone(),
                reason: live.reason,
                handle: reg.handle.clone(),
            })
        })
    }

    /// Every live registration, as `(session id, name, toplevel)`.
    ///
    /// For the shutdown sweep: mutter reaches the same state by unmanaging every window before its
    /// final save (`display.c:1052`), which runs each one through `on_window_unmanaging`. We have
    /// no unmanage-all, so the sweep walks the registrations instead.
    pub fn live_registrations(&self) -> Vec<(String, String, XdgToplevel)> {
        self.live
            .iter()
            .flat_map(|(id, live)| {
                live.toplevels
                    .iter()
                    .map(move |(name, reg)| (id.clone(), name.clone(), reg.toplevel.clone()))
            })
            .collect()
    }

    /// Hands `id` to `resource`, replacing whoever held it.
    ///
    /// The previous holder is told it was `replaced` and everything it owned becomes inert. Its
    /// live registrations do not carry over: what a takeover inherits is the *saved* state, which
    /// this slice does not have yet.
    fn take_over(&mut self, id: String, resource: XdgSessionV1, reason: Reason) {
        if let Some(previous) = self.live.remove(&id) {
            if previous.resource.is_alive() {
                previous.resource.replaced();
            }
        }

        self.live.insert(
            id,
            LiveSession {
                resource,
                reason,
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
                let Ok(reason) = reason.into_result() else {
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
                };

                // An id is *known* if some client holds it right now or the store remembers it
                // from a previous run. Anything else is treated as if NULL had been passed, so it
                // falls through to "make a fresh session" — spec, and mutter, both.
                let known = session_id.and_then(|sid| {
                    let manager = state.session_manager_state();
                    let holder = manager
                        .live
                        .get(&sid)
                        .and_then(|live| live.resource.client());
                    if holder.is_none() && !manager.store.contains(&sid) {
                        return None;
                    }
                    Some((sid, holder.as_ref() == Some(client)))
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
                        let manager = state.session_manager_state();
                        manager.take_over(sid.clone(), session.clone(), reason);
                        manager.store.touch(&sid);
                        state.schedule_session_save();
                        session.restored();
                    }
                    None => {
                        let sid = uuid::Uuid::new_v4().to_string();
                        let session = data_init.init(id, SessionData { id: sid.clone() });
                        let manager = state.session_manager_state();
                        manager.take_over(sid.clone(), session.clone(), reason);
                        manager.store.touch(&sid);
                        state.schedule_session_save();
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
            xdg_session_v1::Request::Remove => {
                // Only the object that still owns the id may forget it: an inert session (one
                // another client took over) must not delete the state it no longer manages.
                if state.session_manager_state().session_of(session).is_some()
                    && state.session_manager_state().store.remove(&data.id)
                {
                    state.schedule_session_save();
                }
            }
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

                // The registration is the same either way; what `restore_toplevel` adds is the
                // intent, which the initial configure acts on. A name the session has no record
                // for simply finds nothing there and degrades to a plain add, with no `restored`
                // event — what the spec prescribes.
                let known = state
                    .session_manager_state()
                    .store
                    .get(&data.id)
                    .is_some_and(|record| record.toplevels.contains_key(&name));
                add_toplevel(
                    state,
                    client,
                    session,
                    data,
                    id,
                    toplevel.clone(),
                    name,
                    data_init,
                );
                if known {
                    state.note_session_restore_requested(&toplevel);
                }
            }
            xdg_session_v1::Request::RemoveToplevel { name } => {
                if state.session_manager_state().session_of(session).is_none() {
                    return;
                }
                let manager = state.session_manager_state();
                manager
                    .session_of(session)
                    .expect("checked above")
                    .toplevels
                    .remove(&name);
                if manager.store.remove_toplevel(&data.id, &name) {
                    state.schedule_session_save();
                }
            }
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, session: &XdgSessionV1, data: &SessionData) {
        // Drops the session when the client goes away, too. Only if this object still owns the
        // id: a takeover already moved it to somebody else.
        let manager = state.session_manager_state();
        if manager.live.get(&data.id).map(|live| &live.resource) != Some(session) {
            return;
        }

        // Freeze the state *before* dropping the registrations, per the spec's "preserving the
        // current state". Afterwards there is nothing left to look these toplevels up under, so an
        // unmap that arrives later — the client tearing its windows down after its session, or the
        // whole client disappearing at once — would save nothing and leave a stale record behind.
        state.save_live_session_toplevels(&data.id);
        state.schedule_session_save();

        state.session_manager_state().live.remove(&data.id);
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

                // The saved state moves with the name, or a window that was unmapped before the
                // rename would leave its record orphaned under a name nothing answers to.
                let old_name = std::mem::replace(&mut *current, new_name.clone());
                drop(current);
                if manager
                    .store
                    .rename_toplevel(&data.session_id, &old_name, &new_name)
                {
                    state.schedule_session_save();
                }
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
