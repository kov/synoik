// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Is there a smartcard in the reader? — `org.gnome.SettingsDaemon.Smartcard`.
//!
//! GNOME does not talk to PC/SC or PKCS#11 itself. gnome-settings-daemon's smartcard plugin owns
//! the readers and exports one object per **token**, and the shell reads those over an
//! `ObjectManager` (`js/misc/smartcardManager.js`). Each token carries `Name`, `Driver`,
//! `IsInserted` and `UsedToLogin`.
//!
//! # Scope: we detect, we do not yet preempt
//!
//! In GNOME an inserted card **displaces** the password conversation — `_preemptingService` becomes
//! `gdm-smartcard` and that becomes the foreground service (`_checkForSmartcard`,
//! `js/gdm/util.js:477-496`; `serviceIsForeground` via `_defaultService`, `:646-658`). That part is
//! deliberately not ported yet: it restructures which conversation owns the entry, on a screen
//! whose one hard requirement is that it can always be answered, and there is no card on this
//! machine to prove it with. What is here is the half that can be checked — the token model, its
//! two rules, and the setting that gates it — so the flag preemption will read already exists and
//! is already right.
//!
//! # The rule on the lock screen is not "is a card in"
//!
//! `_checkForSmartcard` picks between two questions, and which one it asks depends on
//! `_reauthOnly` — which `authPrompt.js:162-168` sets **true for every unlock**. So the login
//! screen asks "is any token inserted" and the lock screen asks "is *the* token you logged in with
//! still inserted". A colleague's card in the second slot unlocks nobody's session, and treating
//! any card as the user's would offer an authentication that could only ever fail.
//!
//! `UsedToLogin` is also **sticky** in GNOME: `_updateToken` sets `_loginToken` and never unsets
//! it, so a token that was used to log in stays the login token until its object goes away
//! (`_removeToken`, `smartcardManager.js:93-105`). Removing the card clears `IsInserted`, not the
//! token's identity — which is what lets "put it back in" work.

use std::collections::HashMap;

use zbus::zvariant::OwnedObjectPath;

const GSD_SMARTCARD: &str = "org.gnome.SettingsDaemon.Smartcard";
const SMARTCARD_PATH: &str = "/org/gnome/SettingsDaemon/Smartcard";
const TOKEN_IFACE: &str = "org.gnome.SettingsDaemon.Smartcard.Token";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// What the watcher tells the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartcardToSynoik {
    /// Whether a card the shell would authenticate with is present, already reduced by
    /// [`Tokens::detected`]. A `bool` rather than the token list because the list is nobody else's
    /// business: the shell's whole interest is "is there one".
    Detected(bool),
}

/// One token object, as far as we care about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenState {
    pub is_inserted: bool,
    pub used_to_login: bool,
}

/// The tokens gsd is exporting — `SmartcardManager`'s `_insertedTokens` and `_loginToken`.
#[derive(Debug, Default)]
pub struct Tokens {
    tokens: HashMap<String, TokenState>,
    /// The token that was used to log in. **Sticky**: set when a token first says so, cleared only
    /// when that object goes away, never merely because the card came out (see the module docs).
    login: Option<String>,
}

impl Tokens {
    /// `_addToken` / `_updateToken` (`smartcardManager.js:60-91`), which are the same thing for our
    /// purposes: we re-read state rather than subscribing per proxy.
    pub fn upsert(&mut self, path: String, state: TokenState) {
        if state.used_to_login {
            self.login = Some(path.clone());
        }
        self.tokens.insert(path, state);
    }

    /// `_removeToken` (`:93-105`) — the object left the bus, so the token is gone rather than out.
    pub fn remove(&mut self, path: &str) {
        self.tokens.remove(path);
        if self.login.as_deref() == Some(path) {
            self.login = None;
        }
    }

    /// `hasInsertedTokens` (`:107-109`) — the **login screen's** question.
    pub fn has_inserted(&self) -> bool {
        self.tokens.values().any(|t| t.is_inserted)
    }

    /// `hasInsertedLoginToken` (`:111-118`) — the **lock screen's** question, and the one that
    /// matters here. Not "a card is in", but "the card this session was unlocked with is in".
    pub fn has_inserted_login_token(&self) -> bool {
        self.login
            .as_ref()
            .and_then(|path| self.tokens.get(path))
            .is_some_and(|t| t.is_inserted)
    }

    /// `_checkForSmartcard` (`util.js:477-486`).
    ///
    /// `reauth_only` is **true for every unlock** (`authPrompt.js:162-168` passes it), so the lock
    /// screen always takes the login-token branch; the parameter exists because the login screen,
    /// if we ever grow one, takes the other.
    pub fn detected(&self, enabled: bool, reauth_only: bool) -> bool {
        if !enabled {
            // The setting is checked first and short-circuits, so a machine with the key off never
            // reports a card however many are in the reader.
            return false;
        }
        if reauth_only {
            self.has_inserted_login_token()
        } else {
            self.has_inserted()
        }
    }
}

/// The match rule for token property changes.
///
/// Narrow on purpose, and **fallible on purpose**: a rule that failed to narrow would be a rule
/// that matches every signal on the session bus, and this process would then wake for all of them
/// to discard all but a handful. Building it is the one place that can go wrong, so it returns the
/// error rather than falling back to something that still "works".
fn token_properties_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(GSD_SMARTCARD)?
        .interface(PROPERTIES)?
        .member("PropertiesChanged")?
        .build())
}

/// Read one token's properties out of an `a{sv}`.
fn token_state(props: &HashMap<String, zbus::zvariant::OwnedValue>) -> TokenState {
    let flag = |name: &str| {
        props
            .get(name)
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    };
    TokenState {
        is_inserted: flag("IsInserted"),
        used_to_login: flag("UsedToLogin"),
    }
}

/// Watch gsd's tokens and report whether one the shell would use is present.
///
/// gsd-smartcard is **not** activated by this: it is a session service that is either running or
/// not, and asking an absent one for its objects fails harmlessly. A machine with no smartcard
/// stack therefore reports `false` once and is never heard from again, which is the common case and
/// costs one failed round trip at startup.
pub fn start(
    enabled: bool,
    to_niri: calloop::channel::Sender<SmartcardToSynoik>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;
    let async_conn = conn.inner().clone();
    conn.inner()
        .executor()
        .spawn(
            async move {
                watch(&async_conn, enabled, to_niri).await;
            },
            "watch smartcard tokens",
        )
        .detach();
    Ok(conn)
}

async fn watch(
    conn: &zbus::Connection,
    enabled: bool,
    to_niri: calloop::channel::Sender<SmartcardToSynoik>,
) {
    use futures_util::StreamExt;

    let mut tokens = Tokens::default();
    // Always on the lock screen — see [`Tokens::detected`].
    let reauth_only = true;
    let mut reported = false;

    let manager = match zbus::Proxy::new(conn, GSD_SMARTCARD, SMARTCARD_PATH, OBJECT_MANAGER).await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            debug!("no smartcard manager: {err:?}");
            return;
        }
    };

    // Subscribe *before* the initial listing, or a card inserted in the gap between them is missed
    // until it is taken out again.
    let added = manager.receive_signal("InterfacesAdded").await.ok();
    let removed = manager.receive_signal("InterfacesRemoved").await.ok();
    let (Some(added), Some(removed)) = (added, removed) else {
        debug!("smartcard manager will not talk to us; not watching for cards");
        return;
    };
    // gsd emits property changes on the token objects themselves, not through the manager, so this
    // is a bus-wide match narrowed by interface rather than a proxy signal.
    let rule = match token_properties_rule() {
        Ok(rule) => rule,
        Err(err) => {
            warn!("cannot build the smartcard match rule: {err:?}");
            return;
        }
    };
    let changed = match zbus::MessageStream::for_match_rule(rule, conn, None).await {
        Ok(stream) => stream,
        Err(err) => {
            debug!("cannot watch smartcard token properties: {err:?}");
            return;
        }
    };

    type Managed =
        HashMap<OwnedObjectPath, HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>>>;
    match manager.call_method("GetManagedObjects", &()).await {
        Ok(reply) => match reply.body().deserialize::<Managed>() {
            Ok(objects) => {
                for (path, ifaces) in objects {
                    if let Some(props) = ifaces.get(TOKEN_IFACE) {
                        tokens.upsert(path.to_string(), token_state(props));
                    }
                }
            }
            Err(err) => debug!("could not read the smartcard token list: {err:?}"),
        },
        Err(err) => {
            debug!("no smartcard tokens: {err:?}");
        }
    }

    let report = |tokens: &Tokens, reported: &mut bool| {
        let detected = tokens.detected(enabled, reauth_only);
        if detected != *reported {
            *reported = detected;
            debug!(
                "smartcard: a login token is {}",
                if detected { "present" } else { "absent" }
            );
            return to_niri.send(SmartcardToSynoik::Detected(detected)).is_ok();
        }
        true
    };
    if !report(&tokens, &mut reported) {
        return;
    }

    let mut added = added.fuse();
    let mut removed = removed.fuse();
    let mut changed = changed.fuse();
    loop {
        futures_util::select! {
            msg = added.next() => {
                let Some(msg) = msg else { return };
                if let Ok((path, ifaces)) = msg
                    .body()
                    .deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>>)>()
                {
                    if let Some(props) = ifaces.get(TOKEN_IFACE) {
                        tokens.upsert(path.to_string(), token_state(props));
                    }
                }
            }
            msg = removed.next() => {
                let Some(msg) = msg else { return };
                if let Ok((path, ifaces)) = msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>() {
                    if ifaces.iter().any(|i| i == TOKEN_IFACE) {
                        tokens.remove(&path.to_string());
                    }
                }
            }
            msg = changed.next() => {
                let Some(Ok(msg)) = msg else { return };
                let Some(path) = msg.header().path().map(|p| p.to_string()) else { continue };
                // Only tokens we already know about: gsd exports drivers on the same tree, and a
                // driver's properties changing is not a card moving.
                let Some(known) = tokens.tokens.get(&path).copied() else { continue };
                let Ok((iface, props, _invalid)) = msg
                    .body()
                    .deserialize::<(String, HashMap<String, zbus::zvariant::OwnedValue>, Vec<String>)>()
                else {
                    continue;
                };
                if iface != TOKEN_IFACE {
                    continue;
                }
                // A partial update: `PropertiesChanged` carries only what moved.
                let mut state = known;
                if let Some(v) = props.get("IsInserted").and_then(|v| bool::try_from(v).ok()) {
                    state.is_inserted = v;
                }
                if let Some(v) = props.get("UsedToLogin").and_then(|v| bool::try_from(v).ok()) {
                    state.used_to_login = v;
                }
                tokens.upsert(path, state);
            }
        }
        if !report(&tokens, &mut reported) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inserted() -> TokenState {
        TokenState {
            is_inserted: true,
            used_to_login: false,
        }
    }
    fn login_token() -> TokenState {
        TokenState {
            is_inserted: true,
            used_to_login: true,
        }
    }

    /// The lock screen asks about **the** card, not about any card.
    ///
    /// `_reauthOnly` is true for every unlock (`authPrompt.js:162-168`), so `_checkForSmartcard`
    /// takes the `hasInsertedLoginToken` branch (`util.js:481-486`). A colleague's card in the
    /// second slot unlocks nobody's session; offering an authentication on the strength of it would
    /// be offering one that could only ever fail. The login screen's question is the other one, and
    /// this pins both so the branch cannot be quietly collapsed into whichever is convenient.
    #[test]
    fn the_lock_screen_only_counts_the_token_that_logged_in() {
        let mut tokens = Tokens::default();
        tokens.upsert("/t/other".to_owned(), inserted());

        assert!(tokens.has_inserted(), "a card is in the reader");
        assert!(
            !tokens.has_inserted_login_token(),
            "but it is not the one this session belongs to"
        );
        assert!(
            !tokens.detected(true, true),
            "the lock screen must not offer it"
        );
        assert!(
            tokens.detected(true, false),
            "the login screen would take any card"
        );

        // The user's own card goes in beside it.
        tokens.upsert("/t/mine".to_owned(), login_token());
        assert!(tokens.detected(true, true));
    }

    /// Taking the card out is not the token going away, and putting it back must work.
    ///
    /// GNOME never clears `_loginToken` on a property change — only `_removeToken` does
    /// (`smartcardManager.js:93-105`). Clearing it when `IsInserted` went false would look right
    /// and would mean the card only ever worked once per session, because nothing would recognise
    /// it on the way back in.
    #[test]
    fn removing_the_card_does_not_forget_which_token_it_was() {
        let mut tokens = Tokens::default();
        tokens.upsert("/t/mine".to_owned(), login_token());
        assert!(tokens.detected(true, true));

        // Card out: `IsInserted` goes false, and gsd stops advertising `UsedToLogin` with it.
        tokens.upsert(
            "/t/mine".to_owned(),
            TokenState {
                is_inserted: false,
                used_to_login: false,
            },
        );
        assert!(!tokens.detected(true, true), "the card is out");

        // ...and back in, still recognised as the login token.
        tokens.upsert("/t/mine".to_owned(), inserted());
        assert!(
            tokens.detected(true, true),
            "the login token was forgotten when the card came out"
        );

        // The *object* going away is different: that token is gone for good.
        tokens.remove("/t/mine");
        tokens.upsert("/t/mine".to_owned(), inserted());
        assert!(
            !tokens.detected(true, true),
            "a token that left the bus must not come back as the login one"
        );
    }

    /// The property-change subscription is narrowed, not a firehose.
    ///
    /// `MatchRule`'s narrowing methods are all fallible, and the tempting way to write that is a
    /// chain with a fallback — which silently yields a rule matching **every signal on the session
    /// bus**, waking the compositor for all of them. It would still "work"; it would just be a
    /// permanent cost nobody could see. So the builder must succeed on every clause.
    #[test]
    fn the_token_subscription_is_narrow() {
        let rule = token_properties_rule().expect("the match rule must build");
        assert_eq!(
            rule.sender().map(|s| s.to_string()).as_deref(),
            Some(GSD_SMARTCARD)
        );
        assert_eq!(
            rule.interface().map(|i| i.to_string()).as_deref(),
            Some(PROPERTIES)
        );
        assert_eq!(
            rule.member().map(|m| m.to_string()).as_deref(),
            Some("PropertiesChanged")
        );
    }

    /// The setting is checked first and wins over any amount of hardware.
    ///
    /// `_checkForSmartcard` short-circuits on it (`util.js:480-481`), and it defaults to **false**
    /// — so on most machines this whole path is inert, which is exactly what it should be.
    #[test]
    fn the_setting_short_circuits_everything() {
        let mut tokens = Tokens::default();
        tokens.upsert("/t/mine".to_owned(), login_token());
        assert!(tokens.detected(true, true));
        assert!(!tokens.detected(false, true));
        assert!(!tokens.detected(false, false));
    }
}
