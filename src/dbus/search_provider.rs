// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The client of apps' `org.gnome.Shell.SearchProvider2` services — the bus half of
//! `js/ui/remoteSearch.js` and of the per-provider steps of `js/ui/search.js`.
//!
//! It owns its session connection and every byte a provider sends: result ids are bounded,
//! metas are validated and parsed into [`ResultMeta`] here, and only those plain values cross to
//! the compositor (see [`crate::search_providers`] for the seam).
//!
//! **Each provider is searched in its own task.** One that hangs holds its own task until the
//! call times out, never the others, and never the compositor. A zbus call cannot be cancelled,
//! so a newer search cancels an older one by making it stale: every reply carries the
//! generation it answers, and a task that finishes for an old generation drops what it found.
//!
//! **Sub-search refines the provider's own last completed search.** gnome-shell decides it once,
//! from the previous *typed* string, and refines whatever results the previous search left —
//! which, when a search was overtaken by typing, are a different string's results, or the empty
//! list a cancelled call returns. Here a provider refines only results it actually produced for
//! a prefix of the new string, and otherwise searches afresh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::glib;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::zvariant::{OwnedValue, Value};

use crate::search_providers::{
    filter_results, ProviderEndpoint, ResultIcon, ResultMeta, SearchToSynoik, SectionResults,
    SynoikToSearch, MAX_LIST_ROWS,
};

const IFACE_1: &str = "org.gnome.Shell.SearchProvider";
const IFACE_2: &str = "org.gnome.Shell.SearchProvider2";

/// GDBus's default call timeout, which is what gnome-shell's proxies wait. Enforced around each
/// call here: zbus applies a connection's `method_timeout` to `Connection::call_method` but not
/// to `Proxy::call_with_flags`, which is the only call that carries the no-auto-start flag.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// Bounds on what a provider can make us hold. A real provider returns tens of results; these
/// only stop a broken or hostile one from growing the compositor's memory.
const MAX_RESULT_IDS: usize = 1000;
const MAX_ID_BYTES: usize = 4096;
const MAX_TEXT_CHARS: usize = 1024;
const MAX_ICON_PX: i32 = 512;

/// Where replies go: the compositor's channel in a session, a test's in a test.
type Reply = Arc<dyn Fn(SearchToSynoik) + Send + Sync>;

/// Connect to the session bus and serve `from_niri` until it closes.
pub fn start(
    to_niri: calloop::channel::Sender<SearchToSynoik>,
    from_niri: async_channel::Receiver<SynoikToSearch>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::connection::Builder::session()?.build()?;
    let reply: Reply = Arc::new(move |msg| {
        let _ = to_niri.send(msg);
    });
    let bus = Bus {
        conn: conn.inner().clone(),
        timeout: CALL_TIMEOUT,
    };
    let client = Client::new(bus, reply);
    conn.inner()
        .executor()
        .spawn(client.serve(from_niri), "serve search providers")
        .detach();
    Ok(conn)
}

#[derive(Default)]
struct Shared {
    /// The search the compositor last asked for; a task answering an older one stays quiet.
    generation: u64,
    /// Per provider: the terms of its last completed search and the ids it returned — what a
    /// longer string refines.
    previous: HashMap<String, (String, Vec<String>)>,
    /// Per provider: the metas already fetched, by the id they were requested for. gnome-shell
    /// keeps a section's result actors the same way (`_resultDisplays`), so a refined search
    /// asks only for the rows it has not shown yet.
    metas: HashMap<String, HashMap<String, ResultMeta>>,
}

/// The connection, and how long a call on it may take.
#[derive(Clone)]
struct Bus {
    conn: zbus::Connection,
    timeout: Duration,
}

struct Client {
    bus: Bus,
    reply: Reply,
    providers: Vec<ProviderEndpoint>,
    shared: Arc<Mutex<Shared>>,
}

impl Client {
    fn new(bus: Bus, reply: Reply) -> Self {
        Self {
            bus,
            reply,
            providers: Vec::new(),
            shared: Arc::default(),
        }
    }

    async fn serve(mut self, from_niri: async_channel::Receiver<SynoikToSearch>) {
        while let Ok(msg) = from_niri.recv().await {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: SynoikToSearch) {
        match msg {
            SynoikToSearch::SetProviders(providers) => {
                self.providers = providers;
                let mut shared = self.shared.lock().unwrap();
                shared.previous.clear();
                shared.metas.clear();
            }
            SynoikToSearch::Search { generation, terms } => {
                self.shared.lock().unwrap().generation = generation;
                for endpoint in &self.providers {
                    let task = search_provider(
                        self.bus.clone(),
                        endpoint.clone(),
                        generation,
                        terms.clone(),
                        self.shared.clone(),
                        self.reply.clone(),
                    );
                    self.spawn(task, "search a provider");
                }
            }
            SynoikToSearch::Reset => {
                let mut shared = self.shared.lock().unwrap();
                shared.previous.clear();
                shared.metas.clear();
            }
            SynoikToSearch::ActivateResult {
                provider,
                id,
                terms,
                timestamp,
            } => {
                let Some(endpoint) = self.endpoint(&provider) else {
                    return;
                };
                let bus = self.bus.clone();
                self.spawn(
                    async move {
                        let result = if endpoint.version2 {
                            call::<()>(&bus, &endpoint, "ActivateResult", &(id, terms, timestamp))
                                .await
                        } else {
                            call::<()>(&bus, &endpoint, "ActivateResult", &(id,)).await
                        };
                        if let Err(err) = result {
                            warn!("search provider {}: ActivateResult: {err}", endpoint.id);
                        }
                    },
                    "activate a search result",
                );
            }
            SynoikToSearch::LaunchSearch {
                provider,
                terms,
                timestamp,
            } => {
                // A version 1 provider has no `LaunchSearch`; the compositor launches its app
                // instead and never sends this.
                let Some(endpoint) = self.endpoint(&provider).filter(|e| e.version2) else {
                    return;
                };
                let bus = self.bus.clone();
                self.spawn(
                    async move {
                        if let Err(err) =
                            call::<()>(&bus, &endpoint, "LaunchSearch", &(terms, timestamp)).await
                        {
                            warn!("search provider {}: LaunchSearch: {err}", endpoint.id);
                        }
                    },
                    "launch a provider's search",
                );
            }
        }
    }

    fn endpoint(&self, id: &str) -> Option<ProviderEndpoint> {
        self.providers.iter().find(|e| e.id == id).cloned()
    }

    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static, name: &str) {
        self.bus.conn.executor().spawn(task, name).detach();
    }
}

/// Call `method` on the provider, honouring `AutoStart=false` the way a gnome-shell proxy built
/// with `DO_NOT_AUTO_START` does: the bus will not start a provider for us.
async fn call<R>(
    bus: &Bus,
    endpoint: &ProviderEndpoint,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> zbus::Result<R>
where
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let iface = if endpoint.version2 { IFACE_2 } else { IFACE_1 };
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&bus.conn)
        .destination(endpoint.bus_name.as_str())?
        .path(endpoint.object_path.as_str())?
        .interface(iface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let flags = if endpoint.auto_start {
        Default::default()
    } else {
        MethodFlags::NoAutoStart.into()
    };
    let reply = std::pin::pin!(proxy.call_with_flags(method, flags, body));
    let timeout = std::pin::pin!(async_io::Timer::after(bus.timeout));
    match futures_util::future::select(reply, timeout).await {
        futures_util::future::Either::Left((reply, _)) => {
            reply?.ok_or_else(|| zbus::Error::Failure("no reply".to_owned()))
        }
        futures_util::future::Either::Right(_) => Err(zbus::Error::InputOutput(
            std::io::Error::from(std::io::ErrorKind::TimedOut).into(),
        )),
    }
}

/// One provider's part of a search (`_doProviderSearch`, then the section's `updateSearch`).
async fn search_provider(
    bus: Bus,
    endpoint: ProviderEndpoint,
    generation: u64,
    terms: Vec<String>,
    shared: Arc<Mutex<Shared>>,
    reply: Reply,
) {
    let current = |shared: &Shared| shared.generation == generation;
    let joined = terms.join(" ");
    let previous = shared
        .lock()
        .unwrap()
        .previous
        .get(&endpoint.id)
        .filter(|(prev, _)| joined.starts_with(prev.as_str()))
        .map(|(_, ids)| ids.clone());
    let found = match previous {
        Some(previous) => {
            call::<Vec<String>>(
                &bus,
                &endpoint,
                "GetSubsearchResultSet",
                &(previous, &terms),
            )
            .await
        }
        None => call::<Vec<String>>(&bus, &endpoint, "GetInitialResultSet", &(&terms,)).await,
    };
    let ids = match found {
        Ok(ids) => bound_ids(ids),
        Err(err) => {
            warn!("search provider {}: {err}", endpoint.id);
            Vec::new()
        }
    };

    {
        let mut shared = shared.lock().unwrap();
        if !current(&shared) {
            return;
        }
        shared
            .previous
            .insert(endpoint.id.clone(), (joined, ids.clone()));
    }

    let results = if ids.is_empty() {
        None
    } else {
        section(&bus, &endpoint, &ids, &shared).await
    };
    if !current(&shared.lock().unwrap()) {
        return;
    }
    reply(SearchToSynoik::Results {
        generation,
        provider: endpoint.id,
        results,
    });
}

/// The rows a provider's section shows, fetching the metas it does not have yet
/// (`_ensureResultActors`, `search.js:226-257`). `None` if the provider answered with metas
/// gnome-shell would reject.
async fn section(
    bus: &Bus,
    endpoint: &ProviderEndpoint,
    ids: &[String],
    shared: &Mutex<Shared>,
) -> Option<SectionResults> {
    let shown = filter_results(ids, MAX_LIST_ROWS);
    let more = if endpoint.version2 {
        ids.len() - shown.len()
    } else {
        0
    };
    let needed: Vec<String> = {
        let shared = shared.lock().unwrap();
        let known = shared.metas.get(&endpoint.id);
        shown
            .iter()
            .filter(|id| known.is_none_or(|known| !known.contains_key(*id)))
            .cloned()
            .collect()
    };
    if !needed.is_empty() {
        let metas = match call::<Vec<HashMap<String, OwnedValue>>>(
            bus,
            endpoint,
            "GetResultMetas",
            &(&needed,),
        )
        .await
        {
            Ok(metas) => metas,
            Err(err) => {
                warn!("search provider {}: GetResultMetas: {err}", endpoint.id);
                return None;
            }
        };
        if metas.len() != needed.len() {
            warn!(
                "search provider {} returned {} result metas for {} results",
                endpoint.id,
                metas.len(),
                needed.len()
            );
            return None;
        }
        let parsed: Option<Vec<ResultMeta>> = metas.iter().map(parse_meta).collect();
        let Some(parsed) = parsed else {
            warn!(
                "search provider {} returned an invalid result meta",
                endpoint.id
            );
            return None;
        };
        let mut shared = shared.lock().unwrap();
        let known = shared.metas.entry(endpoint.id.clone()).or_default();
        known.extend(needed.into_iter().zip(parsed));
    }
    let shared = shared.lock().unwrap();
    let known = shared.metas.get(&endpoint.id)?;
    let rows = shown
        .iter()
        .map(|id| known.get(id).cloned())
        .collect::<Option<Vec<_>>>()?;
    Some(SectionResults { rows, more })
}

fn bound_ids(mut ids: Vec<String>) -> Vec<String> {
    ids.retain(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES);
    ids.truncate(MAX_RESULT_IDS);
    ids
}

/// A value inside a variant, however many times it was wrapped.
fn unwrap_value<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap_value(inner),
        other => other,
    }
}

fn text(value: &Value<'_>) -> Option<String> {
    match unwrap_value(value) {
        Value::Str(s) => Some(s.as_str().chars().take(MAX_TEXT_CHARS).collect()),
        _ => None,
    }
}

/// One `GetResultMetas` dictionary. gnome-shell rejects a meta without an `id` or a `name`
/// (`search.js:254-255`); everything else is optional.
fn parse_meta(meta: &HashMap<String, OwnedValue>) -> Option<ResultMeta> {
    let get = |key: &str| meta.get(key).and_then(|v| text(v));
    let id = get("id").filter(|s| !s.is_empty())?;
    let name = get("name").filter(|s| !s.is_empty())?;
    let description = get("description")
        .and_then(|d| d.lines().next().map(str::to_owned))
        .filter(|d| !d.is_empty());
    // `createIcon`'s precedence: the serialized icon, then the icon string, then raw pixels.
    let icon = meta
        .get("icon")
        .and_then(|v| deserialize_icon(v))
        .or_else(|| get("gicon").and_then(|s| icon_for_string(&s)))
        .or_else(|| meta.get("icon-data").and_then(|v| icon_data(v)));
    Some(ResultMeta {
        id,
        name,
        description,
        icon,
        clipboard_text: get("clipboardText"),
    })
}

/// `g_icon_deserialize` for the icons a provider sends: `('themed', <as>)`, `('file', <s>)`, or
/// a plain string. Bytes and emblems are not drawn.
fn deserialize_icon(value: &Value<'_>) -> Option<ResultIcon> {
    match unwrap_value(value) {
        Value::Str(s) => icon_for_string(s.as_str()),
        Value::Structure(structure) => {
            let [kind, payload] = structure.fields() else {
                return None;
            };
            let Value::Str(kind) = unwrap_value(kind) else {
                return None;
            };
            match (kind.as_str(), unwrap_value(payload)) {
                ("themed", Value::Array(names)) => {
                    let names: Vec<String> = names
                        .iter()
                        .filter_map(|n| text(n))
                        .filter(|n| !n.is_empty())
                        .take(16)
                        .collect();
                    (!names.is_empty()).then_some(ResultIcon::Themed(names))
                }
                ("file", Value::Str(uri)) => file_icon(uri.as_str()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `g_icon_new_for_string`: an absolute path or a `file:` URI is a file, a
/// `. GThemedIcon name…` serialization lists theme names, anything else is one theme name.
fn icon_for_string(s: &str) -> Option<ResultIcon> {
    if s.starts_with('/') || s.starts_with("file:") {
        return file_icon(s);
    }
    if let Some(names) = s.strip_prefix(". GThemedIcon ") {
        let names: Vec<String> = names
            .split(' ')
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect();
        return (!names.is_empty()).then_some(ResultIcon::Themed(names));
    }
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    Some(ResultIcon::Themed(vec![s.to_owned()]))
}

fn file_icon(s: &str) -> Option<ResultIcon> {
    let path = match s.strip_prefix("file://") {
        Some(_) => glib::filename_from_uri(s).ok()?.0,
        None => std::path::PathBuf::from(s),
    };
    path.is_absolute().then_some(ResultIcon::File(path))
}

/// `icon-data`, `(iiibiiay)`: width, height, rowstride, has-alpha, bits per sample, channels,
/// pixels — a GdkPixbuf's fields. Converted to RGBA rows, refusing anything that does not add up.
fn icon_data(value: &Value<'_>) -> Option<ResultIcon> {
    let Value::Structure(structure) = unwrap_value(value) else {
        return None;
    };
    let [w, h, stride, alpha, bits, channels, data] = structure.fields() else {
        return None;
    };
    let int = |v: &Value<'_>| match unwrap_value(v) {
        Value::I32(i) => Some(*i),
        _ => None,
    };
    let (w, h, stride, bits, channels) =
        (int(w)?, int(h)?, int(stride)?, int(bits)?, int(channels)?);
    let Value::Bool(alpha) = unwrap_value(alpha) else {
        return None;
    };
    let Value::Array(data) = unwrap_value(data) else {
        return None;
    };
    if !(1..=MAX_ICON_PX).contains(&w) || !(1..=MAX_ICON_PX).contains(&h) || bits != 8 {
        return None;
    }
    if channels != if *alpha { 4 } else { 3 } || stride < w * channels {
        return None;
    }
    let bytes: Vec<u8> = data
        .iter()
        .map(|b| match b {
            Value::U8(b) => Some(*b),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let (w, h, stride, channels) = (w as usize, h as usize, stride as usize, channels as usize);
    if bytes.len() < stride * (h - 1) + w * channels {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for px in bytes[row * stride..][..w * channels].chunks_exact(channels) {
            rgba.extend_from_slice(&px[..3]);
            rgba.push(if channels == 4 { px[3] } else { 0xff });
        }
    }
    Some(ResultIcon::Pixels {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Instant;

    use zbus::zvariant::{Array, StructureBuilder};

    use super::*;

    /// An app's provider: results are the items whose name holds every term.
    struct FakeProvider {
        items: Vec<(String, String)>,
        /// Return one meta fewer than asked for.
        short_metas: bool,
        /// When set, each search waits for a message here before answering.
        gate: Option<async_channel::Receiver<()>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeProvider {
        fn matching(&self, terms: &[String], within: Option<&[String]>) -> Vec<String> {
            self.items
                .iter()
                .filter(|(id, _)| within.is_none_or(|w| w.contains(id)))
                .filter(|(_, name)| {
                    terms
                        .iter()
                        .all(|t| name.to_lowercase().contains(&t.to_lowercase()))
                })
                .map(|(id, _)| id.clone())
                .collect()
        }

        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[zbus::interface(name = "org.gnome.Shell.SearchProvider2")]
    impl FakeProvider {
        async fn get_initial_result_set(&self, terms: Vec<String>) -> Vec<String> {
            self.log(format!("initial {}", terms.join(" ")));
            if let Some(gate) = &self.gate {
                gate.recv().await.unwrap();
            }
            self.matching(&terms, None)
        }

        fn get_subsearch_result_set(
            &self,
            previous: Vec<String>,
            terms: Vec<String>,
        ) -> Vec<String> {
            self.log(format!("sub {} of {}", terms.join(" "), previous.len()));
            self.matching(&terms, Some(&previous))
        }

        fn get_result_metas(&self, ids: Vec<String>) -> Vec<HashMap<String, OwnedValue>> {
            self.log(format!("metas {}", ids.join(",")));
            let mut metas: Vec<HashMap<String, OwnedValue>> = ids
                .iter()
                .map(|id| {
                    let name = &self.items.iter().find(|(i, _)| i == id).unwrap().1;
                    HashMap::from([
                        (
                            "id".to_owned(),
                            OwnedValue::from(zbus::zvariant::Str::from(id.clone())),
                        ),
                        (
                            "name".to_owned(),
                            OwnedValue::from(zbus::zvariant::Str::from(name.clone())),
                        ),
                        (
                            "description".to_owned(),
                            OwnedValue::from(zbus::zvariant::Str::from(format!(
                                "{name}\nsecond line"
                            ))),
                        ),
                    ])
                })
                .collect();
            if self.short_metas {
                metas.pop();
            }
            metas
        }

        fn activate_result(&self, id: String, terms: Vec<String>, timestamp: u32) {
            self.log(format!("activate {id} {} {timestamp}", terms.join(" ")));
        }

        fn launch_search(&self, terms: Vec<String>, timestamp: u32) {
            self.log(format!("launch {} {timestamp}", terms.join(" ")));
        }
    }

    struct Harness {
        client: Client,
        replies: mpsc::Receiver<SearchToSynoik>,
        calls: Vec<Arc<Mutex<Vec<String>>>>,
        _server: zbus::blocking::Connection,
    }

    /// One server connection hosting a provider per entry of `providers`, each at its own path.
    fn harness(providers: Vec<(Vec<(&str, &str)>, bool)>) -> Harness {
        gated_harness(providers, None, Duration::from_secs(5))
    }

    fn gated_harness(
        providers: Vec<(Vec<(&str, &str)>, bool)>,
        gate: Option<async_channel::Receiver<()>>,
        timeout: Duration,
    ) -> Harness {
        let calls: Vec<_> = providers.iter().map(|_| Arc::default()).collect();
        let fakes: Vec<_> = providers
            .into_iter()
            .zip(&calls)
            .map(|((items, short_metas), calls)| FakeProvider {
                items: items
                    .into_iter()
                    .map(|(id, name)| (id.to_owned(), name.to_owned()))
                    .collect(),
                short_metas,
                gate: gate.clone(),
                calls: Arc::clone(calls),
            })
            .collect();
        let n = fakes.len();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut builder = zbus::blocking::connection::Builder::unix_stream(theirs)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p();
            for (i, fake) in fakes.into_iter().enumerate() {
                builder = builder.serve_at(format!("/p{i}"), fake).unwrap();
            }
            builder.build().unwrap()
        });
        let conn = zbus::blocking::connection::Builder::unix_stream(ours)
            .p2p()
            .build()
            .unwrap();
        let (tx, replies) = mpsc::channel();
        let tx = Mutex::new(tx);
        let reply: Reply = Arc::new(move |msg| tx.lock().unwrap().send(msg).unwrap());
        let bus = Bus {
            conn: conn.inner().clone(),
            timeout,
        };
        let mut client = Client::new(bus, reply);
        client.handle(SynoikToSearch::SetProviders(
            (0..n)
                .map(|i| ProviderEndpoint {
                    id: format!("p{i}.desktop"),
                    bus_name: "org.example.Provider".to_owned(),
                    object_path: format!("/p{i}"),
                    version2: true,
                    auto_start: true,
                })
                .collect(),
        ));
        Harness {
            client,
            replies,
            calls,
            _server: server.join().unwrap(),
        }
    }

    impl Harness {
        fn search(&mut self, generation: u64, terms: &[&str]) -> Vec<SearchToSynoik> {
            self.client.handle(SynoikToSearch::Search {
                generation,
                terms: terms.iter().map(|t| t.to_string()).collect(),
            });
            self.wait(self.calls.len())
        }

        /// The calls are answered on the connection's executor, so wait in wall-clock time.
        fn wait(&self, n: usize) -> Vec<SearchToSynoik> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut got = Vec::new();
            while got.len() < n {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.replies.recv_timeout(left) {
                    Ok(msg) => got.push(msg),
                    Err(_) => break,
                }
            }
            got.sort_by_key(|SearchToSynoik::Results { provider, .. }| provider.clone());
            got
        }

        fn calls(&self, i: usize) -> Vec<String> {
            self.calls[i].lock().unwrap().clone()
        }
    }

    fn rows(msg: &SearchToSynoik) -> Option<(Vec<&str>, usize)> {
        let SearchToSynoik::Results { results, .. } = msg;
        results
            .as_ref()
            .map(|r| (r.rows.iter().map(|m| m.name.as_str()).collect(), r.more))
    }

    #[test]
    fn a_longer_string_refines_the_last_completed_results() {
        let mut h = harness(vec![(
            vec![("1", "apple"), ("2", "apricot"), ("3", "banana")],
            false,
        )]);
        let got = h.search(1, &["ap"]);
        assert_eq!(rows(&got[0]), Some((vec!["apple", "apricot"], 0)));
        let got = h.search(2, &["app"]);
        assert_eq!(rows(&got[0]), Some((vec!["apple"], 0)));
        let got = h.search(3, &["ban"]);
        assert_eq!(rows(&got[0]), Some((vec!["banana"], 0)));
        assert_eq!(
            h.calls(0),
            [
                "initial ap",
                "metas 1,2",
                "sub app of 2",
                // Both metas are known by now; nothing is fetched twice.
                "initial ban",
                "metas 3",
            ]
        );
    }

    #[test]
    fn a_section_shows_five_rows_and_counts_the_rest() {
        let items: Vec<(String, String)> = (0..8)
            .map(|i| (format!("{i}"), format!("item {i}")))
            .collect();
        let items: Vec<(&str, &str)> = items
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let mut h = harness(vec![(items, false)]);
        let got = h.search(1, &["item"]);
        let (names, more) = rows(&got[0]).unwrap();
        assert_eq!(names.len(), MAX_LIST_ROWS);
        assert_eq!(more, 3);
        assert_eq!(
            h.calls(0)[1],
            "metas 0,1,2,3,4",
            "metas only for the rows shown"
        );
        let SearchToSynoik::Results { results, .. } = &got[0];
        assert_eq!(
            results.as_ref().unwrap().rows[0].description.as_deref(),
            Some("item 0"),
            "a row shows the description's first line"
        );
    }

    #[test]
    fn a_wrong_number_of_metas_drops_the_section_only() {
        let mut h = harness(vec![
            (vec![("1", "apple"), ("2", "apricot")], true),
            (vec![("1", "apple")], false),
        ]);
        let got = h.search(1, &["ap"]);
        assert_eq!(got.len(), 2);
        assert_eq!(
            rows(&got[0]),
            None,
            "the provider that miscounted shows nothing"
        );
        assert_eq!(rows(&got[1]), Some((vec!["apple"], 0)));
    }

    #[test]
    fn a_search_overtaken_before_it_answered_stays_quiet() {
        let (open, gate) = async_channel::unbounded();
        let mut h = gated_harness(
            vec![(vec![("1", "apple")], false)],
            Some(gate),
            Duration::from_secs(5),
        );
        for generation in [1, 2] {
            h.client.handle(SynoikToSearch::Search {
                generation,
                terms: vec!["ap".into()],
            });
        }
        open.send_blocking(()).unwrap();
        open.send_blocking(()).unwrap();
        let mut got = h.wait(1);
        // Both answers were released together; give the stale one the time it would need.
        std::thread::sleep(Duration::from_millis(200));
        got.extend(h.replies.try_iter());
        assert_eq!(got.len(), 1, "only the newer search reports: {got:?}");
        let SearchToSynoik::Results { generation, .. } = &got[0];
        assert_eq!(*generation, 2);
    }

    /// A provider that never answers stops counting as searching once the call times out, as
    /// a GDBus proxy's would — or the status would read "Searching" for good.
    #[test]
    fn a_provider_that_never_answers_times_out_empty() {
        let (_open, gate) = async_channel::unbounded();
        let mut h = gated_harness(
            vec![(vec![("1", "apple")], false)],
            Some(gate),
            Duration::from_millis(100),
        );
        let got = h.search(1, &["ap"]);
        assert_eq!(got.len(), 1, "the timed-out provider still reports");
        assert_eq!(rows(&got[0]), None);
    }

    #[test]
    fn activation_passes_the_terms_and_time() {
        let mut h = harness(vec![(vec![("1", "apple")], false)]);
        h.client.handle(SynoikToSearch::ActivateResult {
            provider: "p0.desktop".into(),
            id: "1".into(),
            terms: vec!["ap".into()],
            timestamp: 42,
        });
        h.client.handle(SynoikToSearch::LaunchSearch {
            provider: "p0.desktop".into(),
            terms: vec!["ap".into()],
            timestamp: 43,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.calls(0).len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut calls = h.calls(0);
        calls.sort();
        assert_eq!(calls, ["activate 1 ap 42", "launch ap 43"]);
    }

    fn owned(value: Value<'_>) -> OwnedValue {
        value.try_into().unwrap()
    }

    #[test]
    fn icons_parse_from_each_form_a_provider_uses() {
        let themed = StructureBuilder::new()
            .add_field("themed")
            .append_field(Value::Value(Box::new(Value::from(vec![
                "text-x-generic",
                "text-x-generic-symbolic",
            ]))))
            .build()
            .unwrap();
        assert_eq!(
            deserialize_icon(&Value::from(themed)),
            Some(ResultIcon::Themed(vec![
                "text-x-generic".into(),
                "text-x-generic-symbolic".into()
            ]))
        );
        let file = StructureBuilder::new()
            .add_field("file")
            .append_field(Value::Value(Box::new(Value::from("file:///tmp/a%20b.png"))))
            .build()
            .unwrap();
        assert_eq!(
            deserialize_icon(&Value::from(file)),
            Some(ResultIcon::File("/tmp/a b.png".into()))
        );
        assert_eq!(
            icon_for_string("org.gnome.Calculator"),
            Some(ResultIcon::Themed(vec!["org.gnome.Calculator".into()]))
        );
        assert_eq!(
            icon_for_string("relative/path.png"),
            Some(ResultIcon::Themed(vec!["relative/path.png".into()]))
        );

        // A 2x1 RGB image with a padded rowstride.
        let data = StructureBuilder::new()
            .add_field(2i32)
            .add_field(1i32)
            .add_field(8i32)
            .add_field(false)
            .add_field(8i32)
            .add_field(3i32)
            .append_field(Value::from(Array::from(vec![1u8, 2, 3, 4, 5, 6, 0, 0])))
            .build()
            .unwrap();
        assert_eq!(
            icon_data(&Value::from(data)),
            Some(ResultIcon::Pixels {
                width: 2,
                height: 1,
                rgba: vec![1, 2, 3, 255, 4, 5, 6, 255]
            })
        );
        let short = StructureBuilder::new()
            .add_field(2i32)
            .add_field(2i32)
            .add_field(6i32)
            .add_field(false)
            .add_field(8i32)
            .add_field(3i32)
            .append_field(Value::from(Array::from(vec![1u8, 2, 3])))
            .build()
            .unwrap();
        assert_eq!(
            icon_data(&Value::from(short)),
            None,
            "too few bytes for the size"
        );
    }

    #[test]
    fn a_meta_needs_an_id_and_a_name() {
        let meta = |pairs: &[(&str, &str)]| -> HashMap<String, OwnedValue> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), owned(Value::from(*v))))
                .collect()
        };
        assert!(parse_meta(&meta(&[("id", "a"), ("name", "A")])).is_some());
        assert!(parse_meta(&meta(&[("id", "a")])).is_none());
        assert!(parse_meta(&meta(&[("name", "A")])).is_none());
        assert!(parse_meta(&meta(&[("id", "a"), ("name", "")])).is_none());
        let long = "x".repeat(10 * MAX_TEXT_CHARS);
        let parsed = parse_meta(&meta(&[("id", "a"), ("name", &long)])).unwrap();
        assert_eq!(parsed.name.chars().count(), MAX_TEXT_CHARS);
    }
}
