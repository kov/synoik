# Overview port — inventory & backlog

**Purpose.** The working plan for porting the rest of the GNOME 50.1 **Activities overview** into this
fork, with the near-term goal of **launching apps** — from the **dash** and from **overview search** —
and building out search features incrementally. Same method as `panel-status-port.md`: every element
cited to the 50.1 reference, a shared-component/D-Bus plan up front, and a prioritized slice backlog.

**Reference-first (see CLAUDE.md).** Ground every port in the actual 50.1 source before implementing.
Checkouts: `~/Projects/gnome-shell` (`50.1`), `~/Projects/mutter` (50.1). JS paths below are relative to
the gnome-shell checkout. **Style** (fonts/colors/radii) → `docs/fork/gnome-style-reference.md`;
**child order/placement** → the JS `add_child`/`_init` sequence cited here, never the SCSS
([[reference-first-child-order]]).

**Tenet.** GNOME's way replaces niri's. niri has no dash/app-grid/app-search, so there is nothing of
niri's to discard here — but the overview *shell* we build on **is** niri's, already GNOME-ified (open/
close/zoom/exposé/thumbnail-strip). We keep that and hang GNOME chrome on it.

---

## 1. What exists vs. what's missing

### Reuse (present today, niri-derived, already GNOME-shaped)
- **Overview state machine**: `overview_open` + `overview_progress`, `toggle/open/close_overview`,
  `toggle_overview_to_workspace`, `is_overview_open`, zoom (`overview_zoom`/`compute_overview_zoom`) —
  `src/layout/mod.rs`. Open/close animation via `OverviewOpenCloseAnim`.
- **Window picker (exposé)**: `src/layout/expose.rs` — port of `js/ui/workspace.js`
  `UnalignedLayoutStrategy`.
- **Workspace thumbnail strip**: `src/layout/thumbnails.rs` — port of `js/ui/workspaceThumbnail.js`
  `ThumbnailsBox`.
- **Backdrop / per-workspace overview render**: `src/layout/workspace.rs`, `src/niri.rs`
  (`backdrop_buffer`, `place_within_backdrop`).
- **Triggers**: `Action::{ToggleOverview,OpenOverview,CloseOverview}` (`niri-ipc`/`src/input/mod.rs`),
  the overlay-key (lone Super tap → `ToggleOverview`, mutter `process_special_modifier_key` port,
  `src/input/mod.rs`), the panel **Activities** button (`panel::ROLE_ACTIVITIES`), and the IPC read
  side (`OverviewState`, `Event::OverviewOpenedOrClosed`).
- **Keyboard-focus/a11y**: `KeyboardFocus::Overview` (`src/niri.rs`), a11y `ID_OVERVIEW` (`src/a11y.rs`).
- **UI toolkit**: `src/ui/widget.rs` (`Painter`, `bake`/`bake_content`/`bake_uncached`, `Button`,
  `CardButton`, `TextShaper`/`ShapedText`, `Align`, `style`), the panel role/hit-test model
  (`src/ui/panel.rs`), the popover framework (`src/ui/popover.rs`). Font sizing via `ui::pt_to_px`.
- **Symbolic icon loader**: `src/render_helpers/icon.rs` `IconCache` — monochrome SVG → recolored
  buffer, theme search with Adwaita/hicolor fallback. **Symbolic only.**
- **A command launcher already exists**: `src/ui/run_dialog.rs` (Alt+F2, `js/ui/runDialog.js`) — PATH
  spawn with `command-history`. Keep as-is; it is *not* app search and does no `.desktop` resolution.

### Missing (everything the launch-apps goal needs)
- **No app catalog** — no `Shell.AppSystem` equivalent: no `.desktop` enumeration, lookup, or search;
  no `gio::AppInfo`/`DesktopAppInfo` use (gio is a dep, used only for GSettings/D-Bus today).
- **No favorites model** — `org.gnome.shell favorite-apps` is unread.
- **No running-app ↔ `.desktop` matching** — window `app_id`/WM_CLASS is tracked for introspection
  only; nothing maps it to an app entry (needed for running dots + running apps in the dash).
- **No full-color app-icon loader** — `IconCache` recolors monochrome SVGs; app icons are full-color
  raster PNG (`.../48x48/apps/…`) or full-color SVG (`.../scalable/apps/…`), size-directory-selected,
  with `index.theme` inheritance. New capability.
- **No dash, no app grid, no overview search** — no `AppIcon` tile, no search entry, no results view,
  no `SearchProvider` plumbing.
- **No overview chrome layout** — the master `ControlsManagerLayout` (search entry top, dash bottom,
  workspaces/app-grid interpolated by state) is unported; our overview renders only the picker.
- **Unexported D-Bus** — `org.gnome.Shell` lacks `ShowApplications`, `FocusApp`, `FocusSearch`, and the
  `OverviewActive` property (we own the name in `src/dbus/gnome_shell.rs`).

---

## 2. GNOME 50.1 architecture (cited reference)

### 2.1 Overview & master layout
- `js/ui/overview.js` — `Overview` singleton (`Main.overview`) owns an `OverviewActor` (a vertical
  `St.BoxLayout`, primary-monitor-constrained) whose single child is a `ControlsManager`. Shown-state
  machine `HIDDEN/HIDING/SHOWING/SHOWN`; `show(state=WINDOW_PICKER)`, `hide()`, `toggle()`,
  `showApps()`→`show(APP_GRID)`, `selectApp(id)`, `focusSearch()`. Modal grab via
  `Shell.ActionMode.OVERVIEW`. `ANIMATION_TIME=250`.
- `js/ui/overviewControls.js` — **the master layout**. `ControlsState` = `HIDDEN:0, WINDOW_PICKER:1,
  APP_GRID:2` are *continuous* values on an `OverviewAdjustment` (`St.Adjustment`, lower=HIDDEN,
  value=WINDOW_PICKER, upper=APP_GRID); `getStateTransitionParams()` is the interpolation primitive.
  `ControlsManager._init` child order (**matters**): (1) `searchEntryBin` (centered `St.Entry`,
  `search-entry`, hint "Type to search"), (2) `appDisplay`, (3) `dash`, (4) `searchController`
  (`new SearchController(searchEntry, dash.showAppsButton)`), (5) `thumbnailsBox`, (6)
  `workspacesDisplay`. `ControlsManagerLayout.vfunc_allocate`: search at top; dash at bottom capped to
  `DASH_MAX_HEIGHT_RATIO=0.16` of height; thumbnails below search; workspaces interpolated between
  per-state cached boxes (fills mid-region in WINDOW_PICKER, shrinks to `SMALL_WORKSPACE_RATIO=0.15` at
  top in APP_GRID); appDisplay fills the rest; searchController fills under search. `_update()` lerps
  `fitModeAdjustment` (SINGLE→ALL) and thumbnails opacity/scale/translationY per state.
  `_updateAppDisplayVisibility`: appDisplay visible only when `state > WINDOW_PICKER && !searchActive`.
  `_onSearchChanged` cross-fades appDisplay/workspaces/searchController over
  `SIDE_CONTROLS_ANIMATION_TIME=250`. `_onShowAppsButtonToggled` eases the adjustment WINDOW_PICKER↔
  APP_GRID.

### 2.2 Dash — `js/ui/dash.js`
- `Dash` (`St.Widget`, name `dash`): a `dash-background` + a `_box` app-well (`DashIconsLayout`,
  horizontal) + a trailing `ShowAppsIcon`. Data via `Shell.AppSystem.get_default()`; redisplay is
  **deferred work** triggered by AppSystem `installed-changed`/`app-state-changed` and
  `AppFavorites` `changed`.
- `_redisplay()`: `newApps` = favorites (`AppFavorites.getFavoriteMap()`) then running apps
  (`_appSystem.get_running()`) not already favorited; diff → add/remove/move `DashItemContainer`s; a
  `dash-separator` after the favorites when `0 < nFavorites < nIcons`.
- `DashIcon` extends `AppDisplay.AppIcon` (`showLabel:false`, tooltip `St.Label` on hover via
  `DashItemContainer`). `_adjustIconSize()` picks from `[16,22,24,32,48,64]` to fit
  `setMaxSize(w,h)` (called by the layout). `ShowAppsIcon` is the `view-app-grid-symbolic` toggle
  **and** an unfavorite drop target (`AppFavorites.isFavorite` + `favorite-apps` writability).
  DnD reorder/add favorites: `handleDragOver`/`acceptDrop` → `AppFavorites.moveFavoriteToPos` /
  `addFavoriteAtPos`.

### 2.3 App grid — `js/ui/appDisplay.js`
- `AppDisplay` (paged `AppGrid` in a horizontal-paging `St.ScrollView` + `PageIndicators`). `_loadApps`:
  `Shell.AppSystem.get_default().get_installed()` minus favorites and parental-controls-hidden, plus
  `FolderIcon`s from `org.gnome.desktop.app-folders folder-children` (per-folder relocatable schema
  `org.gnome.desktop.app-folders.folder`). Page layout persisted in `org.gnome.shell app-picker-layout`
  (`aa{sv}`, per-page `appId→{position:i}`, via `PageManager`).
- `AppIcon` (`IconGrid.BaseIcon` from `app.create_icon_texture` + running `_dot` when
  `state !== STOPPED` + `AppMenu`). **`activate(button)`**: `openNewWindow` if `can_open_new_window()`
  && RUNNING or Ctrl/middle-click → `app.open_new_window(-1)` else `app.activate()`, then
  `Main.overview.hide()`. Folders: `FolderIcon`/`FolderView`/`AppFolderDialog`; drop-app-on-app makes a
  folder.

### 2.4 Search — `js/ui/search.js` + `searchController.js`
- `SearchController` wraps the shared `St.Entry` (primary `edit-find-symbolic`, clear
  `edit-clear-symbolic`); `_onTextChanged` tokenizes → `SearchResultsView.setTerms`; drives
  Tab/arrow/Enter nav and Escape.
- `SearchResultsView`: provider registry `_providers[]`. Providers **with `appInfo`** (remote) render
  as `ListSearchResults` (cap `MAX_LIST_SEARCH_RESULTS_ROWS=5`); providers **without** (the app
  provider) render as `GridSearchResults`. `setTerms` debounces 150ms → `_doSearch`; sub-search
  (string extends previous) → `getSubsearchResultSet`, else `getInitialResultSet`; then
  `getResultMetas` → display. gsettings `org.gnome.desktop.search-providers`
  (`disabled`/`enabled`/`disable-external`/`sort-order`).
- **Built-in `AppSearchProvider`** (`appDisplay.js`): `id='applications'`, `maxResults=6`,
  `getInitialResultSet` = `Shell.AppSystem.search(query)` (→ **`g_desktop_app_info_search`**) sorted by
  `Shell.AppUsage`, then `SystemActions.getMatchingActions(terms)` (logout/lock/…). Result
  `activate()` → `provider.activateResult` → `app.activate()` + `Main.overview.toggle()`.
- **Provider protocol** (duck-typed): `id`, `isRemoteProvider`, `canLaunchSearch`, opt `appInfo`,
  `getInitialResultSet`, `getSubsearchResultSet`, `getResultMetas`→`[{id,name,description?,
  createIcon(size),clipboardText?}]`, `activateResult(id,terms)`, `launchSearch(terms)`.
- **Remote providers** — `js/ui/remoteSearch.js`: discovered from `$XDG_DATA_DIRS/gnome-shell/
  search-providers/*.ini` (`[Shell Search Provider]`: `BusName`,`ObjectPath`,`DesktopId`,`Version`,
  `DefaultDisabled`); D-Bus **`org.gnome.Shell.SearchProvider2`** (`GetInitialResultSet(as)→as`,
  `GetSubsearchResultSet(as,as)→as`, `GetResultMetas(as)→aa{sv}`, `ActivateResult(s,as,u)`,
  `LaunchSearch(as,u)`). **Untrusted content** — see the process seam in §4.

### 2.5 App model, favorites, launch
- `Shell.AppSystem.get_default()` (C): `get_installed()`→`Gio.AppInfo[]`, `get_running()`→`Shell.App[]`,
  `lookup_app(id)`, static `search(query)`; signals `installed-changed`/`app-state-changed`.
- `AppFavorites` (`js/ui/appFavorites.js`): singleton over `org.gnome.shell favorite-apps` (`as`);
  `getFavoriteMap/getFavorites/isFavorite/add/move/remove` write the strv back and emit `changed`.
- `Shell.App`: `activate()`, `open_new_window(-1)`, `can_open_new_window()`, `state`,
  `create_icon_texture(size)`, `get_id/get_name`, `is_window_backed()`, `app_info`. Usage ordering via
  `Shell.AppUsage`.

### 2.6 Exported D-Bus (`data/dbus-interfaces/org.gnome.Shell.xml`, `js/ui/shellDBus.js`)
- On `org.gnome.Shell` at `/org/gnome/Shell`: `ShowApplications()`→`show(APP_GRID)`,
  `FocusApp(s id)`→`selectApp(id)`, `FocusSearch()`→`focusSearch()`, property **`OverviewActive`**
  (`b`, rw) mirroring `Main.overview.visible`. No dedicated AppSystem/search D-Bus is exported —
  enumeration/search are in-process; the only search D-Bus is the *consumed* remote `SearchProvider2`.

### 2.7 Data-source cheat sheet
| Concern | schema : key | type |
|---|---|---|
| Dash favorites | `org.gnome.shell` : `favorite-apps` | `as` |
| App-grid layout | `org.gnome.shell` : `app-picker-layout` | `aa{sv}` |
| Folders | `org.gnome.desktop.app-folders` : `folder-children` + relocatable `.folder` | `as`/… |
| Search providers | `org.gnome.desktop.search-providers` : `disabled/enabled/disable-external/sort-order` | `as`/`b` |
| Keybinds | `org.gnome.shell.keybindings` : `toggle-overview`,`toggle-application-view`,`shift-overview-up/down` | — |

---

## 3. Shared UI components to build (plan-ahead)

These are the reusable primitives the dash, app grid, and search results all need — build once in the
toolkit, per the CLAUDE.md "toolkit-first, no faked chrome" tenet.

1. **`AppEntry` catalog / `AppSystem` (backing model, not UI).** A compositor-owned service — sibling
   of `GnomeSettings` — over `gio::AppInfo`/`gio::DesktopAppInfo`: `installed()`, `lookup(id)`,
   `search(query)` (faithful, via `DesktopAppInfo::search` = the same `g_desktop_app_info_search` GNOME
   uses), `favorites()` (read/write `favorite-apps`), and `launch(entry, action)`. Watches
   `AppInfoMonitor` (installed-changed) + `favorite-apps`. Feeds the UI via setters like the panel
   model; kept inspectable. This is the single foundational dependency of everything below.
2. **App-icon loader (full-color).** Extend `render_helpers/icon.rs` (or a sibling) from symbolic-only
   to a themed **`gicon`/name → best file → decoded premultiplied buffer**: size-directory selection
   (`48x48`,`64x64`,`256x256`,`scalable`) + `index.theme` inheritance, raster (PNG via `image`) *and*
   full-color SVG (via `resvg`, **no recolor**), `application-x-executable` fallback, absolute-path and
   embedded-pixbuf gicons. Cached by (key, physical size). See the icon-loading decision in §6.
3. **`widget::AppIcon` tile.** icon + optional label + running dot (`app-grid-running-dot`), hover/press
   states, a hit rect, click→activate. One primitive shared by dash, app grid, and grid search results
   (GNOME shares it too: `DashIcon`/`GridSearchResult` extend `AppIcon`). Built on `Painter`/`bake`.
4. **`widget::Entry` (text entry).** Editable single-line entry with caret, selection, placeholder,
   focus ring, primary/secondary icons — the `search-entry`. `run_dialog.rs` already has a bespoke
   text field; factor the shared entry and adopt it there too. Feeds keyboard input while
   `KeyboardFocus::Overview`.
5. **Scroll/paged container + tooltip.** A paged grid+indicators for the app grid; a hover tooltip
   label for dash icons. Introduce when their consumer lands (grid / dash), not before.

---

## 4. D-Bus & settings integrations (plan-ahead)

- **Export (we own `org.gnome.Shell`)** — add to `src/dbus/gnome_shell.rs`: `ShowApplications`,
  `FocusApp(s)`, `FocusSearch`, and the `OverviewActive` (`b`, rw) property + change signal. These are
  thin shims onto the overview state machine + the new AppSystem. `Eval` stays a refusing no-op.
- **GSettings** — read/write `org.gnome.shell favorite-apps` and (later) `app-picker-layout`; read
  `org.gnome.desktop.app-folders` and `org.gnome.desktop.search-providers`. All via `gio::Settings`
  like `src/gnome.rs` (bind into `GnomeSettings` or the new AppSystem service).
- **Consume `org.gnome.Shell.SearchProvider2`** (remote search, later slice) — a session-bus client
  discovering `*.ini` providers and proxying the five methods. This ingests **untrusted app content**;
  per [[untrusted-content-process-seam]] keep a plain-data-channel seam so it can split into its own
  process: the provider client owns its bus conn, does all parsing on its side, and hands the UI only
  validated result structs (id/name/description/icon/clipboardText).
- **Launch integration** — app launch goes through `gio::AppInfo::launch` (handles `DBusActivatable`,
  `Terminal=true`, field codes, startup-notify) rather than the raw PATH spawn in `run_dialog`.
  Faithful systemd-scope wrapping (`app-<id>.scope`) and startup-notification/focus-stealing timestamps
  are a follow-up refinement, not MVP.

---

## 5. Prioritized backlog (slices toward "launch apps")

Each slice = one advise→implement→adversarial-review cycle (Fable subagent, model `fable`), one commit,
test-first, reference-cited on both axes (placement + style), per the `panel-status-port.md` method.
Verifiability is classified per item: catalog/search/launch → headless conformance tests
(`src/tests/gnome.rs`); chrome rendering → Vulkan render test + `NIRI_VK_VALIDATION=1` grep gate;
open/close & cross-fade **animation** → largely live-only ([[headless-animation-clock-trap]]).

- **S1 — AppSystem catalog + launch (no UI). ✅ DONE (`d82c5ba8`).** `src/app_system.rs`: an
  `AppSystem` model (sibling of `GnomeSettings`, owned on `Niri`) over `gio_unix::DesktopAppInfo`
  (needs `gio-unix` + the `v2_58` feature for `search`) behind `AppCatalog`/`AppLauncher` seams —
  installed/lookup/faithful grouped `search`/`launch`, plus favorites mirroring `AppFavorites` in
  **resolved space** (`favorite-apps` read + `set_favorite_apps` writer via the existing gsettings
  pipeline). `AppInfoMonitor` watcher thread → calloop channel. Note: `AppEntry` drops `executable`
  (nullable-binding panic on link/DBus-activated apps); `commandline` covers it. 8 headless tests
  (model + real-gio smoke) + a corpus wiring test; Fable-reviewed (favorites resolved-space fix
  landed from the review).
- **S2 — Full-color app-icon loader.** §3.2. Headless test: a known installed app's icon resolves +
  decodes to visible non-monochrome coverage across scales (skips cleanly if theme absent, like the
  existing icon tests).
- **S3 — Dash chrome (favorites) + `widget::AppIcon` + click-to-launch.** Render the dash at the
  overview bottom (background + favorites app-well + show-apps button), hit-test, click→`launch`. Start
  **favorites-only** (defer running apps to S6). Pins: render test for the dash bake; conformance test
  that a click on a favorite's rect calls `launch` with the right entry.
- **S4 — Overview search entry + `AppSearchProvider` + launch.** `widget::Entry` at overview top;
  typing routes through `KeyboardFocus::Overview` → terms → `AppSystem.search` → grid results;
  Enter/click → `launch` + close overview. Local app provider only. Pins: conformance test
  terms→results ordering and activate→launch; render test for the results grid.
- **S5 — Overview chrome layout (`ControlsManager` port).** Formalize placement: search-entry-top,
  dash-bottom, results-replace-picker, driven by an `OverviewAdjustment` analogue; wire
  `search-active` visibility. (S3/S4 may hardcode positions; S5 makes the layout faithful and is the
  seam APP_GRID needs.)
- **S6 — Running apps + running dots.** window `app_id`/WM_CLASS → `.desktop` matching (StartupWMClass
  rules); `get_running()` feeds dash trailing icons + the `AppIcon` running dot; `dash-separator`
  between favorites and running. Conformance test the matching table.
- **S7 — `org.gnome.Shell` D-Bus.** `ShowApplications`/`FocusApp`/`FocusSearch`/`OverviewActive`.
  XML-signature-cited; small.
- **S8 — App grid (APP_GRID state).** Paged grid, show-apps-button toggles WINDOW_PICKER↔APP_GRID,
  `app-picker-layout` persistence. Larger; not required for launching (dash + search cover it).
- **S9+ — Incremental search + polish.** Remote `SearchProvider2` (with the §4 process seam),
  `SystemActions` results, `Shell.AppUsage` ordering, favorites DnD reorder / add-remove, folders,
  usage stats.

**Rough dependency order:** S1 → S2 → {S3, S4} → S5 → S6/S7 → S8 → S9+. S1 gates everything; S2 gates
any icon rendering; S7 is independent and small.

---

## 6. Decisions (settled 2026-07-23)
- **D-A — App-icon loading → adopt `freedesktop-icons`** for theme *resolution* only (inheritance +
  size dirs), keeping our own `resvg`/`image` decode + premultiplied-buffer cache. App-icon lookup is
  exactly where `IconCache`'s deferred inheritance/size machinery bites; don't reimplement the spec.
- **D-B — Catalog backend → `gio::DesktopAppInfo`** (faithful — `search` is the same
  `g_desktop_app_info_search` GNOME uses; already a dep).
- **D-C — Foundation first.** S1 (AppSystem catalog+launch) + S2 (icon loader) land first — no visible
  change, fully headless-testable, and every UI slice hard-depends on them — then S3 (dash) for the
  first daily-drivable win.
- **D-D — Dash is favorites-only in v1.** Running apps + running dots deferred to S6 (they need the
  fiddly window↔`.desktop` StartupWMClass matching).
