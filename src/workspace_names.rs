// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! The persistent list of named workspaces.
//!
//! A named workspace is furniture: it exists because the user said so, not because something is
//! living on it, so it outlives the windows that were on it, the display it belongs to going
//! away, and the session ending (`docs/fork/multi-display.md` §6). This is the only thing that
//! carries one across a logout or a reboot — the session store carries *windows*, and a workspace
//! whose windows all closed before logout would otherwise come back as an ordinary empty.
//!
//! # Not `org.gnome.desktop.wm.preferences workspace-names`
//!
//! That is where GNOME keeps them, and the tenet says GNOME's surface wins — but mutter keys the
//! array by *global workspace index* (`prefs.c:1870-1924`), and an index is not an identity here:
//! workspaces are per-monitor, so the number shifts whenever anything above it is added, closed or
//! reordered, and it cannot say which display the workspace belongs to. A key we could not write
//! correctly is not a key we can adopt. Divergence, deliberate.
//!
//! # A whole snapshot, every time
//!
//! The file is rewritten in full from live layout state whenever that state stops matching it —
//! there are no incremental edits, so there is no way for the file to drift from the strip. The
//! list is small (it is exactly the workspaces a user has bothered to name) and the comparison is
//! what decides whether a write happens at all.
//!
//! Entries are canonically ordered by home display and ordinal so that "did this change" is a
//! comparison of the meaning, not of whichever display happens to be hosting a workspace whose own
//! display is unplugged.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::output_identity::OutputIdentity;
use crate::session_state::StoreWriter;

/// The format we write. A file claiming anything higher is refused outright, as in the session
/// store: better to start empty than to half-read a file and overwrite it with the half we
/// understood.
pub const VERSION: u32 = 1;

/// One named workspace, in the frame that survives the display being unplugged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedWorkspace {
    pub name: String,
    /// The display it belongs to — the same identity the session store and `monitors.xml` use, so
    /// a workspace comes home to the panel it was on rather than to a connector name.
    #[serde(default, skip_serializing_if = "OutputIdentity::is_empty")]
    pub home: OutputIdentity,
    /// Where in that display's own strip it sat. What makes a replug restore an arrangement
    /// rather than an unordered set.
    #[serde(default)]
    pub ordinal: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    workspaces: Vec<NamedWorkspace>,
}

/// The in-memory list, and its file when it has one.
///
/// A store with no `path` never touches the filesystem: that is what headless test instances get,
/// so the suite can neither read nor clobber the real file.
#[derive(Debug, Default)]
pub struct WorkspaceNameStore {
    path: Option<PathBuf>,
    workspaces: Vec<NamedWorkspace>,
    dirty: bool,
    writer: Option<StoreWriter>,
}

impl WorkspaceNameStore {
    /// A store backed by `path`, loading whatever is already there. A missing file is the first
    /// run, not an error; anything else is reported and the caller gets an empty store.
    pub fn load(path: PathBuf) -> (Self, Option<crate::session_state::LoadError>) {
        use crate::session_state::LoadError;

        let mut store = Self {
            path: Some(path),
            ..Self::default()
        };
        let path = store.path.as_deref().expect("just set");

        let contents = match std::fs::read(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return (store, None),
            Err(err) => return (store, Some(LoadError::Io(err))),
        };

        let file: StoreFile = match serde_json::from_slice(&contents) {
            Ok(file) => file,
            Err(err) => return (store, Some(LoadError::Parse(err))),
        };
        if file.version > VERSION {
            return (
                store,
                Some(LoadError::TooNew {
                    found: file.version,
                }),
            );
        }

        store.workspaces = file.workspaces;
        (store, None)
    }

    /// A store that never writes anything. The suite's, and the default for a headless instance.
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn workspaces(&self) -> &[NamedWorkspace] {
        &self.workspaces
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Replaces the list with what the layout currently holds. Returns whether anything actually
    /// changed — a snapshot equal to the last one costs nothing, which is what lets the caller
    /// take one every frame.
    pub fn set(&mut self, workspaces: Vec<NamedWorkspace>) -> bool {
        if self.workspaces == workspaces {
            return false;
        }
        self.workspaces = workspaces;
        self.dirty = true;
        true
    }

    pub fn serialize(&self) -> Result<Vec<u8>, serde_json::Error> {
        let file = StoreFile {
            version: VERSION,
            workspaces: self.workspaces.clone(),
        };
        serde_json::to_vec_pretty(&file)
    }

    /// Hands the current list to the writer thread, if this store has a path.
    pub fn save(&mut self) -> Result<(), io::Error> {
        self.dirty = false;

        let Some(path) = self.path.clone() else {
            return Ok(());
        };

        let bytes = self
            .serialize()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        self.writer
            .get_or_insert_with(|| StoreWriter::spawn(path, "workspace names store"))
            .queue(bytes);
        Ok(())
    }

    /// Writes and waits for the worker. For the shutdown path only.
    pub fn flush(&mut self) {
        if self.save().is_ok() {
            if let Some(writer) = self.writer.take() {
                writer.finish();
            }
        }
    }
}

/// `$XDG_DATA_HOME/synoik/workspaces.json`, falling back to `~/.local/share`.
pub fn default_path() -> Option<PathBuf> {
    let data_home = match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(data_home.join("synoik").join("workspaces.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, ordinal: usize) -> NamedWorkspace {
        NamedWorkspace {
            name: String::from(name),
            home: OutputIdentity::from_connector("DP-1"),
            ordinal,
        }
    }

    #[test]
    fn a_store_round_trips_through_json() {
        let mut store = WorkspaceNameStore::in_memory();
        assert!(store.set(vec![named("Mail", 0), named("Code", 2)]));

        let bytes = store.serialize().unwrap();
        let file: StoreFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(file.version, VERSION);
        assert_eq!(file.workspaces, store.workspaces);
    }

    #[test]
    fn an_unchanged_snapshot_is_not_a_write() {
        let mut store = WorkspaceNameStore::in_memory();
        assert!(store.set(vec![named("Mail", 0)]));
        store.save().unwrap();
        assert!(!store.is_dirty());

        assert!(
            !store.set(vec![named("Mail", 0)]),
            "the same list must not dirty the store: the snapshot is taken every frame"
        );
        assert!(!store.is_dirty());
    }
}
