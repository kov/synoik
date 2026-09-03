// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The session's NetworkManager secret agent.
//!
//! GNOME registers one per session (`js/ui/components/networkAgent.js`, backed by
//! `src/shell-network-agent.c`) and answers NetworkManager when a connection needs a password.
//! Without one, a secret request that arrives outside GNOME Settings — a VPN coming up, an 802.1X
//! re-auth, anything activated from the shell's own network menu — has nobody to ask and the
//! connection simply fails. (Settings itself is not affected: it registers an agent of its own
//! through `libnma`, which is why plain Wi-Fi joins work in a session with no shell agent at all.)
//!
//! # The three halves
//!
//! **The bus half.** We export `org.freedesktop.NetworkManager.SecretAgent` at NM's fixed path and
//! call `RegisterWithCapabilities`. NM then calls `GetSecrets` on us, and — as with polkit —
//! *the call does not return until the user is done*: its reply is the answer.
//!
//! **The store half.** Agent-owned secrets live in the keyring, not in NM
//! ([`crate::dbus::secret_service`]). A request is answered from there without a dialog whenever
//! it can be, which is what makes a saved network reconnect silently.
//!
//! **The asking half.** What to ask is [`crate::network_secret`]; the dialog is
//! [`crate::ui::network_secret_dialog`]. This module only carries text between them and NM.
//!
//! # Not here
//!
//! VPN secrets. Upstream hands every VPN request to the plugin's own auth binary over a spawned
//! process and a second line protocol (`VPNRequestHandler`, `networkAgent.js:419-671`); we answer
//! `NoSecrets` instead, so NM fails the connection promptly rather than waiting on a dialog that
//! is never coming. Registering with `capabilities = 0` rather than `VPN_HINTS` says so on the
//! wire.

use std::collections::HashMap;

use futures_util::StreamExt as _;
use zbus::zvariant::{OwnedValue, Value};

use super::secret_service::{
    self, Secret, SecretSession, ATTR_SETTING_KEY, ATTR_SETTING_NAME, ATTR_UUID,
};
use crate::network_secret::{self, ConnectionInfo, SecretContent, WepKeyType};

/// NM's fixed object path for a secret agent (`NM_DBUS_PATH_SECRET_AGENT`). Not ours to choose:
/// NM looks the agent up on the caller's bus name at exactly this path.
const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const NM_BUS: &str = "org.freedesktop.NetworkManager";
const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const AGENT_MANAGER_IFACE: &str = "org.freedesktop.NetworkManager.AgentManager";

/// The identifier we register under. Upstream's own (`networkAgent.js:676`) — NM uses it only for
/// logging and to tell agents apart, but matching it keeps `nmcli agent` output familiar.
const AGENT_IDENTIFIER: &str = "org.gnome.Shell.NetworkAgent";

/// `NMSecretAgentGetSecretsFlags`.
mod flags {
    pub const ALLOW_INTERACTION: u32 = 0x1;
    pub const REQUEST_NEW: u32 = 0x2;
    pub const USER_REQUESTED: u32 = 0x4;
    pub const WPS_PBC_ACTIVE: u32 = 0x8;
}

/// `NMSettingSecretFlags`.
mod secret_flags {
    /// The agent stores it — the only kind we save (`shell-network-agent.c:685-687`).
    pub const AGENT_OWNED: u32 = 0x1;
    /// Never stored; ask every time (`has_always_ask`).
    pub const NOT_SAVED: u32 = 0x2;
}

/// A connection as NM sends it: setting name → key → value.
type NmConnection = HashMap<String, HashMap<String, OwnedValue>>;

/// What the agent tells the compositor.
#[derive(Debug)]
pub enum NetworkAgentToSynoik {
    /// NetworkManager wants secrets we do not have. Ask the user.
    Begin(Box<SecretRequest>),
    /// NM withdrew the request — the connection went away, or another agent answered. Take the
    /// dialog down without answering.
    Cancel { request_id: String },
}

/// One outstanding ask, resolved off the compositor thread.
#[derive(Debug)]
pub struct SecretRequest {
    /// `{connection path}/{setting name}` — NM's own identity for the request, and ours.
    pub request_id: String,
    /// What to draw.
    pub content: SecretContent,
    /// The request came from a deliberate user action (they clicked a network), so the dialog can
    /// go straight up. When this is false NM asked on its own — a laptop waking onto a network
    /// that wants re-auth — and GNOME raises a *notification* first, letting the user choose the
    /// moment (`networkAgent.js:797-801`). The choice is the compositor's: it owns notifications.
    pub user_requested: bool,
}

/// What the compositor tells the agent.
pub enum NetworkAgentRequest {
    /// The user filled the dialog in. **Carries secrets** — see the `Debug` impl.
    Respond {
        request_id: String,
        values: HashMap<String, String>,
    },
    /// The user cancelled, or the notification was dismissed unanswered.
    Dismiss { request_id: String },
}

/// Hand-written so a stray `{:?}` cannot put a Wi-Fi password in the journal. A derived one would
/// print every value, and every type embedding this would inherit that.
impl std::fmt::Debug for NetworkAgentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Respond { request_id, values } => f
                .debug_struct("Respond")
                .field("request_id", request_id)
                .field("values", &format_args!("<{} redacted>", values.len()))
                .finish(),
            Self::Dismiss { request_id } => f
                .debug_struct("Dismiss")
                .field("request_id", request_id)
                .finish(),
        }
    }
}

/// The errors NM understands from an agent (`nm-errors.h`, `NMSecretAgentError`). The name is what
/// NM matches on: `UserCanceled` is what tells it the user said no, as opposed to a failure, and
/// it is the difference between NM giving up and NM asking the next agent.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    UserCanceled(String),
    AgentCanceled(String),
    InternalError(String),
    NoSecrets(String),
}

/// The object NM calls.
struct SecretAgent {
    calls: async_channel::Sender<AgentCall>,
}

/// One call on its way to [`Agent`], with the channel its reply comes back on.
struct AgentCall {
    kind: CallKind,
    reply: async_channel::Sender<Result<NmConnection, AgentError>>,
}

/// Named for the operation, not for NM's method: the `…Secrets` suffix every method carries says
/// nothing here, and clippy is right that four of them read as noise.
enum CallKind {
    Get {
        connection: NmConnection,
        path: String,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    },
    Cancel {
        path: String,
        setting_name: String,
    },
    Save {
        connection: NmConnection,
    },
    Delete {
        connection: NmConnection,
    },
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl SecretAgent {
    /// Blocks until the user answers, the keyring supplies the secrets, or NM cancels.
    ///
    /// NM's own agent timeout (120 s) is the only clock; we do not add one, because a shorter one
    /// would cancel a dialog the user is still typing in.
    async fn get_secrets(
        &self,
        connection: NmConnection,
        connection_path: zbus::zvariant::OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    ) -> Result<NmConnection, AgentError> {
        self.dispatch(CallKind::Get {
            connection,
            path: connection_path.as_str().to_owned(),
            setting_name,
            hints,
            flags,
        })
        .await
    }

    async fn cancel_get_secrets(
        &self,
        connection_path: zbus::zvariant::OwnedObjectPath,
        setting_name: String,
    ) -> Result<(), AgentError> {
        self.dispatch(CallKind::Cancel {
            path: connection_path.as_str().to_owned(),
            setting_name,
        })
        .await
        .map(|_| ())
    }

    async fn save_secrets(
        &self,
        connection: NmConnection,
        _connection_path: zbus::zvariant::OwnedObjectPath,
    ) -> Result<(), AgentError> {
        self.dispatch(CallKind::Save { connection })
            .await
            .map(|_| ())
    }

    async fn delete_secrets(
        &self,
        connection: NmConnection,
        _connection_path: zbus::zvariant::OwnedObjectPath,
    ) -> Result<(), AgentError> {
        self.dispatch(CallKind::Delete { connection })
            .await
            .map(|_| ())
    }
}

impl SecretAgent {
    async fn dispatch(&self, kind: CallKind) -> Result<NmConnection, AgentError> {
        let (reply, answer) = async_channel::bounded(1);
        self.calls
            .send(AgentCall { kind, reply })
            .await
            .map_err(|_| AgentError::InternalError("the agent task is gone".to_owned()))?;
        answer
            .recv()
            .await
            .map_err(|_| AgentError::InternalError("the agent task dropped the call".to_owned()))?
    }
}

/// Start the agent. Returns the system-bus connection to keep alive and the sender the compositor
/// answers dialogs with.
pub fn start(
    to_niri: calloop::channel::Sender<NetworkAgentToSynoik>,
) -> anyhow::Result<(
    zbus::blocking::Connection,
    async_channel::Sender<NetworkAgentRequest>,
)> {
    let (to_agent, from_niri) = async_channel::unbounded();
    let (calls_tx, calls_rx) = async_channel::unbounded();

    let conn = zbus::blocking::connection::Builder::system()?
        .serve_at(AGENT_PATH, SecretAgent { calls: calls_tx })?
        .build()?;

    // Registering may fail simply because NetworkManager is not up yet; the name watch below
    // registers us the moment it appears, so a failure here is not fatal.
    if let Err(err) = register(conn.inner()).await_blocking() {
        debug!("could not register the network agent yet: {err:#}");
    }

    conn.inner()
        .executor()
        .spawn(watch_nm(conn.inner().clone()), "network-agent-register")
        .detach();
    conn.inner()
        .executor()
        .spawn(run(calls_rx, from_niri, to_niri), "network-agent")
        .detach();

    Ok((conn, to_agent))
}

/// A tiny shim so [`start`] can call the async registration once before spawning anything.
trait AwaitBlocking {
    type Output;
    fn await_blocking(self) -> Self::Output;
}

impl<F: std::future::Future> AwaitBlocking for F {
    type Output = F::Output;
    fn await_blocking(self) -> Self::Output {
        async_io::block_on(self)
    }
}

async fn register(conn: &zbus::Connection) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let manager = zbus::Proxy::new(conn, NM_BUS, AGENT_MANAGER_PATH, AGENT_MANAGER_IFACE).await?;
    // Capabilities 0 (`NM_SECRET_AGENT_CAPABILITY_NONE`), not `VPN_HINTS`: we ship no VPN plugin
    // UI, and claiming the capability would invite hints we cannot act on.
    manager
        .call::<_, _, ()>("RegisterWithCapabilities", &(AGENT_IDENTIFIER, 0u32))
        .await
        .context("RegisterWithCapabilities")?;
    debug!("registered as the session's NetworkManager secret agent");
    Ok(())
}

/// Re-register whenever NetworkManager appears.
///
/// NM forgets its agents when it restarts, and it is not unusual for it to start *after* the
/// compositor. An agent that registered once at startup and never again is an agent that silently
/// stops answering after the first `systemctl restart NetworkManager`.
async fn watch_nm(conn: zbus::Connection) {
    let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("network agent: cannot watch for NetworkManager: {err}");
            return;
        }
    };
    let Ok(mut changed) = dbus
        .receive_name_owner_changed_with_args(&[(0, NM_BUS)])
        .await
    else {
        warn!("network agent: cannot subscribe to NetworkManager's name");
        return;
    };

    while let Some(signal) = changed.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner().is_none() {
            continue;
        }
        if let Err(err) = register(&conn).await {
            warn!("network agent: re-registering with NetworkManager failed: {err:#}");
        }
    }
}

/// One ask the user has not answered yet.
struct Pending {
    reply: async_channel::Sender<Result<NmConnection, AgentError>>,
    setting_name: String,
    uuid: String,
    id: String,
    /// The `<key> → flags` map for the setting being asked about, so the response knows which
    /// answers may be written to the keyring.
    key_flags: HashMap<String, u32>,
    save: bool,
}

/// The agent's whole state: the outstanding asks, keyed by NM's request id.
async fn run(
    calls: async_channel::Receiver<AgentCall>,
    responses: async_channel::Receiver<NetworkAgentRequest>,
    to_niri: calloop::channel::Sender<NetworkAgentToSynoik>,
) {
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut keyring: Option<SecretSession> = None;

    enum Event {
        Call(AgentCall),
        Response(NetworkAgentRequest),
    }

    // Pinned because `async_channel::Receiver` is `!Unpin` — it parks an event listener in place.
    let mut events = std::pin::pin!(futures_util::stream::select(
        calls.map(Event::Call),
        responses.map(Event::Response),
    ));

    while let Some(event) = events.next().await {
        match event {
            Event::Call(call) => handle_call(call, &mut pending, &mut keyring, &to_niri).await,
            Event::Response(response) => {
                handle_response(response, &mut pending, &mut keyring).await
            }
        }
    }
}

async fn handle_call(
    call: AgentCall,
    pending: &mut HashMap<String, Pending>,
    keyring: &mut Option<SecretSession>,
    to_niri: &calloop::channel::Sender<NetworkAgentToSynoik>,
) {
    let AgentCall { kind, reply } = call;
    match kind {
        CallKind::Get {
            connection,
            path,
            setting_name,
            hints,
            flags,
        } => {
            get_secrets(
                connection,
                path,
                setting_name,
                hints,
                flags,
                reply,
                pending,
                keyring,
                to_niri,
            )
            .await;
        }
        CallKind::Cancel { path, setting_name } => {
            let request_id = format!("{path}/{setting_name}");
            cancel(&request_id, pending, to_niri);
            let _ = reply.send(Ok(NmConnection::new())).await;
        }
        CallKind::Save { connection } => {
            // NM asks us to store the secrets it already holds. Upstream deletes first, then
            // writes every agent-owned one (`shell-network-agent.c:792-808`).
            if let Some(session) = session(keyring).await {
                save_connection(session, &connection).await;
            }
            let _ = reply.send(Ok(NmConnection::new())).await;
        }
        CallKind::Delete { connection } => {
            if let Some(session) = session(keyring).await {
                let info = read_connection(&connection);
                let mut attrs = HashMap::new();
                attrs.insert(ATTR_UUID, info.uuid.as_str());
                if let Err(err) = session.delete_matching(&attrs).await {
                    warn!("network agent: could not delete stored secrets: {err}");
                }
            }
            let _ = reply.send(Ok(NmConnection::new())).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn get_secrets(
    connection: NmConnection,
    path: String,
    setting_name: String,
    hints: Vec<String>,
    flags: u32,
    reply: async_channel::Sender<Result<NmConnection, AgentError>>,
    pending: &mut HashMap<String, Pending>,
    keyring: &mut Option<SecretSession>,
    to_niri: &calloop::channel::Sender<NetworkAgentToSynoik>,
) {
    let request_id = format!("{path}/{setting_name}");

    // A second request for the same (connection, setting) supersedes the first, which is answered
    // `AgentCanceled` so NM is not left holding a call (`shell-network-agent.c:366-372`).
    cancel(&request_id, pending, to_niri);

    let info = read_connection(&connection);

    // VPN never reaches a dialog here — see the module docs.
    if setting_name == "vpn" {
        debug!("network agent: refusing a VPN secret request (no plugin UI)");
        let _ = reply
            .send(Err(AgentError::NoSecrets(
                "VPN secrets need the plugin's own auth dialog, which synoik does not run"
                    .to_owned(),
            )))
            .await;
        return;
    }

    let key_flags = secret_key_flags(&connection, &setting_name);
    let ask_outright = flags & flags::REQUEST_NEW != 0
        || (flags & flags::ALLOW_INTERACTION != 0 && is_always_ask(&connection));

    if !ask_outright {
        // The store answers first when it can, which is what makes a saved network reconnect with
        // no dialog at all (`shell-network-agent.c:396-406`).
        if let Some(session) = session(keyring).await {
            let mut attrs = HashMap::new();
            attrs.insert(ATTR_UUID, info.uuid.as_str());
            attrs.insert(ATTR_SETTING_NAME, setting_name.as_str());
            match session.search(&attrs).await {
                Ok(found) if !found.is_empty() => {
                    let _ = reply.send(Ok(wrap(&setting_name, found))).await;
                    return;
                }
                Ok(_) if flags & flags::ALLOW_INTERACTION == 0 => {
                    // Nothing stored and no leave to ask: answer with the empty setting, as
                    // upstream does, and let NM fail the activation rather than hang.
                    let _ = reply.send(Ok(wrap(&setting_name, HashMap::new()))).await;
                    return;
                }
                Ok(_) => (),
                Err(err) => {
                    let _ = reply
                        .send(Err(AgentError::InternalError(format!(
                            "reading secrets from the keyring: {err}"
                        ))))
                        .await;
                    return;
                }
            }
        }
    }

    let Some(content) = network_secret::content(
        &info,
        &setting_name,
        &hints,
        flags & flags::WPS_PBC_ACTIVE != 0,
    ) else {
        let _ = reply
            .send(Err(AgentError::NoSecrets(format!(
                "nothing to ask for a {} connection's {setting_name}",
                info.kind
            ))))
            .await;
        return;
    };

    pending.insert(
        request_id.clone(),
        Pending {
            reply,
            setting_name,
            uuid: info.uuid.clone(),
            id: info.id.clone(),
            key_flags,
            // Upstream saves whenever it was allowed to interact at all (`:492-494`).
            save: flags & (flags::ALLOW_INTERACTION | flags::REQUEST_NEW) != 0,
        },
    );

    let request = SecretRequest {
        request_id,
        content,
        user_requested: flags & flags::USER_REQUESTED != 0,
    };
    if to_niri
        .send(NetworkAgentToSynoik::Begin(Box::new(request)))
        .is_err()
    {
        warn!("network agent: the compositor is gone");
    }
}

async fn handle_response(
    response: NetworkAgentRequest,
    pending: &mut HashMap<String, Pending>,
    keyring: &mut Option<SecretSession>,
) {
    match response {
        NetworkAgentRequest::Dismiss { request_id } => {
            if let Some(entry) = pending.remove(&request_id) {
                let _ = entry
                    .reply
                    .send(Err(AgentError::UserCanceled(
                        "the network dialog was cancelled by the user".to_owned(),
                    )))
                    .await;
            }
        }
        NetworkAgentRequest::Respond { request_id, values } => {
            let Some(entry) = pending.remove(&request_id) else {
                return;
            };

            if entry.save {
                if let Some(session) = session(keyring).await {
                    for (key, value) in &values {
                        // Only agent-owned secrets are ours to keep. A system-owned one is NM's
                        // business and an always-ask one is nobody's
                        // (`shell-network-agent.c:685-687`).
                        if entry.key_flags.get(key).copied().unwrap_or(0)
                            != secret_flags::AGENT_OWNED
                        {
                            continue;
                        }
                        let mut attrs = HashMap::new();
                        attrs.insert(ATTR_UUID, entry.uuid.as_str());
                        attrs.insert(ATTR_SETTING_NAME, entry.setting_name.as_str());
                        attrs.insert(ATTR_SETTING_KEY, key.as_str());
                        let label = secret_service::item_label(&entry.id, &entry.setting_name, key);
                        // Replace, never accumulate: `CreateItem` is called with `replace`.
                        if let Err(err) = session
                            .store(&label, &attrs, &Secret::new(value.clone()))
                            .await
                        {
                            warn!("network agent: could not save a secret to the keyring: {err}");
                        }
                    }
                }
            }

            let secrets = values
                .into_iter()
                .map(|(k, v)| (k, Secret::new(v)))
                .collect();
            let _ = entry
                .reply
                .send(Ok(wrap(&entry.setting_name, secrets)))
                .await;
        }
    }
}

/// Answer an outstanding request with `AgentCanceled` and tell the compositor to drop its dialog.
fn cancel(
    request_id: &str,
    pending: &mut HashMap<String, Pending>,
    to_niri: &calloop::channel::Sender<NetworkAgentToSynoik>,
) {
    let Some(entry) = pending.remove(request_id) else {
        return;
    };
    // The reply channel is bounded at 1 and nothing else has used it, so this cannot block.
    let _ = entry.reply.try_send(Err(AgentError::AgentCanceled(
        "cancelled by NetworkManager".to_owned(),
    )));
    let _ = to_niri.send(NetworkAgentToSynoik::Cancel {
        request_id: request_id.to_owned(),
    });
}

/// Open the keyring session lazily, and only once. A session that cannot be opened is not fatal:
/// the agent still asks, it just cannot remember.
async fn session(keyring: &mut Option<SecretSession>) -> Option<&SecretSession> {
    if keyring.is_none() {
        match zbus::Connection::session().await {
            Ok(conn) => match SecretSession::open(&conn).await {
                Ok(session) => *keyring = Some(session),
                Err(err) => warn!("network agent: no keyring, secrets will not be saved: {err}"),
            },
            Err(err) => warn!("network agent: no session bus for the keyring: {err}"),
        }
    }
    keyring.as_ref()
}

/// Write every agent-owned secret in `connection` to the keyring, after clearing what is there.
async fn save_connection(session: &SecretSession, connection: &NmConnection) {
    let info = read_connection(connection);
    let mut all = HashMap::new();
    all.insert(ATTR_UUID, info.uuid.as_str());
    if let Err(err) = session.delete_matching(&all).await {
        debug!("network agent: could not clear stored secrets before saving: {err}");
    }

    for (setting_name, setting) in connection {
        let key_flags = secret_key_flags(connection, setting_name);
        for (key, value) in setting {
            if key_flags.get(key).copied().unwrap_or(0) != secret_flags::AGENT_OWNED {
                continue;
            }
            let Ok(value) = String::try_from(value.try_clone().unwrap_or(OwnedValue::from(0u32)))
            else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            let mut attrs = HashMap::new();
            attrs.insert(ATTR_UUID, info.uuid.as_str());
            attrs.insert(ATTR_SETTING_NAME, setting_name.as_str());
            attrs.insert(ATTR_SETTING_KEY, key.as_str());
            let label = secret_service::item_label(&info.id, setting_name, key);
            if let Err(err) = session.store(&label, &attrs, &Secret::new(value)).await {
                warn!("network agent: could not save a secret to the keyring: {err}");
            }
        }
    }
}

/// Wrap a setting's secrets in the `a{sa{sv}}` NM expects back: only the setting it asked about.
fn wrap(setting_name: &str, secrets: HashMap<String, Secret>) -> NmConnection {
    let setting = secrets
        .into_iter()
        .filter_map(|(key, secret)| {
            OwnedValue::try_from(Value::new(secret.expose()))
                .ok()
                .map(|value| (key, value))
        })
        .collect();
    HashMap::from([(setting_name.to_owned(), setting)])
}

/// `<key> → <key>-flags` for one setting: which of its secrets the agent may store.
fn secret_key_flags(connection: &NmConnection, setting_name: &str) -> HashMap<String, u32> {
    let Some(setting) = connection.get(setting_name) else {
        return HashMap::new();
    };
    setting
        .iter()
        .filter_map(|(key, value)| {
            let key = key.strip_suffix("-flags")?;
            Some((key.to_owned(), u32::try_from(value.try_clone().ok()?).ok()?))
        })
        .collect()
}

/// `is_connection_always_ask` (`shell-network-agent.c:202-243`): does any secret in the settings
/// relevant to this connection's type carry `NOT_SAVED`?
///
/// Scoped to the type setting plus the security ones upstream pairs with it, deliberately: a
/// blanket scan of every setting would let an unrelated `NOT_SAVED` flag force a dialog onto every
/// activation.
fn is_always_ask(connection: &NmConnection) -> bool {
    let kind = connection
        .get("connection")
        .and_then(|s| s.get("type"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default();

    let mut relevant = vec![kind.as_str()];
    match kind.as_str() {
        "802-11-wireless" => relevant.extend(["802-11-wireless-security", "802-1x"]),
        "802-3-ethernet" => relevant.extend(["pppoe", "802-1x"]),
        _ => (),
    }

    relevant.into_iter().any(|name| {
        secret_key_flags(connection, name)
            .values()
            .any(|flags| flags & secret_flags::NOT_SAVED != 0)
    })
}

/// Flatten NM's connection dict into the non-secret facts the dialog's shape depends on.
fn read_connection(connection: &NmConnection) -> ConnectionInfo {
    let string = |setting: &str, key: &str| -> Option<String> {
        let value = connection.get(setting)?.get(key)?.try_clone().ok()?;
        String::try_from(value).ok()
    };
    let number = |setting: &str, key: &str| -> Option<u32> {
        let value = connection.get(setting)?.get(key)?.try_clone().ok()?;
        u32::try_from(value).ok()
    };

    let ssid = connection
        .get("802-11-wireless")
        .and_then(|s| s.get("ssid"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<u8>::try_from(v).ok())
        // NM carries an SSID as raw bytes, which are not required to be UTF-8. `utils_ssid_to_utf8`
        // makes them printable rather than refusing; lossy conversion is the same bargain.
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());

    let mut identities = HashMap::new();
    for (setting, key) in [
        ("802-1x", "identity"),
        ("pppoe", "username"),
        ("pppoe", "service"),
    ] {
        if let Some(value) = string(setting, key) {
            identities.insert(key.to_owned(), value);
        }
    }

    let eap = connection
        .get("802-1x")
        .and_then(|s| s.get("eap"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .and_then(|methods| methods.into_iter().next());

    ConnectionInfo {
        kind: string("connection", "type").unwrap_or_default(),
        id: string("connection", "id").unwrap_or_default(),
        uuid: string("connection", "uuid").unwrap_or_default(),
        ssid,
        key_mgmt: string("802-11-wireless-security", "key-mgmt"),
        auth_alg: string("802-11-wireless-security", "auth-alg"),
        wep_tx_keyidx: number("802-11-wireless-security", "wep-tx-keyidx").unwrap_or(0),
        wep_key_type: WepKeyType::from_nm(
            number("802-11-wireless-security", "wep-key-type").unwrap_or(0),
        ),
        eap,
        identities,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.try_clone().unwrap()))
            .collect()
    }

    fn text(s: &str) -> OwnedValue {
        OwnedValue::try_from(Value::new(s)).unwrap()
    }

    fn wifi_connection(psk_flags: u32) -> NmConnection {
        HashMap::from([
            (
                "connection".to_owned(),
                setting(&[
                    ("type", text("802-11-wireless")),
                    ("id", text("Café")),
                    ("uuid", text("uuid-1")),
                ]),
            ),
            (
                "802-11-wireless".to_owned(),
                setting(&[(
                    "ssid",
                    OwnedValue::try_from(Value::new(vec![0x43u8, 0x61, 0x66, 0xc3, 0xa9])).unwrap(),
                )]),
            ),
            (
                "802-11-wireless-security".to_owned(),
                setting(&[
                    ("key-mgmt", text("wpa-psk")),
                    (
                        "psk-flags",
                        OwnedValue::try_from(Value::new(psk_flags)).unwrap(),
                    ),
                ]),
            ),
        ])
    }

    #[test]
    fn a_connection_flattens_to_what_the_dialog_needs() {
        let info = read_connection(&wifi_connection(secret_flags::AGENT_OWNED));
        assert_eq!(info.kind, "802-11-wireless");
        assert_eq!(info.id, "Café");
        assert_eq!(info.uuid, "uuid-1");
        assert_eq!(info.ssid.as_deref(), Some("Café"));
        assert_eq!(info.key_mgmt.as_deref(), Some("wpa-psk"));
    }

    #[test]
    fn an_ssid_that_is_not_utf8_still_reads() {
        let mut connection = wifi_connection(0);
        connection.get_mut("802-11-wireless").unwrap().insert(
            "ssid".to_owned(),
            OwnedValue::try_from(Value::new(vec![0xffu8, 0xfe])).unwrap(),
        );
        // Lossy, not absent: an un-decodable SSID must still produce a dialog the user can answer.
        assert!(read_connection(&connection).ssid.is_some());
    }

    #[test]
    fn only_agent_owned_secrets_are_ours_to_store() {
        let agent = secret_key_flags(
            &wifi_connection(secret_flags::AGENT_OWNED),
            "802-11-wireless-security",
        );
        assert_eq!(agent.get("psk"), Some(&secret_flags::AGENT_OWNED));

        // System-owned (0) is NetworkManager's own store; we must not copy it into the keyring.
        let system = secret_key_flags(&wifi_connection(0), "802-11-wireless-security");
        assert_eq!(system.get("psk"), Some(&0));
    }

    #[test]
    fn a_not_saved_secret_forces_a_dialog() {
        assert!(is_always_ask(&wifi_connection(secret_flags::NOT_SAVED)));
        assert!(!is_always_ask(&wifi_connection(secret_flags::AGENT_OWNED)));
    }

    #[test]
    fn always_ask_ignores_settings_the_type_does_not_use() {
        let mut connection = wifi_connection(secret_flags::AGENT_OWNED);
        // A stale pppoe setting on a wireless connection must not force every activation to prompt.
        connection.insert(
            "pppoe".to_owned(),
            setting(&[(
                "password-flags",
                OwnedValue::try_from(Value::new(secret_flags::NOT_SAVED)).unwrap(),
            )]),
        );
        assert!(!is_always_ask(&connection));
    }

    #[test]
    fn the_reply_carries_only_the_setting_that_was_asked_about() {
        let wrapped = wrap(
            "802-11-wireless-security",
            HashMap::from([("psk".to_owned(), Secret::new("hunter2hunter2".to_owned()))]),
        );
        assert_eq!(wrapped.len(), 1);
        let setting = &wrapped["802-11-wireless-security"];
        assert_eq!(
            String::try_from(setting["psk"].try_clone().unwrap()).unwrap(),
            "hunter2hunter2"
        );
    }

    #[test]
    fn a_response_never_prints_its_values() {
        let response = NetworkAgentRequest::Respond {
            request_id: "/org/freedesktop/NetworkManager/Settings/1/802-11-wireless-security"
                .to_owned(),
            values: HashMap::from([("psk".to_owned(), "hunter2hunter2".to_owned())]),
        };
        let printed = format!("{response:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("<1 redacted>"), "{printed}");
    }
}
