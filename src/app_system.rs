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

use std::collections::HashMap;
use std::path::PathBuf;

use gio::glib;
use gio_unix::prelude::*;
use gio_unix::DesktopAppInfo;

/// A plain-data snapshot of an app's icon (`g_app_info_get_icon()`), resolved to
/// pixels by [`crate::render_helpers::icon::AppIconCache`]. Mirrors the `GIcon`
/// cases St's `st_icon_theme_lookup_by_gicon_for_scale` handles for `.desktop`
/// apps; loadable/bytes/pixbuf gicons map to [`Fallback`](Self::Fallback) for now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppIconRef {
    /// `GThemedIcon`: icon names in priority order (`g_themed_icon_get_names`).
    Themed(Vec<String>),
    /// `GFileIcon`: an absolute file path.
    File(PathBuf),
    /// No icon, a pathless (URI-backed) file, or an unsupported `GIcon` type; the
    /// loader substitutes `application-x-executable` (GNOME's fallback).
    Fallback,
}

/// One installed application — a plain-data snapshot of a `GDesktopAppInfo`,
/// inspectable and cheap to clone.
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
    /// which drives the new-window launch preference *and* fills the action section
    /// of the app context menu (`AppMenu.setApp`, `appMenu.js:229-242`).
    pub actions: Vec<DesktopAction>,
    /// `g_app_info_should_show()`. Consumers filter on this; the catalog keeps
    /// everything so favorites/launch can still resolve `NoDisplay` apps.
    pub should_show: bool,
    /// The app's icon descriptor (`g_app_info_get_icon()`), resolved to pixels by
    /// the [`AppIconCache`](crate::render_helpers::icon::AppIconCache).
    pub icon: AppIconRef,
    /// `g_desktop_app_info_get_startup_wm_class()` — the `StartupWMClass` key that
    /// window↔app matching consults first (see [`AppSystem::app_for_window`]).
    pub startup_wm_class: Option<String>,
}

/// One `.desktop` action — `[Desktop Action <id>]` — as the app menu shows it.
///
/// The id is what launches (`g_desktop_app_info_launch_action`); the name is the
/// localized label GNOME puts in the menu (`g_app_info_get_action_name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopAction {
    pub id: String,
    pub name: String,
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

/// One mapped window as the running-app tracker sees it — the plain-data seam
/// between the compositor's window list and the app model, so the whole matching
/// + grouping + ordering policy is testable without a live `Layout`.
///
/// GNOME's `ShellWindowTracker` reads a `MetaWindow`; we read a toplevel. See
/// [`AppSystem::app_for_window`] for what the single `app_id` costs us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningWindow {
    /// The xdg-shell `app_id` — our only `WM_CLASS` analogue.
    pub app_id: Option<String>,
    /// `Mapped::get_focus_timestamp()`, standing in for
    /// `shell_app_get_last_user_time()` in [`shell_app_compare`]'s last clause.
    /// `None` (never focused) sorts last, as GNOME's `0` does.
    ///
    /// [`shell_app_compare`]: AppSystem::running
    pub last_focus: Option<std::time::Duration>,
}

/// An application with at least one open window — an entry of
/// `shell_app_system_get_running()` (`shell-app-system.c:508`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningApp {
    /// The resolved desktop id.
    pub id: String,
    /// How many windows resolved to this app.
    pub n_windows: usize,
    /// The most recent `last_focus` among them — the app's user time.
    pub last_focus: Option<std::time::Duration>,
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
    /// `StartupWMClass` → desktop id, rebuilt on every [`refresh`](Self::refresh)
    /// (`scan_startup_wm_class_to_id`, `shell-app-system.c:107`).
    startup_wm_class_to_id: HashMap<String, String>,
    /// The raw window snapshot, kept so a catalog [`refresh`](Self::refresh) can
    /// re-resolve it (an app installed while its window is open then matches).
    windows: Vec<RunningWindow>,
    /// `windows` resolved, grouped and ordered — `get_running()`'s answer.
    running: Vec<RunningApp>,
}

/// Desktop-id prefixes tried when a bare `WM_CLASS`-derived basename misses
/// (`vendor_prefixes`, `shell-app-system.c:29-33`).
const VENDOR_PREFIXES: &[&str] = &["gnome-", "fedora-", "mozilla-", "debian-"];

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
            startup_wm_class_to_id: HashMap::new(),
            windows: Vec::new(),
            running: Vec::new(),
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
            startup_wm_class_to_id: HashMap::new(),
            windows: Vec::new(),
            running: Vec::new(),
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
            startup_wm_class_to_id: HashMap::new(),
            windows: Vec::new(),
            running: Vec::new(),
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
    /// This enumerates eagerly on the main thread, so the caller is expected not to
    /// call it per ping: `Niri::queue_app_catalog_reload` coalesces a burst onto one
    /// reload, the way gnome-shell's `ShellAppCache` does. (GNOME also runs the
    /// enumeration itself off-thread; we do not, yet.)
    ///
    /// Returns whether the enumeration actually differs from the one it replaces.
    /// A ping is not proof of a change — glib's monitors fire for any write under a
    /// watched directory, and one arrives shortly after startup on a catalog that is
    /// already loaded — so the caller can skip re-deriving everything downstream.
    pub fn refresh(&mut self) -> bool {
        let installed = self.catalog.enumerate();
        let changed = installed != self.installed;
        self.installed = installed;
        self.scan_startup_wm_class_to_id();
        // Re-resolve the open windows: an app installed while its window was
        // already mapped now matches.
        self.recompute_running();
        changed
    }

    /// Rebuild the `StartupWMClass` → id table (`scan_startup_wm_class_to_id`,
    /// `shell-app-system.c:107-149`). Two entries can claim the same key; GNOME
    /// breaks the tie in favour of, in order, the entry whose **id equals the
    /// key** and the entry that **should show**. Both tie-breaks look only at ids
    /// seen *earlier* in the enumeration, so the scan order is part of the
    /// behavior and this is a faithful single pass, not a re-sort.
    fn scan_startup_wm_class_to_id(&mut self) {
        let mut table: HashMap<String, String> = HashMap::new();
        // Ids seen so far that do not `should_show` — the `no_show_ids` array.
        let mut no_show: Vec<&str> = Vec::new();

        for entry in &self.installed {
            let Some(wm_class) = entry.startup_wm_class.as_deref() else {
                continue;
            };
            if !entry.should_show {
                no_show.push(&entry.id);
            }

            let mut old = table.get(wm_class).map(|s| s.as_str());
            // Prefer the entry whose id *is* the WM class.
            if old.is_some() && startup_wm_class_is_exact_match(&entry.id, wm_class) {
                old = None;
            }
            // Prefer a shown entry over a hidden incumbent.
            if let Some(incumbent) = old {
                if entry.should_show && no_show.contains(&incumbent) {
                    old = None;
                }
            }
            if old.is_none() {
                table.insert(wm_class.to_string(), entry.id.clone());
            }
        }

        self.startup_wm_class_to_id = table;
    }

    /// The app whose `.desktop` declares `StartupWMClass=<wm_class>`
    /// (`shell_app_system_lookup_startup_wmclass`, `shell-app-system.c:456`).
    pub fn lookup_startup_wmclass(&self, wm_class: &str) -> Option<AppEntry> {
        let id = self.startup_wm_class_to_id.get(wm_class)?;
        self.lookup(id)
    }

    /// The app whose `.desktop` *basename* matches `wm_class`
    /// (`shell_app_system_lookup_desktop_wmclass`, `shell-app-system.c:405`).
    /// Tried verbatim first — that is what resolves reverse-DNS ids like
    /// `org.example.Foo.Bar` — then canonicalized (lowercased, spaces to dashes,
    /// which is what handles "Fedora Eclipse").
    pub fn lookup_desktop_wmclass(&self, wm_class: &str) -> Option<AppEntry> {
        if let Some(app) = self.lookup_heuristic_basename(&format!("{wm_class}.desktop")) {
            return Some(app);
        }
        let canonicalized = wm_class.to_lowercase().replace(' ', "-");
        self.lookup_heuristic_basename(&format!("{canonicalized}.desktop"))
    }

    /// Look up a heuristically-derived desktop id, retrying under each vendor
    /// prefix (`shell_app_system_lookup_heuristic_basename`,
    /// `shell-app-system.c:376`).
    fn lookup_heuristic_basename(&self, name: &str) -> Option<AppEntry> {
        if let Some(app) = self.lookup(name) {
            return Some(app);
        }
        VENDOR_PREFIXES
            .iter()
            .find_map(|prefix| self.lookup(&format!("{prefix}{name}")))
    }

    /// Resolve a window's `app_id` to a desktop id — our
    /// `get_app_from_window_wmclass` (`shell-window-tracker.c:146`).
    ///
    /// **Divergence: one string, not two.** GNOME runs a four-step ladder because
    /// X11 gives it a `WM_CLASS` *pair*: it tries the instance against
    /// `StartupWMClass`, then the class, then the instance against `.desktop`
    /// basenames, then the class. xdg-shell has a single `app_id`, so the ladder
    /// collapses to its two distinct lookups. That costs us exactly GNOME's
    /// Chromium case — a Chromium web-app window whose class is
    /// `Chromium-browser` but whose *instance* is `crx_<id>` resolves to the
    /// browser instead of the web app, because we never see the instance. XWayland
    /// clients reach us through xwayland-satellite, which has already flattened the
    /// pair into one `app_id`.
    ///
    /// Also unported: `check_app_id_prefix`'s sandbox scoping
    /// (`meta_window_get_sandboxed_app_id`) — we have no sandbox id on the
    /// toplevel, so a sandboxed app cannot currently be told from a host app
    /// claiming its `WM_CLASS`.
    pub fn app_for_window(&self, app_id: &str) -> Option<AppEntry> {
        self.lookup_startup_wmclass(app_id)
            .or_else(|| self.lookup_desktop_wmclass(app_id))
    }

    /// Replace the open-window snapshot (what the compositor's map/unmap/focus
    /// bookkeeping feeds in). Returns whether the resolved running list changed —
    /// the dash redisplay trigger.
    pub fn set_windows(&mut self, windows: Vec<RunningWindow>) -> bool {
        if windows == self.windows {
            return false;
        }
        self.windows = windows;
        let before = self.running.clone();
        self.recompute_running();
        before != self.running
    }

    /// Resolve, group and order the window snapshot into [`running`](Self::running).
    ///
    /// Ordering is `shell_app_compare` (`shell-app.c:839`) reduced to the running
    /// set: every app here is running and has windows, and we have no minimized
    /// state to speak of, so the two leading clauses are vacuous and the rule is
    /// "most recently used first". Ties break by id — GNOME's tie order is its
    /// hash-table iteration order, i.e. arbitrary; ours is merely deterministic.
    fn recompute_running(&mut self) {
        let mut apps: Vec<RunningApp> = Vec::new();
        for window in &self.windows {
            // A window with no `app_id`, or one that resolves to nothing, is
            // dropped. GNOME instead synthesizes a window-backed `ShellApp` and
            // shows it in the dash; that needs an icon we cannot get from a
            // toplevel, so it is deferred (`overview-port.md` S6).
            let Some(entry) = window
                .app_id
                .as_deref()
                .and_then(|id| self.app_for_window(id))
            else {
                continue;
            };
            match apps.iter_mut().find(|a| a.id == entry.id) {
                Some(app) => {
                    app.n_windows += 1;
                    app.last_focus = app.last_focus.max(window.last_focus);
                }
                None => apps.push(RunningApp {
                    id: entry.id,
                    n_windows: 1,
                    last_focus: window.last_focus,
                }),
            }
        }

        // Most recent first; never-focused (`None`) last, as GNOME's `0` sorts.
        apps.sort_by(|a, b| {
            b.last_focus
                .cmp(&a.last_focus)
                .then_with(|| a.id.cmp(&b.id))
        });

        self.running = apps;
    }

    /// The apps with at least one open window, in `shell_app_compare` order
    /// (`shell_app_system_get_running`, `shell-app-system.c:508`).
    pub fn running(&self) -> &[RunningApp] {
        &self.running
    }

    /// Whether `id` has at least one open window — what the running dot reads
    /// (`AppIcon._updateRunningStyle`, `appDisplay.js:3007`).
    pub fn is_running(&self, id: &str) -> bool {
        self.running.iter().any(|a| a.id == id)
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
            if entry.actions.iter().any(|a| a.id == "new-window") {
                ResolvedLaunch::Action("new-window".to_string())
            } else {
                ResolvedLaunch::Default
            }
        }
    }
}

/// Whether `id` is `wm_class` with an optional `.desktop` suffix — the
/// `StartupWMClass` table's primary tie-break
/// (`startup_wm_class_is_exact_match`, `shell-app-system.c:90`).
fn startup_wm_class_is_exact_match(id: &str, wm_class: &str) -> bool {
    matches!(id.strip_prefix(wm_class), Some("") | Some(".desktop"))
}

/// Build an [`AppEntry`] from a `GAppInfo`. Returns `None` for entries without an
/// id (GNOME drops these too).
fn make_entry(info: &gio::AppInfo) -> Option<AppEntry> {
    let id = info.id()?.to_string();
    let actions = info
        .downcast_ref::<DesktopAppInfo>()
        .map(|d| {
            d.list_actions()
                .iter()
                .map(|id| DesktopAction {
                    name: d.action_name(id).to_string(),
                    id: id.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let icon = icon_ref(info.icon(), &id);
    let startup_wm_class = info
        .downcast_ref::<DesktopAppInfo>()
        .and_then(|d| d.startup_wm_class())
        .map(|s| s.to_string());
    Some(AppEntry {
        id,
        name: info.name().to_string(),
        description: info.description().map(|s| s.to_string()),
        commandline: info.commandline(),
        actions,
        should_show: info.should_show(),
        icon,
        startup_wm_class,
    })
}

/// Extract the plain-data icon descriptor from a `g_app_info_get_icon()` result —
/// the `GThemedIcon`/`GFileIcon` cases St resolves for `.desktop` apps
/// (`st-icon-theme.c` `st_icon_theme_lookup_by_gicon_for_scale`); everything else
/// (loadable/bytes/pixbuf, pathless URI files) maps to
/// [`AppIconRef::Fallback`].
fn icon_ref(icon: Option<gio::Icon>, id: &str) -> AppIconRef {
    let Some(icon) = icon else {
        return AppIconRef::Fallback;
    };
    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        return AppIconRef::Themed(themed.names().iter().map(|s| s.to_string()).collect());
    }
    if let Some(file) = icon.downcast_ref::<gio::FileIcon>() {
        return match file.file().path() {
            Some(path) => AppIconRef::File(path),
            None => AppIconRef::Fallback,
        };
    }
    tracing::debug!("unsupported GIcon type for {id:?}; using the fallback icon");
    AppIconRef::Fallback
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
/// cache-backed) and launches through [`scoped_launch_context`], so the app lands
/// in its own systemd scope. Startup-notify/timestamp wrapping is still a deferred
/// refinement (`overview-port.md` §4).
struct GioLauncher;

impl AppLauncher for GioLauncher {
    fn launch(&self, entry: &AppEntry, verb: &ResolvedLaunch) -> Result<(), String> {
        let desktop = DesktopAppInfo::new(&entry.id)
            .ok_or_else(|| format!("no desktop entry: {}", entry.id))?;
        let context = scoped_launch_context();
        match verb {
            ResolvedLaunch::Default => desktop
                .launch(&[], Some(&context))
                .map_err(|e| e.to_string()),
            ResolvedLaunch::Action(name) => {
                desktop.launch_action(name, Some(&context));
                Ok(())
            }
        }
    }
}

/// A launch context that moves whatever it launches into its own systemd scope.
///
/// GNOME builds the same thing in `shell-global.c:1221` (`create_app_launch_context`) and hooks
/// `launched` at line 1206; see [`crate::utils::spawning::start_app_scope`] for why the scope
/// matters. Ours carries only the scope hookup — GNOME's context also plumbs the startup
/// notification and target workspace, which we do not have yet.
fn scoped_launch_context() -> gio::AppLaunchContext {
    let context = gio::AppLaunchContext::new();
    context.connect_launched(|_context, info, platform_data| {
        let Some(pid) = launched_pid(platform_data) else {
            return;
        };
        // GNOME: "If pid == 0 the application was launched through D-Bus activation, therefore
        // it's already in its own unit" (`shell-global.c:1194`).
        if pid == 0 {
            return;
        }

        let id = info
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|| info.executable().to_string_lossy().into_owned());
        crate::utils::spawning::start_app_scope(&id, pid as u32);
    });
    context
}

/// The `pid` out of a `launched` signal's `platform_data` dictionary.
fn launched_pid(platform_data: &glib::Variant) -> Option<i32> {
    glib::VariantDict::new(Some(platform_data))
        .lookup::<i32>("pid")
        .ok()
        .flatten()
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

/// The real GIO-enumerated apps — a test helper so other modules (the icon
/// loader) can exercise real `AppEntry` icon descriptors without a live `Niri`.
#[cfg(test)]
pub fn gio_installed_for_test() -> Vec<AppEntry> {
    GioCatalog.enumerate()
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
            icon: AppIconRef::Fallback,
            startup_wm_class: None,
        }
    }

    /// The same, declaring a `StartupWMClass`.
    pub fn fake_with_wm_class(id: &str, name: &str, wm_class: &str) -> Self {
        Self {
            startup_wm_class: Some(wm_class.to_string()),
            ..Self::fake(id, name)
        }
    }
}

#[cfg(test)]
mod tests {
    /// A ping is not proof of a change. glib's monitors fire for any write under a
    /// watched directory and one lands a few seconds into every session, so the
    /// reload has to check rather than trust the signal — everything it re-derives
    /// downstream is either wasted or, in the icon caches' case, destructive.
    #[test]
    fn refresh_reports_whether_the_catalog_actually_changed() {
        let catalog = FakeCatalog::new(vec![AppEntry::fake("a.desktop", "A")]);
        let apps = catalog.apps.clone();
        let mut system =
            AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));

        assert!(
            !system.refresh(),
            "re-enumerating the same catalog is not a change"
        );

        apps.borrow_mut().push(AppEntry::fake("b.desktop", "B"));
        assert!(system.refresh(), "an added app is a change");
        assert!(!system.refresh(), "and it is only a change once");

        apps.borrow_mut().clear();
        assert!(system.refresh(), "a removed app is a change too");
    }

    use super::*;

    /// The `launched` signal's `platform_data` is an untyped `a{sv}`, so reading the pid out of it
    /// is the kind of thing that silently yields `None` forever and takes the systemd scope with
    /// it — nothing downstream errors, apps just quietly keep landing in the compositor's cgroup.
    /// Pin the shape GIO actually sends (`g_desktop_app_info_launch_uris_with_spawn` puts an `i`
    /// under "pid"), including the D-Bus-activation case that legitimately has no pid.
    #[test]
    fn the_launched_pid_is_read_out_of_the_platform_data() {
        let with_pid = glib::VariantDict::new(None);
        with_pid.insert("pid", 4321i32);
        with_pid.insert("startup-notification-id", "gnome-shell-1");
        assert_eq!(launched_pid(&with_pid.end()), Some(4321));

        // D-Bus activation: GNOME reads this as "already in its own unit" and skips the scope.
        let activated = glib::VariantDict::new(None);
        activated.insert("pid", 0i32);
        assert_eq!(launched_pid(&activated.end()), Some(0));

        // No pid at all, and a pid of the wrong type, must both decline rather than panic.
        let empty = glib::VariantDict::new(None);
        assert_eq!(launched_pid(&empty.end()), None);

        let wrong_type = glib::VariantDict::new(None);
        wrong_type.insert("pid", "4321");
        assert_eq!(launched_pid(&wrong_type.end()), None);
    }

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
            // Icon extraction never panics on real catalog data.
            let _ = &entry.icon;
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

    /// The icon descriptor maps the two `GIcon` cases that matter for `.desktop`
    /// apps; a missing icon and unsupported kinds map to `Fallback`.
    #[test]
    fn icon_ref_extracts_themed_and_file() {
        let themed = gio::ThemedIcon::new("firefox").upcast::<gio::Icon>();
        match icon_ref(Some(themed), "x") {
            AppIconRef::Themed(names) => assert!(
                names.iter().any(|n| n == "firefox"),
                "themed names missing the base name: {names:?}"
            ),
            other => panic!("expected Themed, got {other:?}"),
        }

        let file =
            gio::FileIcon::new(&gio::File::for_path("/nonexistent/x.png")).upcast::<gio::Icon>();
        assert_eq!(
            icon_ref(Some(file), "x"),
            AppIconRef::File(PathBuf::from("/nonexistent/x.png"))
        );

        assert_eq!(icon_ref(None, "x"), AppIconRef::Fallback);
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

    /// A desktop action arrives with the localized *name* GNOME puts in the menu, not
    /// just the id it launches by. Worth pinning against real GIO: the two are
    /// different strings from different calls, and nothing downstream would notice a
    /// menu labelled `new-window` instead of "New Window" except a person reading it.
    #[test]
    fn desktop_actions_carry_their_display_names() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!("gsrs-actions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("action-probe.desktop");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(
            b"[Desktop Entry]\nType=Application\nName=Probe\nExec=/bin/true\nActions=new-window;\n\
              \n[Desktop Action new-window]\nName=New Window\nExec=/bin/true\n",
        )
        .unwrap();
        drop(f);

        let info = DesktopAppInfo::from_filename(&path).expect("the probe entry loads");
        let entry = make_entry(info.upcast_ref::<gio::AppInfo>()).expect("it has an id");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            entry.actions,
            vec![DesktopAction {
                id: "new-window".to_owned(),
                name: "New Window".to_owned(),
            }],
        );
    }

    /// `NewWindow` prefers the `new-window` desktop action when present, else
    /// falls back to a plain relaunch (`shell_app_open_new_window`).
    #[test]
    fn new_window_prefers_new_window_desktop_action() {
        let with_action = AppEntry {
            actions: vec![DesktopAction {
                id: "new-window".to_owned(),
                name: "New Window".to_owned(),
            }],
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

    // ---- Window ↔ app matching (S6) ----

    fn system_with(apps: Vec<AppEntry>) -> AppSystem {
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(NullLauncher))
    }

    fn win(app_id: &str, secs: Option<u64>) -> RunningWindow {
        RunningWindow {
            app_id: Some(app_id.to_owned()),
            last_focus: secs.map(std::time::Duration::from_secs),
        }
    }

    /// The ladder's first rung wins: a `StartupWMClass` claim beats a `.desktop`
    /// basename that would also match (`get_app_from_window_wmclass`,
    /// `shell-window-tracker.c:191-212`).
    #[test]
    fn startup_wm_class_beats_the_desktop_basename() {
        let system = system_with(vec![
            AppEntry::fake("Emacs.desktop", "Emacs Basename"),
            AppEntry::fake_with_wm_class("org.gnu.emacs.desktop", "Emacs", "Emacs"),
        ]);
        assert_eq!(
            system.app_for_window("Emacs").map(|e| e.id),
            Some("org.gnu.emacs.desktop".to_owned())
        );
    }

    /// The basename lookup tries the class verbatim before canonicalizing, which
    /// is what makes reverse-DNS ids resolve (`shell_app_system_lookup_desktop_wmclass`
    /// "handles org.example.Foo.Bar.desktop applications").
    #[test]
    fn desktop_basename_matches_verbatim_before_canonicalizing() {
        let system = system_with(vec![
            AppEntry::fake("org.example.Foo.Bar.desktop", "Foo Bar"),
            AppEntry::fake("org.example.foo.bar.desktop", "Lowercased Decoy"),
        ]);
        assert_eq!(
            system.app_for_window("org.example.Foo.Bar").map(|e| e.id),
            Some("org.example.Foo.Bar.desktop".to_owned()),
            "the verbatim id must win; canonicalizing first would pick the decoy"
        );
    }

    /// ...and canonicalizes when it has to: lowercase, spaces to dashes. This is
    /// GNOME's cited "Fedora Eclipse" case (`shell-app-system.c:427-430`), which
    /// also needs a vendor prefix.
    #[test]
    fn desktop_basename_canonicalizes_case_and_spaces() {
        let system = system_with(vec![AppEntry::fake("fedora-eclipse.desktop", "Eclipse")]);
        assert_eq!(
            system.app_for_window("Fedora Eclipse").map(|e| e.id),
            Some("fedora-eclipse.desktop".to_owned())
        );
    }

    /// Vendor prefixes are retried in order (`vendor_prefixes`,
    /// `shell-app-system.c:29-33`).
    #[test]
    fn vendor_prefixes_are_tried_for_a_bare_basename() {
        let system = system_with(vec![AppEntry::fake("gnome-terminal.desktop", "Terminal")]);
        assert_eq!(
            system.app_for_window("terminal").map(|e| e.id),
            Some("gnome-terminal.desktop".to_owned())
        );
        assert_eq!(system.app_for_window("nonesuch"), None);
    }

    /// When two entries claim the same `StartupWMClass`, the one whose id *is* the
    /// class wins — even though it is enumerated second, i.e. the tie-break really
    /// evicts an incumbent (`scan_startup_wm_class_to_id`, `shell-app-system.c:134-139`).
    #[test]
    fn startup_wm_class_table_prefers_the_exact_id_match() {
        let system = system_with(vec![
            AppEntry::fake_with_wm_class("other.desktop", "Other", "Navigator"),
            AppEntry::fake_with_wm_class("Navigator.desktop", "Navigator", "Navigator"),
        ]);
        assert_eq!(
            system.app_for_window("Navigator").map(|e| e.id),
            Some("Navigator.desktop".to_owned())
        );
    }

    /// A shown entry evicts a hidden incumbent for the same class
    /// (`shell-app-system.c:141-144`). The reverse order must NOT evict, so this
    /// pins the asymmetry rather than "last one wins".
    #[test]
    fn startup_wm_class_table_prefers_a_shown_entry_over_a_hidden_one() {
        let hidden_first = system_with(vec![
            AppEntry {
                should_show: false,
                ..AppEntry::fake_with_wm_class("hidden.desktop", "Hidden", "Steam")
            },
            AppEntry::fake_with_wm_class("shown.desktop", "Shown", "Steam"),
        ]);
        assert_eq!(
            hidden_first.app_for_window("Steam").map(|e| e.id),
            Some("shown.desktop".to_owned()),
            "a shown entry must evict a hidden incumbent"
        );

        let shown_first = system_with(vec![
            AppEntry::fake_with_wm_class("shown.desktop", "Shown", "Steam"),
            AppEntry {
                should_show: false,
                ..AppEntry::fake_with_wm_class("hidden.desktop", "Hidden", "Steam")
            },
        ]);
        assert_eq!(
            shown_first.app_for_window("Steam").map(|e| e.id),
            Some("shown.desktop".to_owned()),
            "a hidden entry must not evict a shown incumbent"
        );
    }

    // ---- Running-app tracking ----

    /// Windows group by resolved app, and the app's user time is the most recent
    /// among its windows.
    #[test]
    fn running_groups_windows_by_app() {
        let mut system = system_with(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("b.desktop", "B"),
        ]);
        assert!(system.set_windows(vec![
            win("a", Some(1)),
            win("b", Some(2)),
            win("a", Some(9))
        ]));

        let running = system.running();
        assert_eq!(running.len(), 2);
        let a = running.iter().find(|r| r.id == "a.desktop").unwrap();
        assert_eq!(a.n_windows, 2);
        assert_eq!(a.last_focus, Some(std::time::Duration::from_secs(9)));
        assert!(system.is_running("a.desktop"));
        assert!(!system.is_running("c.desktop"));
    }

    /// `shell_app_compare` reduced to the running set: most recently used first,
    /// never-focused last (`shell-app.c:860-868`).
    #[test]
    fn running_sorts_most_recently_used_first() {
        let mut system = system_with(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("b.desktop", "B"),
            AppEntry::fake("c.desktop", "C"),
        ]);
        system.set_windows(vec![win("a", Some(5)), win("b", None), win("c", Some(7))]);
        let ids: Vec<&str> = system.running().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["c.desktop", "a.desktop", "b.desktop"]);
    }

    /// A window that resolves to nothing is dropped rather than shown as a
    /// window-backed app (recorded divergence from `_shell_app_new_for_window`).
    #[test]
    fn unmatched_windows_are_dropped() {
        let mut system = system_with(vec![AppEntry::fake("a.desktop", "A")]);
        system.set_windows(vec![
            win("a", Some(1)),
            win("nonesuch", Some(2)),
            RunningWindow {
                app_id: None,
                last_focus: Some(std::time::Duration::from_secs(3)),
            },
        ]);
        let ids: Vec<&str> = system.running().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a.desktop"]);
    }

    /// `set_windows` reports only *resolved* changes — the dash redisplay trigger.
    /// A window of an unknown app appearing changes nothing observable.
    #[test]
    fn set_windows_reports_resolved_changes_only() {
        let mut system = system_with(vec![AppEntry::fake("a.desktop", "A")]);
        assert!(system.set_windows(vec![win("a", Some(1))]));
        assert!(
            !system.set_windows(vec![win("a", Some(1)), win("nonesuch", Some(2))]),
            "an unresolvable window must not trigger a redisplay"
        );
        assert!(
            system.set_windows(vec![win("a", Some(4))]),
            "a focus-order change must trigger one"
        );
    }

    /// Installing an app while its window is already open resolves it on the next
    /// refresh — the reason the raw window snapshot is kept.
    #[test]
    fn refresh_re_resolves_open_windows() {
        let catalog = FakeCatalog::new(Vec::new());
        let mut system = AppSystem::with_parts(Box::new(catalog.clone()), Box::new(NullLauncher));
        system.set_windows(vec![win("a", Some(1))]);
        assert!(system.running().is_empty());

        catalog
            .apps
            .borrow_mut()
            .push(AppEntry::fake("a.desktop", "A"));
        system.refresh();
        let ids: Vec<&str> = system.running().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a.desktop"]);
    }
}
