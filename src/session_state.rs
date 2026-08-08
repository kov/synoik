// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! The persistent store behind `xdg_session_management_v1`.
//!
//! Plain data and serde, no Wayland types: everything here is testable on its own. See
//! `docs/fork/session-management-port.md` for the decisions this encodes.
//!
//! The on-disk shape mirrors mutter's (`meta-wayland-xdg-session-state.c`) concept for concept —
//! numeric window states, a floating and a tiled rect, a workspace index — so that the two stay
//! comparable, but it is JSON rather than gvdb, which is a glib implementation detail.
//!
//! **Forward compatibility is deliberate.** A `version` newer than ours makes the whole load fail
//! closed rather than silently dropping records we cannot read, and within a record a `state` value
//! we do not understand parses as "do not restore" while still round-tripping to disk untouched.
//! synoik cannot represent minimize or edge half-tiling yet; both are intended, so their slots are
//! carried rather than dropped.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The format we write. A file claiming anything higher is refused outright.
pub const VERSION: u32 = 1;

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

/// `[x, y, width, height]`, in logical coordinates, as mutter stores it.
pub type Rect = [i32; 4];

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

    /// Likewise: no minimize yet, but the slot is part of the format.
    #[serde(rename = "is-minimized", default, skip_serializing_if = "is_false")]
    pub is_minimized: bool,

    /// Workspace *index*, not id: ids are runtime-only and meaningless across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<u32>,
}

fn is_false(b: &bool) -> bool {
    !b
}

impl ToplevelRecord {
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
    /// Set by every mutation, cleared by a successful write. The debounce timer is what turns this
    /// into an actual save.
    dirty: bool,
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
        let file: StoreFile = serde_json::from_slice(contents)?;
        if file.version > VERSION {
            return Err(LoadError::TooNew {
                found: file.version,
            });
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

    /// Writes the store to its path, if it has one. Clears the dirty flag on success.
    pub fn save(&mut self) -> Result<(), io::Error> {
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return Ok(());
        };

        let bytes = self
            .serialize()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        write_atomically(&path, &bytes)?;
        self.dirty = false;
        Ok(())
    }
}

/// Write through a temporary file in the same directory, so a crash mid-write leaves the previous
/// store intact rather than a truncated one.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
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
        let bytes = br#"{"version": 1, "sessions": {"s": {"last-used": 1,
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
        store.save().unwrap();
        assert!(!store.is_dirty(), "a successful save clears the flag");

        let (loaded, err) = SessionStore::load(path);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            loaded.get("kept").unwrap().toplevels["w"].restorable_state(),
            Some(WindowState::Maximized)
        );

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
