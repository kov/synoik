// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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
use std::time::Duration;

use gio::glib;
use gio_unix::prelude::*;
use gio_unix::DesktopAppInfo;

use crate::layout::workspace::WorkspaceId;
use crate::window::mapped::MappedId;

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
    /// The XDG `SingleMainWindow` key, or GNOME's older `X-GNOME-SingleWindow`
    /// fallback — `Some(true)` for an app that declares it only ever has one window.
    ///
    /// `None` means *neither key is present*, which is not the same as `Some(false)`:
    /// `shell_app_can_open_new_window` only consults the key when
    /// `g_desktop_app_info_has_key` says it is there, and otherwise carries on down
    /// the ladder (`shell-app.c:629-642`).
    pub single_main_window: Option<bool>,
    /// `g_app_info_should_show()`. Consumers filter on this; the catalog keeps
    /// everything so favorites/launch can still resolve `NoDisplay` apps.
    pub should_show: bool,
    /// The app's icon descriptor (`g_app_info_get_icon()`), resolved to pixels by
    /// the [`AppIconCache`](crate::render_helpers::icon::AppIconCache).
    pub icon: AppIconRef,
    /// `g_desktop_app_info_get_startup_wm_class()` — the `StartupWMClass` key that
    /// window↔app matching consults first (see [`AppSystem::app_for_window`]).
    pub startup_wm_class: Option<String>,
    /// The `Categories` key, split on `;` — what a *category-based* app folder
    /// matches its members against (`_getCategories`/`_listsIntersect`,
    /// `appDisplay.js:79-95`). Nothing else reads it.
    pub categories: Vec<String>,
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
/// (`appDisplay.js:3060`). The caller computes the mode from modifiers and from
/// the app's state; see `State::activate_app_icon`, which owns the three-way
/// stopped/starting/running split that decides whether a launch happens at all.
///
/// Activating an *existing* window is deliberately not a `LaunchMode`: it never
/// crosses the launcher seam. The one branch still missing is `open_new_window`
/// through the app's exported action group, which needs an action muxer we do
/// not have (`docs/fork/app-lifecycle-port.md` §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchMode {
    /// `shell_app_activate` — for a stopped app this is just launch.
    Activate,
    /// `shell_app_open_new_window` — prefer the `new-window` desktop action.
    NewWindow,
    /// `g_desktop_app_info_launch_action` of a named `.desktop` action — what an app
    /// menu's action row asks for (`shell_app_launch_action`, `appMenu.js:239`).
    /// Unlike [`NewWindow`](Self::NewWindow) there is no fallback: the row exists
    /// because the action does.
    Action(String),
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
    /// The compositor's handle for the window — what a per-window menu verb
    /// ("Open Windows", Quit) acts on. GNOME passes the `MetaWindow` itself.
    pub id: MappedId,
    /// The xdg-shell `app_id` — our only `WM_CLASS` analogue.
    pub app_id: Option<String>,
    /// The client's pid, which is the only handle we have on its **sandbox**.
    ///
    /// mutter reads `/proc/<pid>/root/.flatpak-info` once at window construction and hangs the
    /// result on the window (`meta_window_update_sandboxed_app_id`, `window.c:1043-1059`); we have
    /// no such per-window slot, so the read is cached by pid in
    /// [`sandbox_ids`](AppSystem::sandbox_ids) instead. `None` for a window whose client
    /// credentials are unknown.
    pub pid: Option<i32>,
    /// The toplevel title — an "Open Windows" row's label, falling back to the app
    /// name when empty (`_updateWindowsSection`, `appMenu.js:283`).
    pub title: Option<String>,
    /// Whether this window is demanding attention (`Mapped::is_urgent`).
    ///
    /// Carried per window rather than per app because it clears on focus, one window at a time.
    pub urgent: bool,

    /// `Mapped::get_focus_timestamp()`, standing in for
    /// `shell_app_get_last_user_time()` in [`shell_app_compare`]'s last clause.
    /// `None` (never focused) sorts last, as GNOME's `0` does.
    ///
    /// [`shell_app_compare`]: AppSystem::running
    pub last_focus: Option<Duration>,
}

/// An application with at least one open window — an entry of
/// `shell_app_system_get_running()` (`shell-app-system.c:508`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningApp {
    /// The resolved desktop id.
    pub id: String,
    /// Its windows, most recently used first — `shell_app_get_windows()`
    /// (`shell-app.c:733`), whose order is `shell_app_compare_windows` (`:692`)
    /// reduced the same way [`AppSystem::running`] reduces `shell_app_compare`.
    pub windows: Vec<RunningWindow>,
    /// The most recent `last_focus` among them — the app's user time.
    pub last_focus: Option<Duration>,
}

impl RunningApp {
    /// Whether any of the app's windows is demanding attention.
    pub fn is_urgent(&self) -> bool {
        self.windows.iter().any(|window| window.urgent)
    }

    /// Whether this app was synthesized from a window rather than resolved to a desktop entry —
    /// see [`is_window_backed`]. Its [`id`](Self::id) will not look up.
    pub fn is_window_backed(&self) -> bool {
        is_window_backed(&self.id)
    }

    /// What to call this app when its id resolves to no entry.
    ///
    /// One place for the fallback so that every surface showing a window-backed app agrees, and
    /// so that no consumer ever renders the raw `window:5`. GNOME reaches the same strings from
    /// `shell_app_get_name`, which falls back to the window title for an info-less app
    /// (`shell-app.c:186-197`).
    ///
    /// The title first, then the `app_id` it failed to resolve, and only then the bare id — an
    /// unresolvable window is precisely the case where the title is all the user has.
    pub fn fallback_label(&self) -> &str {
        let first = self.windows.first();
        let title = first
            .and_then(|w| w.title.as_deref())
            .filter(|t| !t.is_empty());
        title
            .or_else(|| first.and_then(|w| w.app_id.as_deref()))
            .unwrap_or(&self.id)
    }

    /// How many windows resolved to this app — `shell_app_get_n_windows()`.
    pub fn n_windows(&self) -> usize {
        self.windows.len()
    }
}

/// An app's lifecycle state — `ShellAppState` (`shell-app.h`).
///
/// **Derived, not stored.** GNOME keeps this on the app object and transitions it
/// imperatively (`shell_app_state_transition`, `shell-app.c:911`); we recompute it
/// from the two inputs that drive those transitions — an open startup sequence and
/// the window snapshot — so there is no edge to miss. See
/// [`AppSystem::app_state`] and `docs/fork/app-lifecycle-port.md` §3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Stopped,
    Starting,
    Running,
}

/// An in-flight launch — mutter's `MetaStartupSequence` (`startup-notification.c`).
///
/// On Wayland mutter mints the id itself rather than waiting for the client:
/// `meta_launch_context_get_startup_notify_id` (`meta-launch-context.c:158-184`)
/// generates a UUID, registers the sequence and exports the id to the child. We do
/// the same with an xdg-activation token, so the sequence exists from the moment we
/// spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSequence {
    /// The xdg-activation token handed to the child — the sequence's id. `None`
    /// when the launcher could not mint one (headless tests, GIO failures); the
    /// sequence then completes by app-id match only, which is mutter's
    /// `find_startup_sequence_by_wmclass` fallback (`display.c:2679`).
    pub token: Option<String>,
    /// Where the app's first window should open, if the launch asked for one —
    /// `meta_startup_sequence_get_workspace` (`startup-notification.c:364`),
    /// applied in `meta_display_apply_startup_properties` (`display.c:2720`).
    pub workspace: Option<WorkspaceId>,
    /// When the sequence times out — `STARTUP_TIMEOUT_MS`
    /// (`startup-notification.c:38`).
    pub expires: Duration,
}

/// mutter's `STARTUP_TIMEOUT_MS` (`startup-notification.c:38`).
pub const STARTUP_TIMEOUT: Duration = Duration::from_millis(15000);

/// The enumerate/lookup/search seam — GIO in production, a fake in tests.
pub trait AppCatalog {
    /// Every installed app, unfiltered (`g_app_info_get_all`).
    fn enumerate(&self) -> Vec<AppEntry>;
    /// Relevance-grouped search (`g_desktop_app_info_search`): outer vec is
    /// relevance tiers, inner vec is ids within a tier.
    fn search(&self, query: &str) -> Vec<Vec<String>>;
}

/// The launch seam — real GIO spawn in production, a recorder in tests so the
/// corpus never spawns a process.
pub trait AppLauncher {
    /// `token` is the xdg-activation token to export to the child, if one was
    /// minted (see [`LaunchContext`]).
    fn launch(
        &self,
        entry: &AppEntry,
        verb: &ResolvedLaunch,
        token: Option<&str>,
    ) -> Result<(), String>;
}

/// The compositor-owned application model. Owned on `Synoik`; fed from the
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
    /// Desktop id → index into [`installed`](Self::installed), rebuilt on every
    /// [`refresh`](Self::refresh). This is what makes [`lookup`](Self::lookup) a hash
    /// probe instead of a disk read — `shell_app_cache_get_info` is documented as
    /// exactly that, "a replacement for g_desktop_app_info_new() that will lookup the
    /// information from the cache instead of (re)loading from disk"
    /// (`shell-app-cache.c:344-370`), over the same `g_app_info_get_all()` list we
    /// enumerate into `installed`.
    ///
    /// It matters because the *misses* are the expensive ones: resolving one window
    /// walks the vendor-prefix ladder, so a handful of ids that do not exist used to
    /// cost a `stat`/`access` path walk each, on the compositor thread, every refresh.
    id_to_installed: HashMap<String, usize>,
    /// The raw window snapshot, kept so a catalog [`refresh`](Self::refresh) can
    /// re-resolve it (an app installed while its window is open then matches).
    windows: Vec<RunningWindow>,
    /// pid → its sandboxed app id, cached because the answer comes from a **file read**.
    ///
    /// [`recompute_running`](Self::recompute_running) runs on every window change, so an
    /// uncached read would put a `/proc` open on that path once per window per refresh.
    /// Misses are cached as `None` too — otherwise every ordinary, unsandboxed window pays a
    /// failed open forever, which is the common case.
    ///
    /// Keyed by pid rather than by window because that is what the answer depends on, and
    /// because a pid's sandbox cannot change under us.
    sandbox_ids: HashMap<i32, Option<String>>,
    /// `windows` resolved, grouped and ordered — `get_running()`'s answer.
    running: Vec<RunningApp>,
    /// Open startup sequences by desktop id — the `STARTING` half of
    /// [`app_state`](Self::app_state). Keyed by id rather than by token because
    /// that is the lookup every consumer does; the token is matched inside.
    starting: HashMap<String, StartupSequence>,
    /// Set whenever [`starting`](Self::starting) changes, cleared by
    /// [`take_state_changed`](Self::take_state_changed). GNOME emits
    /// `app-state-changed` from `shell_app_state_transition` (`shell-app.c:921`) and
    /// the dash redisplays on it (`dash.js:383`); the window half of that signal is
    /// covered by [`set_windows`](Self::set_windows)'s return, but a launch moves an
    /// app to STARTING without touching a single window.
    state_changed: bool,
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
            id_to_installed: HashMap::new(),
            windows: Vec::new(),
            sandbox_ids: HashMap::new(),
            running: Vec::new(),
            starting: HashMap::new(),
            state_changed: false,
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
            id_to_installed: HashMap::new(),
            windows: Vec::new(),
            sandbox_ids: HashMap::new(),
            running: Vec::new(),
            starting: HashMap::new(),
            state_changed: false,
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
            id_to_installed: HashMap::new(),
            windows: Vec::new(),
            sandbox_ids: HashMap::new(),
            running: Vec::new(),
            starting: HashMap::new(),
            state_changed: false,
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
    /// call it per ping: `Synoik::queue_app_catalog_reload` coalesces a burst onto one
    /// reload, the way gnome-shell's `ShellAppCache` does. (GNOME also runs the
    /// enumeration itself off-thread; we do not, yet.)
    ///
    /// Returns whether the enumeration actually differs from the one it replaces. Note that a ping
    /// is not proof of a change — glib's monitors fire for any write under a watched directory, and
    /// one arrives shortly after startup on a catalog that is already loaded. `reload_app_catalog`
    /// used to gate its downstream on this; it no longer does (see its doc for why), so nothing but
    /// the tests reads the return today. It is kept because "did this ping change anything" is the
    /// question those tests exist to ask.
    pub fn refresh(&mut self) -> bool {
        let installed = self.catalog.enumerate();
        let changed = installed != self.installed;
        self.installed = installed;
        self.index_installed();
        self.scan_startup_wm_class_to_id();
        // Re-resolve the open windows: an app installed while its window was
        // already mapped now matches.
        self.recompute_running();
        changed
    }

    /// Rebuild the desktop-id index over [`installed`](Self::installed).
    ///
    /// First entry wins, because `shell_app_cache_get_info` walks its list and returns
    /// the first id that matches (`shell-app-cache.c:361-367`) — the same "scan order
    /// is part of the behavior" property [`scan_startup_wm_class_to_id`] relies on.
    fn index_installed(&mut self) {
        let mut index = HashMap::with_capacity(self.installed.len());
        for (i, entry) in self.installed.iter().enumerate() {
            index.entry(entry.id.clone()).or_insert(i);
        }
        self.id_to_installed = index;
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
    /// `sandbox_id` is the app id the client's *sandbox* claims — [`sandboxed_app_id`]. It does
    /// two distinct jobs here, both of them GNOME's:
    ///
    /// 1. it **scopes** the `app_id` rungs, so a sandboxed window may only match a desktop entry
    ///    whose id starts `<sandbox_id>.` (`check_app_id_prefix`, `shell-window-tracker.c:126-134`,
    ///    applied to all four rungs at `:193-210`). That is what stops a host app claiming a
    ///    sandboxed app's `WM_CLASS`;
    /// 2. it is a **rung of its own** once those miss (`get_app_from_sandboxed_app_id`,
    ///    `:279-288`), which is the only thing that resolves a flatpak whose `app_id` is unrelated
    ///    to its desktop id — Wesnoth ships `org.wesnoth.Wesnoth.desktop`, declares no
    ///    `StartupWMClass`, and sets `app_id` to the bare `wesnoth`.
    ///
    /// That rung is an **exact** lookup, not a heuristic one: `get_app_from_id` (`:226-244`) does
    /// `lookup_app("<id>.desktop")` and nothing else — no canonicalization, no vendor prefixes.
    /// GNOME's comment says why it can afford to be exact ("a corresponding .desktop file is
    /// guaranteed to match", `:421-422`).
    ///
    /// Still unported, deliberately: the Snap half of the sandbox id
    /// (`meta_window_update_snap_id`, `window.c:992-1039`), the GApplication-id rung (`:432-436`),
    /// the pid map (`:438-440`) and startup-notification (`:442-462`). Each is a separate source
    /// of identity, not a fallback this one needs.
    pub fn app_for_window(&self, app_id: &str, sandbox_id: Option<&str>) -> Option<AppEntry> {
        // `check_app_id_prefix` returns TRUE for an unsandboxed window, so an absent sandbox id
        // must accept everything rather than reject everything.
        let allowed = |entry: &AppEntry| match sandbox_id {
            Some(sandbox) => entry.id.starts_with(&format!("{sandbox}.")),
            None => true,
        };

        self.lookup_startup_wmclass(app_id)
            .filter(&allowed)
            .or_else(|| self.lookup_desktop_wmclass(app_id).filter(&allowed))
            .or_else(|| self.lookup(&format!("{}.desktop", sandbox_id?)))
    }

    /// The sandboxed app id for a pid **from the cache only**, never reading.
    ///
    /// For the `&self` resolution sites, which must agree with the running set but cannot take
    /// `&mut` to fill a cache. Warm by construction:
    /// [`recompute_running`](Self::recompute_running) resolves every open window's pid on every
    /// window change, so a window that exists has an entry here. A miss therefore means "not a
    /// window we track", and answering `None` degrades to the pre-sandbox behavior rather than
    /// to a wrong match.
    pub fn sandbox_id_cached(&self, pid: Option<i32>) -> Option<&str> {
        self.sandbox_ids.get(&pid?)?.as_deref()
    }

    /// The sandboxed app id for a pid, reading through the cache.
    fn sandbox_id_for(&mut self, pid: Option<i32>) -> Option<String> {
        let pid = pid?;
        if let Some(cached) = self.sandbox_ids.get(&pid) {
            return cached.clone();
        }
        let id = sandboxed_app_id(pid);
        self.sandbox_ids.insert(pid, id.clone());
        id
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
        // Resolved up front, in one pass, because reading a sandbox id needs `&mut self` for the
        // cache while the loop below borrows `self.windows`. Cheap after the first refresh: every
        // pid, sandboxed or not, is answered from the map.
        let sandbox_ids: Vec<Option<String>> = self
            .windows
            .iter()
            .map(|window| window.pid)
            .collect::<Vec<_>>()
            .into_iter()
            .map(|pid| self.sandbox_id_for(pid))
            .collect();

        let mut apps: Vec<RunningApp> = Vec::new();
        for (window, sandbox_id) in self.windows.iter().zip(&sandbox_ids) {
            // A window that resolves to nothing still gets an app: GNOME's last resort is
            // `_shell_app_new_for_window` (`shell-window-tracker.c:469-471`), so
            // `get_app_for_window` never returns NULL and no window can fall out of the running
            // set. Losing one means it disappears from the app switcher and the dash while it is
            // still on screen — which is exactly what a flatpak whose `app_id` does not match its
            // desktop id used to do.
            let id = window
                .app_id
                .as_deref()
                .and_then(|app_id| self.app_for_window(app_id, sandbox_id.as_deref()))
                .map(|entry| entry.id)
                .unwrap_or_else(|| window_backed_id(window.id));

            match apps.iter_mut().find(|a| a.id == id) {
                Some(app) => {
                    app.windows.push(window.clone());
                    app.last_focus = app.last_focus.max(window.last_focus);
                }
                None => apps.push(RunningApp {
                    id,
                    windows: vec![window.clone()],
                    last_focus: window.last_focus,
                }),
            }
        }

        // Most recent first; never-focused (`None`) last, as GNOME's `0` sorts.
        // Windows within an app take the same order — `shell_app_get_windows()`
        // hands them out sorted by `shell_app_compare_windows`.
        for app in &mut apps {
            app.windows.sort_by(|a, b| {
                b.last_focus
                    .cmp(&a.last_focus)
                    .then_with(|| a.id.get().cmp(&b.id.get()))
            });
        }
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

    /// Whether `id` has at least one open window.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.iter().any(|a| a.id == id)
    }

    /// The running entry for `id`, if it has windows.
    pub fn running_app(&self, id: &str) -> Option<&RunningApp> {
        self.running.iter().find(|a| a.id == id)
    }

    /// `id`'s lifecycle state — `shell_app_get_state()`.
    ///
    /// The sequence is checked *first*, which is `shell_app_sync_running_state`'s
    /// "while STARTING, do nothing" (`shell-app.c:948`): a launching app does not
    /// read STOPPED merely because no window has arrived yet.
    pub fn app_state(&self, id: &str) -> AppState {
        if self.starting.contains_key(id) {
            AppState::Starting
        } else if self.is_running(id) {
            AppState::Running
        } else {
            AppState::Stopped
        }
    }

    /// Whether the running dot shows — every state but STOPPED
    /// (`AppIcon._updateRunningStyle`, `appDisplay.js:3007-3012`).
    /// Whether the app has a window demanding attention — what the dock pokes for.
    pub fn has_urgent_window(&self, id: &str) -> bool {
        self.running_app(id).is_some_and(RunningApp::is_urgent)
    }

    pub fn shows_running_dot(&self, id: &str) -> bool {
        self.app_state(id) != AppState::Stopped
    }

    /// Whether `id` can be asked for a new window — `shell_app_can_open_new_window`
    /// (`shell-app.c:601-672`).
    ///
    /// The ladder, in GNOME's order:
    ///
    /// 1. not RUNNING → stopped yes, starting no (`:610-613`) — exact;
    /// 2. an exported `app.new-window` GAction → yes. **Not implemented**: we have no action muxer
    ///    (`docs/fork/app-lifecycle-port.md` §5);
    /// 3. `SingleMainWindow`, then `X-GNOME-SingleWindow` — the *declared* answer, and the reason
    ///    it is checked with `has_key` rather than read as a boolean;
    /// 4. a `new-window` desktop action → yes;
    /// 5. a unique GtkApplication with no new-window → no. **Not implemented**: it reads the
    ///    window's GTK application object path and unique bus name, which reach mutter over
    ///    `gtk_shell1.set_dbus_properties`, a protocol we do not serve. This is the rung that
    ///    answers "no" for apps like System Monitor, which declare nothing and simply are
    ///    single-window GtkApplications;
    /// 6. otherwise yes — GNOME's own err-on-the-side-of-compatibility default (`:667-671`).
    ///
    /// So a missing rung 5 makes us say yes where GNOME says no, which shows up as a
    /// "New Window" row on an app that has no such thing.
    pub fn can_open_new_window(&self, id: &str) -> bool {
        match self.app_state(id) {
            AppState::Stopped => return true,
            AppState::Starting => return false,
            AppState::Running => (),
        }

        let Some(entry) = self.lookup(id) else {
            // "If the app doesn't have a desktop file, then nothing is possible" (`:624-626`).
            return false;
        };
        if let Some(single) = entry.single_main_window {
            return !single;
        }
        if entry.actions.iter().any(|a| a.id == "new-window") {
            return true;
        }

        // Rung 5 — the unique-GtkApplication heuristic — belongs here, and needs
        // `gtk_shell1.set_dbus_properties`. Without it we fall straight to GNOME's own
        // final answer, which is yes.
        true
    }

    /// Open a startup sequence for `id` — mutter registering one from its launch
    /// context (`meta-launch-context.c:158-184`). A second launch of the same app
    /// replaces the first, as re-keying the id does in mutter's table.
    pub fn begin_startup(
        &mut self,
        id: &str,
        token: Option<String>,
        workspace: Option<WorkspaceId>,
        now: Duration,
    ) {
        self.starting.insert(
            id.to_owned(),
            StartupSequence {
                token,
                workspace,
                expires: now + STARTUP_TIMEOUT,
            },
        );
        self.state_changed = true;
    }

    /// Complete the sequence a mapping window belongs to, returning the workspace
    /// that sequence asked for — `meta_display_apply_startup_properties`
    /// (`display.c:2661-2731`). The window's own activation token is matched first;
    /// failing that we match the resolved app id, which is mutter's
    /// `find_startup_sequence_by_wmclass` fallback.
    pub fn complete_startup(
        &mut self,
        app_id: Option<&str>,
        token: Option<&str>,
        now: Duration,
    ) -> Option<WorkspaceId> {
        self.expire_startups(now);

        let by_token = token.and_then(|token| {
            self.starting
                .iter()
                .find(|(_, seq)| seq.token.as_deref() == Some(token))
                .map(|(id, _)| id.clone())
        });
        let key = by_token.or_else(|| {
            let app_id = app_id?;
            // Unresolvable ids still key the table (that is what `launch` inserts
            // when the catalog is a fake), so fall back to the raw string.
            let desktop_id = self
                // A startup sequence is keyed by the id we launched, not by a window, so there
                // is no client pid to ask about a sandbox here.
                .app_for_window(app_id, None)
                .map(|entry| entry.id)
                .unwrap_or_else(|| app_id.to_owned());
            self.starting
                .contains_key(&desktop_id)
                .then_some(desktop_id)
        })?;

        let seq = self.starting.remove(&key);
        self.state_changed |= seq.is_some();
        seq.and_then(|seq| seq.workspace)
    }

    /// Drop sequences past `STARTUP_TIMEOUT` — `startup_sequence_timeout`
    /// (`startup-notification.c:483-512`). Returns whether any went away, since
    /// that is an app-state change the dash and menus want to see.
    pub fn expire_startups(&mut self, now: Duration) -> bool {
        let before = self.starting.len();
        self.starting.retain(|_, seq| seq.expires > now);
        let changed = self.starting.len() != before;
        self.state_changed |= changed;
        changed
    }

    /// Whether an app changed state since this was last called, and clear the flag —
    /// our `app-state-changed` (`shell-app.c:921`), for the surfaces that redisplay
    /// on it. Only the *sequence* half: the window half is
    /// [`set_windows`](Self::set_windows)'s return value.
    pub fn take_state_changed(&mut self) -> bool {
        std::mem::take(&mut self.state_changed)
    }

    /// The apps with an open startup sequence — for the corpus, and for whoever
    /// schedules the expiry sweep.
    pub fn starting_apps(&self) -> impl Iterator<Item = &str> {
        self.starting.keys().map(|s| s.as_str())
    }

    /// The installed apps that should be shown (`g_app_info_should_show`) — the
    /// view every dash/grid/search consumer wants.
    pub fn installed(&self) -> impl Iterator<Item = &AppEntry> {
        self.installed.iter().filter(|e| e.should_show)
    }

    /// A single app by desktop id, unfiltered — `shell_app_system_lookup_app`
    /// (`shell-app-system.c:340-358`), which resolves through the app *cache*, never
    /// off disk.
    ///
    /// So this answers from the last enumeration, not from the filesystem: an id that
    /// is not in `g_app_info_get_all()` does not resolve here even if a `.desktop`
    /// file for it exists (a `Hidden=true` entry, say). That is GNOME's answer too —
    /// its cache is that same list — and it is what keeps a lookup off the compositor
    /// thread's critical path. See [`id_to_installed`](Self::id_to_installed).
    pub fn lookup(&self, id: &str) -> Option<AppEntry> {
        let i = *self.id_to_installed.get(id)?;
        self.installed.get(i).cloned()
    }

    /// An app by the id apps identify themselves with — a desktop file's basename **without**
    /// `.desktop`: the fdo `desktop-entry` hint, MPRIS's `DesktopEntry`, an app name.
    ///
    /// Every reference call site spells this the same way — `lookup_app(`${id}.desktop`)`
    /// (`notificationDaemon.js:80,83`, `mpris.js:168`) — so the `.desktop` convention lives here
    /// once instead of at each caller.
    ///
    /// The suffix is appended **unconditionally, like GNOME's**: an id that already ends in
    /// `.desktop` becomes `foo.desktop.desktop` and resolves to nothing. That looks like a bug to
    /// fix and is not one — the fdo spec defines `desktop-entry` as the name *without* the
    /// extension, so tolerating a suffixed id would resolve apps GNOME leaves unresolved, and the
    /// notification/media header would then diverge on exactly the inputs this exists to match.
    pub fn lookup_desktop_id(&self, id: &str) -> Option<AppEntry> {
        if id.is_empty() {
            return None;
        }
        self.lookup(&format!("{id}.desktop"))
    }

    /// The app a notification came from — `FdoNotificationDaemon._getApp`
    /// (`js/ui/notificationDaemon.js:74-86`), which is what gives a notification's *source* its
    /// title and icon: `get title() { app?.get_name() ?? appName }`, `get icon() {
    /// app?.get_icon() ?? appIcon }` (`:396-399`). The `app_icon` call parameter is only the
    /// fallback for when no app resolves — which is why a browser's web notification, sent with an
    /// empty `app_icon` and a `desktop-entry` hint, still shows the browser's own logo.
    ///
    /// GNOME's steps, in order: the sender's pid, then the `desktop-entry` hint, then the app
    /// name.
    ///
    /// **Divergence — the pid step is missing.** GNOME asks
    /// `WindowTracker.get_app_from_pid(pid)` first and we have no pid→app map; ours is
    /// [`app_for_window`](Self::app_for_window), keyed by `app_id`. So an app that sends neither a
    /// usable hint nor a name matching its desktop id falls back to `app_icon` where GNOME would
    /// still have found it. `NotifyRequest` already carries the pid, so this is a wiring job the
    /// day window↔pid tracking exists, not a redesign.
    pub fn app_for_notification(
        &self,
        desktop_entry: Option<&str>,
        app_name: &str,
    ) -> Option<AppEntry> {
        desktop_entry
            .and_then(|e| self.lookup_desktop_id(e))
            .or_else(|| self.lookup_desktop_id(app_name))
    }

    /// An app folder's members, in display order — `FolderView._loadApps`
    /// (`appDisplay.js:2164-2199`).
    ///
    /// The folder's explicitly-placed `apps` come first, in their saved order, then
    /// every *shown* app whose `Categories` intersect the folder's `categories`
    /// (`_listsIntersect`, `appDisplay.js:86-93`). A candidate is dropped when it is
    /// in `excluded-apps`, is not installed, is a favorite (favorites live in the
    /// dash, never in the grid), or is already in the list.
    ///
    /// Parental controls are the one filter of GNOME's we do not apply: we have no
    /// `ParentalControlsManager`, so a malcontent-hidden app is visible everywhere
    /// in our grid, not just inside folders.
    ///
    /// An empty result means the folder is not displayed at all
    /// (`appDisplay.js:1523-1527`); the caller decides that.
    pub fn folder_members(&self, folder: &crate::gnome::AppFolder) -> Vec<AppEntry> {
        let mut members: Vec<AppEntry> = Vec::new();
        let push = |id: &str, members: &mut Vec<AppEntry>| {
            if folder.excluded_apps.iter().any(|e| e == id) || self.is_favorite(id) {
                return;
            }
            if members.iter().any(|m| m.id == id) {
                return;
            }
            if let Some(app) = self.lookup(id) {
                members.push(app);
            }
        };
        for id in &folder.apps {
            push(id, &mut members);
        }
        if !folder.categories.is_empty() {
            let matched: Vec<String> = self
                .installed()
                .filter(|e| e.categories.iter().any(|c| folder.categories.contains(c)))
                .map(|e| e.id.clone())
                .collect();
            for id in matched {
                push(&id, &mut members);
            }
        }
        members
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
    /// without spawning, and — on success — opens the startup sequence that puts
    /// the app in [`AppState::Starting`], which is what mutter's launch context
    /// does as a side effect of handing out a startup id.
    pub fn launch(
        &mut self,
        id: &str,
        mode: LaunchMode,
        ctx: &LaunchContext,
    ) -> Result<(), LaunchError> {
        let entry = self.lookup(id).ok_or(LaunchError::UnknownApp)?;
        let verb = resolve_launch(mode, &entry);
        self.launcher
            .launch(&entry, &verb, ctx.token.as_deref())
            .map_err(LaunchError::Failed)?;
        self.begin_startup(&entry.id, ctx.token.clone(), ctx.workspace, ctx.now);
        Ok(())
    }
}

/// The launch context — mutter's `MetaLaunchContext` reduced to what crosses our
/// seam (`meta-launch-context.c`). Built by the caller, which is the only place
/// with an activation state to mint a token from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchContext {
    /// The xdg-activation token exported to the child as `XDG_ACTIVATION_TOKEN` /
    /// `DESKTOP_STARTUP_ID`, and the startup sequence's id.
    pub token: Option<String>,
    /// The workspace the app's first window should open on, if this launch asked
    /// for one (`meta_launch_context_set_workspace`).
    pub workspace: Option<WorkspaceId>,
    /// Now, for the sequence's expiry.
    pub now: Duration,
}

impl LaunchContext {
    /// A context with no token and no workspace — what a test, or a launch that
    /// could not mint a token, uses.
    pub fn bare(now: Duration) -> Self {
        Self {
            token: None,
            workspace: None,
            now,
        }
    }
}

/// Resolve a launch intent to a concrete verb. `Activate` of a stopped app is a
/// plain launch (`shell_app_activate` stopped branch); `NewWindow` prefers the
/// `new-window` desktop action, else falls back to relaunching
/// (`shell_app_open_new_window`). The running-app action-group path is S6.
fn resolve_launch(mode: LaunchMode, entry: &AppEntry) -> ResolvedLaunch {
    match mode {
        LaunchMode::Activate => ResolvedLaunch::Default,
        LaunchMode::Action(action) => ResolvedLaunch::Action(action),
        LaunchMode::NewWindow => {
            if entry.actions.iter().any(|a| a.id == "new-window") {
                ResolvedLaunch::Action("new-window".to_string())
            } else {
                ResolvedLaunch::Default
            }
        }
    }
}

/// The synthetic id of a window-backed app — GNOME's `window:%d` over the window's stable
/// sequence (`shell-app.c:884`), with [`MappedId`] standing in for that sequence.
///
/// It deliberately cannot collide with a desktop id, which always ends `.desktop`.
pub fn window_backed_id(window: MappedId) -> String {
    format!("window:{}", window.get())
}

/// Whether an id came from [`window_backed_id`] rather than from a desktop entry.
///
/// Consumers need this because such an id resolves to no [`AppEntry`]: there is no name, no icon
/// and no actions behind it. GNOME expresses the same split as `app->info == NULL`
/// (`shell_app_get_id`, `shell-app.c:172-177`).
pub fn is_window_backed(id: &str) -> bool {
    id.starts_with("window:")
}

/// The app id a **flatpak** sandbox claims for `pid`, or `None` for anything else.
///
/// mutter's `meta_window_update_flatpak_id` (`window.c:969-989`): read
/// `/proc/<pid>/root/.flatpak-info` as a key file and take `[Application] name`. Going through
/// `/proc/<pid>/root` is the point — it resolves inside the sandbox's own mount namespace, so
/// the file is the *client's* `.flatpak-info` and cannot be spoofed by a host process putting one
/// on its own filesystem. It reads fine unprivileged (verified against a running flatpak).
///
/// Not a `Result`: every failure here — no such pid, not sandboxed, unreadable, malformed — means
/// the same thing to the caller, "no sandbox id", and an unsandboxed window is the common case
/// rather than an error.
///
/// Parsed by hand rather than with a key-file crate: we need exactly one key, the section headers
/// are the only structure that matters, and the file is written by flatpak itself.
fn sandboxed_app_id(pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/root/.flatpak-info")).ok()?;
    parse_flatpak_app_id(&text)
}

/// `[Application] name` out of a `.flatpak-info`, split from the read so it can be tested: a
/// `/proc/<pid>/root` path cannot be fabricated for a made-up pid.
fn parse_flatpak_app_id(text: &str) -> Option<String> {
    let mut in_application = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // The key is only meaningful inside [Application] — `name` also names the *runtime*
            // under [Runtime]. A later section ends the block.
            in_application = section == "Application";
            continue;
        }
        if in_application {
            if let Some(name) = line.strip_prefix("name=") {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
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
    // `has_key` first: an absent key must stay `None`, since a *present* `false` is a
    // positive statement that the app can open new windows.
    let single_main_window = info.downcast_ref::<DesktopAppInfo>().and_then(|d| {
        ["SingleMainWindow", "X-GNOME-SingleWindow"]
            .into_iter()
            .find(|key| d.has_key(key))
            .map(|key| d.boolean(key))
    });
    let icon = icon_ref(info.icon(), &id);
    let startup_wm_class = info
        .downcast_ref::<DesktopAppInfo>()
        .and_then(|d| d.startup_wm_class())
        .map(|s| s.to_string());
    // `Categories` is a trailing-semicolon list; GNOME splits on `;` and keeps
    // whatever falls out (`_getCategories`, `appDisplay.js:79-83`), so an empty
    // tail is the only thing worth dropping.
    let categories = info
        .downcast_ref::<DesktopAppInfo>()
        .and_then(|d| d.categories())
        .map(|c| {
            c.split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    Some(AppEntry {
        id,
        name: info.name().to_string(),
        description: info.description().map(|s| s.to_string()),
        commandline: info.commandline(),
        actions,
        single_main_window,
        should_show: info.should_show(),
        icon,
        startup_wm_class,
        categories,
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

    fn search(&self, query: &str) -> Vec<Vec<String>> {
        DesktopAppInfo::search(query)
            .into_iter()
            .map(|group| group.iter().map(|s| s.to_string()).collect())
            .collect()
    }
}

/// The production launcher. Re-resolves the desktop entry (thread-safe,
/// cache-backed) and launches through [`scoped_launch_context`], so the app lands
/// in its own systemd scope, carrying the startup-notification token.
struct GioLauncher;

impl AppLauncher for GioLauncher {
    fn launch(
        &self,
        entry: &AppEntry,
        verb: &ResolvedLaunch,
        token: Option<&str>,
    ) -> Result<(), String> {
        let desktop = DesktopAppInfo::new(&entry.id)
            .ok_or_else(|| format!("no desktop entry: {}", entry.id))?;
        // The scope is named for the entry we were asked to launch, not for whatever GIO ends up
        // handing back: an action is launched as a synthesized entry with no id of its own.
        let context = scoped_launch_context(token, &entry.id);
        match verb {
            ResolvedLaunch::Default => launch_default(&desktop, &context),
            ResolvedLaunch::Action(name) => launch_action(&desktop, name, &context),
        }
    }
}

/// Launch an entry's default verb with a signal mask the app can actually live with.
///
/// **The child must not inherit the compositor's blocked SIGTERM.** `signals::block_early` blocks
/// SIGHUP/SIGINT/SIGTERM process-wide so the calloop `Signals` source can own them, and a blocked
/// mask survives both `fork` and `execve` — so an app forked out of this process starts life unable
/// to receive the signal that asks it to quit. Nothing can then stop it but SIGKILL: measured
/// 2026-08-03, OBS, Firefox and Epiphany each sat through a five-second logout in silence and were
/// killed, while the same builds on a GNOME 50 session reacted in under a millisecond. The
/// `spawn`/`spawn-at-startup` path already knew this and clears the mask in `pre_exec`
/// (`utils::spawning`); this is the path that did not.
///
/// `g_app_info_launch` offers no hook between fork and exec, so the default verb goes through
/// `launch_uris_as_manager_with_fds`, whose `user_setup` runs in the child in exactly that window.
/// Its other hook, `pid_callback`, is deliberately unused: `as_manager` still emits `launched` on
/// the context, so [`scoped_launch_context`] keeps being the single place a scope is started.
fn launch_default(desktop: &DesktopAppInfo, context: &gio::AppLaunchContext) -> Result<(), String> {
    // A `DBusActivatable` app is started by the bus, so it never inherits anything of ours — and
    // `as_manager` would spawn it directly and lose the activation. GIO keeps that one.
    if desktop.boolean("DBusActivatable") {
        return desktop
            .launch(&[], Some(context))
            .map_err(|e| e.to_string());
    }

    desktop
        .launch_uris_as_manager_with_fds(
            &[],
            Some(context),
            // **Not** `DO_NOT_REAP_CHILD`, which is the tempting mistake here: it makes the app
            // our direct child, and nothing in the compositor ever reaps one — we run
            // calloop, so the child watch GIO hangs on the thread-default
            // `GMainContext` is never iterated, and every app the user launches would
            // leave a zombie behind when it quits. Without it glib spawns through an
            // intermediate fork, so the app is reparented to init and reaped
            // there. The `launched` signal still reports the **app's** pid either way — verified,
            // it is the grandchild's, not the intermediate's — so the scope gets a live process.
            glib::SpawnFlags::SEARCH_PATH,
            Some(Box::new(|| {
                // In the child, after fork, before exec. Failure here would hand the app the same
                // deaf mask, so say so — but there is no way to report it except the log.
                if let Err(err) = crate::utils::signals::unblock_all() {
                    eprintln!("could not reset the child's signal mask: {err}");
                }
            })),
            None,
            None::<std::fs::File>,
            None::<std::fs::File>,
            None::<std::fs::File>,
        )
        .map_err(|e| e.to_string())
}

/// Launch one of an entry's desktop actions ("New Window", "New Private Window", …).
///
/// `g_desktop_app_info_launch_action` has no `as_manager` variant, so it has nowhere to hang the
/// child setup [`launch_default`] depends on — an action launched through it comes up with our
/// blocked SIGTERM and can only be SIGKILLed. So the action is rebuilt as a one-off entry and sent
/// back through `launch_default`, which is the only place that knows how to fork safely.
///
/// A `DBusActivatable` app is exempt and keeps GIO's own path: its actions go out as
/// `org.freedesktop.Application.ActivateAction` and nothing is forked here at all.
fn launch_action(
    desktop: &DesktopAppInfo,
    action: &str,
    context: &gio::AppLaunchContext,
) -> Result<(), String> {
    if desktop.boolean("DBusActivatable") {
        desktop.launch_action(action, Some(context));
        return Ok(());
    }

    match action_as_entry(desktop, action) {
        Some(entry) => launch_default(&entry, context),
        None => {
            // Launching it deaf beats not launching it: the mask costs the user a clean quit at
            // logout, refusing costs them the thing they clicked.
            warn!("could not rebuild the {action:?} action; launching it with our signal mask");
            desktop.launch_action(action, Some(context));
            Ok(())
        }
    }
}

/// An entry's desktop action, as a standalone [`DesktopAppInfo`].
///
/// The action group carries only what makes it *this* action (`Exec`, `Name`, sometimes `Icon`);
/// everything about how to *run* it — the working directory, whether it wants a terminal — belongs
/// to the parent entry and is carried over, as `g_desktop_app_info_launch_action` does.
fn action_as_entry(desktop: &DesktopAppInfo, action: &str) -> Option<DesktopAppInfo> {
    const ENTRY: &str = "Desktop Entry";

    let source = glib::KeyFile::new();
    source
        .load_from_file(desktop.filename()?, glib::KeyFileFlags::NONE)
        .ok()?;

    let group = format!("Desktop Action {action}");
    let built = glib::KeyFile::new();
    built.set_string(ENTRY, "Type", "Application");
    built.set_string(ENTRY, "Exec", source.string(&group, "Exec").ok()?.as_str());
    built.set_string(
        ENTRY,
        "Name",
        source
            .string(&group, "Name")
            .map(|name| name.to_string())
            .unwrap_or_else(|_| desktop.name().to_string())
            .as_str(),
    );

    // `Icon` may be overridden per action; the rest never is.
    for (key, from_action) in [
        ("Icon", true),
        ("Path", false),
        ("Terminal", false),
        ("StartupNotify", false),
        ("StartupWMClass", false),
    ] {
        let value = from_action
            .then(|| source.string(&group, key).ok())
            .flatten()
            .or_else(|| source.string(ENTRY, key).ok());
        if let Some(value) = value {
            built.set_string(ENTRY, key, value.as_str());
        }
    }

    DesktopAppInfo::from_keyfile(&built)
}

/// A launch context that moves whatever it launches into its own systemd scope.
///
/// GNOME builds the same thing in `shell-global.c:1221` (`create_app_launch_context`) and hooks
/// `launched` at line 1206; see [`crate::utils::spawning::start_app_scope`] for why the scope
/// matters.
///
/// The startup notification rides along as `token`. mutter gets it for free by
/// overriding `get_startup_notify_id` on its own `GAppLaunchContext` subclass
/// (`meta-launch-context.c:129`), which GIO then exports to the child; a plain
/// `GAppLaunchContext` has no such id, so we set the two environment variables GIO
/// would have set. `DESKTOP_STARTUP_ID` goes along with it because
/// `g_desktop_app_info` sets both, and XWayland clients (through
/// xwayland-satellite) only understand the latter.
fn scoped_launch_context(token: Option<&str>, id: &str) -> gio::AppLaunchContext {
    let context = gio::AppLaunchContext::new();
    if let Some(token) = token {
        context.setenv("XDG_ACTIVATION_TOKEN", token);
        context.setenv("DESKTOP_STARTUP_ID", token);
    }
    let id = id.to_owned();
    context.connect_launched(move |_context, info, platform_data| {
        let Some(pid) = launched_pid(platform_data) else {
            return;
        };
        // GNOME: "If pid == 0 the application was launched through D-Bus activation, therefore
        // it's already in its own unit" (`shell-global.c:1194`).
        if pid == 0 {
            return;
        }

        // The caller's id, falling back the way GNOME does only if we somehow have none.
        let id = if id.is_empty() {
            info.id()
                .map(|id| id.to_string())
                .unwrap_or_else(|| info.executable().to_string_lossy().into_owned())
        } else {
            id.clone()
        };
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
    fn search(&self, _query: &str) -> Vec<Vec<String>> {
        Vec::new()
    }
}

/// The disconnected launcher — never spawns.
struct NullLauncher;

impl AppLauncher for NullLauncher {
    fn launch(
        &self,
        entry: &AppEntry,
        _verb: &ResolvedLaunch,
        _token: Option<&str>,
    ) -> Result<(), String> {
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
    fn search(&self, _query: &str) -> Vec<Vec<String>> {
        self.search_result.borrow().clone()
    }
}

/// One recorded launch: the resolved entry, the verb, and the activation token
/// that would have been exported to the child.
#[cfg(test)]
pub type RecordedLaunch = (AppEntry, ResolvedLaunch, Option<String>);

/// A launcher that records [`RecordedLaunch`]es instead of spawning.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct RecordingLauncher {
    pub calls: std::rc::Rc<std::cell::RefCell<Vec<RecordedLaunch>>>,
}

#[cfg(test)]
impl AppLauncher for RecordingLauncher {
    fn launch(
        &self,
        entry: &AppEntry,
        verb: &ResolvedLaunch,
        token: Option<&str>,
    ) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push((entry.clone(), verb.clone(), token.map(|t| t.to_owned())));
        Ok(())
    }
}

/// The real GIO-enumerated apps — a test helper so other modules (the icon
/// loader) can exercise real `AppEntry` icon descriptors without a live `Synoik`.
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
            single_main_window: None,
            should_show: true,
            icon: AppIconRef::Fallback,
            startup_wm_class: None,
            categories: Vec::new(),
        }
    }

    /// The same, declaring `Categories` — what a category-based folder matches on.
    pub fn fake_in_categories(id: &str, name: &str, categories: &[&str]) -> Self {
        Self {
            categories: categories.iter().map(|c| c.to_string()).collect(),
            ..Self::fake(id, name)
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
    /// An app must not inherit the compositor's blocked SIGTERM, or nothing short of SIGKILL can
    /// ever ask it to quit — see [`super::launch_default`] for how that shipped and what it cost.
    /// Both doors are covered: the default verb, and a desktop *action*, which reaches the fork by
    /// a different route and was left deaf for one commit longer.
    ///
    /// The mask is set on *this thread* rather than the process: `pthread_sigmask` is thread-local,
    /// so a parallel test binary is unharmed, and the fork inherits from the forking thread either
    /// way — which is precisely the compositor's situation.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_launched_app_can_be_asked_to_quit() {
        use std::io::Write as _;

        use gio_unix::prelude::*;

        use super::{launch_action, launch_default, DesktopAppInfo};

        // Same three the compositor blocks in `signals::block_early`.
        let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTERM);
            libc::sigaddset(&mut blocked, libc::SIGINT);
            libc::sigaddset(&mut blocked, libc::SIGHUP);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()),
                0
            );
        }

        let path =
            std::env::temp_dir().join(format!("synoik-masktest-{}.desktop", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        // `sleep` rather than anything of ours: the test is about the mask the child starts with,
        // and a process that would exit on its own could pass this by being gone already.
        file.write_all(
            b"[Desktop Entry]\nType=Application\nName=synoik mask test\nExec=sleep 30\n\
Actions=newwin;\n\n\
              [Desktop Action newwin]\nName=New Window\nExec=sleep 31\n",
        )
        .unwrap();
        drop(file);

        let desktop = DesktopAppInfo::from_filename(&path).unwrap();
        assert_eq!(
            desktop.list_actions().len(),
            1,
            "the action group did not parse, so the action arm would prove nothing"
        );

        // The mask a child of `launch` comes up with, as `SigBlk`.
        let child_mask = |launch: &dyn Fn(&gio::AppLaunchContext) -> Result<(), String>| {
            let context = gio::AppLaunchContext::new();
            let pid = std::rc::Rc::new(std::cell::Cell::new(0i32));
            let seen = pid.clone();
            context.connect_launched(move |_, _, platform_data| {
                if let Some(pid) = super::launched_pid(platform_data) {
                    seen.set(pid);
                }
            });

            launch(&context).unwrap();

            let pid = pid.get();
            assert_ne!(pid, 0, "the launch reported no pid");
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap()
                    .trim()
                    .to_owned()
            };
            let blk = field("SigBlk:");
            let ppid: i32 = field("PPid:").parse().unwrap();

            // Nothing here reaps: the compositor runs calloop, so GIO's child watch never gets
            // iterated. The app must therefore not be our direct child, or it zombies on quit.
            assert_ne!(
                ppid,
                std::process::id() as i32,
                "the app was launched as our own child; it would zombie when it exits"
            );

            // Not ours to wait for, so kill and move on rather than blocking on a `waitpid` that
            // would only return `ECHILD`.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            u64::from_str_radix(&blk, 16).unwrap()
        };

        let by_verb = child_mask(&|context| launch_default(&desktop, context));
        let by_action = child_mask(&|context| launch_action(&desktop, "newwin", context));

        let _ = std::fs::remove_file(&path);
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &blocked, std::ptr::null_mut()) };

        for (blk, how) in [
            (by_verb, "the default verb"),
            (by_action, "a desktop action"),
        ] {
            for (signal, name) in [
                (libc::SIGTERM, "SIGTERM"),
                (libc::SIGINT, "SIGINT"),
                (libc::SIGHUP, "SIGHUP"),
            ] {
                assert_eq!(
                    blk & (1 << (signal - 1)),
                    0,
                    "{name} is blocked in a child launched through {how} \
                     (SigBlk {blk:#x}); it could not be asked to quit"
                );
            }
        }
    }

    /// A ping is not proof of a change. glib's monitors fire for any write under a
    /// watched directory and one lands a few seconds into every session, so the
    /// reload has to check rather than trust the signal — everything it re-derives
    /// downstream is either wasted or, in the icon caches' case, destructive.
    /// A notification's source presents as its *app*, resolved from the `desktop-entry` hint or
    /// the app name (`_getApp`, `js/ui/notificationDaemon.js:74-86`).
    ///
    /// This is what a browser's web notification needs: it arrives with an **empty `app_icon`**
    /// and identifies itself only by the hint, so without this resolution the card falls back to
    /// the generic executable glyph — which is exactly what it did.
    #[test]
    fn a_notification_resolves_its_app_from_the_hint_or_the_name() {
        let catalog = FakeCatalog::new(vec![
            AppEntry::fake("firefox.desktop", "Firefox"),
            AppEntry::fake("chromium-browser.desktop", "Chromium"),
        ]);
        let system =
            AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));

        // The hint wins.
        let by_hint = system.app_for_notification(Some("firefox"), "");
        assert_eq!(by_hint.map(|a| a.name), Some("Firefox".to_owned()));
        assert_eq!(
            system
                .app_for_notification(Some("chromium-browser"), "")
                .map(|a| a.name),
            Some("Chromium".to_owned())
        );

        // `.desktop` is appended unconditionally, exactly as the reference does
        // (`notificationDaemon.js:80`, `mpris.js:168`), so an already-suffixed id resolves to
        // nothing. Asserted on purpose: "tolerate the suffix" is a tempting robustness fix that
        // would make us resolve apps GNOME does not.
        assert!(system
            .app_for_notification(Some("firefox.desktop"), "")
            .is_none());

        // No hint: fall back to the app name, GNOME's third step.
        assert_eq!(
            system.app_for_notification(None, "firefox").map(|a| a.name),
            Some("Firefox".to_owned())
        );

        // An unresolvable hint must not swallow the name fallback.
        assert_eq!(
            system
                .app_for_notification(Some("not-installed"), "firefox")
                .map(|a| a.name),
            Some("Firefox".to_owned())
        );

        // Nothing matches: the caller keeps the `app_icon` parameter it was given.
        assert!(system
            .app_for_notification(Some("nope"), "also-nope")
            .is_none());
        assert!(system.app_for_notification(None, "").is_none());

        // The MPRIS card's step is the same primitive with no fallbacks — `mpris.js:167-172`
        // consults `DesktopEntry` and nothing else, so a player that publishes none stays
        // unresolved rather than falling back to its `Identity`.
        assert_eq!(
            system.lookup_desktop_id("firefox").map(|a| a.name),
            Some("Firefox".to_owned())
        );
        assert!(system.lookup_desktop_id("").is_none());
    }

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
        // Every enumerated app resolves by id — over *real* catalog data, which is
        // where duplicate and oddly-shaped ids actually occur. `lookup` answers from
        // the enumeration index, not from disk, so this is the round trip that used to
        // be `catalog.lookup`.
        let first = all[0].clone();
        let system = AppSystem::with_parts(
            Box::new(FakeCatalog::new(all.clone())),
            Box::new(NullLauncher),
        );
        let looked_up = system
            .lookup(&first.id)
            .expect("lookup of an enumerated app");
        assert_eq!(looked_up.id, first.id);
        assert_eq!(looked_up.name, first.name);
        assert!(
            system.lookup("definitely-not-installed.desktop").is_none(),
            "an id outside the enumeration must not resolve"
        );

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
            .launch(
                "org.example.App.desktop",
                LaunchMode::Activate,
                &LaunchContext::bare(Duration::ZERO),
            )
            .expect("launch");
        {
            let calls = recorder.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0.id, "org.example.App.desktop");
            assert_eq!(calls[0].0.commandline, Some(PathBuf::from("app %U")));
            assert_eq!(calls[0].1, ResolvedLaunch::Default);
        }

        assert_eq!(
            system.launch(
                "nope.desktop",
                LaunchMode::Activate,
                &LaunchContext::bare(Duration::ZERO)
            ),
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

        let ctx = LaunchContext::bare(Duration::ZERO);
        system
            .launch("w.desktop", LaunchMode::NewWindow, &ctx)
            .unwrap();
        system
            .launch("p.desktop", LaunchMode::NewWindow, &ctx)
            .unwrap();

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

    // ---- App folders ----

    /// `_loadApps` builds a folder from two sources with different rules: the
    /// explicit `apps` list, verbatim and in order, then a category sweep over the
    /// shown apps. Every candidate from either source runs the same gauntlet —
    /// `excluded-apps`, favorites, not-installed, already-there — so a category
    /// match can neither duplicate an explicit member nor resurrect an excluded one.
    #[test]
    fn a_folder_takes_its_explicit_apps_first_then_its_categories() {
        let mut system = system_with(vec![
            AppEntry::fake_in_categories("terminal.desktop", "Terminal", &["System", "Utility"]),
            AppEntry::fake_in_categories("disks.desktop", "Disks", &["Utility"]),
            AppEntry::fake_in_categories("games.desktop", "Games", &["Game"]),
            AppEntry::fake_in_categories("boring.desktop", "Boring", &["Utility"]),
            AppEntry::fake_in_categories("faved.desktop", "Faved", &["Utility"]),
            AppEntry::fake("placed.desktop", "Placed"),
        ]);
        system.set_favorites(vec!["faved.desktop".to_owned()]);

        let folder = crate::gnome::AppFolder {
            id: "Utilities".to_owned(),
            name: "Utilities".to_owned(),
            categories: vec!["Utility".to_owned()],
            // `terminal` is listed *and* matches the category: it must appear once,
            // in the explicit list's position, not the sweep's.
            apps: vec![
                "placed.desktop".to_owned(),
                "terminal.desktop".to_owned(),
                "missing.desktop".to_owned(),
            ],
            excluded_apps: vec!["boring.desktop".to_owned()],
        };

        let members: Vec<String> = system
            .folder_members(&folder)
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            members,
            ["placed.desktop", "terminal.desktop", "disks.desktop"]
        );
    }

    /// A folder with no categories does not sweep at all — and one whose members
    /// all resolve away is empty, which is what makes the caller hide it
    /// (`appDisplay.js:1523-1527`).
    #[test]
    fn a_folder_can_resolve_to_nothing() {
        let mut system = system_with(vec![AppEntry::fake_in_categories(
            "faved.desktop",
            "Faved",
            &["Utility"],
        )]);
        system.set_favorites(vec!["faved.desktop".to_owned()]);

        let folder = crate::gnome::AppFolder {
            id: "Utilities".to_owned(),
            categories: vec!["Utility".to_owned()],
            ..Default::default()
        };
        assert!(system.folder_members(&folder).is_empty());
    }

    // ---- Window ↔ app matching (S6) ----

    fn system_with(apps: Vec<AppEntry>) -> AppSystem {
        AppSystem::with_parts(Box::new(FakeCatalog::new(apps)), Box::new(NullLauncher))
    }

    fn win(app_id: &str, secs: Option<u64>) -> RunningWindow {
        win_with_id(MappedId::next(), app_id, secs)
    }

    /// A window with a *chosen* id — for re-stating the same snapshot, which `win` cannot do
    /// because it mints a fresh id on every call.
    fn win_with_id(id: MappedId, app_id: &str, secs: Option<u64>) -> RunningWindow {
        RunningWindow {
            pid: None,
            id,
            app_id: Some(app_id.to_owned()),
            title: None,
            urgent: false,
            last_focus: secs.map(Duration::from_secs),
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
            system.app_for_window("Emacs", None).map(|e| e.id),
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
            system
                .app_for_window("org.example.Foo.Bar", None)
                .map(|e| e.id),
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
            system.app_for_window("Fedora Eclipse", None).map(|e| e.id),
            Some("fedora-eclipse.desktop".to_owned())
        );
    }

    /// Vendor prefixes are retried in order (`vendor_prefixes`,
    /// `shell-app-system.c:29-33`).
    #[test]
    fn vendor_prefixes_are_tried_for_a_bare_basename() {
        let system = system_with(vec![AppEntry::fake("gnome-terminal.desktop", "Terminal")]);
        assert_eq!(
            system.app_for_window("terminal", None).map(|e| e.id),
            Some("gnome-terminal.desktop".to_owned())
        );
        assert_eq!(system.app_for_window("nonesuch", None), None);
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
            system.app_for_window("Navigator", None).map(|e| e.id),
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
            hidden_first.app_for_window("Steam", None).map(|e| e.id),
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
            shown_first.app_for_window("Steam", None).map(|e| e.id),
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
        assert_eq!(a.n_windows(), 2);
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

    /// A sandboxed app resolves through its **sandbox** id when its `app_id` resolves to nothing.
    ///
    /// The live case this was written for: Wesnoth ships `org.wesnoth.Wesnoth.desktop`, declares
    /// no `StartupWMClass`, and sets `app_id` to the bare `wesnoth`. Both `app_id` rungs miss, so
    /// without `get_app_from_sandboxed_app_id` (`shell-window-tracker.c:279-288`) the window
    /// never becomes its app and drops out of the app switcher.
    #[test]
    fn a_sandboxed_app_resolves_through_its_sandbox_id() {
        let system = system_with(vec![
            AppEntry::fake("org.wesnoth.Wesnoth.desktop", "Battle for Wesnoth"),
            AppEntry::fake("a.desktop", "A"),
        ]);

        // Without the sandbox id, the bare app_id resolves to nothing.
        assert_eq!(system.app_for_window("wesnoth", None), None);
        assert_eq!(
            system
                .app_for_window("wesnoth", Some("org.wesnoth.Wesnoth"))
                .map(|e| e.id),
            Some("org.wesnoth.Wesnoth.desktop".to_owned())
        );

        // The rung is *exact* — `get_app_from_id` appends `.desktop` and does nothing else, no
        // canonicalization and no vendor prefixes.
        assert_eq!(system.app_for_window("wesnoth", Some("nonesuch")), None);
    }

    /// A sandboxed window may only match a desktop entry inside its own sandbox
    /// (`check_app_id_prefix`, `shell-window-tracker.c:126-134`, applied to every `app_id` rung
    /// at `:193-210`). That is what stops a host app claiming a sandboxed app's `WM_CLASS`.
    #[test]
    fn the_sandbox_id_scopes_which_entries_an_app_id_may_match() {
        let system = system_with(vec![
            AppEntry::fake("a.desktop", "A"),
            AppEntry::fake("org.example.App.desktop", "App"),
            AppEntry::fake("org.example.App.Editor.desktop", "Editor"),
        ]);

        // Unsandboxed: the prefix check is vacuous, exactly as it is for a NULL prefix.
        assert_eq!(
            system.app_for_window("a", None).map(|e| e.id),
            Some("a.desktop".to_owned())
        );
        // Sandboxed as something else: `a.desktop` is outside the sandbox, so that match is
        // refused. The window is then attributed to the sandbox's *own* app by the next rung —
        // which is the point of the scoping. A sandboxed client claiming `WM_CLASS=a` must not
        // become the host's A.
        assert_eq!(
            system
                .app_for_window("a", Some("org.example.App"))
                .map(|e| e.id),
            Some("org.example.App.desktop".to_owned()),
        );
        // With no entry for the sandbox either, the refusal is all that is left — the host entry
        // is still not borrowed.
        assert_eq!(system.app_for_window("a", Some("org.nothing.Here")), None);
        // Inside its own sandbox the same shape is allowed: the id starts `org.example.App.`.
        assert_eq!(
            system
                .app_for_window("org.example.App.Editor", Some("org.example.App"))
                .map(|e| e.id),
            Some("org.example.App.Editor.desktop".to_owned())
        );
    }

    /// The `.flatpak-info` parse: only `[Application] name`, and only that section.
    #[test]
    fn the_sandbox_id_is_the_application_name_from_flatpak_info() {
        // Not a pid we can fabricate a `/proc` entry for, so the parse is exercised directly
        // through the same helper the reader uses.
        assert_eq!(
            parse_flatpak_app_id(
                "[Application]\nname=org.wesnoth.Wesnoth\nruntime=runtime/org.fedoraproject.Platform/aarch64/f44\n"
            ),
            Some("org.wesnoth.Wesnoth".to_owned())
        );
        // A `name` outside the [Application] section is a different key entirely — the runtime's.
        assert_eq!(
            parse_flatpak_app_id("[Runtime]\nname=org.fedoraproject.Platform\n"),
            None
        );
        // A later section ends the block.
        assert_eq!(
            parse_flatpak_app_id("[Application]\n[Instance]\nname=nope\n"),
            None
        );
        // Real files have leading sections and blank lines before the one we want.
        assert_eq!(
            parse_flatpak_app_id(
                "[Context]\nshared=network\n\n[Application]\nname=org.gnome.Foo\n"
            ),
            Some("org.gnome.Foo".to_owned())
        );
        // Not flatpak at all.
        assert_eq!(parse_flatpak_app_id(""), None);
        assert_eq!(parse_flatpak_app_id("[Application]\nname=\n"), None);
    }

    /// A window that resolves to nothing still gets an app, synthesized from the window —
    /// GNOME's `_shell_app_new_for_window` last resort (`shell-window-tracker.c:469-471`).
    ///
    /// The invariant is what matters: `get_app_for_window` never returns NULL, so no window can
    /// fall out of the running set. Dropping one takes it out of the app switcher and the dash
    /// while it is still on screen — which is what a flatpak whose `app_id` does not match its
    /// desktop id used to do (Wesnoth: `app_id=wesnoth`, `org.wesnoth.Wesnoth.desktop`).
    #[test]
    fn unmatched_windows_become_window_backed_apps() {
        let mut system = system_with(vec![AppEntry::fake("a.desktop", "A")]);
        let untitled = MappedId::next();
        system.set_windows(vec![
            win("a", Some(1)),
            win("nonesuch", Some(2)),
            RunningWindow {
                pid: None,
                id: untitled,
                app_id: None,
                title: None,
                urgent: false,
                last_focus: Some(Duration::from_secs(3)),
            },
        ]);

        let running = system.running();
        assert_eq!(running.len(), 3, "no window may be lost");
        assert_eq!(
            running.iter().filter(|a| !a.is_window_backed()).count(),
            1,
            "only the resolvable window becomes a real app"
        );
        // The one with no `app_id` at all is window-backed under its own window's id.
        assert!(
            running
                .iter()
                .any(|a| a.id == window_backed_id(untitled) && a.is_window_backed()),
            "a window with no app_id is still an app, keyed by the window"
        );
        // Two unresolvable windows are two apps, never merged: GNOME's grouping fallback is the
        // X11 window group, which Wayland has no analogue for.
        assert_eq!(
            running.iter().filter(|a| a.is_window_backed()).count(),
            2,
            "window-backed apps are per window, not pooled"
        );
    }

    /// A window-backed app is never shown as the raw `window:5`.
    #[test]
    fn a_window_backed_app_falls_back_to_the_title_then_the_app_id() {
        let mut system = system_with(Vec::new());
        let id = MappedId::next();
        let window = RunningWindow {
            pid: None,
            id,
            app_id: Some("nonesuch".to_owned()),
            title: Some("Some Document".to_owned()),
            urgent: false,
            last_focus: None,
        };

        system.set_windows(vec![window.clone()]);
        assert_eq!(system.running()[0].fallback_label(), "Some Document");

        // No title: the `app_id` we could not resolve is still better than the synthetic id.
        system.set_windows(vec![RunningWindow {
            title: None,
            ..window.clone()
        }]);
        assert_eq!(system.running()[0].fallback_label(), "nonesuch");

        // An empty title must not win over the app_id either.
        system.set_windows(vec![RunningWindow {
            title: Some(String::new()),
            ..window.clone()
        }]);
        assert_eq!(system.running()[0].fallback_label(), "nonesuch");

        // Nothing at all: the id, rather than an empty label.
        system.set_windows(vec![RunningWindow {
            title: None,
            app_id: None,
            ..window
        }]);
        assert_eq!(system.running()[0].fallback_label(), window_backed_id(id));
    }

    /// `set_windows` reports only *resolved* changes — the dash redisplay trigger.
    ///
    /// An unresolvable window now counts as one: since it becomes a window-backed app it is a
    /// new entry in the running set, and the dash has something to draw. Before window-backed
    /// apps it was invisible, and this test asserted the opposite.
    #[test]
    fn set_windows_reports_resolved_changes_only() {
        let mut system = system_with(vec![AppEntry::fake("a.desktop", "A")]);
        let a = win("a", Some(1));
        assert!(system.set_windows(vec![a.clone()]));
        assert!(
            system.set_windows(vec![a.clone(), win("nonesuch", Some(2))]),
            "an unresolvable window is a window-backed app, so it does change the running set"
        );
        // What must still *not* trigger one: re-stating the very same snapshot.
        assert!(
            !system.set_windows(vec![
                a.clone(),
                win_with_id(
                    system
                        .running()
                        .iter()
                        .find(|app| app.is_window_backed())
                        .map(|app| app.windows[0].id)
                        .expect("the unresolvable window is window-backed"),
                    "nonesuch",
                    Some(2),
                )
            ]),
            "an unchanged snapshot must not trigger a redisplay"
        );
        let refocused = RunningWindow {
            urgent: false,
            last_focus: Some(Duration::from_secs(4)),
            ..a
        };
        assert!(
            system.set_windows(vec![refocused]),
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
        // Window-backed until the app exists: not lost, just unresolved.
        assert!(system.running()[0].is_window_backed());

        catalog
            .apps
            .borrow_mut()
            .push(AppEntry::fake("a.desktop", "A"));
        system.refresh();
        let ids: Vec<&str> = system.running().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a.desktop"]);
    }
}
