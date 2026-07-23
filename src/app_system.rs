//! The application catalog — our `Shell.AppSystem`/`AppFavorites` equivalent.
//!
//! GNOME's dash, app grid, and overview search all resolve apps through
//! `Shell.AppSystem.get_default()` (`js/ui/dash.js`, `js/ui/appDisplay.js`) and
//! favorites through `AppFavorites.getAppFavorites()` (`js/ui/appFavorites.js`).
//! This module is the compositor-owned, inspectable model those surfaces will
//! read — the sibling of [`crate::gnome::GnomeSettings`] the overview-port plan
//! calls for (`docs/fork/overview-port.md` §3.1). It carries **no UI**; slices S3
//! (dash) and S4 (overview search) consume it.
//!
//! The catalog is backed by GIO: enumeration is `g_app_info_get_all`, lookup is
//! `g_desktop_app_info_new`, and search is **`g_desktop_app_info_search`** — the
//! exact C call `shell_app_system_search` uses, so relevance grouping matches
//! GNOME for free (`js/ui/appDisplay.js` `AppSearchProvider.getInitialResultSet`).
//! GIO and launching sit behind the [`AppCatalog`]/[`AppLauncher`] traits so
//! headless tests drive a fake catalog and assert launches against a recorder
//! without spawning anything.
//!
//! Favorites persist to `org.gnome.shell favorite-apps` (an `as` strv), read and
//! written through the existing [`GnomeSettings`](crate::gnome::GnomeSettings)
//! pipeline; app-database changes arrive via [`gio::AppInfoMonitor`] over a
//! calloop channel (see [`AppSystem::new_gio`]).
//!
//! **Known divergence.** `shell_app_system_search` scrubs invalid UTF-8 in a
//! desktop id to the empty string; we go through GIO's `GString`, whose
//! conversion `debug_assert!`s valid UTF-8 (debug panic / release UB on a
//! non-UTF-8 id). Desktop ids are `.desktop` filenames, so exposure is minimal;
//! a raw-FFI lossy scrub is deferred as over-engineering for now.

use std::path::PathBuf;

use gio::glib;
use gio_unix::prelude::*;
use gio_unix::DesktopAppInfo;

/// One installed application — a plain-data snapshot of a `GDesktopAppInfo`,
/// inspectable and cheap to clone. The full-color icon is deliberately absent;
/// it is the next slice (S2, the themed icon loader), added as a resolved gicon
/// there rather than here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// `g_app_info_get_id()`, e.g. `"org.gnome.Nautilus.desktop"`. Entries whose
    /// id is missing are skipped at enumerate time (GNOME drops them the same way,
    /// `appDisplay.js` `_loadApps`).
    pub id: String,
    /// `g_app_info_get_name()`.
    pub name: String,
    /// `g_app_info_get_description()`.
    pub description: Option<String>,
    /// `g_app_info_get_commandline()` — the full `Exec` line; what launch tests
    /// assert against. `None` for entries with no `Exec` (link-type or
    /// D-Bus-activated apps). We read the command line rather than
    /// `g_app_info_get_executable()` because the latter's binding unwraps a
    /// nullable and panics on those entries.
    pub commandline: Option<PathBuf>,
    /// `g_desktop_app_info_list_actions()` — desktop actions, e.g. `"new-window"`,
    /// which drives the new-window launch preference.
    pub actions: Vec<String>,
    /// `g_app_info_should_show()`. Consumers filter on this; the catalog keeps
    /// everything so favorites/launch can still resolve `NoDisplay` apps.
    pub should_show: bool,
}

/// How a launch was requested — the two verbs of `AppIcon.activate`
/// (`appDisplay.js:3060`). The caller computes the mode from modifiers; the
/// running-window branches (`open_new_window` via the app's action group,
/// activate-existing-window) need running-app tracking and are slice S6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// `shell_app_activate` — for a stopped app this is just launch.
    Activate,
    /// `shell_app_open_new_window` — prefer the `new-window` desktop action.
    NewWindow,
}

/// What [`AppSystem::launch`] resolved an intent to before it crossed the
/// launcher seam — this is what a test asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLaunch {
    /// Plain `g_app_info_launch` of the entry.
    Default,
    /// `g_desktop_app_info_launch_action` of a named desktop action.
    Action(String),
}

/// Why a launch could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// No desktop entry resolved for the id.
    UnknownApp,
    /// The underlying launcher failed.
    Failed(String),
}

/// The enumerate/lookup/search seam — GIO in production, a fake in tests.
pub trait AppCatalog {
    /// Every installed app, unfiltered (`g_app_info_get_all`).
    fn enumerate(&self) -> Vec<AppEntry>;
    /// A single app by desktop id, unfiltered (`g_desktop_app_info_new`).
    fn lookup(&self, id: &str) -> Option<AppEntry>;
    /// Relevance-grouped search (`g_desktop_app_info_search`): outer vec is
    /// relevance tiers, inner vec is ids within a tier.
    fn search(&self, query: &str) -> Vec<Vec<String>>;
}

/// The launch seam — real GIO spawn in production, a recorder in tests so the
/// corpus never spawns a process.
pub trait AppLauncher {
    fn launch(&self, entry: &AppEntry, verb: &ResolvedLaunch) -> Result<(), String>;
}

/// The compositor-owned application model. Owned on `Niri`; fed from the
/// `favorite-apps` gsettings pipeline and the `AppInfoMonitor` channel.
pub struct AppSystem {
    catalog: Box<dyn AppCatalog>,
    launcher: Box<dyn AppLauncher>,
    /// Cached enumeration, refreshed on `installed-changed`.
    installed: Vec<AppEntry>,
    /// Favorite ids in `favorite-apps` order — the persistence source. Kept raw
    /// after an external [`set_favorites`](Self::set_favorites) (like GNOME's
    /// store, which reload never rewrites), but every *mutation* collapses it to
    /// the resolved list, mirroring `AppFavorites`'s `set_strv(_getIds())`. All
    /// favorite operations act in **resolved space** (ids that both look up and
    /// `should_show`) — GNOME's `_favorites` is that filtered map, not the strv.
    stored: Vec<String>,
}

impl AppSystem {
    /// An inert model: empty catalog, a launcher that warns and drops. This is
    /// what headless test instances start with (they then inject fakes via
    /// [`with_parts`](Self::with_parts)) and the fallback when GIO is unavailable.
    pub fn disconnected() -> Self {
        Self {
            catalog: Box::new(EmptyCatalog),
            launcher: Box::new(NullLauncher),
            installed: Vec::new(),
            stored: Vec::new(),
        }
    }

    /// The live GIO-backed model plus a channel that pings on every
    /// `installed-changed`. The initial enumeration runs synchronously here (as
    /// GNOME's `shell_app_system` does at init) — which also arms the desktop-file
    /// directory monitors the [`gio::AppInfoMonitor`] depends on, so enumerate
    /// happens *before* the watcher thread starts.
    pub fn new_gio() -> (Self, calloop::channel::Channel<()>) {
        let (tx, rx) = calloop::channel::channel();

        let mut system = Self {
            catalog: Box::new(GioCatalog),
            launcher: Box::new(GioLauncher),
            installed: Vec::new(),
            stored: Vec::new(),
        };
        system.refresh();

        // The monitor needs a running glib main context; the compositor runs
        // calloop, so a dedicated thread hosts a private context and forwards a
        // ping per change. Re-enumeration happens on the main thread in the
        // calloop handler (only `()` crosses the channel) — mirrors the
        // `gsettings-watch` thread in `crate::gnome`.
        if let Err(err) = std::thread::Builder::new()
            .name("appinfo-watch".to_owned())
            .spawn(move || {
                let ctx = glib::MainContext::new();
                let run = ctx.with_thread_default(|| {
                    // Held alive for the loop's lifetime: a dropped monitor stops
                    // emitting.
                    let monitor = gio::AppInfoMonitor::get();
                    let ping = tx.clone();
                    let _id = monitor.connect_changed(move |_| {
                        let _ = tx.send(());
                    });
                    // Close the startup race: a DB change between the caller's
                    // initial `refresh()` and this monitor being armed fires into a
                    // monitor-less context and is lost (glib destroys the per-dir
                    // file monitor on that fire and only re-creates it on the next
                    // catalog call). One ping now that the monitor exists forces a
                    // re-refresh (which also re-arms the dir monitors).
                    let _ = ping.send(());
                    let main_loop = glib::MainLoop::new(Some(&ctx), false);
                    main_loop.run();
                    drop(monitor);
                });
                if let Err(err) = run {
                    tracing::warn!("appinfo-watch thread could not run: {err}");
                }
            })
        {
            tracing::warn!("failed to spawn appinfo-watch thread: {err}");
        }

        (system, rx)
    }

    /// Inject explicit catalog/launcher parts — the headless-test constructor.
    #[cfg(test)]
    pub fn with_parts(catalog: Box<dyn AppCatalog>, launcher: Box<dyn AppLauncher>) -> Self {
        let mut system = Self {
            catalog,
            launcher,
            installed: Vec::new(),
            stored: Vec::new(),
        };
        system.refresh();
        system
    }

    /// Re-read the catalog (what the `installed-changed` ping triggers). Because
    /// [`favorites`](Self::favorites) resolves against the live catalog at read
    /// time, this is automatically a favorites reload too (`appFavorites.js` reacts
    /// to `installed-changed` the same way).
    ///
    /// TODO(S3): GNOME re-emits `AppFavorites` `changed` when the *resolved*
    /// favorites change across a refresh; the dash redisplay will want that signal.
    /// TODO(perf): every ping re-enumerates eagerly on the main thread; glib
    /// advises deferring the re-read under update bursts (bounded to one rescan per
    /// refresh by the monitor's edge-trigger). Consider debouncing later.
    pub fn refresh(&mut self) {
        self.installed = self.catalog.enumerate();
    }

    /// The installed apps that should be shown (`g_app_info_should_show`) — the
    /// view every dash/grid/search consumer wants.
    pub fn installed(&self) -> impl Iterator<Item = &AppEntry> {
        self.installed.iter().filter(|e| e.should_show)
    }

    /// A single app by desktop id, resolved against the live catalog (unfiltered,
    /// like `shell_app_system_lookup_app`).
    pub fn lookup(&self, id: &str) -> Option<AppEntry> {
        self.catalog.lookup(id)
    }

    /// Relevance-grouped app search, delegated verbatim to the catalog
    /// (`g_desktop_app_info_search`). Provider-level filtering/usage-sorting/
    /// system-actions live above this in slice S4.
    pub fn search(&self, query: &str) -> Vec<Vec<String>> {
        self.catalog.search(query)
    }

    /// Replace the stored favorite id list from the `favorite-apps` gsettings
    /// model (GNOME's `changed::favorite-apps` → `reload`). Returns whether the
    /// **resolved** list changed — the redisplay trigger, matching
    /// `AppFavorites._updateFavorites`, which compares filtered id lists, not the
    /// raw strv.
    pub fn set_favorites(&mut self, ids: Vec<String>) -> bool {
        let before = self.resolved_ids();
        self.stored = ids;
        before != self.resolved_ids()
    }

    /// The stored favorite ids, in order — the persistence source (what to write
    /// back to `favorite-apps`). Raw after an external change; the resolved list
    /// after any mutation (GNOME's store, which `set_strv` collapses on mutation).
    pub fn favorite_ids(&self) -> &[String] {
        &self.stored
    }

    /// The stored ids filtered to those that resolve and `should_show` — the keys
    /// of GNOME's `_favorites` map (`AppFavorites.reload` + `_getIds`). Every
    /// favorite operation works in this space.
    fn resolved_ids(&self) -> Vec<String> {
        self.stored
            .iter()
            .filter(|id| self.lookup(id).is_some_and(|e| e.should_show))
            .cloned()
            .collect()
    }

    /// The resolved favorite apps, in order (`AppFavorites.getFavorites`).
    pub fn favorites(&self) -> Vec<AppEntry> {
        self.resolved_ids()
            .iter()
            .filter_map(|id| self.lookup(id))
            .collect()
    }

    /// Whether `id` is a favorite (`AppFavorites.isFavorite` — membership of the
    /// resolved map, so an uninstalled/hidden stored id is *not* a favorite).
    pub fn is_favorite(&self, id: &str) -> bool {
        self.resolved_ids().iter().any(|f| f == id)
    }

    /// Append a favorite (`AppFavorites.addFavorite`). See
    /// [`add_favorite_at_pos`](Self::add_favorite_at_pos).
    pub fn add_favorite(&mut self, id: &str) -> bool {
        self.add_favorite_at_pos(id, None)
    }

    /// Insert a favorite at `pos` in **resolved space** (clamped), or append when
    /// `None` (`AppFavorites._addFavorite`). Refused (returns `false`) if the id is
    /// already a favorite, does not resolve, or is hidden (`should_show` false).
    /// Persists the resolved list plus the new id, so unresolvable stored ids are
    /// dropped on mutation (GNOME's `set_strv(_getIds())`).
    pub fn add_favorite_at_pos(&mut self, id: &str, pos: Option<usize>) -> bool {
        if self.is_favorite(id) {
            return false;
        }
        if !self.lookup(id).is_some_and(|e| e.should_show) {
            return false;
        }
        let mut ids = self.resolved_ids();
        let at = pos.unwrap_or(ids.len()).min(ids.len());
        ids.insert(at, id.to_string());
        self.stored = ids;
        true
    }

    /// Move an existing favorite to `pos` (`AppFavorites.moveFavoriteToPos` =
    /// remove then re-add, so the re-add re-validates). No-op if `id` is not a
    /// favorite.
    pub fn move_favorite_to_pos(&mut self, id: &str, pos: usize) {
        self.remove_favorite(id);
        self.add_favorite_at_pos(id, Some(pos));
    }

    /// Remove a favorite (`AppFavorites._removeFavorite`). Returns whether it was
    /// a (resolved) favorite; persists the resolved list minus the id.
    pub fn remove_favorite(&mut self, id: &str) -> bool {
        if !self.is_favorite(id) {
            return false;
        }
        self.stored = self
            .resolved_ids()
            .into_iter()
            .filter(|f| f != id)
            .collect();
        true
    }

    /// Launch an app by id with the given mode. Resolves the intent to a
    /// [`ResolvedLaunch`] above the launcher seam so the policy is testable
    /// without spawning.
    pub fn launch(&mut self, id: &str, mode: LaunchMode) -> Result<(), LaunchError> {
        let entry = self.lookup(id).ok_or(LaunchError::UnknownApp)?;
        let verb = resolve_launch(mode, &entry);
        self.launcher
            .launch(&entry, &verb)
            .map_err(LaunchError::Failed)
    }
}

/// Resolve a launch intent to a concrete verb. `Activate` of a stopped app is a
/// plain launch (`shell_app_activate` stopped branch); `NewWindow` prefers the
/// `new-window` desktop action, else falls back to relaunching
/// (`shell_app_open_new_window`). The running-app action-group path is S6.
fn resolve_launch(mode: LaunchMode, entry: &AppEntry) -> ResolvedLaunch {
    match mode {
        LaunchMode::Activate => ResolvedLaunch::Default,
        LaunchMode::NewWindow => {
            if entry.actions.iter().any(|a| a == "new-window") {
                ResolvedLaunch::Action("new-window".to_string())
            } else {
                ResolvedLaunch::Default
            }
        }
    }
}

/// Build an [`AppEntry`] from a `GAppInfo`. Returns `None` for entries without an
/// id (GNOME drops these too).
fn make_entry(info: &gio::AppInfo) -> Option<AppEntry> {
    let id = info.id()?.to_string();
    let actions = info
        .downcast_ref::<DesktopAppInfo>()
        .map(|d| d.list_actions().iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Some(AppEntry {
        id,
        name: info.name().to_string(),
        description: info.description().map(|s| s.to_string()),
        commandline: info.commandline(),
        actions,
        should_show: info.should_show(),
    })
}

/// The production catalog: thin, stateless wrappers over GIO.
struct GioCatalog;

impl AppCatalog for GioCatalog {
    fn enumerate(&self) -> Vec<AppEntry> {
        gio::AppInfo::all().iter().filter_map(make_entry).collect()
    }

    fn lookup(&self, id: &str) -> Option<AppEntry> {
        let desktop = DesktopAppInfo::new(id)?;
        make_entry(desktop.upcast_ref())
    }

    fn search(&self, query: &str) -> Vec<Vec<String>> {
        DesktopAppInfo::search(query)
            .into_iter()
            .map(|group| group.iter().map(|s| s.to_string()).collect())
            .collect()
    }
}

/// The production launcher. Re-resolves the desktop entry (thread-safe,
/// cache-backed) and launches with no launch context — startup-notify/timestamp/
/// systemd-scope wrapping is a deferred refinement (`overview-port.md` §4).
struct GioLauncher;

impl AppLauncher for GioLauncher {
    fn launch(&self, entry: &AppEntry, verb: &ResolvedLaunch) -> Result<(), String> {
        let desktop = DesktopAppInfo::new(&entry.id)
            .ok_or_else(|| format!("no desktop entry: {}", entry.id))?;
        match verb {
            ResolvedLaunch::Default => desktop
                .launch(&[], gio::AppLaunchContext::NONE)
                .map_err(|e| e.to_string()),
            ResolvedLaunch::Action(name) => {
                desktop.launch_action(name, gio::AppLaunchContext::NONE);
                Ok(())
            }
        }
    }
}

/// The disconnected catalog — nothing installed.
struct EmptyCatalog;

impl AppCatalog for EmptyCatalog {
    fn enumerate(&self) -> Vec<AppEntry> {
        Vec::new()
    }
    fn lookup(&self, _id: &str) -> Option<AppEntry> {
        None
    }
    fn search(&self, _query: &str) -> Vec<Vec<String>> {
        Vec::new()
    }
}

/// The disconnected launcher — never spawns.
struct NullLauncher;

impl AppLauncher for NullLauncher {
    fn launch(&self, entry: &AppEntry, _verb: &ResolvedLaunch) -> Result<(), String> {
        tracing::warn!(
            "ignoring launch of {} on a disconnected AppSystem",
            entry.id
        );
        Ok(())
    }
}

// ---- Test doubles, shared with the conformance corpus (`src/tests/gnome.rs`). ----

/// A fake catalog over an in-memory app list plus a canned search result. The
/// `Rc` handles let a test mutate the backing after construction (e.g. to model
/// an uninstall before `refresh`).
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeCatalog {
    pub apps: std::rc::Rc<std::cell::RefCell<Vec<AppEntry>>>,
    pub search_result: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>,
}

#[cfg(test)]
impl FakeCatalog {
    pub fn new(apps: Vec<AppEntry>) -> Self {
        Self {
            apps: std::rc::Rc::new(std::cell::RefCell::new(apps)),
            search_result: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        }
    }
}

#[cfg(test)]
impl AppCatalog for FakeCatalog {
    fn enumerate(&self) -> Vec<AppEntry> {
        self.apps.borrow().clone()
    }
    fn lookup(&self, id: &str) -> Option<AppEntry> {
        self.apps.borrow().iter().find(|e| e.id == id).cloned()
    }
    fn search(&self, _query: &str) -> Vec<Vec<String>> {
        self.search_result.borrow().clone()
    }
}

/// A launcher that records `(entry, verb)` instead of spawning.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct RecordingLauncher {
    pub calls: std::rc::Rc<std::cell::RefCell<Vec<(AppEntry, ResolvedLaunch)>>>,
}

#[cfg(test)]
impl AppLauncher for RecordingLauncher {
    fn launch(&self, entry: &AppEntry, verb: &ResolvedLaunch) -> Result<(), String> {
        self.calls.borrow_mut().push((entry.clone(), verb.clone()));
        Ok(())
    }
}

#[cfg(test)]
impl AppEntry {
    /// A minimal shown entry for tests.
    pub fn fake(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            commandline: Some(PathBuf::from(format!("{} %U", name.to_lowercase()))),
            actions: Vec::new(),
            should_show: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GIO catalog round-trips real installed apps. Skips cleanly on a bare
    /// host with no apps (like the icon tests in `render_helpers/icon.rs`). Only
    /// membership is asserted — never search ranking, which is locale/machine
    /// dependent (`g_desktop_app_info_search` is the spec).
    #[test]
    fn gio_catalog_smoke() {
        let catalog = GioCatalog;
        let all = catalog.enumerate();
        if all.is_empty() {
            eprintln!("skipping gio_catalog_smoke: no apps installed");
            return;
        }
        for entry in &all {
            assert!(
                entry.id.ends_with(".desktop"),
                "unexpected app id {:?}",
                entry.id
            );
        }
        let first = &all[0];
        let looked_up = catalog
            .lookup(&first.id)
            .expect("lookup of an enumerated app");
        assert_eq!(looked_up.id, first.id);
        assert_eq!(looked_up.name, first.name);

        // Searching an installed app's name finds it in *some* relevance group.
        // `all` is in hash order, so try a few entries and require one hit rather
        // than betting on an arbitrary first app with an odd/ambiguous name.
        let found = all.iter().take(5).any(|app| {
            catalog
                .search(&app.name)
                .iter()
                .flatten()
                .any(|id| id == &app.id)
        });
        assert!(found, "search did not surface any of the first few apps");
    }

    // The favorites + launch semantics below pin `AppFavorites`/`shell_app`
    // behavior against the fake seam. They have no `do_action` surface yet; slices
    // S3 (dash) and S4 (search) will add the input-driven conformance tests in
    // `src/tests/gnome.rs` once a click/Enter can reach `launch`.

    /// `installed()` hides `!should_show` apps; `lookup()` still resolves them
    /// (catalog keeps everything — `shell_app_cache` vs the consumer's filter).
    #[test]
    fn installed_filters_should_show_lookup_does_not() {
        let hidden = AppEntry {
            should_show: false,
            ..AppEntry::fake("hidden.desktop", "Hidden")
        };
        let catalog = FakeCatalog::new(vec![
            AppEntry::fake("a.desktop", "A"),
            hidden,
            AppEntry::fake("b.desktop", "B"),
        ]);
        let system =
            AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
        let shown: Vec<_> = system.installed().map(|e| e.id.as_str()).collect();
        assert_eq!(shown, ["a.desktop", "b.desktop"]);
        assert!(system.lookup("hidden.desktop").is_some());
    }

    /// A launch records the resolved entry + verb through the seam and never
    /// spawns; an unknown id errors and records nothing.
    #[test]
    fn launch_records_resolved_argv_without_spawning() {
        let catalog = FakeCatalog::new(vec![AppEntry::fake("org.example.App.desktop", "App")]);
        let recorder = RecordingLauncher::default();
        let mut system = AppSystem::with_parts(Box::new(catalog), Box::new(recorder.clone()));

        system
            .launch("org.example.App.desktop", LaunchMode::Activate)
            .expect("launch");
        {
            let calls = recorder.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0.id, "org.example.App.desktop");
            assert_eq!(calls[0].0.commandline, Some(PathBuf::from("app %U")));
            assert_eq!(calls[0].1, ResolvedLaunch::Default);
        }

        assert_eq!(
            system.launch("nope.desktop", LaunchMode::Activate),
            Err(LaunchError::UnknownApp)
        );
        assert_eq!(
            recorder.calls.borrow().len(),
            1,
            "an unknown launch records nothing"
        );
    }

    /// `NewWindow` prefers the `new-window` desktop action when present, else
    /// falls back to a plain relaunch (`shell_app_open_new_window`).
    #[test]
    fn new_window_prefers_new_window_desktop_action() {
        let with_action = AppEntry {
            actions: vec!["new-window".to_string()],
            ..AppEntry::fake("w.desktop", "W")
        };
        let without = AppEntry::fake("p.desktop", "P");
        let recorder = RecordingLauncher::default();
        let mut system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(vec![with_action, without])),
            Box::new(recorder.clone()),
        );

        system.launch("w.desktop", LaunchMode::NewWindow).unwrap();
        system.launch("p.desktop", LaunchMode::NewWindow).unwrap();

        let calls = recorder.calls.borrow();
        assert_eq!(calls[0].1, ResolvedLaunch::Action("new-window".to_string()));
        assert_eq!(calls[1].1, ResolvedLaunch::Default);
    }

    /// Favorites mirror `AppFavorites` exactly: operations act in **resolved
    /// space** (installed + `should_show`), positions are resolved-space indices,
    /// a NoDisplay app cannot be pinned, mutations erase unresolvable stored ids,
    /// an uninstalled/hidden stored id is not a favorite, and `set_favorites`
    /// reports whether the *resolved* list changed. (An external `set_favorites`
    /// keeps the raw strv until the first mutation, like GNOME's store.)
    #[test]
    fn favorites_mirror_appfavorites() {
        let hidden = AppEntry {
            should_show: false,
            ..AppEntry::fake("b.desktop", "B")
        };
        let catalog = FakeCatalog::new(vec![
            AppEntry::fake("a.desktop", "A"),
            hidden,
            AppEntry::fake("c.desktop", "C"),
            AppEntry::fake("d.desktop", "D"),
        ]);
        let mut system =
            AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));

        // External change: resolved list goes []→[a, c] (b hidden, unknown gone).
        assert!(system.set_favorites(vec![
            "a.desktop".to_string(),
            "unknown.desktop".to_string(),
            "b.desktop".to_string(),
            "c.desktop".to_string(),
        ]));
        // The raw strv is kept until a mutation (GNOME's store, unrewritten by reload).
        assert_eq!(
            system.favorite_ids(),
            ["a.desktop", "unknown.desktop", "b.desktop", "c.desktop"]
        );
        let resolved: Vec<_> = system.favorites().into_iter().map(|e| e.id).collect();
        assert_eq!(resolved, ["a.desktop", "c.desktop"]);
        // isFavorite is resolved-map membership.
        assert!(system.is_favorite("a.desktop"));
        assert!(
            !system.is_favorite("unknown.desktop"),
            "uninstalled not favorite"
        );
        assert!(!system.is_favorite("b.desktop"), "hidden not favorite");

        // Adds: unknown refused (no resolve), NoDisplay refused (shouldShowApp),
        // already-favorite refused.
        assert!(!system.add_favorite("unknown.desktop"), "unknown refused");
        assert!(!system.add_favorite("b.desktop"), "NoDisplay refused");
        assert!(
            !system.add_favorite("a.desktop"),
            "already-favorite refused"
        );

        // add at a resolved-space index; the mutation erases unknown + b from the
        // stored strv (GNOME `set_strv(_getIds())`).
        assert!(system.add_favorite_at_pos("d.desktop", Some(1)));
        assert_eq!(
            system.favorite_ids(),
            ["a.desktop", "d.desktop", "c.desktop"]
        );

        // move = remove + re-add: [a,d,c] → remove a → [d,c] → add a at 2 → [d,c,a].
        system.move_favorite_to_pos("a.desktop", 2);
        assert_eq!(
            system.favorite_ids(),
            ["d.desktop", "c.desktop", "a.desktop"]
        );

        assert!(system.remove_favorite("c.desktop"));
        assert!(
            !system.remove_favorite("c.desktop"),
            "second remove is a no-op"
        );
        assert_eq!(system.favorite_ids(), ["d.desktop", "a.desktop"]);

        // `changed` compares resolved lists.
        assert!(
            !system.set_favorites(vec!["d.desktop".to_string(), "a.desktop".to_string()]),
            "no-op set reports unchanged"
        );
        assert!(system.set_favorites(vec!["a.desktop".to_string()]));
    }

    /// `search` delegates verbatim to the catalog, preserving relevance grouping
    /// and intra-group order.
    #[test]
    fn search_delegates_and_groups() {
        let catalog = FakeCatalog::new(vec![]);
        *catalog.search_result.borrow_mut() = vec![
            vec!["b.desktop".to_string(), "a.desktop".to_string()],
            vec!["c.desktop".to_string()],
        ];
        let system = AppSystem::with_parts(
            Box::new(catalog.clone()),
            Box::new(RecordingLauncher::default()),
        );
        assert_eq!(
            system.search("anything"),
            vec![
                vec!["b.desktop".to_string(), "a.desktop".to_string()],
                vec!["c.desktop".to_string()],
            ]
        );
    }

    /// `refresh()` (what the `installed-changed` ping triggers) re-reads the
    /// catalog; `favorites()` then prunes an uninstalled app while the raw stored
    /// list keeps it.
    #[test]
    fn refresh_reflects_db_change_and_prunes_favorites() {
        let catalog = FakeCatalog::new(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("b.desktop", "B"),
        ]);
        let apps = catalog.apps.clone();
        let mut system =
            AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
        system.set_favorites(vec!["a.desktop".to_string(), "b.desktop".to_string()]);
        assert_eq!(system.installed().count(), 2);
        assert_eq!(system.favorites().len(), 2);

        apps.borrow_mut().retain(|e| e.id != "b.desktop");
        system.refresh();

        assert_eq!(
            system
                .installed()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["a.desktop"]
        );
        assert_eq!(
            system
                .favorites()
                .into_iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            ["a.desktop"]
        );
        assert_eq!(system.favorite_ids(), ["a.desktop", "b.desktop"]);
    }
}
