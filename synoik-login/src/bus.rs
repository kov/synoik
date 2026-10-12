// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `org.gnome.DisplayManager` on the system bus: GDM's `Manager`, the part of it our clients call
//! (`docs/fork/greeter.md` §2.1-§2.2).

use zbus::fdo;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::zvariant::OwnedObjectPath;

use crate::channel::{self, Kind, Target};

const NAME: &str = "org.gnome.DisplayManager";
const PATH: &str = "/org/gnome/DisplayManager/Manager";

/// Our own interface beside GDM's, for root (`synoik-login open-session`).
pub const LOGIN_PATH: &str = "/org/synoik/Login";
pub const LOGIN_INTERFACE: &str = "org.synoik.Login";

/// Own the name and serve the manager. The returned connection must be kept alive.
pub fn serve() -> anyhow::Result<zbus::blocking::Connection> {
    Ok(zbus::blocking::connection::Builder::system()?
        .name(NAME)?
        .serve_at(PATH, Manager)?
        .serve_at(LOGIN_PATH, Login)?
        .build()?)
}

struct Login;

#[zbus::interface(name = "org.synoik.Login")]
impl Login {
    /// Log `user` in without credentials and return the logind session id: a test seat beside
    /// the administrator's (`docs/fork/greeter.md` §2.6). Root only, and only for the users
    /// `[debug] TestUsers` lists, re-read on every call; one session per user on seat0.
    async fn open_session(
        &self,
        user: String,
        background: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let (uid, _) = caller(&header, connection).await?;
        if uid != 0 {
            return Err(fdo::Error::AccessDenied("only root opens sessions".into()));
        }
        if !crate::config::Config::load().test_users.contains(&user) {
            return Err(fdo::Error::AccessDenied(format!(
                "{user} is not in [debug] TestUsers of {}",
                crate::config::CONFIG_PATH
            )));
        }
        if let Ok(id) = seat0_session_of(connection, &user).await {
            return Err(fdo::Error::Failed(format!(
                "{user} already has session {id} on seat0"
            )));
        }
        // Blocking: it waits for the worker to open the session.
        blocking::unblock(move || crate::daemon::open_test_session(&user, background))
            .await
            .map_err(|err| fdo::Error::Failed(format!("{err:#}")))
    }
}

struct Manager;

#[zbus::interface(name = "org.gnome.DisplayManager.Manager")]
impl Manager {
    /// The greeter's login channel (§2.2): only the greeter session's own process may have one.
    async fn open_session(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let (uid, pid) = caller(&header, connection).await?;
        // The greeter runs as a user unit under gnome-session, outside its session's scope: found
        // as its uid's display session, as GDM finds its own (`gdm-manager.c:512`).
        let session = caller_session(connection, pid, uid)
            .await
            .map_err(|err| fdo::Error::AccessDenied(format!("no session for the caller: {err}")))?;
        if crate::daemon::greeter_session().as_deref() != Some(session.id.as_str())
            || session.uid != uid
        {
            return Err(fdo::Error::AccessDenied(
                "a login channel is for the greeter".into(),
            ));
        }
        channel::open(Target {
            kind: Kind::Login,
            client_uid: uid,
        })
        .map_err(|err| fdo::Error::Failed(format!("{err:#}")))
    }

    /// A channel that verifies `username` again (§2.2, §2.5): for the caller's own session — the
    /// lock screen's — or, from the greeter, for the seat0 session `username` already has. With
    /// no such session the answer is `AccessDenied`, which is what sends the greeter to a login
    /// channel instead (`js/gdm/util.js:508-529`).
    async fn open_reauthentication_channel(
        &self,
        username: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let (uid, pid) = caller(&header, connection).await?;
        let caller = caller_session(connection, pid, uid)
            .await
            .map_err(|err| fdo::Error::AccessDenied(format!("no session for the caller: {err}")))?;

        let session = if crate::daemon::greeter_session().as_deref() == Some(caller.id.as_str())
            && caller.uid == uid
        {
            seat0_session_of(connection, &username)
                .await
                .map_err(|err| fdo::Error::AccessDenied(format!("{username}: {err}")))?
        } else {
            if caller.name != username || caller.uid != uid || caller.seat != "seat0" {
                return Err(fdo::Error::AccessDenied(
                    "a reauthentication channel is for the caller's own session on seat0".into(),
                ));
            }
            caller.id
        };

        channel::open(Target {
            kind: Kind::Reauth {
                user: username,
                session,
            },
            client_uid: uid,
        })
        .map_err(|err| fdo::Error::Failed(format!("{err:#}")))
    }

    /// The session tells the display manager it is up; nothing here depends on it.
    fn register_session(&self) {}

    fn register_display(&self) {}

    #[zbus(property)]
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}

/// The calling process's uid and pid, from the bus.
async fn caller(header: &Header<'_>, connection: &zbus::Connection) -> fdo::Result<(u32, u32)> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?
        .to_owned();
    let credentials = fdo::DBusProxy::new(connection)
        .await?
        .get_connection_credentials(BusName::Unique(sender))
        .await?;
    match (credentials.unix_user_id(), credentials.process_id()) {
        (Some(uid), Some(pid)) => Ok((uid, pid)),
        _ => Err(fdo::Error::AccessDenied("no credentials".into())),
    }
}

/// `username`'s graphical session on seat0, if it has one.
async fn seat0_session_of(connection: &zbus::Connection, username: &str) -> zbus::Result<String> {
    let reply = connection
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "ListSessions",
            &(),
        )
        .await?;
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        reply.body().deserialize()?;
    for (id, _, name, seat, path) in sessions {
        if name != username || seat != "seat0" {
            continue;
        }
        let proxy = zbus::Proxy::new(
            connection,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )
        .await?;
        let class: String = proxy.get_property("Class").await?;
        let state: String = proxy.get_property("State").await?;
        if class == "user" && state != "closing" {
            return Ok(id);
        }
    }
    Err(zbus::Error::Failure("has no session on seat0".into()))
}

struct Session {
    id: String,
    name: String,
    uid: u32,
    seat: String,
}

/// The session a caller acts for: the one its process is in, else — a compositor run as a user
/// service lives in the user's manager, outside any session — the user's display session, as
/// logind names it (GDM looks the same way, `common/gdm-common.c:812-866`).
async fn caller_session(
    connection: &zbus::Connection,
    pid: u32,
    uid: u32,
) -> zbus::Result<Session> {
    let by_pid = logind(connection, "GetSessionByPID", &(pid,)).await;
    let path = match by_pid {
        Ok(path) => path,
        Err(_) => {
            let user = logind(connection, "GetUser", &(uid,)).await?;
            let proxy = zbus::Proxy::new(
                connection,
                "org.freedesktop.login1",
                user,
                "org.freedesktop.login1.User",
            )
            .await?;
            let (_, display): (String, OwnedObjectPath) = proxy.get_property("Display").await?;
            if display.as_str() == "/" {
                return Err(zbus::Error::Failure(
                    "the user has no display session".into(),
                ));
            }
            display
        }
    };
    session(connection, path).await
}

async fn logind<B>(
    connection: &zbus::Connection,
    method: &str,
    body: &B,
) -> zbus::Result<OwnedObjectPath>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let reply = connection
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            method,
            body,
        )
        .await?;
    reply.body().deserialize()
}

async fn session(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Session> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )
    .await?;
    let id: String = proxy.get_property("Id").await?;
    let name: String = proxy.get_property("Name").await?;
    let (uid, _): (u32, OwnedObjectPath) = proxy.get_property("User").await?;
    let (seat, _): (String, OwnedObjectPath) = proxy.get_property("Seat").await?;
    Ok(Session {
        id,
        name,
        uid,
        seat,
    })
}
