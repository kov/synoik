// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A minimal Secret Service client (`org.freedesktop.secrets`), for the network agent's store.
//!
//! GNOME's NetworkManager agent is not only the thing that *asks* for a Wi-Fi password — it is
//! also where that password lives. `shell-network-agent.c` looks agent-owned secrets up in the
//! keyring before it considers a dialog (`:396-406`) and writes them back after the user answers
//! (`:492-500`), so an agent without a store re-prompts on every single connect.
//!
//! We speak the wire protocol rather than link libsecret, the same trade the polkit agent makes
//! with `polkit-agent-helper-1`: the surface we need is five methods, and linking would marry a
//! `GMainContext` to calloop for nothing.
//!
//! **Item compatibility with gnome-shell is deliberate and load-bearing.** libsecret's
//! `secret_attributes_build` normally stamps an `xdg:schema` attribute onto every item, but the
//! network agent's schema carries `SECRET_SCHEMA_DONT_MATCH_NAME` (`shell-network-agent.c:74-84`),
//! which suppresses it. So an item is identified by exactly [`ATTR_UUID`], [`ATTR_SETTING_NAME`]
//! and [`ATTR_SETTING_KEY`] — no schema attribute — and a session that has run gnome-shell finds
//! its own saved passwords here, and vice versa. Adding `xdg:schema` would silently orphan both.
//!
//! # Secrecy
//!
//! [`Secret`] holds plaintext. It has a hand-written [`std::fmt::Debug`] that redacts, and every
//! type that carries one must do the same — a derived `Debug` anywhere up the chain puts a Wi-Fi
//! password in the journal.

use std::collections::HashMap;

use futures_util::StreamExt as _;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const SECRETS_BUS: &str = "org.freedesktop.secrets";
const SECRETS_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";

/// The default collection's alias path — the "Login" keyring on a normal session.
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";

/// The attribute keys gnome-shell tags network secrets with (`shell-network-agent.h:49-51`).
pub const ATTR_UUID: &str = "connection-uuid";
pub const ATTR_SETTING_NAME: &str = "setting-name";
pub const ATTR_SETTING_KEY: &str = "setting-key";

/// A path meaning "no prompt is needed" in every Secret Service reply that can carry one.
const NO_PROMPT: &str = "/";

/// One stored secret's plaintext.
///
/// Constructed only from a keyring read or a dialog answer, and consumed by the D-Bus reply to
/// NetworkManager. It deliberately does not implement `Clone`-free ergonomics like `Deref<str>`:
/// every place that reaches the string should be visible in a grep for [`Secret::expose`].
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The plaintext. Named so that a review of every caller is one grep.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Hand-written so a stray `{:?}` cannot put a password in the journal.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<redacted, {} chars>)", self.0.len())
    }
}

/// The Secret Service's `Secret` struct, `(oayays)`: session, parameters, value, content type.
type WireSecret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// An open session with the secret service.
///
/// The session is what secrets travel over. We negotiate the **plain** algorithm: the transport is
/// the user's own session bus, so the dh-ietf1024 handshake would encrypt a channel against an
/// attacker who, being on that bus already, can call `GetSecrets` themselves. libsecret makes the
/// same choice when told to (`SECRET_SERVICE_PLAIN`).
pub struct SecretSession {
    conn: zbus::Connection,
    session: OwnedObjectPath,
}

impl SecretSession {
    pub async fn open(conn: &zbus::Connection) -> zbus::Result<Self> {
        let service = service_proxy(conn).await?;
        let (_output, session): (OwnedValue, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::new("")))
            .await?;
        Ok(Self {
            conn: conn.clone(),
            session,
        })
    }

    /// Every unlocked item matching `attributes`, as `setting-key → secret`.
    ///
    /// Locked items are unlocked first (`SECRET_SEARCH_UNLOCK`, `shell-network-agent.c:403`),
    /// which on a locked keyring puts gnome-keyring's own prompt up. An item the user refuses to
    /// unlock is skipped rather than failing the lookup — upstream does the same, treating a
    /// `NULL` secret as absent (`:293-295`).
    pub async fn search(
        &self,
        attributes: &HashMap<&str, &str>,
    ) -> zbus::Result<HashMap<String, Secret>> {
        let service = service_proxy(&self.conn).await?;
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            service.call("SearchItems", &(attributes)).await?;

        let mut paths = unlocked;
        if !locked.is_empty() {
            let (mut newly, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                service.call("Unlock", &(&locked)).await?;
            if prompt.as_str() != NO_PROMPT {
                if let Some(result) = self.run_prompt(&prompt).await? {
                    newly.extend(Vec::<OwnedObjectPath>::try_from(result).unwrap_or_default());
                }
            }
            paths.extend(newly);
        }
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        // One `GetSecrets` for all of them; the reply keys by item path, so the setting key has to
        // come back off each item's own attributes.
        let secrets: HashMap<OwnedObjectPath, WireSecret> =
            service.call("GetSecrets", &(&paths, &self.session)).await?;

        let mut out = HashMap::new();
        for (path, (_session, _params, value, _content_type)) in secrets {
            let Ok(attrs) = self.item_attributes(&path).await else {
                continue;
            };
            let Some(key) = attrs.get(ATTR_SETTING_KEY) else {
                continue;
            };
            let Ok(value) = String::from_utf8(value) else {
                warn!("keyring item {path} holds a non-UTF-8 secret; skipping");
                continue;
            };
            out.insert(key.clone(), Secret::new(value));
        }
        Ok(out)
    }

    /// Unlock one object, prompting if the service asks. `Ok(false)` means the user refused, or
    /// there is no one to ask.
    ///
    /// A locked collection is not an edge case: the login keyring is locked for any session that
    /// did not unlock it at login (a `systemd-run` seat, a remote shell), and `CreateItem` on one
    /// fails with `org.freedesktop.Secret.Error.IsLocked` rather than unlocking on demand.
    /// libsecret's store path unlocks first (`secret_service_store` → `_secret_service_unlock`),
    /// so we must too or every saved Wi-Fi password is lost on such a session.
    pub async fn unlock(&self, path: &ObjectPath<'_>) -> zbus::Result<bool> {
        let service = service_proxy(&self.conn).await?;
        let (unlocked, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            service.call("Unlock", &(vec![path.to_owned()])).await?;
        if !unlocked.is_empty() {
            return Ok(true);
        }
        if prompt.as_str() == NO_PROMPT {
            return Ok(false);
        }
        let Some(result) = self.run_prompt(&prompt).await? else {
            return Ok(false);
        };
        Ok(!Vec::<OwnedObjectPath>::try_from(result)
            .unwrap_or_default()
            .is_empty())
    }

    /// Store one secret, replacing any item with the same attributes.
    pub async fn store(
        &self,
        label: &str,
        attributes: &HashMap<&str, &str>,
        secret: &Secret,
    ) -> zbus::Result<()> {
        let path = ObjectPath::try_from(DEFAULT_COLLECTION).expect("a literal path");
        if !self.unlock(&path).await? {
            return Err(zbus::Error::Failure(
                "the default keyring collection is locked".to_owned(),
            ));
        }

        let collection = zbus::Proxy::new(&self.conn, SECRETS_BUS, path, COLLECTION_IFACE).await?;

        let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
        properties.insert("org.freedesktop.Secret.Item.Label", Value::new(label));
        properties.insert(
            "org.freedesktop.Secret.Item.Attributes",
            Value::new(attributes.clone()),
        );

        let wire: WireSecret = (
            self.session.clone(),
            Vec::new(),
            secret.expose().as_bytes().to_vec(),
            "text/plain".to_owned(),
        );

        let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = collection
            .call("CreateItem", &(properties, wire, true))
            .await?;
        if item.as_str() == NO_PROMPT && prompt.as_str() != NO_PROMPT {
            self.run_prompt(&prompt).await?;
        }
        Ok(())
    }

    /// Delete every item matching `attributes`. Errors on individual items are swallowed, as
    /// upstream does when clearing before a re-save (`shell-network-agent.c:770`).
    pub async fn delete_matching(&self, attributes: &HashMap<&str, &str>) -> zbus::Result<()> {
        let service = service_proxy(&self.conn).await?;
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            service.call("SearchItems", &(attributes)).await?;

        for path in unlocked.into_iter().chain(locked) {
            let item = zbus::Proxy::new(&self.conn, SECRETS_BUS, path.clone(), ITEM_IFACE).await?;
            match item.call::<_, _, OwnedObjectPath>("Delete", &()).await {
                Ok(prompt) if prompt.as_str() != NO_PROMPT => {
                    let _ = self.run_prompt(&prompt).await;
                }
                Ok(_) => (),
                Err(err) => debug!("could not delete keyring item {path}: {err}"),
            }
        }
        Ok(())
    }

    async fn item_attributes(
        &self,
        path: &ObjectPath<'_>,
    ) -> zbus::Result<HashMap<String, String>> {
        let item = zbus::Proxy::new(&self.conn, SECRETS_BUS, path.to_owned(), ITEM_IFACE).await?;
        item.get_property("Attributes").await
    }

    /// Drive a prompt to its `Completed` signal. `Ok(None)` means the user dismissed it.
    ///
    /// The signal has to be subscribed **before** `Prompt` is called: the service is free to
    /// complete a prompt that needs no interaction immediately, and a subscription opened after
    /// the call would miss it and wait forever.
    async fn run_prompt(&self, path: &ObjectPath<'_>) -> zbus::Result<Option<OwnedValue>> {
        let prompt =
            zbus::Proxy::new(&self.conn, SECRETS_BUS, path.to_owned(), PROMPT_IFACE).await?;
        let mut completed = prompt.receive_signal("Completed").await?;
        // An empty window id: we have no X11 window to parent to, and gnome-keyring treats the
        // string as advisory.
        prompt.call::<_, _, ()>("Prompt", &("")).await?;

        let Some(signal) = completed.next().await else {
            return Ok(None);
        };
        let (dismissed, result): (bool, OwnedValue) = signal.body().deserialize()?;
        Ok((!dismissed).then_some(result))
    }
}

async fn service_proxy(conn: &zbus::Connection) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(conn, SECRETS_BUS, SECRETS_PATH, SERVICE_IFACE).await
}

/// The item label gnome-shell gives a saved network secret
/// (`shell-network-agent.c:create_keyring_add_attr_list`). Matching it is cosmetic — the
/// attributes are what identify an item — but it is what Passwords and Keys shows the user.
pub fn item_label(connection_id: &str, setting_name: &str, setting_key: &str) -> String {
    format!("Network secret for {connection_id}/{setting_name}/{setting_key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("hunter2hunter2".to_owned());
        let printed = format!("{secret:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("14 chars"), "{printed}");
    }

    /// A live round trip against the session's real keyring: store, find, delete, confirm gone.
    ///
    /// `#[ignore]`d because it needs an unlocked keyring on a session bus, which CI has not — run
    /// it by hand (`cargo test --lib secret_service -- --ignored --nocapture`) after touching any
    /// of the wire calls. It is the only check that the attribute shape and the `plain` session
    /// are right; nothing else here talks to the service.
    #[test]
    #[ignore = "needs a live, unlocked session keyring"]
    fn a_stored_secret_comes_back() {
        let uuid = format!("synoik-test-{}", std::process::id());
        let rt = async_io::block_on(async {
            let conn = zbus::Connection::session().await?;
            let session = SecretSession::open(&conn).await?;

            let mut attrs = HashMap::new();
            attrs.insert(ATTR_UUID, uuid.as_str());
            attrs.insert(ATTR_SETTING_NAME, "802-11-wireless-security");
            let mut store_attrs = attrs.clone();
            store_attrs.insert(ATTR_SETTING_KEY, "psk");

            session
                .store(
                    &item_label("Test", "802-11-wireless-security", "psk"),
                    &store_attrs,
                    &Secret::new("correct horse battery".to_owned()),
                )
                .await?;

            // Searched on the two-attribute prefix, exactly as GetSecrets does.
            let found = session.search(&attrs).await?;
            session.delete_matching(&attrs).await?;
            let after = session.search(&attrs).await?;
            Ok::<_, zbus::Error>((found, after))
        });

        let (found, after) = match rt {
            Ok(pair) => pair,
            Err(zbus::Error::Failure(msg)) if msg.contains("locked") => {
                // Same convention as the Vulkan tests without a device: a precondition this host
                // does not meet is a skip, not a failure. Unlock the login keyring and re-run.
                eprintln!("skipping: {msg} — unlock the login keyring and re-run");
                return;
            }
            Err(err) => panic!("keyring round trip: {err}"),
        };
        assert_eq!(
            found.get("psk").map(Secret::expose),
            Some("correct horse battery")
        );
        assert!(after.is_empty(), "the item survived deletion");
    }

    #[test]
    fn the_label_matches_upstreams() {
        assert_eq!(
            item_label("Café", "802-11-wireless-security", "psk"),
            "Network secret for Café/802-11-wireless-security/psk"
        );
    }
}
