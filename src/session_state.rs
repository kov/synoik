// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! The persistent store behind `xdg_session_management_v1`.
//!
//! Plain data and serde, no Wayland types: everything here is testable on its own. See
//! `docs/fork/session-management-port.md` for the decisions this encodes.
//!
//! The on-disk shape starts from mutter's (`meta-wayland-xdg-session-state.c`) — numeric window
//! states, a floating and a tiled rect, a workspace — but it is JSON rather than gvdb, which is a
//! glib implementation detail, and it **diverges on the frame of reference**.
//!
//! # A saved rect is anchored to a display, not to the desktop
//!
//! Mutter stores rects in global coordinates and picks the monitor back out of them on restore
//! (`determine_monitor_for_rect`). That only works while the monitor origins hold still: move
//! them between save and restore and every rect silently shifts by the difference, which then
//! gets re-saved, so the error ratchets. It also cannot express the thing we want — restoring a
//! session under a *different* monitor configuration, where there is no global frame to share.
//!
//! So a record carries an [`OutputIdentity`] and its rects are **output-local**. Restore finds the
//! display by identity and replays the rect on it, wherever that display now sits and whatever
//! else is connected.
//!
//! # A workspace index is approximate, so a name comes with it
//!
//! Workspaces are dynamic and, since they are per-monitor here, a bare index is an index into a
//! stack that grows and shrinks. It is kept as the fallback, but a *named* workspace is matched by
//! name first: that is the only handle that survives a restart.
//!
//! **Forward compatibility is deliberate.** A `version` newer than ours makes the whole load fail
//! closed rather than silently dropping records we cannot read, and within a record a `state` value
//! we do not understand parses as "do not restore" while still round-tripping to disk untouched.
//! synoik cannot represent minimize or edge half-tiling yet; both are intended, so their slots are
//! carried rather than dropped.
//!
//! **The write happens off the compositor thread.** Serializing is cheap and stays inline — that is
//! what pins the bytes to live state at save time — but the write itself is an `fsync`, measured at
//! 9-22 ms for a full store on a warm NVMe and with a much worse tail on a busy disk. The
//! compositor has one thread, so that would be dropped frames on the very interactions (a window
//! closing, a session going away) that schedule a save. See [`StoreWriter`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The format we write. A file claiming anything higher is refused outright.
///
/// v2 moved rects from global to output-local and added [`ToplevelRecord::output`]. A v1 record
/// has no display to anchor its rect to, and a global rect cannot be converted into one without
/// knowing the layout that wrote it, so [`sanitize_legacy`] drops v1 geometry and keeps the rest.
pub const VERSION: u32 = 2;

/// How many sessions survive a load, most-recently-used first.
///
/// Entries are tiny, so the cap is generous on purpose; the protocol declares eviction policy an
/// implementation detail. Enforced at load only — a single run that somehow created more than this
/// many sessions keeps them all until the next start, which beats evicting a session a client is
/// still holding.
pub const MAX_SESSIONS: usize = 1000;

/// A window state, in mutter's numbering (`meta-wayland-xdg-session-state.c:32-41`).
///
/// The numbers are the file format, so they are pinned here rather than derived from declaration
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Floating = 1,
    Maximized = 2,
    TiledLeft = 3,
    TiledRight = 4,
    Fullscreen = 5,
}

impl WindowState {
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Floating,
            2 => Self::Maximized,
            3 => Self::TiledLeft,
            4 => Self::TiledRight,
            5 => Self::Fullscreen,
            // Written by a synoik newer than this one. Keep the record, skip the restore.
            _ => return None,
        })
    }

    pub fn as_raw(self) -> u32 {
        self as u32
    }

    /// Whether synoik can actually put a window into this state today.
    ///
    /// Edge half-tiling is not ported yet (and minimize is a separate flag with the same story),
    /// so a record written by a future synoik — or migrated from mutter's store — can name a state
    /// we have to ignore for now without that being a parse error.
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Floating | Self::Maximized | Self::Fullscreen)
    }
}

/// `[x, y, width, height]`, in logical coordinates, **relative to the record's
/// [`ToplevelRecord::output`]** — see the module docs for why this is not mutter's global frame.
pub type Rect = [i32; 4];

/// The display a record's rects are anchored to, in `monitors.xml`'s identity fields.
///
/// Deliberately the same four fields `<monitorspec>` carries, so that one notion of "which
/// display" serves both stores and the deferred identity-only matching lands in both at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputIdentity {
    pub connector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl OutputIdentity {
    /// Whether this names the same display as `other`.
    ///
    /// Connector-exact, with the EDID fields as a veto when both sides carry one: the same rule
    /// `monitors.xml` matching uses today. Matching a display across a *renamed* connector is the
    /// deferred half, and it is deferred here for the same reason — both stores should gain it
    /// together, or a session and its layout would disagree about which display is which.
    pub fn matches(&self, other: &Self) -> bool {
        if !self.connector.eq_ignore_ascii_case(&other.connector) {
            return false;
        }

        let agrees = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            // One side did not record it. Absence is not a mismatch: an output with no EDID is
            // normal, and a record written before we read one must still match.
            _ => true,
        };

        agrees(&self.vendor, &other.vendor)
            && agrees(&self.product, &other.product)
            && agrees(&self.serial, &other.serial)
    }
}

/// What we remember about one toplevel, keyed in its session by the client-chosen name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToplevelRecord {
    /// The window state, in [`WindowState`]'s numbering. Kept raw so a value we do not understand
    /// survives a load/save round trip instead of being flattened to a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<u32>,

    #[serde(
        rename = "floating-rect",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub floating_rect: Option<Rect>,

    /// Written only by a synoik that has edge half-tiling. Carried through untouched until then.
    #[serde(
        rename = "tiled-rect",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tiled_rect: Option<Rect>,

    /// Whether the window was minimized. Applied *after* the sizing state on restore, which is
    /// the order mutter takes (`meta-wayland-xdg-session-state.c:468-476`) — a window is restored
    /// to the size it would have had, then hidden.
    #[serde(rename = "is-minimized", default, skip_serializing_if = "is_false")]
    pub is_minimized: bool,

    /// Workspace *index* within [`Self::output`]'s stack, not id: ids are runtime-only and
    /// meaningless across restarts. Approximate by construction — the stack is dynamic — so it is
    /// the fallback for [`Self::workspace_name`], never the first choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<u32>,

    /// The name of the workspace the window was on, when it had one.
    ///
    /// The only workspace handle that survives a restart, so restore matches it ahead of the
    /// index. A workspace the user bothered to name is one they expect to find again.
    #[serde(
        rename = "workspace-name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_name: Option<String>,

    /// The display [`Self::floating_rect`] and [`Self::tiled_rect`] are relative to.
    ///
    /// `None` only in a record we could not anchor — then the rects are unusable and the normal
    /// placement chain decides, rather than a position being replayed against nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputIdentity>,
}

fn is_false(b: &bool) -> bool {
    !b
}

impl ToplevelRecord {
    /// Strips what a record written before `version` 2 cannot mean any more.
    ///
    /// v1 rects were global and v1 workspace indices had no display to be an index into. Neither
    /// can be recovered without the monitor layout that wrote them, which is not stored, so both
    /// go. The window state and the minimized flag are frame-independent and stay: a session that
    /// comes back maximized on the wrong monitor still beats one that comes back not at all.
    fn sanitize_legacy(&mut self) {
        self.floating_rect = None;
        self.tiled_rect = None;
        self.workspace = None;
        self.workspace_name = None;
        self.output = None;
    }

    /// The state to actually restore into, or `None` when there is nothing usable to apply.
    pub fn restorable_state(&self) -> Option<WindowState> {
        self.state
            .and_then(WindowState::from_raw)
            .filter(|state| state.is_supported())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Unix microseconds, matching mutter's `last-used`. Drives the [`MAX_SESSIONS`] eviction.
    #[serde(rename = "last-used", default)]
    pub last_used: u64,

    #[serde(default)]
    pub toplevels: HashMap<String, ToplevelRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    sessions: HashMap<String, SessionRecord>,
}

#[derive(Debug)]
pub enum LoadError {
    /// Written by a newer synoik. Fail the whole load rather than drop what we cannot read.
    TooNew {
        found: u32,
    },
    Parse(serde_json::Error),
    Io(io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooNew { found } => {
                write!(f, "session store is version {found}, newer than {VERSION}")
            }
            Self::Parse(err) => write!(f, "session store is not valid JSON: {err}"),
            Self::Io(err) => write!(f, "error reading the session store: {err}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<serde_json::Error> for LoadError {
    fn from(err: serde_json::Error) -> Self {
        Self::Parse(err)
    }
}

impl From<io::Error> for LoadError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// The in-memory session store.
///
/// A store with no `path` never touches the filesystem: that is what headless test instances get,
/// so the suite can neither read nor clobber the real session file.
#[derive(Debug, Default)]
pub struct SessionStore {
    path: Option<PathBuf>,
    sessions: HashMap<String, SessionRecord>,
    /// Set by every mutation, cleared when a write is queued. The debounce timer is what turns
    /// this into an actual save.
    dirty: bool,
    /// Started on the first save, so a store that is never written costs no thread.
    writer: Option<StoreWriter>,
}

impl SessionStore {
    /// A store backed by `path`, loading whatever is already there.
    ///
    /// A missing file is not an error — it is the first run. Anything else (unreadable, malformed,
    /// too new) is reported, and the caller gets an *empty* store: we would rather start over than
    /// half-read a file and then overwrite it with the half we understood.
    pub fn load(path: PathBuf) -> (Self, Option<LoadError>) {
        let mut store = Self {
            path: Some(path),
            ..Self::default()
        };

        let path = store.path.as_deref().expect("just set");
        let contents = match std::fs::read(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return (store, None),
            Err(err) => return (store, Some(err.into())),
        };

        match Self::parse(&contents) {
            Ok(sessions) => {
                store.sessions = sessions;
                store.evict_to_cap();
                (store, None)
            }
            Err(err) => (store, Some(err)),
        }
    }

    /// A store that exists only in memory. Used by headless test instances.
    pub fn in_memory() -> Self {
        Self::default()
    }

    fn parse(contents: &[u8]) -> Result<HashMap<String, SessionRecord>, LoadError> {
        let mut file: StoreFile = serde_json::from_slice(contents)?;
        if file.version > VERSION {
            return Err(LoadError::TooNew {
                found: file.version,
            });
        }

        // Sessions themselves survive a version change: a client still holds its id, and dropping
        // the store would turn every `get_session` into a fresh one, which is worse than a session
        // that restores nothing. Only the geometry a v1 record can no longer express is discarded.
        if file.version < VERSION {
            for session in file.sessions.values_mut() {
                for toplevel in session.toplevels.values_mut() {
                    toplevel.sanitize_legacy();
                }
            }
        }

        Ok(file.sessions)
    }

    /// Drops all but the [`MAX_SESSIONS`] most-recently-used sessions.
    fn evict_to_cap(&mut self) {
        if self.sessions.len() <= MAX_SESSIONS {
            return;
        }

        let mut by_recency: Vec<_> = self
            .sessions
            .iter()
            .map(|(id, record)| (record.last_used, id.clone()))
            .collect();
        // Most recent first; the id breaks ties so the outcome does not depend on hash order.
        by_recency.sort_unstable_by(|a, b| b.cmp(a));

        for (_, id) in by_recency.drain(MAX_SESSIONS..) {
            self.sessions.remove(&id);
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&SessionRecord> {
        self.sessions.get(id)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// The record for `id`, creating it if this is a session we have not seen, and stamping it as
    /// used now either way.
    pub fn touch(&mut self, id: &str) -> &mut SessionRecord {
        self.dirty = true;
        let record = self.sessions.entry(id.to_owned()).or_default();
        record.last_used = now_micros();
        record
    }

    /// Forgets a session entirely — `xdg_session_v1.remove`.
    pub fn remove(&mut self, id: &str) -> bool {
        let removed = self.sessions.remove(id).is_some();
        self.dirty |= removed;
        removed
    }

    /// Forgets one toplevel within a session — `xdg_session_v1.remove_toplevel`.
    pub fn remove_toplevel(&mut self, id: &str, name: &str) -> bool {
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        let removed = session.toplevels.remove(name).is_some();
        self.dirty |= removed;
        removed
    }

    /// Moves a toplevel's record to a new name — `xdg_toplevel_session_v1.rename`.
    ///
    /// Overwrites whatever was under `new`; the protocol rejects renaming onto a name that is
    /// live, and a stale record under an unheld name has no claim on it.
    pub fn rename_toplevel(&mut self, id: &str, old: &str, new: &str) -> bool {
        let Some(session) = self.sessions.get_mut(id) else {
            return false;
        };
        let Some(record) = session.toplevels.remove(old) else {
            return false;
        };
        session.toplevels.insert(new.to_owned(), record);
        self.dirty = true;
        true
    }

    /// Records what we know about one toplevel — the save-on-unmap snapshot.
    ///
    /// Stamps the session as used, since a window closing is the session being used.
    pub fn save_toplevel(&mut self, id: &str, name: &str, record: ToplevelRecord) {
        let session = self.touch(id);
        session.toplevels.insert(name.to_owned(), record);
    }

    /// Serializes the current state.
    ///
    /// Deliberately separate from writing it: the bytes are produced synchronously from live state
    /// at the moment the save fires, so a session removed a microsecond later cannot be resurrected
    /// by an in-flight write. That is why there are no tombstones here.
    pub fn serialize(&self) -> Result<Vec<u8>, serde_json::Error> {
        let file = StoreFile {
            version: VERSION,
            sessions: self.sessions.clone(),
        };
        serde_json::to_vec_pretty(&file)
    }

    /// Hands the current state to the writer thread, if this store has a path.
    ///
    /// Returns once the bytes are queued, not once they are on disk: see [`StoreWriter`]. A write
    /// that then fails is warned about by the worker and retried by [`Self::flush`] on the way out.
    pub fn save(&mut self) -> Result<(), io::Error> {
        self.dirty = false;

        let Some(path) = self.path.clone() else {
            return Ok(());
        };

        let bytes = self
            .serialize()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        self.writer
            .get_or_insert_with(|| StoreWriter::spawn(path))
            .queue(bytes);
        Ok(())
    }

    /// Writes synchronously and waits for the worker to finish. For the shutdown path only.
    ///
    /// Unconditional rather than dirty-gated: this is the one place that can still recover from a
    /// queued write having failed, and it runs once per session.
    pub fn flush(&mut self) {
        if self.save().is_ok() {
            if let Some(writer) = self.writer.take() {
                writer.finish();
            }
        }
    }
}

/// The thread that owns writing the store to disk.
///
/// Long-lived and single, rather than a thread per save, so writes cannot land out of order: the
/// channel is the ordering. It coalesces — if several saves queued while one write was in flight,
/// only the newest is written, since each payload is a complete snapshot.
#[derive(Debug)]
struct StoreWriter {
    queue: mpsc::Sender<Vec<u8>>,
    thread: JoinHandle<()>,
}

impl StoreWriter {
    fn spawn(path: PathBuf) -> Self {
        let (queue, rx) = mpsc::channel::<Vec<u8>>();
        let builder = std::thread::Builder::new().name("session store".to_owned());
        let thread = builder
            .spawn(move || {
                // Ends when the sender drops, which is the compositor going away.
                while let Ok(mut bytes) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        bytes = newer;
                    }
                    if let Err(err) = write_atomically(&path, &bytes) {
                        tracing::warn!("error saving the session store: {err}");
                    }
                }
            })
            .expect("could not start the session store thread");

        Self { queue, thread }
    }

    fn queue(&self, bytes: Vec<u8>) {
        // The worker only ends when we drop it, so a closed channel means it panicked; the warning
        // it would have logged is already out.
        let _ = self.queue.send(bytes);
    }

    /// Drains everything queued and joins.
    fn finish(self) {
        let Self { queue, thread } = self;
        drop(queue);
        let _ = thread.join();
    }
}

/// Write through a temporary file in the same directory, so a crash mid-write leaves the previous
/// store intact rather than a truncated one.
///
/// The `sync_all` is what makes that true of a power loss rather than only of a process crash:
/// without it the rename can be durable before the bytes are, and the store comes back empty.
/// glib's `g_file_set_contents`, which mutter writes through, fsyncs for the same reason.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, path)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/synoik/session.json`, falling back to `~/.local/share`.
pub fn default_path() -> Option<PathBuf> {
    let data_home = match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(data_home.join("synoik").join("session.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: u32) -> ToplevelRecord {
        ToplevelRecord {
            state: Some(state),
            floating_rect: Some([10, 20, 300, 400]),
            workspace: Some(2),
            ..Default::default()
        }
    }

    #[test]
    fn a_store_round_trips_through_json() {
        let mut store = SessionStore::in_memory();
        store
            .touch("session-a")
            .toplevels
            .insert(String::from("main"), record(2));
        store
            .touch("session-b")
            .toplevels
            .insert(String::from("other"), record(5));

        let bytes = store.serialize().unwrap();
        let parsed = SessionStore::parse(&bytes).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed["session-a"].toplevels["main"].floating_rect,
            Some([10, 20, 300, 400])
        );
        assert_eq!(
            parsed["session-b"].toplevels["other"].restorable_state(),
            Some(WindowState::Fullscreen)
        );
    }

    #[test]
    fn a_newer_version_fails_the_whole_load() {
        let bytes = br#"{"version": 99, "sessions": {}}"#;
        let err = SessionStore::parse(bytes).unwrap_err();
        assert!(
            matches!(err, LoadError::TooNew { found: 99 }),
            "expected TooNew, got {err:?}"
        );
    }

    #[test]
    fn an_unreadable_state_keeps_the_record_but_does_not_restore() {
        // Half-tiling and anything else we do not implement yet.
        let bytes = br#"{"version": 2, "sessions": {"s": {"last-used": 1,
            "toplevels": {"w": {"state": 3, "tiled-rect": [0, 0, 100, 100], "is-minimized": true}}}}}"#;
        let parsed = SessionStore::parse(bytes).unwrap();
        let toplevel = &parsed["s"].toplevels["w"];

        assert_eq!(
            toplevel.restorable_state(),
            None,
            "tiled-left is not portable to synoik yet, so nothing is restored"
        );
        assert_eq!(toplevel.state, Some(3), "but the value is kept verbatim");

        // ...and survives a save, so a synoik that grows half-tiling still finds it.
        let mut store = SessionStore::in_memory();
        store.sessions = parsed;
        let round_tripped = SessionStore::parse(&store.serialize().unwrap()).unwrap();
        assert_eq!(round_tripped["s"].toplevels["w"].state, Some(3));
        assert!(round_tripped["s"].toplevels["w"].is_minimized);
        assert_eq!(
            round_tripped["s"].toplevels["w"].tiled_rect,
            Some([0, 0, 100, 100])
        );
    }

    #[test]
    fn a_version_1_record_keeps_its_session_but_loses_its_geometry() {
        // v1 rects were global and its workspace index named no display, so neither can be read
        // back without the monitor layout that wrote them. The session and the frame-independent
        // half of the record survive: the client still holds this id, and dropping the session
        // would cost it the identity as well as the position.
        let bytes = br#"{"version": 1, "sessions": {"s": {"last-used": 1, "toplevels": {"w": {
            "state": 2, "floating-rect": [2048, 100, 800, 600], "workspace": 3,
            "is-minimized": true}}}}}"#;
        let parsed = SessionStore::parse(bytes).unwrap();
        let toplevel = &parsed["s"].toplevels["w"];

        assert_eq!(
            toplevel.state,
            Some(2),
            "the state does not depend on a frame"
        );
        assert!(toplevel.is_minimized);
        assert_eq!(
            toplevel.floating_rect, None,
            "a global rect has no meaning now"
        );
        assert_eq!(
            toplevel.workspace, None,
            "an index with no display is not one"
        );
        assert_eq!(toplevel.output, None);
    }

    #[test]
    fn a_current_record_keeps_the_display_it_is_anchored_to() {
        let bytes = br#"{"version": 2, "sessions": {"s": {"toplevels": {"w": {
            "floating-rect": [10, 20, 800, 600], "workspace": 3, "workspace-name": "mail",
            "output": {"connector": "DP-2", "serial": "ABC123"}}}}}}"#;
        let toplevel = &SessionStore::parse(bytes).unwrap()["s"].toplevels["w"];

        assert_eq!(toplevel.floating_rect, Some([10, 20, 800, 600]));
        assert_eq!(toplevel.workspace, Some(3));
        assert_eq!(toplevel.workspace_name.as_deref(), Some("mail"));
        let output = toplevel.output.clone().unwrap();
        assert_eq!(output.connector, "DP-2");
        assert_eq!(output.serial.as_deref(), Some("ABC123"));
    }

    #[test]
    fn a_display_matches_on_its_connector_and_is_vetoed_by_a_differing_edid() {
        let saved = OutputIdentity {
            connector: "DP-2".into(),
            serial: Some("ABC123".into()),
            ..Default::default()
        };

        let live = |connector: &str, serial: Option<&str>| OutputIdentity {
            connector: connector.into(),
            serial: serial.map(str::to_owned),
            ..Default::default()
        };

        assert!(
            saved.matches(&live("dp-2", Some("abc123"))),
            "case is not identity"
        );
        assert!(
            saved.matches(&live("DP-2", None)),
            "an output with no EDID is normal, and absence is not a mismatch"
        );
        assert!(
            !saved.matches(&live("DP-2", Some("XYZ789"))),
            "a different display on the same connector is a different display"
        );
        assert!(
            !saved.matches(&live("DP-1", Some("ABC123"))),
            "matching the same panel across a renamed connector is deliberately deferred"
        );
    }

    #[test]
    fn an_unknown_state_value_from_the_future_is_not_a_parse_error() {
        let bytes = br#"{"version": 1, "sessions": {"s": {"toplevels": {"w": {"state": 42}}}}}"#;
        let parsed = SessionStore::parse(bytes).unwrap();
        assert_eq!(parsed["s"].toplevels["w"].restorable_state(), None);
        assert_eq!(parsed["s"].toplevels["w"].state, Some(42));
    }

    #[test]
    fn the_cap_keeps_the_most_recently_used() {
        let mut store = SessionStore::in_memory();
        for i in 0..MAX_SESSIONS + 10 {
            let record = store.touch(&format!("session-{i:04}"));
            // Explicit rather than wall-clock: `touch` stamps them all within the same microsecond.
            record.last_used = i as u64;
        }
        assert_eq!(store.len(), MAX_SESSIONS + 10);

        store.evict_to_cap();

        assert_eq!(store.len(), MAX_SESSIONS);
        assert!(
            store.contains(&format!("session-{:04}", MAX_SESSIONS + 9)),
            "the newest must survive"
        );
        assert!(
            !store.contains("session-0000"),
            "the oldest must be evicted"
        );
    }

    #[test]
    fn removing_a_session_marks_the_store_dirty_only_if_it_existed() {
        let mut store = SessionStore::in_memory();
        store.touch("present");
        store.dirty = false;

        assert!(!store.remove("absent"));
        assert!(
            !store.is_dirty(),
            "a no-op removal must not schedule a save"
        );

        assert!(store.remove("present"));
        assert!(store.is_dirty());
    }

    #[test]
    fn a_save_is_visible_to_the_next_load() {
        let dir = std::env::temp_dir().join(format!("synoik-session-store-{}", std::process::id()));
        let path = dir.join("session.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = SessionStore {
            path: Some(path.clone()),
            ..SessionStore::default()
        };
        store
            .touch("kept")
            .toplevels
            .insert(String::from("w"), record(2));
        store.flush();
        assert!(!store.is_dirty(), "a queued save clears the flag");

        let (loaded, err) = SessionStore::load(path);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            loaded.get("kept").unwrap().toplevels["w"].restorable_state(),
            Some(WindowState::Maximized)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queued_saves_coalesce_to_the_last_one() {
        let dir = std::env::temp_dir().join(format!("synoik-session-queue-{}", std::process::id()));
        let path = dir.join("session.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = SessionStore {
            path: Some(path.clone()),
            ..SessionStore::default()
        };

        // Three saves in a row, as a burst of debounce firings would do. Each payload is a whole
        // snapshot, so the file must end up matching the newest and not some interleaving.
        for i in 0..3 {
            store.touch(&format!("session-{i}"));
            store.save().unwrap();
        }
        store.flush();

        let (loaded, err) = SessionStore::load(path);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(loaded.len(), 3, "the last write wins, and it had all three");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_a_first_run_not_an_error() {
        let path = std::env::temp_dir()
            .join(format!("synoik-session-absent-{}", std::process::id()))
            .join("session.json");
        let (store, err) = SessionStore::load(path);
        assert!(err.is_none(), "{err:?}");
        assert!(store.is_empty());
        assert!(!store.is_dirty());
    }
}
