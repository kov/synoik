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
    `indicatorsPadding` (differs only at narrow widths), no folders/`app-picker-layout`/DnD reorder;
    sort is `to_lowercase` not locale collation. The thumbnails' transient scale/translation shrink
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
    `indicatorsPadding`, they ride the centering gutter, not a fixed 10% band. Render pins the chevron +
    hover wash.
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
      surface uploads once. Lower payoff (favorites are excluded from the grid, so cross-surface overlap
      is mostly search∩grid), but clean; do after (1)+(2).
  - **Next (deferred, pending live measurement):** touchpad **swipe**, page-slide animation, keyboard
    paging (from S8b's deferred list); `indicatorsPadding` reserve so the arrows sit in a fixed side band
    rather than the gutter; split theme-resolution (main) from decode (worker) per GNOME
    `st-texture-cache.c:976` (marginal — resolution is cheap/cached).
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
- **S10 — Keyboard-navigation pass (deferred, 2026-07-24).** Reported live: in the search results
  **Tab steps forward but Shift+Tab also steps forward** instead of backward. Cause is pinned, not a
  logic bug in the search: `overview_search.rs:262` already treats `Keysym::ISO_Left_Tab` as
  select-prev, but `input/mod.rs:734` hands `handle_key` the **raw** keysym, and xkb only yields
  `ISO_Left_Tab` as the *modified* sym — so Shift+Tab arrives as plain `Tab` and falls into the
  select-next arm, leaving that `ISO_Left_Tab` match dead for real input. (`modified` is already in
  scope at that call site, used for `key_char`.) Fix belongs in a full keyboard-nav pass rather than
  a one-key patch: audit every UI surface that keys off `raw` for the same shifted-keysym blind spot,
  and pin Shift+Tab per surface. GNOME reference: `js/ui/search.js` / `st-focus-manager`.
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
