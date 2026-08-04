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

  **RESOLVED 2026-08-03 — see `docs/fork/session-end.md`.** Both gaps below are closed and the
  ordering fix they pointed at is implemented. Kept for the diagnosis, which is still the clearest
  statement of the failure; do not act on the "two gaps" as open work.

  **This deferral has a live consequence: apps we launch die badly on logout.** Observed 2026-07-27 —
  Firefox aborts every logout, and the core says why:

  ```
  Error reading events from display: Broken pipe
  ```

  EPIPE, not a protocol error: the compositor's socket goes away while the client is still running,
  GTK3 makes that fatal (`gdk/wayland/gdkeventsource.c` `g_error`), and Firefox's `HandleGLibMessage`
  turns it into a crash. Journal ordering, one logout: niri's last frame `10:26:16.384` →
  `Stopped target graphical-session.target` `.389` → `ANOM_ABEND comm="firefox"` `10:26:17.405`. The
  client outlived the compositor by a second.

  Two gaps against GNOME, both confirmed rather than assumed:

  1. **`GioLauncher` passes `AppLaunchContext::NONE`** (`src/app_system.rs:685`), so a launched app
     gets no scope and stays in the compositor's own cgroup — `org.gnome.Shell@user.service`, which
     is the unit niri runs as. On logout it is a stray in a stopping service: the 09:52 logout shows
     `org.gnome.Shell@user.service: Killing process 478236 (firefox) with signal SIGABRT`, i.e. the
     `TimeoutStopSec=5` + `10-timeout-abort.conf` cleanup. GNOME instead connects the launch
     context's `launched` signal and calls `gnome_start_systemd_scope(app_name, pid, …)` —
     `src/shell-global.c:1182-1207`, "Start async request; we don't care about the result".
  2. **Our own scopes are not part of the session.** `start_systemd_scope`
     (`src/utils/spawning.rs:403`) does create `app-niri-<name>-<pid>.scope` for niri's spawn path,
     but `systemctl show` says `PartOf=` empty, where a GNOME-launched scope carries
     `PartOf=graphical-session.target` — the property that gets the app a SIGTERM at session
     teardown instead of nothing.

  Natural experiment in the same session: `ghost` and `kitty` had scopes and exited cleanly, Firefox
  had none and aborted. Suggestive, **not conclusive** — kitty/ghost may simply tolerate EPIPE where
  GTK3 calls it fatal.

  So (1)+(2) are small, match the reference, and are worth doing on their own merits, but neither is
  proven to fix the crash: the client still has to be *gone* before the compositor is, and in the
  trace above the target stopped 5ms after niri's last frame. **The actual fix is shutdown ordering
  — the compositor outliving its clients** (gnome-session's EndSession phases run before any target
  stops), which is its own piece of work and wants the session-manager integration designed first.
  Do not file (1)+(2) as "fixes the Firefox crash".

  **Outcome.** (1) was fixed by `start_app_scope`, and (2) turned out to be fixed with it: our
  `app-gnome-*` scopes inherit gnome-session's `app-gnome-.scope.d/override.conf`, so they do carry
  `PartOf=graphical-session.target` — verified live, `DropInPaths` names that file. The prediction
  in the last paragraph held: they did not fix the crash, and the ordering did. One correction to
  the mechanism assumed above — gnome-session's EndSession phases do **not** run before the targets
  stop in any way that helps, because in a GNOME 50 session no app is a registered session client
  (GTK4 dropped `RegisterClient` entirely). What actually stops apps is systemd, in the same job
  transaction that stops the shell. `docs/fork/session-end.md` has the measured timeline.

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
- **S2 — Full-color app-icon loader. ✅ DONE (`b43d85b9`).** `render_helpers::icon::AppIconCache`
  (sibling of the symbolic `IconCache`, sharing the factored `render_svg_pixmap`): `AppIconRef`
  descriptor (themed/file/fallback) extracted from `GIcon` in the catalog; `freedesktop-icons`
  resolution (inheritance + size dirs); full-color decode keeping the icon's own colors (raster via
  `image` — premultiply-before-resize to avoid fringing — SVG via `resvg`); `application-x-executable`
  fallback; premultiplied `Abgr8888` scale-tagged output. `org.gnome.desktop.interface icon-theme`
  plumbed to both caches (re-theme on change, clear on installed-changed). 6 headless tests
  (colors-kept, premultiply, real-app decode, scale-tag, fallback, descriptor); Fable-reviewed
  (channel order confirmed correct from tiny-skia/DRM sources; premultiply-order + lazy-fallback fixes
  landed).
- **S3 — Dash chrome (favorites) + `widget::AppIcon` + click-to-launch. ✅ DONE.** `src/ui/dash.rs`:
  the `Dash` widget (rounded `dash-background` pill bottom-center, favorites app-well of `widget::AppIcon`
  tiles, trailing show-apps button), `layout`/`hit_test` sharing one `DashLayout`, `render` baking the
  pill (hover fill) with full-color icons on top, faded by `expose_progress`. Click intercept in
  `on_pointer_button` (inside the gnome-mode block): a favorite launches (`LaunchMode::Activate` — all
  apps are stopped in S3) + closes the overview; show-apps/background consumed inertly; every button
  consumed on a hit so nothing falls through to the overview pan grabs. Gated to when the dash is
  actually visible (`is_overview_open && !locked && !screenshot-ui`) so an invisible dash can't eat
  clicks / launch into a locked session. Favorites snapshot via `sync_dash_favorites`; icon uploads
  dropped on installed-changed and icon-theme change. **Favorites-only** (running apps → S6). Divergences
  recorded in the module doc: launch-on-press (vs GNOME's release), right-click AppMenu consumed inertly,
  touch falls through. Pins: 8 conformance tests (`overview_dash_*`) + 3 `dash.rs` unit tests + a Vulkan
  render test pinning the hover-lightens sign. **Not yet live-validated** (fade smoothness / real icons /
  hover feel — see `pending-live-validation`).
- **S4 — Overview search entry + `AppSearchProvider` + launch. ✅ DONE.** `src/ui/overview_search.rs`:
  the `OverviewSearch` model (query, result snapshot, keyboard selection, mouse hover) + a new
  `widget::Entry` toolkit primitive (pill chrome + placeholder/text + caret + find/clear glyph
  geometry & hit-testing). Typing while `KeyboardFocus::Overview` engages the entry — the key block
  sits inside the `should_intercept_key` Forward branch, **press-only** on the shared
  `suppressed_keys` (releases are owned globally; a local release arm would leak), with an `Ignored`
  outcome so unhandled and modifier-decorated keys fall through unconsumed. `Niri::sync_overview_search`
  runs the app provider (`AppSystem::search(terms.join(' '))` → tiers → `should_show` → cap 6) and
  feeds results back. Enter activates the selection, a click activates its tile, the clear glyph
  clears; all launch + close the overview. Escape clears while active, else falls through to the
  hardcoded bind that closes. Search resets on overview *enter* (visibility rising edge in
  `State::refresh`) — deliberately not on close (query stays through the fade) nor on a
  screenshot/lock round-trip (GNOME keeps it). Pointer/hover gated by the new shared
  `Niri::overview_ui_visible()` (also fixed the dash click gate's missing `is_gnome_mode`).
  **The active entry body consumes clicks** (it is an opaque control drawn over the thumbnail strip
  pre-S5); the inactive entry is fully click-through so it can't eat thumbnail clicks. Divergences
  in the module doc: no 150ms debounce, no `AppUsage` ordering, no `SystemActions`, caret-at-end
  only (Left/Up = selection-prev), modified keys refused, no compose/autorepeat, results drawn over
  the picker, per-output draw with one shared selection, no `reset-search` outside-click gesture.
  Pins: 12 conformance tests (`overview_search_*`) + 12 `overview_search.rs` unit tests + a Vulkan
  render test (entry pill fill = `ENTRY_BG`, selected tile lighter than unselected). **Not yet
  live-validated.**
- **S5a — Overview chrome layout (`ControlsManagerLayout` port). ✅ DONE.**
  `src/ui/overview_layout.rs`: a pure-geometry port of `ControlsManagerLayout.vfunc_allocate`
  (`overviewControls.js:155-248`) — every control gets an allocated box computed top-down from the
  work area, with gnome-shell's ratios verbatim (`DASH_MAX_HEIGHT_RATIO` 0.16, `VERTICAL_SPACING_RATIO`
  0.02, thumbnails spacing adjustments 0.6/0.4). `Monitor::controls_layout()` is the single accessor;
  the dash, search entry, results card and thumbnail strip all consume boxes instead of hardcoded
  anchors, and the strip's *duplicate* panel-strut derivation is gone (one work area, one allocator).
  The measured heights are published by their owners (`dash::PREFERRED_HEIGHT`,
  `overview_search::PREFERRED_ENTRY_HEIGHT`, `thumbnails::preferred_height`), standing in for
  gnome-shell's St theme-node lookups.
  **The window picker is expressed through zoom + offset, not a sub-rect**: `GNOME_OVERVIEW_WORKSPACE_SCALE`
  (the fixed 0.8) is gone — the zoom target is now `picker_box.h / view_size.h`, so it follows the
  chrome, and the workspace row's vertical offset interpolates 0 → `picker_box.y` on the overview
  progress (gnome-shell blends its `HIDDEN` and `WINDOW_PICKER` boxes the same way,
  `overviewControls.js:207-216`). Anchoring the row at the box outright would make every overview
  open/close jump and break the "active workspace at y = 0 exactly" pointer guarantee. Zoom is
  therefore per-monitor: `Layout::overview_zoom()` → `overview_zoom_for_output()`, and the workspace
  strip's `from_zoom` continuity derivation goes through `Monitor::zoom_at(progress)` so both ends use
  the same target. The DnD edge-scroll cross-axis band now comes from the same static offset rather
  than assuming a centered row.
  **`ThumbnailsBox.expandFraction` is ported as a real animation** (`overviewControls.js:358-366`):
  the picker box contains the thumbnails band, and the strip threshold is crossed *inside* the
  overview (drag onto the trailing desktop), so an un-eased flip would pop the zoom.
  S4's inactive-entry click-through hack is deleted: with real boxes the entry no longer overlaps the
  strip, and a pass-through would instead land on the workspace and leave the overview — so the pill
  consumes inertly whether or not a search is active.
  Divergences recorded in the module docs: thumbnails measured against the view rather than
  gnome-shell's work-area porthole (54px vs 52 at 1080p); no `Dash._adjustIconSize`, so a capped dash
  box overflows instead of shrinking icons; the strip folds its expand into the existing slide rather
  than easing its own box height; `ControlsState::AppGrid` boxes deferred to S8.
  Pins: 6 `overview_layout.rs` unit tests (the full reference table at two resolutions, the collapsed
  and half-expanded variants, the dash cap) + re-pinned `thumbnails.rs` band tests + 5 conformance
  tests, including the mid-animation offset guard and the mid-expand picker continuity.
  **Not yet live-validated.**
- **S5b — `search-active` cross-fade. ✅ DONE.** A second continuous adjustment on `Niri`
  (`overview_search_fade`, fixed 250ms `EASE_OUT_QUAD` per `SIDE_CONTROLS_ANIMATION_TIME`, armed on the
  `is_active()` edge in `advance_animations`) fades the window picker and the thumbnails out and the
  results card in (`_onSearchChanged`, `overviewControls.js:609-643`). The fade lives on `Niri`, not
  `Layout` — `Layout` stays ignorant of the search; the picker/thumbnails are composited as *groups*
  through `OffscreenBuffer` + `OffscreenRenderElement::with_alpha` at the `render_workspaces` /
  `render_thumbnails` call sites, because a per-element alpha would double-darken wherever previews
  overlap. The composite is fail-open (falls back to a plain push on error, so a fade problem can
  never blank the overview) and only runs strictly between the ends. The entry pill does **not** ride
  the fade (gnome-shell's `searchEntryBin` is outside the cross-fade); only the results do.
  Reactivity: the picker (`window_under`) and the strip (`thumbnail_workspace_under`) go inert while
  searching, and — the part that matters — `OverviewSearch::hit_test` now consumes anywhere in the
  allocated results strip, because without it a click beside the card reached the faded-out picker and
  read as "clicked the empty desktop", leaving the overview. Divergences: reactivity flips with the
  boolean rather than at the ease's end (gnome-shell flips in `onComplete`, so its previews stay
  clickable for 250ms after a search starts); the results card disappears on deactivate instead of
  fading out (the picker still fades back in under it).
  Pins: 2 conformance tests (fade eases, mid value, reactivity, click-consumption) + a Vulkan render
  test that measures the blend itself — it samples the preview center at fade 0, mid-fade and fade 1
  and asserts the mid frame is `S·α + B·(1−α)` from the two *measured* ends, so it fails both if the
  group is pushed straight through and if it is dropped (mutation-verified both ways).
  **Two traps that made three earlier attempts at this test lie**, worth knowing before writing any
  full-frame render test here: the startup "Important Hotkeys" overlay (`Niri::new`) covers the picker
  and is dismissed by the *first key press*, so engaging the search changes the frame by a whole panel
  unless you `hotkey_overlay.hide()` first; and a `green > 200` filter matches **white** — the panel
  clock, the entry caret and the card text all clear it — so the reference must come from the
  preview's own `expose_target_rect`, not from a colour filter.
- **S6a — Window↔app matching + the running-app model. ✅ DONE.** `src/app_system.rs`: a port of
  `get_app_from_window_wmclass` (`shell-window-tracker.c:146`) — the `StartupWMClass` table first,
  then a `.desktop` basename tried **verbatim before canonicalizing** (lowercase, spaces→dashes),
  each retried under `vendor_prefixes`. The table is built by a faithful single pass of
  `scan_startup_wm_class_to_id` (`shell-app-system.c:107`), including both order-dependent
  tie-breaks: an entry whose id *is* the class evicts an incumbent, and a shown entry evicts a
  hidden one. `AppEntry` gained `startup_wm_class`.
  **Divergence — one string, not two.** xdg-shell has a single `app_id` where X11 has a `WM_CLASS`
  pair, so GNOME's four-rung ladder collapses to its two distinct lookups; the cost is exactly the
  Chromium web-app case (class `Chromium-browser`, instance `crx_<id>`). XWayland reaches us through
  xwayland-satellite, which has already flattened the pair. `check_app_id_prefix`'s sandbox scoping
  is unported (no sandbox id on a toplevel).
  Running apps are resolved, grouped and ordered **inside the model** from a plain
  `Vec<RunningWindow>`, so the whole policy is unit-testable. Ordering is `shell_app_compare`
  (`shell-app.c:839`) reduced to the running set — every app there has windows and we have no
  minimized state, so it is "most recently used first" off `Mapped::get_focus_timestamp`, ties by id
  (GNOME's tie order is hash-table order, i.e. arbitrary). The raw window snapshot is kept so a
  catalog refresh re-resolves it. **Windows matching nothing are dropped**, where GNOME synthesizes a
  window-backed `ShellApp` and dashes it — that needs an icon a toplevel cannot give us.
  `State::refresh` re-snapshots unconditionally and reports only resolved changes (the
  keyboard-layout-indicator pattern: one window walk, no invalidation bookkeeping, immune to a
  missed edge). Pins: 9 `app_system.rs` unit tests + a conformance test driving a real window's
  map/unmap, both mutation-verified.
- **S6b — Running apps in the dash. ✅ DONE.** `Dash` now takes `set_items(items, n_favorites)`:
  favorites, then running non-favorites in `get_running()` order, each carrying `running`
  (`Dash._redisplay`, `dash.js:677-699`). The `.dash-separator` appears iff
  `nFavorites > 0 && nFavorites < nIcons` — `nIcons` counts app icons only, since `_showAppsIcon`
  lives outside `_box` (`dash.js:350-356`) — and takes its own 9px advance out of the item run, with
  `hit_test` subtracting that band so tile indices don't shift and the divider itself is inert
  `Background`. `DashHit::Favorite` became `DashHit::App`.
  **The running dot needs its own bake layer.** GNOME adds `_dot` to the icon container *after* the
  icon (`appDisplay.js:2955-2964`) and the dash's `offset-y: -$dash_padding` (`_dash.scss:72-78`)
  lifts it onto the icon's lower edge — so it draws **over** the icon, while the pill chrome draws
  under it. Baking it into the pill made it invisible (the first render test caught exactly this:
  both probes read the icon's blue). It bakes only when something is running.
  **Trap: `Painter::hairline` clears, it does not blend.** Painting `$system_borders_color` (white at
  10%) raw replaces the opaque pill with an alpha-26 pixel — a transparent slot through the dash to
  the wallpaper — and every geometry test still passes, because the box is in the right place either
  way. Pre-blend with `style::over(DASH_BG, …)`; the render test asserts *opacity* to catch it.
  Divergence: **clicking a running app relaunches it** rather than raising its window
  (`shell_app_activate`); that needs `RunningApp` to carry window ids and a focus action — deferred.
  Pins: 3 `dash.rs` unit tests + a conformance test (real window → dash item + divider → click →
  unmap) + a Vulkan render test on the divider's opacity and the dot's color, mutation-verified both
  ways. **Not yet live-validated** — in particular the dot's vertical offset puts it *on* the icon,
  which is what the cited rule produces but is worth an eyeball.
- **S7 — `org.gnome.Shell` D-Bus.** `ShowApplications`/`FocusApp`/`FocusSearch`/`OverviewActive`.
  XML-signature-cited; small.
- **S8 — App grid (APP_GRID state).** Decisions (Gustavo, 2026-07-24): **full ControlsState port**
  for integration (not a search-style cross-fade hack), and **S8a = single-page grid + launch**
  (defer paging/PageIndicators, folders, `app-picker-layout` persistence, DnD reorder).
  - **S8a-geometry ✅ DONE (`c0f38bd3`).** `overview_layout` gained the continuous `state` axis
    (HIDDEN/WINDOW_PICKER/APP_GRID): per-state workspaces + a new `app_display` box, interpolated like
    `ControlsManagerLayout`. Pure geometry; callers pass `WINDOW_PICKER` so behaviour is unchanged.
    Pins cover the app-grid boxes + midpoint interpolation.
  - **S8a-state ✅ DONE (`3860eb95`).** Per-monitor eased `app_grid_fraction` drives WINDOW_PICKER↔
    APP_GRID (`overview_layout::state`); the show-apps button (`DashHit::ShowApps`) toggles it,
    Escape / a search close returns to the picker, and closing the overview from the grid resets the
    state. `controls_layout()` passes `WINDOW_PICKER + fraction`.
  - **S8a-view ✅ DONE (`560b3f1c` shared tile primitive + `87d30d6b` grid).** The labelled
    `.overview-tile` render (hover/selection wash + caption) was factored into `widget::TileMetrics`
    + `Painter::labelled_tile`, shared by the search results and the grid (pixel-identical). New
    `src/ui/app_grid.rs`: installed-minus-favorites, name-sorted, fill-by-width tiles into
    `app_display`; click/middle-click → launch + close; own hover for the wash. Rendered below the
    dash+search at overview-fade × (1 − search-fade) — the grid **slides, it does not fade** with the
    state axis (`overviewControls.js:582-627`; the box motion is the animation). Gated reactivity on
    `is_app_grid_open() && !search.is_active()`.
  - **S8a-thumbnails-fade ✅ DONE (`070d5be6`).** The thumbnails strip fades out with the grid
    (opacity 255→0 × `app_grid_fraction`, `_getThumbnailsBoxParams`) instead of sitting opaque above
    the shrunk picker. Deferred: GNOME's additional scale-0.5 + translationY-+h/2 shrink (transient —
    the box ends invisible) and making the faded strip non-reactive (`visible=false` in APP_GRID).
  - **S8b-pagination ✅ DONE (`a6da91ad`).** Replaced the fill-by-width grid with GNOME's paginated
    `IconGrid`: page mode `(cols,rows)` by aspect ratio from `{3×8,4×6,6×4,8×3}` (a wide app-display →
    8×3, no longer full width), icon shrinks to the largest that fits in `max(w,h)` **square cells**,
    spacing grows 12→36 then centers (`_calculateSpacing` FILL). Overflow paginates: a **dots row**
    (active full / inactive 2/3-scale+half-alpha), navigable by a **wheel notch** (150ms debounce), a
    **dot click**, or **reset to page 0** on a fresh overview open. Render pins the dots.
  - **S8 divergences / follow-ups (deferred, documented in `app_grid.rs`):** no touchpad **swipe**
    (continuous scroll over the grid is consumed but inert), no page-slide animation (snap),
    no keyboard paging (`Page_Up/Down`) or in-grid arrow/Enter keynav, no
    `indicatorsPadding`, no folders/`app-picker-layout`/DnD reorder;
    sort is `to_lowercase` not locale collation. (Every one of those has since landed; this entry
    records what S8 shipped with.) The thumbnails' transient scale/translation shrink
    and the faded-strip non-reactivity (S8a) also remain. Floor/ceil vs transition-bracket
    interpolation noted in `overview_layout.rs` (`b6b3d86b`).
- **S8c — App-grid performance + nav polish.**
  - **Async icon decode ✅ DONE (`e7a1c2ed`).** The ~24 first-frame icon rasterizations that froze the
    open animation now run on a worker thread (like the wallpaper decoder): miss → enqueue + draw
    nothing that frame; result lands via a calloop source → redraw, so icons pop in during the slide.
    Generation counter + in-flight dedup + negative cache + `catch_unwind`; headless keeps the sync path.
  - **Nav arrows ✅ DONE (`bb835487`).** The `.page-navigation-arrow` flat circular buttons in the side
    gutters (`carousel-arrow-{previous,next}-symbolic` chevron, 60px disc, flat at rest / `HOVER_WASH` on
    hover), shown only when a previous/next page exists, stepping the page ∓1 on click. Chevrons bundled
    into `resources/icons/` + `embedded_icon()` (gresource icons, not on-disk). Divergence: absent
    `indicatorsPadding` at the time, they rode the centering gutter rather than a fixed 10% band;
    G1 has since reserved it, so they sit in the band. Render pins the chevron + hover wash.
  - **Icon-warmth performance (ACTIVE — reference-checked against GNOME 50.1).** GNOME never shows a
    blank tile because the AppDisplay is built at `ControlsManager` construction (`overviewControls.js:372`),
    kept resident, and its `_redisplay` runs off the **idle/deferred-work queue** at startup
    (`appDisplay.js:1339` `initializeDeferredWork`) — so every app icon decodes+uploads during post-login
    idle, minutes before the overview opens, into a shared `keyed_cache` that is `POLICY_FOREVER`
    (`st-texture-cache.c:998`) and coalesces concurrent requests (`ensure_request`, `:877-910`). We
    already match the async worker + in-flight coalescing + decode-cache-forever; the gap is *when* the
    work happens. Plan, in order:
    - **(1) Prewarm the whole grid + favorites' *decode* at startup idle ✅ DONE (`4eea62e4`).** GNOME's
      resident-grid + idle `_redisplay`. `Niri::prewarm_app_icons` requests the dash (64px) + grid (96px)
      icon decodes off-thread for every connected output scale; `buffer()` dedups cached/in-flight keys so
      it is idempotent. Triggered once a scale exists (`add_output`) and re-warmed on content/theme change
      (installed-changed, favorite-apps, icon-theme). Gated on `has_worker()` (before the worker exists,
      `buffer()` would decode inline). Also fixes a latent bug: the grid's icon uploads weren't cleared on
      an icon-theme change (added when the grid landed after that handler). Pinned by
      `overview_prewarm_requests_dash_and_grid_icon_decodes`. Makes blank-then-real rare (only a truly cold
      first run or a brand-new app). **Live-validation pending.**
    - **(2) Batch the first-frame GPU uploads ✅ DONE (`ca164e9e`).** Live-confirmed the residual hitch
      survived (1). Each `NiriTexture::upload` was a separate cbuf + submit + blocking `wait_for_fences`
      (`niri-vk/src/gpu.rs` `run_commands`); ~24 serialized fence round-trips is the Venus stutter. Added a
      reusable `TextureBatch` (start/`upload`/`finish`) to niri-vk: stages N textures (staging +
      image/view/sampler, no commands) then records every copy into **one** cbuf, one submit, one wait. The
      single `Texture::upload` now shares resource creation (`build_pending`) + copy recording
      (`record_upload_copy`) so the paths can't drift. `VulkanRenderer::import_memory_batch` wraps it; the
      grid render collects a page's 2+ pending icon uploads and imports them in one batch. Fable-reviewed
      (no CRITICAL; also fixed two pre-existing descriptor-pool/texture leaks on `make_texture_set` failure
      it surfaced). Pinned by a 2-distinct-icon render test + `NIRI_VK_VALIDATION` clean. **Live-validation
      pending.** Reusable for any future many-textures-at-once path.
    - **(3) Shared GPU-texture cache across dash + grid + search (DEFERRED, recorded).** GNOME keeps ONE
      Cogl texture per gicon+size shell-wide; we currently upload per surface (each keeps its own
      `AppIconUploads`). Fold the three surfaces' upload caches into one shared, gicon+size-keyed texture
      cache (dash included — if we do it at all, all three) so an app that appears in more than one
      surface uploads once. **✅ DONE:** one `SharedAppIconUploads` (`Rc<RefCell<AppIconUploads>>`)
      that the dash, the grid, an open folder's view, the search results and the drag proxy all draw
      from. Search and grid both ask for 96px, so typing a query no longer re-uploads what the grid
      has resident. Each surface keeps its own renderer-context check and clears the shared map when
      it sees a new context; the first one through does the work. Pinned by `Rc::ptr_eq` — which
      icon is shared depends on the sizes each surface asks for, but the wiring is what can silently
      come undone.
  - **Next (deferred, pending live measurement):** split theme-resolution (main) from decode
    (worker) per GNOME `st-texture-cache.c:976` (marginal — resolution is cheap/cached). Everything
    else on this list has since landed: the touchpad swipe and the page slide (§10), keyboard
    paging, and `indicatorsPadding` (`indicators_w`, reserved before the page padding, so the
    arrows sit in a fixed side band).
- **S8d — Overview chrome fidelity (reported live, 2026-07-24). ✅ DONE.** Three gaps Gustavo spotted
  against real GNOME, all reference-cited and live-checked on a headless seat:
  - **Panel background over the backdrop (`5ce62549`).** GNOME drops the top panel to
    `background-color: transparent` in the overview (`#panel:overview`, `_panel.scss:98-102`), so the
    `#overviewGroup` fill runs unbroken from the top of the screen down; we kept painting opaque black,
    leaving a visible break (loudest with a search active, where everything below the panel is flat).
    The fade rides the bar's own clear color on the monitor's overview progress — GNOME's 250ms
    `$panel_transition_duration` *is* the overview `ANIMATION_TIME` — with the settled desktop and
    overview bars cached side by side and the opaque region dropped while it is translucent. The
    backdrop itself moved from niri's `#262626` to `$system_base_color` `#222226`.
  - **`FitMode` for the workspace row (`438a20fd`).** The app grid uses `FitMode.ALL` — every workspace
    laid out inside the allocation, the run centered as a whole — where the picker uses `SINGLE`, which
    slides the row so the *active* workspace is centered (`workspacesView.js:85-88,128-204`). We only
    had SINGLE, so opening the grid from a non-middle workspace pushed the row off to one side. The row
    now blends between the two boxes on the show-apps fraction, and `_getSpacing`'s `(1 - fitMode)`
    factor comes with it (the fitted row packs at `WORKSPACE_MIN_SPACING`, not the max the small
    app-grid zoom would otherwise reach). **Divergence:** past ~7 workspaces the width binds and
    gnome-shell narrows each box out of aspect; we keep one zoom per monitor, so the packed row
    overflows the edges instead.
  - **Search fades the whole workspace row (`88290a78`).** The other half of the flat-dark search view:
    S5b's cross-fade covered only the *picker*, so under a search the previews dimmed but the wallpaper
    rectangle, its shadow and any workspace-scoped layer-shell surfaces stayed opaque — a lit desktop
    with a card over it. gnome-shell fades the whole `workspacesDisplay` (`_onSearchChanged`,
    `overviewControls.js:628-637`) and a Workspace owns its `WorkspaceBackground`, so the row is now one
    group: layer surfaces + picker + wallpaper + shadows. Still a group, not a per-element alpha —
    they overlap, and independent fades composite the overlap twice.
  - **Inactive-workspace shrink (`74536094`).** `WORKSPACE_INACTIVE_SCALE` 0.94 about a centered pivot
    (`_updateWorkspacesState`, `:243-266`; `workspace.js:1039`) — the "one we are in is a little
    bigger" read. The zoom became per workspace; the slot keeps the full size so the row's advance and
    anchor don't move, and hit-testing / drag targets / xray / shadows all follow the same rect.
    **Divergence:** gnome-shell scales overview-only actors, so ours ramps in with the overview
    progress or a desktop workspace switch would shrink both workspaces mid-slide.
- **S8e — Overview interaction fidelity (reported live, 2026-07-24). ✅ DONE.** A second batch
  Gustavo spotted, all reference-cited and headless-pinned:
  - **Double-Super opens the app grid (`e4e4a0bf`).** A second overlay-key tap that lands while the
    overview is still opening shifts a state *up* instead of toggling shut
    (`overviewControls.js:419-438`). With animations on the "quick enough" test is not a timer at
    all — gnome-shell asks whether its state adjustment is mid-transition upward, so the escalation
    window *is* the open animation; with animations off it falls back to comparing against the
    previous overlay-key time (`Overview.ANIMATION_TIME`, 250ms). Both arms pinned. The shift
    clamps at APP_GRID (`_shiftState`, `:669-676`), hence `open_app_grid()` and not the show-apps
    toggle.
  - **The overview's icons activate on release (`ca457085`).** Dash icons, the show-apps button,
    app-grid tiles and page controls and search results are all St.Buttons, and an St.Button acts on
    the *release*, only if it lands on the same widget (`clutter-click-gesture.c:68-81`;
    `st-button.c:429-435` leaves `recognize-on-press` off). We launched on the press, which both
    diverged and left no room for a press to start a drag. The scattered per-widget click handling
    collapsed into one `overview_hit` + one `activate_overview_hit`, which is what makes "same
    widget" a single comparison.
  - **Hovering a preview grows it (`e809e26e`) and shows its close button (`5942dfcf`).**
    `showOverlay` eases the pointed-at preview up by `WINDOW_ACTIVE_SIZE_INC` (5px each direction,
    200ms EASE_OUT_QUAD), restacks it above its neighbours, and fades in the close button
    (`windowPreview.js:310-352,620`). The growth is in *screen* pixels — gnome-shell bakes the row
    scale into the slots it allocates (`workspace.js:690-736`) — so the hover scale is derived from
    the zoomed size and applied in workspace space, about the center, fading in with the overview.
    The button is centered on the preview's top-right corner (`:203-218`), hit-tests before the rest
    of the chrome (it overhangs its preview), and asks the window to close.
    **Divergences:** hit-testing still uses the unscaled slot, so the 5px halo isn't hoverable (GNOME
    leaves on the *scaled* actor, giving it hysteresis); no `_windowCanClose()` gate; the button is
    always on the right (GNOME follows `Meta.prefs_get_button_layout()`).
    **Still missing from the overlay:** the title caption (`.window-caption`) and the always-visible
    app icon (`ICON_SIZE` 64, `ICON_OVERLAP` 0.7) — both need a per-window text/app resolution the
    picker doesn't do yet.
  - **The dragged preview shrinks (`81f3946d`).** `dragActorMaxSize: WINDOW_DND_SIZE` (256px,
    `windowPreview.js:14,108`), eased over `SCALE_ANIMATION_TIME` once the drag starts
    (`dnd.js:261-288`). We carried the full picker footprint the whole way, covering the thumbnail
    strip being dragged toward. **Gap:** gnome-shell also drops the drag actor to
    `DRAGGING_WINDOW_OPACITY` (100/255); ours stays opaque — the moved tile renders straight into the
    output elements and an alpha needs the offscreen-group path.
  - **Dragging an app icon onto a workspace launches it there (`135b2773`).** `Workspace.acceptDrop`
    → `source.app.open_new_window(workspaceIndex)` (`workspace.js:1429-1434`). The drag begins once
    the pointer leaves the `drag-threshold` box (`st-dnd-start-gesture.c:73-90`) and cancels the
    click. Since we have no GIO launch context to carry the workspace, the intent is parked on `Niri`
    and claimed at map time by the first window that resolves to the app, expiring on mutter's
    `STARTUP_TIMEOUT_MS` (15s). **Divergences:** hardcoded threshold 8 (the mouse schema isn't in the
    gsettings model yet); a drop into a thumbnail *gap* does nothing where ThumbnailsBox would create
    the workspace; favorites reordering inside the dash still unported.
  - Also in the batch, outside the overview: the niri focus ring / border are off in GNOME mode
    (`f524a7dc`) and the startup hotkey overlay is gone (`f1221928`).
  - **Live-checked on a headless seat (2026-07-24):** the hover growth (preview's top/left edges move
    ~4-5px out, centered), the close button on the preview's corner, double-Super landing in the app
    grid, and the app-icon drag actor tracking the pointer. The last of those was a real bug the
    headless tests could not see — the drag offset was taken against the pointer at drag *start*
    instead of the icon, so the icon stayed at the press point (`3b3ff265`). Still un-checked on a
    real seat: the *feel* (the 200ms ease timings; the harness runs with animations off).
- **S8f — Fit-mode transition geometry (reported live, 2026-07-24). ✅ DONE (`9a5e0959`, `77f9c464`).**
  Three symptoms, one cause, fixed in two passes. Entering the app grid the workspace row slid ~85px
  *left* of its landing spot and snapped back; grid → picker did the same in reverse; and closing the
  overview *from* the grid ended with the row shifted right, snapping left only after the animation
  finished.
  - **Cause.** `workspaces_strip_axis` built both the fit-single and the fit-all row at the
    **current** zoom and lerped them on the fit fraction. That makes each end of the lerp a function
    of the very parameter driving it, so the row's path bends instead of running straight between two
    fixed points. gnome-shell does not do this: `_getInitialBoxes` (`workspacesView.js:281-324`) sees
    that the two ends disagree on the fit mode and interpolates between the workspaces box of the
    *initial* state and of the *final* state — `getWorkspacesBoxForState` reads a per-state cache
    (`overviewControls.js:256-258`), so those rectangles do not move while the transition runs. It
    falls back to the live allocation only when both ends share a fit mode (the plain open/close).
  - **Fix, pass 1 (`9a5e0959`).** Build each row at its own endpoint state's zoom, both expressed
    against the blended slot so the lerp is like for like, and gate the fit fraction on the overview
    progress. Killed the entering-the-grid overshoot and the terminal snap.
  - **Fix, pass 2 (`77f9c464`) — the residual, and the general shape.** Closing from the grid *with
    the active workspace in the middle of a longer row* still swung: the active jumped ~40px right and
    the workspaces behind it up to ~240px. The endpoint zooms were still evaluated at the live open
    progress, so both drifted toward a near-desktop zoom as the close ran — and a fit-all row at that
    zoom is **degenerate**: the workspaces overflow the view, so the run pins to the left gap instead
    of centering, and blending toward it throws the row sideways. gnome-shell cannot reach that state,
    because its single 0..2 adjustment passes through `WINDOW_PICKER` and unwinds the fit *before* the
    zoom starts. So reconstruct that axis from our two scalars — the show-apps fraction is a second
    unit on top of the open one, `overview_state() = open * (1 + show_apps)` — and derive the fit
    mode, the chrome layout and the zoom from it (`open_fraction`). The app-grid leg then sits at a
    fully-open zoom *by construction*, which is what makes freezing the endpoint boxes exact rather
    than approximate; the fit-single leg keeps using the live allocation. `open_fraction` deliberately
    keeps the raw (unclamped) progress when no app grid is in play, or the open spring's overshoot
    would be flattened.
  - **Tests (the reason this was reportable-but-not-caught).** Every overview test asserted *ends*,
    and both ends were always right. Three layers now:
    `Fixture::sample_animation` / `sample_workspace_geo` pin the clock at exact fractions of a
    transition and read the real render geometry at each, so a mid-flight excursion is assertable at
    all. Above that, `overview_grid_transition_moves_the_row_monotonically`,
    `overview_close_from_the_app_grid_lands_on_the_desktop`,
    `overview_grid_transitions_are_monotonic_with_the_active_workspace_in_the_middle` and
    `overview_close_from_the_app_grid_refits_before_it_zooms` (the row never overlaps itself; it is
    fully unfitted by the time it reaches full width). Under it, `fit_single_row`/`fit_all_row` are
    pure functions with clock-free sweeps (`monitor::row_tests`) pinning centering, rigidity, the end
    gaps and the overflow divergence.
    Every one was mutation-checked — reverting each half of each fix reddens a different test.
    **The shape matters as much as the sampling:** with two workspaces the active one is an end of the
    run, so the fit-all and fit-single rows want it in nearly the same place and a bad blend barely
    moves it. The middle-active row is what exposed pass 2; a two-workspace test stayed green through
    the whole bug.
    The sampling helper's contract also matters: trigger the transition first and do **not** round-trip
    a client inside the closure, or the lazy clock resets and re-times everything (the
    headless-animation-clock trap).
  - **Live-validated (2026-07-24)** on a private headless seat with animations **on** and
    `slowdown 30.0`, three workspaces with the active in the middle — burst screenshots, column-profiled
    for the row's extent. Picker → grid: active left edge 69 → 166 → 261 → 426 → 565 → 666 → 735 → 779
    → 803 → 813 → 820, strictly monotone into its settled spot. Grid → picker (Escape): 819 → 737 →
    653 → 569 → 489 → 418 → 361 → 319 → 291 → 274 → 268. Close from the grid: 820 → 648 → 366 → 202 →
    124 → 72 → 42 → 24 → 13 → 7 → 4 → 2 → 1 → 0, no terminal snap. These track the sampled harness
    numbers to a couple of pixels, i.e. the live render path and the sampled geometry agree.
  - **Follow-up (not done).** `overview_state()` reconstructs the axis but the two scalars still exist
    underneath, so a caller can still read the raw open progress where it wants the state. Collapsing
    them into one stored 0..2 adjustment touches every `overview_progress` consumer, the gesture path,
    and `workspace_render_idx`'s synchronized-animation pairing. Chrome opacity fades still key off the
    raw progress rather than the state, so on a close from the grid they are not in the reference's
    phase order.
- **S9+ — Incremental search + polish.** Remote `SearchProvider2` (with the §4 process seam),
  `SystemActions` results, `Shell.AppUsage` ordering, favorites DnD reorder / add-remove, folders,
  usage stats.
- **S10 — Shifted-keysym blind spot. ✅ FIXED.** Reported live: in the search results **Tab stepped
  forward but so did Shift+Tab**. Not a logic bug in the search — `ISO_Left_Tab` is the *modified*
  keysym and the key path hands surfaces the **raw** one, so Shift+Tab arrives as a plain `Tab` and
  that match was dead for real input. The search now takes the shift state and decides the direction
  from it. Audited the other two surfaces that key off `raw` for Tab: the app grid already reads
  `mods.shift`, and the end-session dialog has two buttons and toggles either way. Pinned by a test
  that drives Shift+Tab through the input path — a unit call with `ISO_Left_Tab` passes against the
  bug, which is why it survived.
- **S11 — Overview scale on the internal display (ROOT-CAUSED + FIXED, 2026-07-26; live validation
  pending).** Reported live: on the laptop's **internal** display the overview's **rounded corners
  and the relative scale of its elements look quite off** — sizes that read correctly on the
  3840x2160 external panel do not on the smaller, differently-shaped internal one.
  **Diagnosis (from the `(internal|external){,-grid}.png` screenshot pairs, measured in physical
  px):** it was the first axis — *which scale is in force* — and nothing else. The chrome is
  pixel-for-pixel the **same physical size** in both screenshots (search entry 704x80 at the same
  y, clock glyphs 22px, dash Firefox clip 118x87; app grid keeps 8 columns and only its pitch
  adapts, 360→215px), so every component rendered consistently at scale 2 with the same logical
  constants — **not** a per-component divergence, and the corner radii were correct for the scale
  in force. What was wrong is that scale 2 applied at all: the internal panel is the **same krun
  connector `Virtual-1` at a different mode** (the VM window moving screens changes the mode,
  3840x2160 ↔ 2048x1330, and the EDID with it), and `monitors_xml::setting_for` matched on
  connector alone with `<mode>` parsed-but-ignored — so the scale 2 persisted *for the 4K mode*
  was applied to 2048x1330, shrinking the desktop to a **1024x665 logical canvas**. Fixed-logical
  chrome then legitimately eats ~2x the relative space while the workspace preview (which fits the
  screen) shrinks; the grid's label center-clipping is just the pitch collapsing. mutter treats
  the stored mode as part of the config: assignment fails when the mode isn't available
  (`meta-monitor-config-manager.c:327` "Invalid mode") and `ensure_configured`
  (`meta-monitor-manager.c:684`) falls back to the guessed default; stored configs are also keyed
  on the full monitorspec set (`meta-monitor-config-store.c:2195`), so a changed EDID alone would
  already unmatch them.
  **Fix (`5a4d4458`, this branch):** parse `<mode>` into `MonitorSetting` and gate applicability
  on the output's *current* mode (width/height exact, rate within mutter's
  `MAXIMUM_REFRESH_RATE_DIFF` 0.001 — real mutter writes full-precision rates while our modes
  carry integer mHz; no stored or current mode → no veto). Both consumers
  (`reload_output_config`, `add_output`) pass the mode. With the store inapplicable the internal
  mode falls to the DPI guess: 2048x1330 at 320x180mm → 1.25 → logical **1638x1064**. Pinned by
  `saved_scale_is_pinned_to_its_mode` in `src/monitors_xml.rs`.
  **Not fixed / follow-ups:** (1) `applied_display_config` (a config live-applied earlier in the
  session) sits *above* the store in precedence and is keyed by connector only, so it can carry a
  stale scale across a mode change the same way — mutter invalidates its "current" config when
  the monitor set changes; ours should too. (2) The grid's label overflow is a center-clip with
  no ellipsis (`waita De` for `Adwaita Demo`); GNOME ellipsizes. Cosmetic, only visible on tiny
  logical widths — folded into the adaptive-chrome design below. (3) The missing
  workspace-thumbnail strip in the internal shot is **not** a bug: 2 workspaces there, 3 in the
  external shot, and GNOME's strip only shows above `NUM_WORKSPACES_THRESHOLD = 2`
  (`workspaceThumbnail.js:16,697`). Related and already fixed at a different layer: the expose
  xray was sampling the backdrop unscaled (`39e25b69`) — done, not this.
  **Approved follow-on divergence (Gustavo, 2026-07-26): adaptive overview chrome** — everything
  in the overview except the panel and text sizes adapts to the logical canvas (GNOME's fixed
  constants read comically on small canvases: near-round 30px preview corners, picker gaps
  pegged at the 80px clamp, 64px dash icons over 24px grid icons). Full piece inventory, ramp
  design, testing plan and sequencing: `docs/fork/adaptive-overview-chrome.md`.

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

---

## 7. App-grid drag-reorder & page peeks (cited plan, 2026-07-28)

Goal: dragging an app *within* the grid reorders it, and the left/right page previews appear during a
drag so hovering (or bumping) an edge switches pages. Everything below is 50.1 as shipped.

### 7.1 The four moving parts

**(a) The reserved side bands — `indicatorsPadding`.** `BaseAppViewGridLayout.vfunc_allocate`
(`appDisplay.js:405-430`) sets `grid.indicatorsPadding` to `_getIndicatorsWidth(box)` on both sides,
which is `max(width * PAGE_PREVIEW_RATIO / 2, widest arrow's min width)` — `PAGE_PREVIEW_RATIO` is
`0.20` (`appDisplay.js:47`), so **10% of the app-display width per side**. `AppGrid._updatePadding`
(`appDisplay.js:162-171`) *adds* it to the `.icon-grid` `page-padding-left/right`, so the reserve is
permanent, not drag-only: the grid content box is `page-padding` **plus** 10% each side, and the
navigation arrows live in the reserve rather than in centering slack. We skipped this (listed as a
divergence in `ui/app_grid.rs`); the peek cannot be built without it, since it is the room the
adjacent page's icons slide into.

**(b) The hint bands.** `_prevPageIndicator` / `_nextPageIndicator` are plain `St.Widget`s styled
`.page-navigation-hint {previous,next}` (`appDisplay.js:528-549`), allocated to exactly the reserve
box. `_app-grid.scss:150-170`: a horizontal gradient from `rgba(255,255,255,0.05)` to transparent,
running *inward*, with `$modal_radius * 1.5` rounding on the two **outer** corners only; while
a drag hovers one it gains `.dnd`, a flat `rgba(255,255,255,0.1)`. `$modal_radius` is
`$base_border_radius * 2` = 16 (`_common.scss:33,40`), so the rounding is **24 px**. They fade in over
`PAGE_INDICATOR_FADE_TIME` 200 ms and are shown on **item-drag-begin** — any overview item drag, not
only a grid one (`_onDragBegin` → `showPageIndicators`, `appDisplay.js:923-930`). The *next* hint is
always shown during a drag even on the last page, because dropping there creates a page
(`appDisplay.js:270-274`).

**(c) The peek itself.** `showPageIndicators` eases a 0→1 adjustment over
`PAGE_PREVIEW_ANIMATION_TIME` 150 ms `EASE_OUT_CUBIC` and drops the grid's clip
(`appDisplay.js:441-453`). `_syncPageIndicators` (`:364-397`) then translates, by that value: the
hints inward from `∓indicatorsWidth`, the arrows outward, and — the visible part —
`_translatePreviousPageIcons` / `_translateNextPageIcons` (`:311-362`) slide the *adjacent page's*
icons so that the previous page's last-column icon and the next page's first-column icon come to rest
just inside the reserve. Current-page icons are pinned at 0.

**(d) The reorder.** `_getDropTarget` (`appDisplay.js:1156-1201`) wraps
`IconGrid.getDropTarget` (`iconGrid.js:1032-1120`), which walks the page's items, rejects a point
outside the grid rows entirely (`INVALID`), returns `EMPTY_SPACE` past the last item, and otherwise
classifies the hit as `START_EDGE` / `ON_ICON` / `END_EDGE` against a 20 px `*_DIVIDER_LEEWAY` at each
tile edge. The wrapper then nudges the target to the adjacent item when the reflow would push the
*wrong* way (an insertion that can't "naturally push the item away"), except in the first/last column.
`_maybeMoveItem` (`:768-810`) ignores `INVALID`, `ON_ICON`, the source's own slot and any target on
another page, and otherwise commits the move after `DELAYED_MOVE_TIMEOUT` **200 ms** of the target
holding still — the grid reflows live, mid-drag. `acceptDrop` (`:997-1023`) commits a pending delayed
move if the drop beat the timer. `_onDragCancelled` `_redisplay()`s, i.e. the live reflow is
provisional until the drop. Persistence is `_savePages` (`:1387-1404`) writing
`org.gnome.shell app-picker-layout` as one `{id: {position}}` dict per page — which we already *read*
(`3e6c5f41`).

**(e) Page switching during a drag.** Two mechanisms, `_onDragMotion` (`:932-959`):
1. **Edge bump** — `_dragMaybeSwitchPageImmediately` (`:854-904`): within
   `DRAG_PAGE_SWITCH_IMMEDIATELY_THRESHOLD_PX` **20 px** of the container's left/right edge, switch at
   once, then repeat every `DRAG_PAGE_SWITCH_REPEAT_TIMEOUT` **1000 ms**. Latched by
   `_lastOvershootCoord` so one bump is one switch until the pointer moves >20 px back inside.
   **Disabled when there is more than one monitor** (`:856-858`), where it would fight dragging to the
   next monitor.
2. **Hint hover** — hovering a hint band arms `DRAG_PAGE_SWITCH_INITIAL_TIMEOUT` **1000 ms**
   (`:906-921`), then the same 1000 ms repeat.
Dropping *on* a hint band moves the item to that page (`acceptDrop`, `:1004-1013`).

### 7.2 Slices

* **G1 — `indicatorsPadding`.** Reserve `max(10% of the band, arrow disc + margins)` on each side; lay
  the grid inside the remainder; move the navigation arrows into the reserve. Pure layout; closes a
  listed divergence. Watch: it changes mode/icon-size selection, which is the point.
* **G2 — drop target + live reorder.** `AppGrid::drop_target_at` → `(page, position, DragLocation)`
  with the leeway + the reflow nudge; the 200 ms delayed move; reordering `entries` provisionally;
  committing on drop by writing `app-picker-layout`; reverting on cancel.
* **G3 — the peek.** Hint bands (gradient + outer-corner rounding + `.dnd` fill), the 150 ms
  0→1 adjustment, and the adjacent pages' icons translated into the reserve.
* **G4 — page switching.** Hint-hover 1 s + 1 s repeat, edge-bump immediate + repeat (single monitor
  only), and drop-on-a-hint moving the item to that page.

**Known deferrals** (state them, don't silently skip): no page-*slide* animation, so a switch is a
snap; no folders, so a drop can never make one; and the reorder is grid-internal — dragging a grid
icon onto the dash still pins it, which is the existing behaviour.

## 8. App folders — see & open (cited plan, 2026-07-27)

Goal: the grid shows the user's folders as folder tiles, their apps stop appearing at the top level,
and clicking one opens the folder dialog to launch from. **Folder *editing* is out of scope** (see the
deferrals at the end). Everything below is 50.1 as shipped.

### 8.1 The model

**Where folders live.** `org.gnome.desktop.app-folders folder-children` is an `as` of folder ids; each
id has a *relocatable* `org.gnome.desktop.app-folders.folder` at
`${folderSettings.path}folders/<id>/` (`appDisplay.js:1510-1513`), with keys `name`, `translate`,
`categories`, `apps`, `excluded-apps`.

**Membership** — `FolderView._loadApps` (`appDisplay.js:2164-2199`): the `apps` list first, in order,
then every installed app whose `Categories` intersect the folder's `categories`
(`_getCategories`/`_listsIntersect`, `appDisplay.js:79-95` — `Categories` split on `;`). `addAppId`
drops an id that is in `excluded-apps`, is not installed, is a favorite, is hidden by parental
controls, or is already in the list.

**The name.** `_getFolderName` (`appDisplay.js:97-104`): `name`, and if the folder's `translate`
boolean is set, looked up as a `.directory` file. `shell_util_get_translated_folder_name`
(`shell-app-cache.c:95-147`) scans `$XDG_DATA_DIRS/desktop-directories/<name>` — **user data dir
first**, then system dirs — and reads `[Desktop Entry] Name` as a locale string; **first added wins**.
On this machine `Utilities` → `/usr/share/desktop-directories/X-GNOME-Utilities.directory`.

**Where a folder sits in the grid.** `_redisplay` (`appDisplay.js:1508-1533`) pushes folder icons into
the same `appIcons` array as apps and collects `appsInsideFolders`, which the app list is then filtered
against. So folder ids share the `app-picker-layout` id space and sort by the same comparator — on
this machine `'Utilities': <{'position': <7>}>`, a slot we currently leave as a hole. **An empty
folder is destroyed, not displayed** (`icon.visible`, `appDisplay.js:1523-1527`).

### 8.2 The folder tile

`FolderIcon` (`appDisplay.js:2284-2461`) is `style_class: 'overview-tile app-folder'` — i.e. the same
tile as an app, but *raised*: `.app-folder { @include tile_button($bg:$system_base_color, $raised:
true) }` (`_app-grid.scss:40-42`), so it has a filled button background at rest rather than the app
tile's transparent one. Its "icon" is composed, `createFolderIcon(size)`
(`appDisplay.js:2138-2162`): a homogeneous 2×2 `Clutter.GridLayout` sized `size×size`, holding up to
the first four member icons at `floor(FOLDER_SUBICON_FRACTION * size)` = **0.4×** each
(`appDisplay.js:31`), attached at `(i % 2, floor(i / 2))` — so 4 cells, blank where the folder has
fewer than four apps. `button_mask: PRIMARY`, and `vfunc_clicked` just calls `open()`.

### 8.3 The dialog

`AppFolderDialog` (`appDisplay.js:2463-2600`) is a full-monitor actor (`MonitorConstraint({primary:
true})`) whose background eases to `DIALOG_SHADE_NORMAL` = `rgba(0,0,0,0.8)` (`appDisplay.js:57`),
holding a `.app-folder-dialog-container` bin (`padding-top: $panel_height`) around the
`.app-folder-dialog` box. `_app-grid.scss:53-110`: **720×720** (`$app_folder_size`), radius
`$modal_radius * 4` = **64**, `background-color: $system_overlay_bg_color` (= `system_base_color`
mixed 90% with the fg), `box-shadow: inset 0 0 0 1px $system_borders_color`, its `.overview-tile`s
re-themed against the overlay bg, and `.page-indicators { margin-bottom: $base_padding * 4 }`. The
name sits in `.folder-name-container` (`padding: $base_padding*4 $base_padding*6`, `padding-bottom:
0`) as a `%title_1` label — **20pt, weight 800** (`_common.scss:246-249`). The inner grid is
`FolderGrid` (`appDisplay.js:2067-2084`): **3 columns × 3 rows**, `allow_incomplete_pages: false`,
centered both ways, one grid mode only. A click whose coordinates fall outside `_viewBox.allocation`
pops down (`appDisplay.js:2486`).

### 8.4 The open/close animation

`_zoomAndFadeIn` (`appDisplay.js:2660-2695`): the dialog child starts translated to the source icon's
transformed position, scaled to `source.width / child.width`, opacity 0, and eases to identity over
`FOLDER_DIALOG_ANIMATION_TIME` **200 ms** — `EASE_OUT_EXPO` for the transform, `EASE_OUT_QUAD` for the
shade. `_zoomAndFadeOut` (`:2697-2740`) is the reverse. The **source icon itself** fades out over
`FOLDER_DIALOG_ANIMATION_TIME / 2` while open, delayed by `TIME - duration` on the way back
(`appDisplay.js:2442-2451`).

### 8.5 Slices

* **F1 — the model.** *(landed `5296ead7`.)* `GnomeSettings` reads `folder-children` + each
  relocatable folder schema + the `.directory` name translation; `AppEntry` gains `categories`;
  `Niri::sync_app_grid` resolves membership *before* the sort, drops members from the top level,
  drops empty folders, and gives each folder entry its `app-picker-layout` slot. No chrome — fully
  headless-testable.
* **F2 — the folder tile.** A grid entry becomes app-or-folder; render the raised `.app-folder` tile
  with the 2×2 sub-icon composition at 0.4×. Click opens (no dialog yet — assert the state).
* **F3 — the dialog.** *(landed.)* The 720² panel with its title, its own 3×3 paginated grid,
  launching from inside, click-outside/Esc to close. The inner view is **the app-grid widget
  itself**: GNOME's `FolderGrid extends AppGrid` (`appDisplay.js:2066-2084`) differs only in the
  page modes (one 3×3, no re-flow) and the page alignment (`CENTER` — the spacing stays at its
  base and the slack becomes leading padding, where the top-level grid's `FILL` grows the
  spacing), so `AppGrid::folder_view` sets those two and hover, captions, pagination, dots,
  arrows and the batched icon uploads all come along. Divergences: no rename entry or edit
  button (editing is deferred — the label centers on its own where GNOME balances the button
  with a ghost actor), the panel is *clamped* to the work area rather than allowed to overflow a
  short screen, and it draws on every output like the rest of our overview chrome.
* **F4 — the zoom-and-fade.** *(landed.)* The 200 ms popup/popdown against the source tile's rect,
  the shade ramp, and the source icon's half-duration fade. gnome-shell moves exactly one actor —
  the `.app-folder-dialog-container`, which fills the monitor — translating it onto the source
  icon's top-left and scaling it by `source.width / child.width` **per axis**, so the scale is
  deliberately non-uniform; everything inside rides along, which for a flat set of axis-aligned
  quads is the same as mapping each element's box through the transform (`ui::folder_dialog::Zoom`,
  the same `set_location`/`set_size` trick the popover's open scale uses). The shade is the dialog
  *actor's* own `background_color`, not part of that container, so it only fades.
  The three quantities are independently curved off one duration — transform `EASE_OUT_EXPO`,
  shade `EASE_OUT_QUAD`, panel opacity riding the transform's ease in and its own quad out — so the
  animation is a **linear** timeline with the curves applied per quantity, not one eased value.
  The source tile fades out over `TIME/2` as the dialog opens and back in as it shrinks home, which
  is what makes the panel appear to become the icon again; in the grid that tile leaves the shared
  label and folder-background bakes and is re-emitted on its own, with the fading *id* in the bake
  revision and the alpha deliberately not (else the page's text re-shapes every frame).
  **Divergence, chosen on the seat 2026-07-28: the close's transform is `EASE_OUT_QUAD`, not GNOME's
  `EASE_OUT_EXPO`.** Reported as "the folder disappears before the icon is restored". Measured, the
  gap is real *and* faithful: exponential puts 82% of the travel in the first quarter, so GNOME's
  panel is within 3% of the tile by the halfway mark and spends the whole back half as a stationary
  speck — exactly when the source icon's `TIME/2`-delayed fade is coming up, leaving no motion to
  carry the eye across the hand-over. Quad puts the panel's size on the same curve as its opacity
  (and the shade), so all three are locked and the panel arrives at the tile at the instant it
  finishes fading. The source tile's fade keeps GNOME's timing on both halves, and the *open* half
  keeps expo — it has no gap to close.
  A pure cross-fade (`source = 1 - content`, no delay) was tried first and closes the gap
  arithmetically, but the slower collapse read better on the seat; it is the motion, not the summed
  opacity, that carries the hand-over.
  Divergences: an interrupted transition runs only the time it has left, where Clutter re-eases
  from the current value over a full duration (they agree whenever nothing is interrupted); and
  leaving the app grid drops the dialog outright rather than letting it fade with the overview
  group, so a re-opened overview can never catch a ghost still shrinking inside it.

**Deferrals** (state them, don't silently skip): no drag-to-create a folder, no drag in/out of one, no
rename entry (so `translate` is only ever written by the seed, never by the user), and no auto-delete
of a folder emptied by removal — all of those are the *editing* half, which needs the rename popup and
the per-folder writes that go with a drag.

`_ensureDefaultFolders` was in that list and came **out** of it: it is a one-shot seed, not
interactive editing, and without it the feature is invisible on any profile that has never run stock
gnome-shell — which is exactly what happened on the live seat, where `folder-children` read `@as []`
while a profile that had run gnome-shell read `['Utilities', 'YaST']`. Landed with F2's follow-up.

## 9. App-grid keyboard navigation (cited plan, 2026-07-28)

The grid is mouse-only today: arrows in the overview still run niri's window binds
(`hardcoded_overview_bind`, `input/mod.rs`), so with the app grid open Left/Right move *window*
focus behind it and nothing can be launched without the pointer.

### 9.1 Where the behavior actually lives

There is no keyboard-navigation code in `appDisplay.js` — arrows are **St's spatial focus
navigation** over the `can_focus` icons, and `AppDisplay` only adds the paging keys:

* `appDisplay.js:1599-1618` `_onKeyPressEvent`: `if (this._displayingDialog) return EVENT_STOP;`
  then `Page_Up` → `goToPage(currentPage - 1)`, `Page_Down` → `+1`, `Home` → `goToPage(0)`,
  `End` → `goToPage(nPages - 1)`; anything else propagates.
* `appDisplay.js:2788-2789` `AppFolderDialog.vfunc_key_press_event` → `navigate_from_event`, and
  `:2516` `global.focus_manager.add_group(this)` — the dialog is its own focus group, which is why
  the paging keys above are swallowed while it is up but the arrows still work inside it.
* `can_focus: true` on `AppIcon` (`:1855`) and `FolderIcon` (`:2289`); activation is `St.Button`'s
  own Enter/space.
* `st-widget.c:1932-2030`: `filter_by_position` keeps the children **strictly** in the requested
  direction (bbox thresholds `x1 >= rbox.x2 - 0.1` for RIGHT, etc. — 0.1 px of slop, and *no*
  overlap test despite the comment), then `sort_by_distance` picks the nearest by **midpoint**
  squared distance. So "right" is genuinely spatial, not `index + 1`.
* `iconGrid.js:1196-1208`: every child gets a `key-focus-in` handler calling
  `_ensureItemIsVisible` → `goToPage(getItemPage(item))` — **focus drags the page with it**.
* `appDisplay.js:1901` `const expand = this._forcedHighlight || this.hover || this.has_key_focus();`
  with `vfunc_key_focus_in/out` → `_updateMultiline` (`:2010-2017`): key focus expands the caption
  exactly like hover, and the two are independent (hovering does not move key focus).
* The focus *look* is `.overview-tile:focus` → `tile_button` flat → `button(focus, …, flat)`
  (`_drawing.scss:308-327`, `:361-372`): `focus_ring()` = `box-shadow: inset 0 0 0 2px` in
  `st-transparentize($accent_color, .2)` (i.e. accent @ 0.8 — the same ring `widget::Button`
  already draws), over `background-color: focus_bg_color(transparentize($system_base_color, .75))`
  = `st-mix($accent, rgba(#222226, .25), 5%)`. `st-mix` is St's own premultiplied LERP
  (`st-theme-node.c:637-693`), not Sass's, so that resolves to ≈ `rgba(37, 51, 71, 0.29)` at the
  default blue.

### 9.2 How it maps onto our grid

Our grid lays out only the current page, so the spatial rule needs the viewport it is a window
onto. Reconstruct it: the virtual rect of absolute entry `i` is its in-page cell rect shifted by
`(i / per_page - current_page) * area.width` — pages are laid out edge to edge at the band's own
width. With that, GNOME's filter-then-nearest runs verbatim and every edge case comes out right for
free: Right from the last column lands on the same row of the next page; Down on the last row finds
no candidate and does nothing; a short last page falls back to *its* nearest tile rather than a
hole. Then `set_page(i / per_page)`, which is `_ensureItemIsVisible`.

Because the folder dialog's inner view **is** the app-grid widget (§8.5 F3), all of this lands in
the dialog at the same time, including its own pagination past nine apps.

### 9.3 Slices

* **K1 — the focus cursor.** *(landed.)* `AppGrid` gains `focused: Option<usize>` plus `focus_navigate(dir)`
  (the spatial rule above, paging as it goes), the ring/wash element, and the caption expansion
  under key focus. Input: Left/Right/Up/Down drive the grid instead of `FocusColumnLeft`/… while
  the grid or a folder is open, and Enter/space activates the focused tile — launching an app or
  opening its folder, the same two paths a click takes.
* **K2 — the paging keys.** *(landed, with K1.)* `Page_Up`/`Page_Down`/`Home`/`End`, swallowed while a folder is up.

* **K3 — Tab.** *(landed.)* Tab and Shift+Tab walk the icons, entering the grid when nothing
  is focused.

**Divergences to state:** GNOME reaches the grid from the search entry, whose own focus is the
starting point of the first arrow press; we have no stage-wide focus chain, so the first arrow with
nothing focused takes the first tile of the *current page*. Tab, which GNOME does define here, keeps
GNOME's entry point instead: the first icon of the whole grid, paging back to it.

### 9.4 What Tab actually is (K3)

Not group cycling — that is **Ctrl+Alt+Tab**, a separate accessibility switcher with its own popup
(`ctrlAltTab.js`). Plain Tab in the overview is two behaviours that meet at the focus manager:

* `StFocusManager` walks **up** from the focused actor for a registered group and navigates within it,
  with `wrap_around` set for Tab and `ISO_Left_Tab` (`st-focus-manager.c:82-124`). The app grid **is**
  such a group — not registered directly, but through `ctrlAltTabManager.addGroup(this.appDisplay, …)`
  (`overviewControls.js:392`), which calls `focus_manager.add_group` for any `St.Widget`
  (`ctrlAltTab.js:41-43`). This is also what makes the *arrows* of §9.3 work, so the two share a
  foundation.
* When the focus manager finds no group — focus on the search entry, or nowhere — the event reaches
  `ControlsManager`'s `connect_after` stage handler, which uses Tab (**and Down**) to *enter* the
  grid at its first icon and `ISO_Left_Tab` to enter at its last (`overviewControls.js:441-473`).

Tab's traversal is **child order**, not the arrows' spatial one: `st_widget_real_navigate_focus`
walks `st_widget_get_focus_chain` for the tab directions (`st-widget.c:2086-2103`) and reverses it
for backward. So at the end of a row Tab goes to the row below where Right goes to the next page —
the sharpest test of having ported two traversals rather than one.

An open folder is its own group (`appDisplay.js:2516`), so Tab cycles inside it and never escapes to
the grid behind.

## 10. App-grid page slide & swipe (cited plan, 2026-07-28)

Paging works (dots, arrows, wheel, keys) but every page change is a hard cut: the grid draws
exactly one page and swaps it in a single frame. GNOME slides, and the *same* state that
slides is what a touchpad swipe drags — which is why these are one plan, not two.

### 10.1 The state GNOME animates

The grid is a scroll view over all pages laid side by side, and the page position is one
continuous adjustment value:

* `iconGrid.js:1348-1378` `goToPage`: sets `_currentPage = pageIndex` **immediately**, then
  eases the adjustment to `pageIndex * pageWidth`, `EASE_OUT_CUBIC` over `PAGE_SWITCH_TIME`
  **300 ms** (`iconGrid.js:13`). So the logical page changes at once — hit-testing, key focus
  and drop targets all follow the destination — and only the *view* lags.
* `appDisplay.js:706-735`: `_swipeBegin` cancels the running transition and confirms snap
  points `0..nPages-1` at the current fractional progress with `Math.round(progress)` as the
  cancel target; `_swipeUpdate` sets `adjustment.value = progress * page_size` **1:1**;
  `_swipeEnd` eases to `endProgress * page_size` with the same `EASE_OUT_CUBIC` over a
  velocity-derived duration, then `goToPage(endProgress, false)` to settle the bookkeeping.
* `swipeTracker.js`: one page is `TOUCHPAD_BASE_WIDTH` **400 px** of horizontal travel
  (`:14,183`); scroll events are scaled by `SCROLL_MULTIPLIER` **10** (`:18`). The end point
  is `_getEndProgress` (`:601-631`): below `VELOCITY_THRESHOLD_TOUCHPAD` **0.6** it snaps to
  the nearest point, otherwise it projects with `DECELERATION_TOUCHPAD` **0.997** (a parabola
  past `VELOCITY_CURVE_THRESHOLD` 2) and takes the point that projection lands on. The settle
  duration is `|Δprogress| / velocity * DURATION_MULTIPLIER` (3, the derivative of
  `easeOutCubic` at 0) clamped to `[100, 400·log2(1+nPoints)]` (`:642-655`).
* The **wheel is not the swipe.** `_onScroll` (`appDisplay.js:658-704`) is only reached when
  the tracker declines the event; it does a whole `goToPage` behind a `SCROLL_TIMEOUT_TIME`
  cooldown. We already have that half, and the continuous-scroll branch beside it is already
  consumed and reserved for exactly this.
* What does **not** slide: the page indicators and the navigation arrows are in `_box`,
  outside the scroll view — `goToPage` tells `_appGridLayout` separately
  (`appDisplay.js:1251-1252`).

### 10.2 How it maps onto our grid

`AppGrid::current_page` keeps its meaning — the destination, which everything logical reads —
and gains a continuous `page_pos` beside it, in pages. `render` draws the pages `page_pos`
spans (one when settled, two mid-slide), each offset by `(page - page_pos) * area.size.w`.
The `app_display` band is the full output width, so a page sliding out leaves the screen on
its own; there is nothing to clip against.

The catch is the caches. `bake` (the page's labels + chrome) and `folder_bake` are one each
and keyed by `content_rev`, which *bumps on a page change* — so two pages on screen at once
would fight over one texture. They become keyed by page, and `long_labels` by `(page, tile)`;
`content_rev` then stops bumping on a page change at all, which is a small win in its own
right: after the first visit a page switch re-bakes nothing.

### 10.3 Slices

* **P1 — the slide.** *(landed.)* `page_pos`, the two-page render, the per-page caches, `EASE_OUT_CUBIC`
  over 300 ms. Dots and arrows stay put. Wheel/dot/arrow/key paging all inherit it.
* **P2 — the swipe.** *(landed.)* The reserved continuous-scroll branch drives `page_pos` 1:1 at 400 px
  per page, and the release projects to a snap point and eases there. Reuses the existing
  `ScrollSwipeGesture` (begin/update/end from axis events) and `SwipeTracker`
  (velocity + `projected_end_pos`), the same pair the overview's own scroll swipe uses.
  A **pointer drag** on the grid background pans it too: the tracker's `Clutter.PanGesture`
  takes `min_n_points: 1` and `allowDrag` defaults to true (`swipeTracker.js:367-404`), so
  a plain click-drag is a swipe — the only route to one on a machine with no touchpad.
  It is 1:1 with the *content*: `_swipeBegin` confirms the swipe with the grid's own
  allocation width and `_updatePanGesture` divides by that (`appDisplay.js:713-716`,
  `swipeTracker.js:578-585,710-711`), so the pages travel exactly as far as the pointer.
  `TOUCHPAD_BASE_WIDTH` 400 is a touchpad-only override, for a device whose physical size
  Clutter cannot know — using it for a drag makes the grid ~5× too fast on a 1920 px band.
  It is judged on release against the lower
  `VELOCITY_THRESHOLD_TOUCH` 0.3, and its sign is *inverted* relative to the scroll path:
  `_getGestureDirFactor` is -1 for LTR (`:689-695`), because the pages follow the pointer.
  A press that lands on an icon belongs to that icon's own DND instead.

**A unit trap worth recording**, found by the test failing: GNOME's velocity history holds
**raw pixel deltas** (`swipeTracker.js:597,676`), and `_getEndProgress` compares them
against `VELOCITY_THRESHOLD_TOUCHPAD` *before* the normalization at `:644` — so 0.6 is
0.6 px/ms, not 0.6 pages/ms. It then adds a pixel-scale projection (`velocity * slope`) to
a page-scale progress and clamps the result to the snap points either side of where the
gesture began. Those units do not agree, and the consequence is that **any** velocity past
the threshold overshoots and is decided by the clamp: a flick moves exactly one page in the
direction of travel. We reproduce that outcome rather than the arithmetic that reaches it,
because the arithmetic only works by way of the clamp.

## 10.4 Resting caption height — chosen divergence (2026-07-28)

GNOME's collapsed tile caption is **one** line: `StLabel` puts `PANGO_ELLIPSIZE_END` on its
`ClutterText` (`st-label.c:331`) and `_updateMultiline` turns wrapping off outright until the
tile is hovered/focused (`appDisplay.js:1891-1924`). Ours rests at **two**
(`widget::TILE_LABEL_LINES`), so most two-word names are readable without hovering; hover still
expands, now up to `TILE_LABEL_EXPAND_LINES` (3), and anything past that ellipsizes rather than
losing text — the cap itself stays a divergence, deliberately (a bake needs a size up front, and
a pathological name should not be able to grow a tile without bound).

The room comes from the tile's own box: a second line takes the bottom padding plus 6px of the
row gap, which at minimum row spacing is still 18px clear of the icon below.

**Search results rest at the same count.** They are the same `.overview-tile` (`search.js:142`),
and `expandTitleOnHover: false` (`appDisplay.js:1837-1841`) only stops them *expanding* on hover —
the resting line count is `StLabel`'s, so it is the same one line in GNOME and the same divergence
here. Their card reserves the overhang (`LABEL_OVERHANG`) and the selection wash grows with the
caption it covers, as the grid's does for an expanded one.

Three clips had to grow with the second line: `Painter::labelled_tile` (which clipped to the tile
box), the neighbouring-page peek bake (sized from the block) and the results card (one bake, so a
line past its edge is simply not drawn).

## 11. App-folder editing (cited plan, 2026-07-28)

§8 landed folders read-only. This is the other half: making them, filling them, emptying them
and naming them. All four are drag-and-drop plus a gsettings write, and we already have the
drag machinery — [`DragLocation::OnIcon`] is exactly GNOME's "over the body of another icon,
outside the divider leeways", which today means "not an insertion point".

### 11.1 The four operations

* **Create** (`AppIcon.acceptDrop`, `appDisplay.js:3152-3160`): dropping app A on app B calls
  `view.createFolder([B.id, A.id])` — the *hovered* icon first, which is where the folder is
  placed. `createFolder` (`:1699-1751`) appends a fresh `GLib.uuid_string_random()` to
  `folder-children`, writes `name` + `apps` into the new relocatable store, redisplays, then
  moves the folder to B's old `(page, position)` — adjusted down by however many of the folded
  apps sat before it on that page — and saves the layout.
* **Name it** (`_findBestFolderName`, `:114-144`): the first category common to *every* app,
  whose `<category>.directory` has a translated name; otherwise "Unnamed Folder". Note the
  categories come from the apps, so this is the same data §8's folder reading already parses.
* **Join** (`FolderIcon.acceptDrop`, `:2400-2409` → `FolderView.addApp`, `:2223-2237`): append
  the id to `apps`, and drop it from `excluded-apps` if it was there (only categories-based
  folders can have it). `_canAccept` (`:2385-2397`) refuses a source already in the folder.
* **Leave** (`AppDisplay.acceptDrop`, `:1680-1696` → `FolderView.removeApp`, `:2239-2272`):
  dragging an icon out of the dialog onto the grid removes it from `apps` and pops the dialog
  down. **If it was the last app the folder is deleted**: every key of the relocatable schema
  is reset (which is how a relocatable store is removed) and the id comes out of
  `folder-children`. For a categories-based folder the app is instead added to `excluded-apps`,
  because it would otherwise come straight back from its category.
* **Rename** (`_addFolderNameEntry`, `:2531-2601`): an `icon-button` toggles a
  `.folder-name-entry` in place of the `.folder-name-label`; `activate` commits and shows the
  label again. The button is balanced by an equally-sized ghost actor so the label stays
  centred — the divergence §8 recorded, now to be undone.

### 11.2 The hover affordance

Creating a folder is the one drop that is *not* announced by an insertion gap, so GNOME gives
it its own: after **500 ms** of hovering another icon's body, that icon takes the `:drop`
pseudo-class and its own icon eases down to `FOLDER_SUBICON_FRACTION` (0.4) with its label
hidden — a preview of the 2×2 it is about to become (`_setHoveringByDnd` + `_showFolderPreview`,
`appDisplay.js:3102-3149`). Leaving before the timeout fires cancels it. This is the same
"hold still to commit" idiom as the 200 ms reflow delay §7 already implements, and it is what
keeps a folder from forming every time a drag crosses an icon.

### 11.3 Slices

* **E1 — create.** ✅ The `OnIcon` drop makes a folder: the 500 ms preview, the uuid + name +
  `apps` write, and the folder landing in the target's slot.

  Two things worth carrying forward. The id is **minted by the caller** (`gnome::new_folder_id`)
  rather than by the writer, because the model has to place the folder in the grid *now* — the
  gsettings write only comes back through the watcher reload, and by then `_savePages` has
  already had to name the folder to give it the hovered icon's slot. And the position correction
  (`:1725-1733`) is a per-page `reduce` in GNOME; over one flat list it is just "did the source
  sit earlier", which agrees with GNOME within a page and is right, rather than one slot off,
  across pages.
* **E2a — join.** ✅ Dropping an app on a folder tile adds it (`FolderIcon.acceptDrop` →
  `FolderView.addApp`). The `apps` write is a **read-modify-write on the settings thread**, not a
  write-back of the resolved members: a categories-based folder sweeps in apps that were never in
  `apps`, and persisting the resolved list would freeze the sweep. `excluded-apps` loses the app
  at the same time, which is the only way back in for one that was excluded.

  The two drop states share a field (`AppGrid::drop_hover`) but not a schedule: a folder takes
  `:drop` the instant the drag reaches it, an app only after 500 ms, because on an app the state
  *is* the offer to make a folder and every icon a drag crosses would otherwise flash it.
* **E2b — leave.** ✅ Dragging an app out of the open folder dialog removes it, with the
  emptied-folder delete (reset every relocatable key, drop the id from `folder-children`) and the
  `excluded-apps` push that a categories-based folder needs to make the removal stick.

  The app is not in the top-level grid while it lives in a folder, so the drag begins by putting
  a **placeholder** there (`_ensurePlaceholder`, `:1434-1448`) — withdrawn if the drop goes
  nowhere, and the real tile if it lands. Only a landing *in the grid* takes the app out of the
  folder: pinned to the dash or dropped on a workspace it stays put, which is what GNOME's
  `AppDisplay.acceptDrop` (and only that one) calling `removeApp` amounts to.

  While the dialog is up it owns the drag — the grid under it is covered and resolves no target
  (`_onDragMotion` returns CONTINUE when `_currentDialog`). Leaving the panel lightens the shade
  and starts a 500 ms countdown to popdown; a drop before that fires is the dialog's own
  `acceptDrop`, which pops down, removes and focuses the app in the grid.

  **Divergence (deliberate):** GNOME deletes the folder when its **`apps` key** empties
  (`:2245-2262`). For a categories-based folder that means removing a single swept-in member
  destroys the whole folder, because `apps` was empty to begin with. We delete when the folder
  has no *members* left — identical for every explicit-apps folder, and what the user is
  actually asking for.

* **E2c — reorder inside a folder.** ✅ `FolderView.acceptDrop` (`:2213-2221`). `FolderView` is a
  `BaseAppView`, so it inherits the same `_maybeMoveItem` the app display uses: a drag that stays
  inside the panel arms the same 200 ms delayed move against the folder's own view, and the drop
  writes `_orderedItems.map(item => item.id)` straight back to the folder's `apps`.

  That write is deliberately **not** a read-modify-write, and GNOME's is not either: for a
  categories-based folder it lists the swept-in members in `apps` explicitly. `categories` stays,
  so later installs are still swept in; only what was already there gains a position.

  The drop boundary is the **view**, not the panel: the folder's name row has no delegate of its
  own, so a drop there bubbles to the dialog actor (which covers the monitor) and
  `AppFolderDialog.acceptDrop` (`:2857-2865`) takes the app *out* — the same as a drop on the
  shade. `_withinDialog` (`:2807-2810`) measures the panel, but it only drives the backdrop
  lightening and the popdown countdown, never who accepts.

  Two grid-side bugs fell out of the same drag path: the frame a drag *begins* on drove the grid
  behind the dialog (the motion path was gated on `_currentDialog`, the begin path was not), and a
  drop onto another grid icon never took the app out of the folder it came from, so dragging one
  from folder A into folder B copied it.

  **Divergence (deliberate) — folding/joining from inside a folder.** GNOME refuses it: both
  `AppIcon._canAccept` (`:3118-3123`) and `FolderIcon._canAccept` (`:2386-2392`) require the
  *source's* view to be an `AppDisplay`, so an icon dragged out of a folder falls through to a
  plain grid reorder plus `removeApp`. Ours accepts, because our icon offers to — it takes the
  `:drop` state for a folder-sourced drag like any other — and an offer that silently does
  something else is worse than the divergence.

  **Page-switching inside a folder ✅** — the same three mechanisms the grid has, pointed at
  the folder's view while the dialog holds the drag: the preview bands slide in
  (`showPageIndicators` is `BaseAppView`'s, so the folder's view peeks too), the edge bump
  flips at once, hovering a band flips after a beat, and a drop *on* a band sends the member
  to that page and follows it (`:827-959`, `:1004-1013`).

  Two more, both from a folder big enough to paginate: the dialog never asked its inner view
  whether an animation was running, so the page slide advanced only on frames something else had
  asked for; and nothing clipped the view, so the outgoing and incoming pages slid across the
  desktop on either side of the panel (`clip_to_allocation` — the app grid gets it from the output
  edge, a 700px island in mid-screen does not).
* **E3 — rename.** ✅ The edit button and the entry in the dialog.

  The name band now sizes to its tallest child (the entry, i.e. the line plus `%entry_common`'s
  9px), because GNOME's stack is allocated for the label *and* the entry at once — so the grid
  area starts 18px lower than it used to. The label left the panel bake and became its own
  element: it cross-fades with the entry over 300 ms, and a per-frame alpha inside a bake is the
  re-shape-every-frame trap.

  **Divergences:** the caret is at the end only (as in the overview search), so there is no
  mid-string editing; the one selection GNOME sets up — select-all on open — *is* modeled,
  because typing over the old name depends on it. Escape still pops the dialog down rather than
  just leaving the entry, which is what GNOME does too (the entry does not consume it, so the
  dialog's grab does). A checked edit button draws like a hovered one: `.icon-button` inside
  `.app-folder-dialog` restyles only normal/hover/active.

## 11b. The collapsing search entry + the shared editing model (landed 2026-08-02)

**Divergence, approved.** GNOME rests the entry as the full 24em pill with `hint_text: _('Type
to search')` inside it (`overviewControls.js:324-331`). We rest as a **puck** — a `PUCK_D` = 56px
circle, so `$forced_circular_radius` makes it round — parked at the right end of the same
footprint, holding only `edit-find-symbolic`. **No hint text at all**, in the pill or beside it:
at rest this is a *button*, sized as one (a touch under the dash's 64px icon, the biggest round
target on the overview) rather than as a shrunken text field. Clicking the puck or typing grows
it leftward to GNOME's pill with the **right edge pinned**, the find glyph sliding from the
puck's centre into the leading gutter. Escape/clear collapses it again. Everything about the
*expanded* entry — width, height, radius, fill, insets, font — is still GNOME's. Animated by
`Niri::overview_search_expand`, a twin of the existing `overview_search_fade`, whose progress is
pushed into the model so **hit-testing follows the animating pill** rather than snapping to its
destination.

Two consequences of the puck being *bigger* than the pill:
* The pill's **height** is now a parameter of `Entry::layout`/`Entry::bake` (and of
  `EntryStyle::radius`, or the shrinking puck would square off its corners), lerped from
  `PUCK_D` to `Entry::HEIGHT` about a **fixed centre** so the control opens symmetrically.
* `PREFERRED_ENTRY_HEIGHT` reserves the **puck's** footprint, not the pill's, so the resting
  button does not overhang the thumbnail strip. The open pill therefore centres in that band
  instead of sitting on GNOME's literal `margin-top` — the price of the divergence.
* The find glyph exists at two fixed sizes — `PUCK_ICON_PX` = 24 and `Entry::ICON_PX` = 16 —
  **cross-faded**, never a px lerped with the expansion: `IconCache` is keyed `(name, px,
  color)`, so a per-frame size re-rasterizes the SVG every frame and accretes a cache entry per
  step (the same trap the alpha-in-the-tint note above describes).

**Icon insets were wrong and are now cited.** `Entry::ICON_INSET` was 16. `st_entry_allocate`
puts an icon box flush with the **content** box, at zero extra offset (`st-entry.c:452-467`),
so the glyph centre is `9` (`%entry_common` padding, `_common.scss:177`) + `4`
(`.search-entry-icon { padding: 0 $base_margin }`, `_search-entry.scss:13`) + `8` (half a 16px
glyph) = **21**. Text starts one whole icon box plus `priv->spacing` further in — and that
spacing is **hardcoded** `6.0f` in `st_entry_init` (`st-entry.c:1025`), not a CSS property and
with no setter — so `9 + (4+16+4) + 6` = **39**. The `.search-entry-icon { margin-top: 2px }`
optical nudge (`_search-entry.scss:12`) rides on top.

**`ui::text_edit::TextEdit` — the shared editing model.** Every entry hand-rolled
`push`/`pop`, so all five shared one ceiling: caret at the end, no selection, and any modified
key refused outright. `TextEdit` owns the string, caret and selection bound and implements
GNOME's bindings, which live in two files: `clutter-text.c:4338-4430` (motion, Ctrl word
motion, Home/End, `Ctrl-a` = **select all** and *not* beginning-of-line, `Ctrl-BackSpace` /
`Ctrl-Delete`, Return = activate) and `st-entry.c:743-762` (`Ctrl-u`/`Ctrl-k`, the two Emacs
bindings GNOME ships unconditionally). Shift-selection is `clutter_text_add_move_binding`
(`:3545-3577`) — every motion registered four ways, each handler collapsing the selection only
when Shift is absent. Word boundaries come from `unicode-segmentation`, the UAX #29 annex
Pango's `is_word_start`/`is_word_end` log attrs implement.

Adopted by **all five** entries: overview search, polkit password, lock screen, run dialog,
folder rename. `widget::Entry` grew a measured caret, a selection wash
(`selection-background-color`, the accent at 30%) and horizontal scroll, replacing the U+258F
glyph that used to be appended to the string.

**Divergence — the Emacs key theme.** `org.gnome.desktop.interface gtk-key-theme` is a pure
GTK mechanism; greps for `key-theme` over `gnome-shell/src`+`js` and all of mutter return
nothing, so shell entries ignore it. We honor it: a user who set that key means it for every
field on their desktop. Off by default, so shipped behavior is still GNOME's; `KeyTheme::Emacs`
only adds bindings, with one deliberate override (`Ctrl-a` becomes beginning-of-line).

**Not ported:** the `Ctrl-c`/`Ctrl-v`/`Ctrl-x` clipboard set (`st-entry.c:672-739`) — it needs
the Wayland selection, so it belongs to a caller, not to a plain-data model.

**Traps this turned up.**
* **The clear glyph's hit disc covers the whole puck.** `Entry::hit` gives the 16px glyph a
  generous 32px target, which is right inside a 352px pill and catastrophic inside a 56px one:
  any click on a resting-but-active entry landed on Clear and wiped the query. It is now
  hittable only at full expansion — which is also exactly when it finishes fading in, so what
  you can hit is what you can see.
* **The lock screen's first keystroke.** `type_char` raised the prompt **before** testing
  whether the entry was live, because raising it is what makes it live — typing a password
  blind from the clock page must not eat its first letter (`unlockDialog.js:672-692`). Routing
  through `entry_key` reintroduced the bug until the order was restored; the corpus caught it.
* **`polkit_dialog::clear_entry` promised zeroing it never did** — a plain `clear()` under a
  comment saying it overwrote the bytes first. Both password entries now use
  `TextEdit::secure_clear`, which carries the volatile zero + preallocation over.
* **The caret must ride the pen origin, not the ink box.** `text_band` anchors a run's *ink*;
  caret and selection are measured from the pen origin (`anchor * scale - ink_x`), or they
  drift by the first glyph's left bearing.
* **A caret is not in a text-keyed revision.** `Entry::bake` folds cursor/selection/scroll into
  the caller's revision itself rather than trusting every caller to remember — moving the caret
  changes what is drawn without changing the text.
* **A caret sized from the ink box vanishes exactly when it matters.** Both entry surfaces first
  derived the caret's height (and, in the search entry, its existence) from the drawn glyphs —
  so an emptied field drew *nothing at all*, which reads as a dead control. The caret is gated
  on **focus**, never on there being text, and its band is a fixed inset (or the run's line box),
  never the ink.
* **A kill buffer is a second copy of the password.** `Ctrl-u`/`Ctrl-k` are *default-theme*
  bindings and reach both password entries. GNOME discards what they delete (`st-entry.c:743-762`
  calls `clutter_text_delete_text` and nothing else) and has no `Ctrl-y` to paste it back, so
  remembering it there bought nothing and cost a full unzeroed copy of the secret, surviving
  `secure_clear`. `TextEdit::kill` is now written only under `KeyTheme::Emacs`, zeroed by
  `secure_clear`, and zeroed before every overwrite.
* **A setter nothing calls is a feature nothing has.** `OverviewSearch` held its own `key_theme`
  field with a `set_key_theme` that was never wired, so four entries honored `gtk-key-theme` and
  the flagship one silently did not. The theme is now a `handle_key` argument read live at the
  call site, like the other four — there is no field to forget to feed.
* **`TextEdit`'s Activate arm is plain-only, so a caller that owns Return must claim it first.**
  Ctrl+Enter belongs to whoever owns the field (GNOME's open-in-new-window), so the model refuses
  it — which silently broke the run dialog, where `CONTROL_MASK` on the activate event means "run
  in a terminal" (`runDialog.js:113-114`, `_run(input, inTerminal)` `:204,218`), and the folder
  rename, which committed on Return with any modifier.

**Known limitations, deliberate.** The caret maps an offset to an x by re-measuring
`text[..offset]`, which assumes logical order is visual order — wrong for **RTL/bidi** runs, where
the caret lands at the LTR-prefix width. Fixing it needs an index→x mapping out of the shaper
(cosmic-text has the per-glyph byte ranges; `niri_vk::text::ShapedRun` does not expose them), not
a change in the widget. And `Entry::bake`'s horizontal scroll is derived from the caret alone
rather than held as view state, so walking Left through overflowing text holds the caret at the
trailing edge and slides the text under it where GNOME would hold the viewport — monotonic, and
it never hides the caret, but it is not GNOME's.

## 12. Chrome divergences on the thumbnail strip (landed 2026-07-28)

Approved as one batch; the first three landed in `66953ae5` + `d2e5bae9`.

* **The search entry floats right.** ✅ It no longer takes a full-width row at the top of the
  work area (`ControlsLayout::search_entry` is now pill-wide and right-anchored), so the strip
  and the picker start one `spacing_top` below the panel instead of a whole entry height
  further down. The two full-width content surfaces — the search results strip and the app
  grid — still reserve the entry's height, because they would otherwise render underneath it.
* **The strip gets double the band.** ✅ `MAX_THUMBNAIL_SCALE` 0.05 → 0.10, so a thumbnail
  (which keeps the output's aspect) covers four times the area. Judged live at 1024×665
  (`NIRI_HEADLESS_MODE=2048x1330` + scale 2). **Superseded 2026-08-03 (§12.7):** the strip is the
  app-grid row, so its size is `small_workspace_height` and its band is the full view width,
  overlapping the floating pill rather than dodging it.
* **Dragging a thumbnail reorders the workspaces.** ✅ macOS Mission Control's gesture, on top
  of gnome-shell's window-onto-thumbnail drop, which is kept — the two are told apart by what
  the press landed on. `ThumbGrab` (`src/input/thumb_grab.rs`) recognizes like `MoveGrab`: under
  8px the release is the plain click that activates the workspace.
* **Picker slots keeping clear of the workspace edges.** ✅ `82c5c4c4`. Neither suspect was
  missing — we already inset by the work area and already apply
  `WINDOW_PREVIEW_MAXIMUM_SCALE = 0.95` (`workspace.js:18`). What made the bottom tight is that
  gnome-shell lays out over the raw work area (`_getAdjustedWorkarea`, `:573-581`) while the
  background the previews sit on is the whole monitor, so the top panel's strut is clearance
  the top edge gets and the bottom does not: 40px at the sides, 51 above, **22 below**.
  `Workspace::expose_area` now symmetrizes the working area about the view — each axis inset by
  the *larger* of its two struts, which is still a subset of the working area (so a bottom dock
  is respected, just matched at the top) and costs the preview no size, because the cap still
  binds and the slot only moves. **A padding constant would have been a no-op**: it has to
  exceed the slack under that cap (~26px at 1920×1080) before it moves anything at all.

### 12.1 Preview close button is unforgiving to hit ✅ `00cd6ff6`

Gustavo, from live use: the close button on a hovered picker preview can only be clicked on the
part of it that overlaps the window. The button *overhangs* its preview
(`window_preview::close_rect`, hit-tested first in `State::overview_hit`), but moving the pointer
onto the overhanging part leaves the preview's own hover region — so the preview de-emphasizes
and the button fades out from under the pointer.

The bug was in what counts as "still hovering this preview", not in the button's hit rect —
gnome-shell gets it for free because the button is a *child actor* of the preview and so is
inside its own reactive box. The slot hit test now falls back to `preview_hover_under`, which
takes the preview whose `window_preview::hover_rect` holds the pointer: the slot plus a little
slop and, whatever the slop, the whole close rect. Only previews *already* showing an overlay
count, so it can only hold a hover the slot started — it never steals one from a neighbour and
cannot arm one from outside, where the button is not drawn to be aimed at.

### 12.2 Both overview workspace rows overflow rather than shrink, and scroll ✅

Gustavo, from live use: the thumbnail strip auto-scaled to fit as workspaces were added, but the
workspaces shown in the **app-grid** state did not, so with enough of them some became
unreachable. Asked which way to settle it, he chose **scrolling for both** — and the thumbnail
strip's shrink-to-fit went with it: "scrolling is actually better, we can do the same for the
thumb strip (both should not overlap the search box btw), and the scroll offset should follow
the selected workspace."

**Divergence (approved 2026-07-29), in both directions.** gnome-shell fits, at both sites: the
fit-all row narrows every box to `availableWidth / n` (`_getFirstFitAllWorkspaceBox`,
`workspacesView.js:127-169`) and `ThumbnailsBox.vfunc_get_preferred_height` shrinks the
thumbnails below `MAX_THUMBNAIL_SCALE` until the row fits. Fitting past a certain count means
specks; we scroll instead. One rule, one helper — `layout::monitor::scroll_to_follow`: a row
that fits is centered (which reduces to gnome-shell's centering exactly, so nothing moves below
the overflow point), and one that does not scrolls to center the active workspace, clamped so
the run never leaves a gap at either end.

- **The app-grid row** (`fit_all_row`, `src/layout/monitor.rs`). The overflow itself was already
  a recorded divergence — we keep one aspect-locked zoom per monitor, so the run overflows where
  gnome-shell would have narrowed it. What made the tail unreachable was pinning that run at the
  left gap. Now it follows the selection. Nothing clips it: the row sits below the floating entry
  and runs off the screen edges, where there is nothing to overlap.
- **The thumbnail strip** (`thumbnails::strip_geometry`). The scale is now the cap, full stop, so
  `preferred_height` is a constant and the band no longer changes size with the workspace count.
  The row scrolls inside the band, on the *fractional* active index, so it tracks a workspace
  switch as it animates and stays locked to the indicator ring.

**The strip is clipped to its band**, which is what keeps a scrolled row off the floating search
entry. That needed the crop to reach three more layers — the wallpaper, the solid-color backing
and the two rings — so `MonitorInnerRenderElement` gained `CroppedSolidColor` and
`CroppedRoundedTexture`, and the rings now go through the existing `InsertHint` (cropped) variant.
The crop for a thumbnail's *contents* is expressed in workspace coordinates, because it is applied
before the rescale and relocate that place the miniature; the rings are already in view
coordinates and clip against the band directly. The band slides with the row on the way in, or the
clip would eat the whole strip during the open transition. Hit-testing follows the clip:
`Strip::thumb_under` and `drop_target` require the band, so only what is drawn can be aimed at.

Live-validated on the headless seat at 13 workspaces: the strip scrolls with a partial thumbnail
peeking at each band edge, the pill is untouched, and the app grid's row keeps the active
workspace on screen.

### 12.3 Panel / quick-settings buttons that launch apps must leave the overview ✅

Gustavo, from live use: a button on the panel or in quick settings that starts an application
should exit the overview, the way launching from the dash or a search result does. Today it
launches behind an overview that stays up.

There is one choke point: every such button resolves to `PopoverAction::Spawn`, and the handler
(`src/input/mod.rs:1132`) is `PopoverAction::Spawn(command) => spawn(command, None)` — no
`close_overview()`, unlike the dash/search/grid launch paths which all call it
(`:5266,5280,5297,5345`). The launchers that reach it today are the dateMenu's Events and World
Clocks cards (`src/ui/calendar.rs`, currently hardcoded `gtk-launch` — see the app-system
revisit note) and the quick-settings rows that spawn a session command
(`src/ui/quick_settings.rs`).

**✅ DONE.** The premise that GNOME gets this for free was wrong, and that mattered for the
neighbours: `app.activate()` does *not* hide the overview. gnome-shell writes
`Main.overview.hide(); Main.panel.close…(); app.activate()` by hand into every handler that
starts an app — the quick-settings system rows (`js/ui/status/system.js:53-57,150-154`),
`addSettingsAction` (`js/ui/popupMenu.js:709-720`), the dateMenu's three cards
(`js/ui/dateMenu.js:300-302,376-381,597-600`), `AppMenu` (`js/ui/appMenu.js:60,69,94,240`, which
we already matched). So the rule is per-handler, and it is exactly "did the shell itself start
something".

Landed as `close_overview()` on `PopoverAction::Spawn` — the single choke point every panel/QS
launcher resolves to — plus the notification paths, where the same rule cuts a *finer* line than
"a notification was clicked":

- `open_notification_app` and the Gtk `activateAction` do activate the app, so both hide
  (`js/ui/notificationDaemon.js:375-381,512-519`). Both now return whether they really dispatched,
  and the caller closes on that. GNOME's hide sits *after* `openApp`'s `app == null` early return,
  so a source we can't resolve leaves the overview up — ours falls out the same way.
- The **fdo** action path only emits `ActionInvoked` and leaves raising to the app, so it does
  **not** hide.

Two neighbours audited and deliberately left alone: `PopoverAction::Screenshot` (GNOME's
screenshot button closes the QS menu and calls `Main.screenshotUI.open()`, which contains no
`Main.overview.hide()` — `js/ui/status/system.js:120-127`), and `AppToggleFavorite` (pinning
raises nothing). Pinned by `overview_closes_when_a_panel_button_launches_an_app`, which asserts
all three answers — close, stay, stay — since two of them look like they should close.

Still open next door: the dateMenu cards spawn `gtk-launch <app>` instead of resolving the
default handler (the app-system revisit note). They leave the overview correctly now either way.

### 12.4 The thumbnail strip is the app-grid row's twin ✅

Gustavo, having seen the two rows side by side: *"would it be possible to replace the thumb strip
with the same widget used for the workspaces while in app grid mode? I like how they behave a lot
and their spacing and size … I do like the idea of the cursor you added to the thumb strip, but I
am thinking perhaps we could make it be the drop shadow being stronger and accent-colored?"*

**Not the same widget instance — that one is impossible.** In the window-picker state the
workspace row *is* the big picker; the strip is a second, simultaneous drawing of the same
workspaces at a different size. Two live views of one row, on screen at once, cannot be one
actor. What is shareable is everything that made the app-grid row look right, and that is what
landed:

- **Size.** A thumbnail is `overview_layout::small_workspace_height` — one expression, used by the
  app-grid picker box *and* the strip, so the two can't drift. 157px at the 1920×1080/35 reference,
  against the 108 of yesterday's doubled cap. Pinned by asserting the strip's thumbnail against the
  *rendered* app-grid workspace, so a change on either side has to move both.
- **Spacing.** `WORKSPACE_MIN_SPACING` (24), ramped, instead of the theme's `$base_padding` (8).
- **Corner + shadow.** `Monitor::thumbnail_corner_radius` is the picker's own background curve
  evaluated at the thumbnail's height, and the wallpaper, the shadow and the clip all read it from
  that one accessor — the same reason `workspace_background_radius` is one accessor.
- **The inactive shrink.** `WORKSPACE_INACTIVE_SCALE` about the slot's center, exactly as
  `_updateWorkspacesState` does it for the row (`workspacesView.js:243-266`).

**The indicator ring is gone**, replaced by the accent glow: `thumb_active_shadow`, the same
shadow geometry spread wider, at full alpha, in the system accent color, which
`set_gnome_accent_color` now recolors instead of the ring. gnome-shell's
`.workspace-thumbnail-indicator` border is dropped.

**The row centers the active workspace rather than clamping at its ends** (Gustavo, once the
strip was workspace-sized): with the active thumbnail pinned to the band's center, the
overflowing side almost always ends on a workspace *poking in* at the band edge, which is the
"there is more this way" affordance for free — and it is the picker's own fit-single behaviour
(`center_on_focus`, beside the clamped `scroll_to_follow` the app-grid row keeps; a full-width
row with no clamp would leave most of a screen empty beside the last workspace). A row that
still *fits* is centered as a whole and stays put, so a two- or three-workspace strip does not
slide about on every switch. **The tradeoff to keep in mind:** at the first or last workspace
roughly half the band is empty. That is what "nothing further this way" looks like, and it is
the price of never ending on a whole thumbnail.

**The shadow goes through the miniature's own transform** (fixed 2026-07-29, reported live: "the
drop shadow seems to animate slightly out of step with the workspace … can clearly see the accent
glow starting far out and then animating to be hugging the thumb"). Two defects, both from
treating the shadow as a thing sized *near* its caster rather than *by* it:

- It was baked at two sizes, full and pre-shrunk by `WORKSPACE_INACTIVE_SCALE`, and each
  thumbnail picked one. But the shrink is *ramped in with the overview progress* and is
  fractional for both workspaces in a switch, so the shadow was a fixed few percent off its
  caster for the whole of every animation — and after a scale change the stale bake was a
  different size entirely, which is the glow visibly closing in. Now one bake at the slot size,
  put through the same rescale+relocate that draws the miniature, so it cannot drift by
  construction. (Its crop moves into the same pre-transform space as the contents' crop.)
- Its geometry was a fixed logical constant: a 14px blur under a 157px thumbnail at scale 1 and
  the same 14px under a 95px one at scale 2 — half again as deep for its caster. Now derived
  from the thumbnail's own height (**adaptive chrome, rule 2**), rebuilt each frame so a scale
  change reaches the config and not just the bake, which is also what lets the accent color
  change under us.

**The active workspace is centered even when the whole row fits** (fixed 2026-07-29). Reported
twice as "the centering does not happen"; the first diagnosis — that a scale change was the
trigger — was **wrong**, and the second report narrowed it correctly: adding a workspace to a
short row. The cause was an exception this port invented, not gnome-shell's: a row that *fit* its
band was centered as a whole and stayed put, on the reasoning that a two-workspace strip sliding
on every switch would be worse. That leaves the active workspace off center by up to half the
row, and on a big canvas it is glaring — four workspaces on Gustavo's 3072×1728 give a run of
1880 in a band of 2320, so the row fits and the active one sits **714px left of the middle**
(computed `x0 = 596`, which matched the screenshot's leftmost thumbnail exactly).

`center_on_focus` now pins the focus to the band's center unconditionally, which is what was
asked for in the first place ("the selected workspace stick to the center of the strip"). Two
consequences to know about, both inherent:
- A short strip **slides on every switch**, and clicking a non-active thumbnail slides it out from
  under the pointer — so a second click on the same spot hits a different workspace. The corpus
  test re-aims between the two clicks, and that is a real interaction note, not just a test detail.
- Workspaces that *would* have fit can now scroll past the band edge, because the row is anchored
  to the active one rather than packed. With four workspaces on that canvas, the fourth is fully
  outside the band while the active one is first.

`scroll_to_follow` — the app-grid row — keeps gnome-shell's centering for a run that fits; only
the strip diverges.

**Trap worth keeping.** The band clip from §12.2 is a *horizontal* concern — it exists to keep the
row out of the floating entry's column. Clipping the shadow to the band as well cut the glow flat
along the thumbnail's top and bottom edges, so the active workspace read as two accent side
stripes. Shadows keep the band's x range and get the band's height plus `SHADOW_GLOW_MARGIN`,
still sliding with the band so the entrance clips correctly. It looked *fine* in the geometry
tests and wrong on the first screenshot.

### 12.5 An edge fade at the ends of the strip (reported 2026-07-29, NOT started)

The overflowing edge of the strip currently ends on a hard cut. gnome-shell has a canonical
effect for exactly this and we should use it rather than invent one: **`StScrollViewFade`**
(`src/st/st-scroll-view-fade.c`), applied through the `hfade` / `vfade` style classes. Two
details worth copying — it is a plain **linear alpha ramp**, `ratio *= distance_from_edge /
fade_offset`, and it fades **only the edge that has more content past it**
(`fade_edges_left`/`_right` come from the adjustment's position). The offset is `68px`
(`_scrollbars.scss:4-5`, and `DEFAULT_FADE_OFFSET` in the C). `hfade` on a horizontal strip has
precedent in the switcher popup (`switcherPopup.js:414`).

**How to build it — decided (Gustavo, 2026-07-29): extend the renderer, do NOT bake offscreen.**
The obvious cheap route is to render the strip to an offscreen and draw *that* through the fade
pipeline we already have (`GradientFadeTextureRenderElement`, which the MRU switcher uses on
clipped thumbnails, `render_gradient_fade` in `vulkan/frame.rs`). It was rejected: "offscreen
baking has bitten us many times in terms of performance and out of date content." The remaining
route is a general **edge-fade verb** every strip pipeline honours, alongside the crop — the
existing fade pipeline is texture-only, and the strip draws windows, wallpaper and solid colors
through different ones.

**Do NOT paint a gradient "fog" rect in the backdrop color.** It looks identical while the
overview is settled and wrong during the open animation, when the backdrop is still blending
with the desktop.

Lower priority than it was: the centering in §12.4 means the overflowing side already ends on a
partial thumbnail, so the fade is polish on an affordance that works, not the affordance itself.

### 12.6 Workspace previews want wallpaper padding on every side (reported 2026-07-29, NOT started)

Gustavo, from live use: a preview shows wallpaper along its top, where the panel's strut leaves
room, but a workspace whose window is maximized runs its window right to the other three edges.
He wants that padding on **all** sides, so every preview has some wallpaper poking out around
its contents — it is what separates a workspace from the dark overview backdrop.

Note this is *not* the same as §12 divergence 4 (`Workspace::expose_area`), which symmetrized the
**slot** the preview is laid out in. This is about padding *inside* the preview, between its
background and the windows drawn on it, and it will interact with that symmetrization — check
both together, and check it against a maximized window, which is the case that shows nothing
today.

### 12.7 The strip and the app-grid row became one row (landed 2026-08-03)

Gustavo, from live use of the thumb strip: he wants the strip to *be* the app-grid row — same
content (exposé previews, not raw window positions), same full-width placement, same drop shadow,
with the accent glow kept on the selected one; and the strip's reorder / close-an-empty
affordances available on the app-grid row too. "The UX I am looking for is the user can't tell the
difference between the thumb strip and the workspaces in the app grid, without affecting how the
overview workspaces work."

Landed as one row rather than two matched ones — the full design and its consequences are in
`docs/fork/dynamic-workspaces-divergence.md` §2b. In short: `ControlsLayout::workspace_row` is
state-independent, `thumbnails::strip_geometry` is the fit-all row, `render_thumbnails` draws
`render_expose`, and the picker **fades away** on the show-apps leg instead of travelling into the
row. Follow-up decision taken in the same session: the row's top sits on the search puck's
*midline*, so the floating entry overlaps its top-right corner rather than the row tucking under
the entry's reserved height.

Two items above are affected: §12.5's edge fade now applies to a full-width row (and §12.4's
"centering leaves a partial thumbnail at the edge" no longer holds — fit-all centers the run, so a
run that fits ends on whole thumbnails at both ends, and only an overflowing one is cut).
