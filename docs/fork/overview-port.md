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
    (continuous scroll over the grid is consumed but inert), no page-slide animation (snap), no side
    **nav-arrows**, no keyboard paging (`Page_Up/Down`) or in-grid arrow/Enter keynav, no
    `indicatorsPadding` (differs only at narrow widths), no folders/`app-picker-layout`/DnD reorder;
    sort is `to_lowercase` not locale collation. The thumbnails' transient scale/translation shrink
    and the faded-strip non-reactivity (S8a) also remain. Floor/ceil vs transition-bracket
    interpolation noted in `overview_layout.rs` (`b6b3d86b`).
- **S8c — App-grid performance + nav polish.**
  - **Async icon decode ✅ DONE (`e7a1c2ed`).** The ~24 first-frame icon rasterizations that froze the
    open animation now run on a worker thread (like the wallpaper decoder): miss → enqueue + draw
    nothing that frame; result lands via a calloop source → redraw, so icons pop in during the slide.
    Generation counter + in-flight dedup + negative cache + `catch_unwind`; headless keeps the sync path.
  - **Next (deferred, pending live measurement):** (1) **batch the GPU uploads** if the residual
    submit+fence-wait-per-texture still hitches on Venus (or cap N-per-frame); (2) **prewarm** dash
    favorites at startup + adjacent grid pages on page-land (kills first-open pop-in + makes paging
    pop-free); (3) the **left/right page-navigation arrows** (`.page-navigation-arrow`, flanking the
    grid, wired to `set_page`) — the one visible nav gap Gustavo noted; (4) touchpad **swipe**,
    page-slide animation, keyboard paging (from S8b's deferred list).
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
