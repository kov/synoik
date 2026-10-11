// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! AccountsService — who the session's user is (`org.freedesktop.Accounts`).
//!
//! GNOME reaches this through libaccountsservice rather than raw D-Bus
//! (`AccountsService.UserManager .get_default().get_user(name)`, `unlockDialog.js:589-591`,
//! `screenShield.js:651-652`), so the protocol has to be reconstructed here: `FindUserByName` on
//! `/org/freedesktop/Accounts` gives a `/org/freedesktop/Accounts/User<uid>` path, and the
//! account's properties live on `org.freedesktop.Accounts.User` there.
//!
//! It replaces the passwd entry we used before for the real name, and adds two things passwd cannot
//! answer: the user's avatar, and whether the account has a password at all.
//!
//! # Everything here fails closed
//!
//! The properties arrive **asynchronously**, over a service that can be absent, slow, or refuse us.
//! Meanwhile the lock screen is a thing you can already be looking at. So every value has a safe
//! reading for "we do not know yet", and it is never the permissive one:
//!
//! - `password_mode` unknown means **the account has a password**. The opposite default would make
//!   the first lock after boot — before the reply lands — a shield that any keypress raises. See
//!   [`crate::screen_shield::ScreenShield::lock`].
//! - `real_name` unknown means show the login name, never a placeholder. GNOME blanks the label
//!   entirely while `is_loaded` is false (`userWidget.js:159-166`); a login name is friendlier and
//!   is what we already had.
//! - `icon_file` unknown means the themed `avatar-default-symbolic`.

use std::path::PathBuf;

use futures_util::StreamExt;

const ACCOUNTS_NAME: &str = "org.freedesktop.Accounts";
const ACCOUNTS_PATH: &str = "/org/freedesktop/Accounts";
const ACCOUNTS_IFACE: &str = "org.freedesktop.Accounts";
const USER_IFACE: &str = "org.freedesktop.Accounts.User";

/// `ActUserPasswordMode` (`AccountsService-1.0.gir`), as the `PasswordMode` property's `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasswordMode {
    /// Password set normally. **The default**, deliberately — see the module docs.
    #[default]
    Regular,
    /// The user will choose a password at next login. Still a password as far as we are concerned.
    SetAtLogin,
    /// No password at all: `lock` covers the screen but must not require authentication
    /// (`screenShield.js:656-659`).
    None,
}

impl From<i32> for PasswordMode {
    fn from(value: i32) -> Self {
        match value {
            1 => Self::SetAtLogin,
            2 => Self::None,
            // 0 is `REGULAR`; anything AccountsService grows later reads as "has a password",
            // which is the fail-closed direction.
            _ => Self::Regular,
        }
    }
}

impl PasswordMode {
    /// Whether this account can be got into without authenticating.
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// The account, as far as the lock screen cares.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserAccount {
    /// `RealName`. Empty when unset — the caller falls back to the login name.
    pub real_name: String,
    /// `IconFile`, if it is a regular file we could stat. See [`AccountIcon`].
    pub icon_file: Option<AccountIcon>,
    pub password_mode: PasswordMode,
}

/// The account picture: the path AccountsService reported, **plus the identity of the bytes at
/// it**.
///
/// Both halves are needed and neither is optional, which is why this is a type with one
/// constructor rather than two fields on [`UserAccount`]:
///
/// - the **path** must be re-checked on disk, because AccountsService will happily keep reporting
///   one that has been deleted (`userWidget.js:73-76`); doing it here keeps it off the render path.
/// - the **stamp** is what makes a change visible at all. AccountsService reuses one path per user
///   (`/var/lib/AccountsService/icons/<name>`), so changing your picture in Settings emits an
///   argument-less `Changed` and a byte-identical `IconFile`. With nothing that moves, the re-read
///   account compares equal to the one we hold, the update is dropped as a no-op, and every cache
///   downstream — all keyed by path — serves the previous picture for the rest of the session.
///   gnome-shell gets this from `StTextureCache`'s per-file `GFileMonitor`
///   (`st-texture-cache.c:1087-1133`); this is the cheap equivalent.
///
/// [`read`](Self::read) is the only way to build one, from a single `metadata` call, so the two can
/// neither disagree about which file they describe nor drift apart by someone adding a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIcon {
    pub path: PathBuf,
    /// Modification time and length. Private: nothing reads it, it only has to *differ*.
    stamp: (std::time::SystemTime, u64),
}

impl AccountIcon {
    /// `None` unless `path` is a regular file we can stat.
    pub fn read(path: PathBuf) -> Option<Self> {
        let meta = path.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Self {
            stamp: (meta.modified().ok()?, meta.len()),
            path,
        })
    }
}

#[derive(Debug, Clone)]
pub enum AccountsToSynoik {
    /// The account's properties, on first read and on every change.
    UserChanged(UserAccount),
    /// Whether the "Other User" button has anywhere to go: more than one non-system account exists
    /// on this machine (`has_multiple_users`, `unlockDialog.js:922`).
    MultipleUsers(bool),
    /// `can_switch()` (`unlockDialog.js:922`) — see
    /// [`crate::dbus::user_switching::seat_id`] for why this reduces to "we have a seat".
    ///
    /// It rides this channel rather than logind's because it is one of the same button's four
    /// gates and is resolved once at startup, not watched: a session does not change seats.
    CanSwitch(bool),
    /// The greeter's user list, in display order, on first read and on every change.
    Users(Vec<LoginUser>),
}

/// One entry of the login screen's user list (`UserList.addUser`, `loginDialog.js:262-288`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginUser {
    /// `UserName`.
    pub name: String,
    /// `RealName`; empty when unset.
    pub real_name: String,
    pub icon_file: Option<AccountIcon>,
    pub uid: u32,
    /// `is_logged_in()`: the user has a session on our seat (`:logged-in`,
    /// `loginDialog.js:104-109`).
    pub logged_in: bool,
    /// `LoginFrequency`, the first key of libaccountsservice's ordering (`act_user_collate`).
    pub login_frequency: u64,
}

impl LoginUser {
    /// The name the list shows: the real name, or the login name without one.
    pub fn display_name(&self) -> &str {
        if self.real_name.is_empty() {
            &self.name
        } else {
            &self.real_name
        }
    }
}

/// The list the login screen shows, from every account AccountsService reported: no system
/// accounts, no locked ones, none without a name (`UserList.addUser`, `loginDialog.js:262-276`).
/// Ordered as libaccountsservice collates them — most frequent logins first, then by name.
pub fn login_list(mut users: Vec<(LoginUser, bool, bool)>) -> Vec<LoginUser> {
    users.retain(|(user, system, locked)| !system && !locked && !user.name.is_empty());
    let mut users: Vec<LoginUser> = users.into_iter().map(|(user, _, _)| user).collect();
    users.sort_by(|a, b| {
        b.login_frequency.cmp(&a.login_frequency).then_with(|| {
            a.display_name()
                .to_lowercase()
                .cmp(&b.display_name().to_lowercase())
        })
    });
    users
}

/// Read one account for the list: the entry, and whether it is a system account and locked.
async fn read_login_user(user: &zbus::Proxy<'_>, seated: &[u32]) -> (LoginUser, bool, bool) {
    let uid = user.get_property::<u64>("Uid").await.unwrap_or(u64::MAX) as u32;
    let login_user = LoginUser {
        name: user
            .get_property::<String>("UserName")
            .await
            .unwrap_or_default(),
        real_name: user
            .get_property::<String>("RealName")
            .await
            .unwrap_or_default(),
        icon_file: user
            .get_property::<String>("IconFile")
            .await
            .ok()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .and_then(AccountIcon::read),
        uid,
        logged_in: seated.contains(&uid),
        login_frequency: user
            .get_property::<u64>("LoginFrequency")
            .await
            .unwrap_or_default(),
    };
    // Unknown reads as the excluding answer: a list that briefly misses someone is better than
    // one that offers a system account.
    let system = user
        .get_property::<bool>("SystemAccount")
        .await
        .unwrap_or(true);
    let locked = user.get_property::<bool>("Locked").await.unwrap_or(true);
    (login_user, system, locked)
}

/// The uids with a `user`-class session on seat0 that is not closing — libaccountsservice's
/// `is_logged_in`, which counts the sessions on our seat.
async fn seated_uids(conn: &zbus::Connection) -> Vec<u32> {
    let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "ListSessions",
            &(),
        )
        .await
    else {
        return Vec::new();
    };
    let Ok(sessions) =
        reply
            .body()
            .deserialize::<Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)>>()
    else {
        return Vec::new();
    };
    let mut uids = Vec::new();
    for (_, uid, _, seat, path) in sessions {
        if seat != "seat0" {
            continue;
        }
        let Ok(session) = zbus::Proxy::new(
            conn,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )
        .await
        else {
            continue;
        };
        let class = session.get_property::<String>("Class").await;
        let state = session.get_property::<String>("State").await;
        if class.as_deref() == Ok("user") && state.as_deref() != Ok("closing") {
            uids.push(uid);
        }
    }
    uids
}

async fn read_login_list(conn: &zbus::Connection) -> Vec<LoginUser> {
    let Ok(accounts) = zbus::Proxy::new(conn, ACCOUNTS_NAME, ACCOUNTS_PATH, ACCOUNTS_IFACE).await
    else {
        return Vec::new();
    };
    let paths = accounts
        .call_method("ListCachedUsers", &())
        .await
        .ok()
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<Vec<zbus::zvariant::OwnedObjectPath>>()
                .ok()
        })
        .unwrap_or_default();
    let seated = seated_uids(conn).await;
    let mut users = Vec::new();
    for path in paths {
        if let Ok(user) = zbus::Proxy::new(conn, ACCOUNTS_NAME, path, USER_IFACE).await {
            users.push(read_login_user(&user, &seated).await);
        }
    }
    login_list(users)
}

/// The greeter's watcher: the login screen's user list, re-read whenever an account is added,
/// deleted or changed (`loginDialog.js:1523-1545` — `Locked` flipping arrives as `Changed`) or a
/// session starts or ends on the machine (the `:logged-in` mark).
pub fn start_user_list(
    to_niri: calloop::channel::Sender<AccountsToSynoik>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    let async_conn = conn.inner().clone();
    let future = async move {
        use futures_util::stream::{self, StreamExt as _};

        // Any `Changed` from any user object, and the manager's own signals: one match rule each,
        // subscribed before the first read so nothing lands in between unseen.
        let rule = |iface: &'static str, member: &'static str| {
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(iface)
                .and_then(|b| b.member(member))
                .map(|b| b.build())
        };
        let mut streams = Vec::new();
        for (iface, member) in [
            (ACCOUNTS_IFACE, "UserAdded"),
            (ACCOUNTS_IFACE, "UserDeleted"),
            (USER_IFACE, "Changed"),
            ("org.freedesktop.login1.Manager", "SessionNew"),
            ("org.freedesktop.login1.Manager", "SessionRemoved"),
        ] {
            let Ok(rule) = rule(iface, member) else {
                continue;
            };
            match zbus::MessageStream::for_match_rule(rule, &async_conn, None).await {
                Ok(stream) => streams.push(stream.map(|_| ()).boxed()),
                Err(err) => warn!("error subscribing to {iface}.{member}: {err:?}"),
            }
        }
        let mut events = stream::select_all(streams);

        loop {
            let users = read_login_list(&async_conn).await;
            if to_niri.send(AccountsToSynoik::Users(users)).is_err() {
                break;
            }
            if events.next().await.is_none() {
                break;
            }
        }
    };

    conn.inner()
        .executor()
        .spawn(future, "watch the login user list")
        .detach();

    Ok(conn)
}

/// Read the properties we care about off an already-resolved user proxy.
async fn read_account(user: &zbus::Proxy<'_>) -> UserAccount {
    let real_name = user
        .get_property::<String>("RealName")
        .await
        .unwrap_or_default();

    let icon_file = user
        .get_property::<String>("IconFile")
        .await
        .ok()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        // A path that no longer exists becomes `None` here rather than a permanently missing
        // avatar; the themed fallback at least draws something.
        .and_then(AccountIcon::read);

    let password_mode = user
        .get_property::<i32>("PasswordMode")
        .await
        .map_or(PasswordMode::Regular, PasswordMode::from);

    UserAccount {
        real_name,
        icon_file,
        password_mode,
    }
}

/// Look up one account by name, for a caller that only wants a snapshot.
///
/// The watcher below follows *our* user forever; the polkit dialog needs whoever polkitd named,
/// which is usually `root`, and needs it exactly once per request. `None` means AccountsService
/// could not answer — the caller must fall back to the conservative reading, which for
/// [`PasswordMode`] is "this account has a password".
pub async fn account_for(conn: &zbus::Connection, username: &str) -> Option<UserAccount> {
    let accounts = zbus::Proxy::new(conn, ACCOUNTS_NAME, ACCOUNTS_PATH, ACCOUNTS_IFACE)
        .await
        .ok()?;
    let path = accounts
        .call_method("FindUserByName", &(username))
        .await
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<zbus::zvariant::OwnedObjectPath>()
        })
        .ok()?;
    let user = zbus::Proxy::new(conn, ACCOUNTS_NAME, path, USER_IFACE)
        .await
        .ok()?;
    Some(read_account(&user).await)
}

/// Whether more than one ordinary account exists.
///
/// libaccountsservice computes `has_multiple_users` from its cached list; the wire equivalent is
/// `ListCachedUsers`, which is already the "real people" list — system accounts are not cached.
async fn multiple_users(conn: &zbus::Connection) -> bool {
    let Ok(accounts) = zbus::Proxy::new(conn, ACCOUNTS_NAME, ACCOUNTS_PATH, ACCOUNTS_IFACE).await
    else {
        return false;
    };
    accounts
        .call_method("ListCachedUsers", &())
        .await
        .ok()
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<Vec<zbus::zvariant::OwnedObjectPath>>()
                .ok()
        })
        .is_some_and(|users| users.len() > 1)
}

pub fn start(
    username: String,
    to_niri: calloop::channel::Sender<AccountsToSynoik>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;

    let async_conn = conn.inner().clone();
    let future = async move {
        let accounts =
            match zbus::Proxy::new(&async_conn, ACCOUNTS_NAME, ACCOUNTS_PATH, ACCOUNTS_IFACE).await
            {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!("error creating the AccountsService proxy: {err:?}");
                    return;
                }
            };

        let path = match accounts
            .call_method("FindUserByName", &(username.as_str()))
            .await
            .and_then(|reply| {
                reply
                    .body()
                    .deserialize::<zbus::zvariant::OwnedObjectPath>()
            }) {
            Ok(path) => path,
            Err(err) => {
                // Not fatal, and not even unusual: a machine with no AccountsService, or an account
                // it does not know about. The defaults are the conservative ones.
                warn!("AccountsService does not know {username}: {err:?}");
                return;
            }
        };

        let user = match zbus::Proxy::new(&async_conn, ACCOUNTS_NAME, path, USER_IFACE).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating the AccountsService user proxy: {err:?}");
                return;
            }
        };

        // Subscribe before the first read, so a change landing in between is not lost — the same
        // ordering the presence watcher needs, and for the same reason.
        //
        // Both signals, deliberately: the properties are `emits-change`, so
        // `PropertiesChanged` covers the usual case, but AccountsService also emits its own
        // argument-less `Changed` on the user object, which is what libaccountsservice listens to
        // (`userWidget.js:122-125` connects `changed`). Either one just means "re-read".
        let changed = match user.receive_signal("Changed").await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to the AccountsService user: {err:?}");
                return;
            }
        };
        // `PasswordMode` stands in for the whole property set here: any `PropertiesChanged` on
        // this object carries it or not, and either way the reaction is to re-read everything.
        let props_changed = user.receive_property_changed::<i32>("PasswordMode").await;

        // The *manager's* signals, for the other half of the model: whether this machine has
        // anybody else to log in as. GNOME re-evaluates the switch-user button on
        // `notify::has-multiple-users` (`unlockDialog.js:640-643`), and creating a second account
        // is precisely the event that should make that button appear — read once at startup, it
        // never would until the next reboot.
        let user_added = accounts.receive_signal("UserAdded").await;
        let user_deleted = accounts.receive_signal("UserDeleted").await;

        let _ = to_niri.send(AccountsToSynoik::UserChanged(read_account(&user).await));
        let _ = to_niri.send(AccountsToSynoik::MultipleUsers(
            multiple_users(&async_conn).await,
        ));
        let _ = to_niri.send(AccountsToSynoik::CanSwitch(
            crate::dbus::user_switching::seat_id(&async_conn)
                .await
                .is_some(),
        ));

        // One stream of "something changed, and which half", so the loop is a plain `next()`
        // rather than an N-way hand-rolled select that grows a branch per source.
        let mut events = {
            use futures_util::stream::{self, StreamExt as _};

            let mut streams: Vec<stream::BoxStream<'_, Wake>> = vec![
                changed.map(|_| Wake::Account).boxed(),
                props_changed.map(|_| Wake::Account).boxed(),
            ];
            for signal in [user_added, user_deleted].into_iter().flatten() {
                streams.push(signal.map(|_| Wake::Users).boxed());
            }
            stream::select_all(streams)
        };

        while let Some(wake) = events.next().await {
            let msg = match wake {
                Wake::Account => AccountsToSynoik::UserChanged(read_account(&user).await),
                Wake::Users => AccountsToSynoik::MultipleUsers(multiple_users(&async_conn).await),
            };
            if to_niri.send(msg).is_err() {
                break;
            }
        }
    };

    conn.inner()
        .executor()
        .spawn(future, "monitor AccountsService")
        .detach();

    Ok(conn)
}

/// Which half of the model a signal asks us to re-read.
///
/// Both halves are watched because both change while the lock screen is up: the user's own
/// properties (their picture, their name, whether they have a password) and whether the machine has
/// a second account at all.
#[derive(Debug, Clone, Copy)]
enum Wake {
    Account,
    Users,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every unknown reads as "this account has a password".
    ///
    /// The permissive default is the dangerous one and it is dangerous *quietly*: the shield still
    /// covers the screen, so a lock that silently became a screensaver looks exactly like a lock
    /// until someone taps a key and is let straight in. AccountsService being slow, absent, or not
    /// knowing the account all land here.
    #[test]
    fn an_unknown_password_mode_still_takes_a_password() {
        assert!(!PasswordMode::default().is_none(), "the default");
        assert!(!PasswordMode::from(0).is_none(), "REGULAR");
        assert!(
            !PasswordMode::from(1).is_none(),
            "SET_AT_LOGIN is still a password"
        );
        assert!(
            !PasswordMode::from(7).is_none(),
            "a value AccountsService grows later must not unlock the screen"
        );
        assert!(
            !PasswordMode::from(-1).is_none(),
            "nor must a nonsense one — the property is a signed int on the wire"
        );

        // The one value that means it.
        assert!(PasswordMode::from(2).is_none(), "NONE");

        // And an account nobody has answered for yet.
        assert!(!UserAccount::default().password_mode.is_none());
        assert_eq!(UserAccount::default().icon_file, None);
        assert_eq!(UserAccount::default().real_name, "");
    }

    fn user(name: &str, real: &str, freq: u64) -> LoginUser {
        LoginUser {
            name: name.to_owned(),
            real_name: real.to_owned(),
            icon_file: None,
            uid: 1000,
            logged_in: false,
            login_frequency: freq,
        }
    }

    /// `UserList.addUser` skips system, locked and nameless accounts (`loginDialog.js:262-276`);
    /// the rest come most-frequent first, then by the shown name.
    #[test]
    fn the_login_list_is_filtered_and_collated() {
        let list = login_list(vec![
            (user("zed", "", 0), false, false),
            (user("amy", "Amy Pond", 0), false, false),
            (user("gdm", "", 9), true, false),
            (user("old", "Old Account", 9), false, true),
            (user("", "Nameless", 9), false, false),
            (user("bob", "Bob", 5), false, false),
        ]);
        let names: Vec<_> = list.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["bob", "amy", "zed"]);
    }
}
