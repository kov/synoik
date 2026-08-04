# Application lifecycle port (cited plan, 2026-07-29)

What `ShellApp` is beyond a catalog entry: a **state machine** (`STOPPED → STARTING → RUNNING`),
a **window list**, and the verbs that act on a *running* app (activate a particular window, quit).
`src/app_system.rs` today is the catalog half only — `RunningApp` carries a window *count*, and an
app has no state at all. Every gap below traces back to that.

Reference: `~/Projects/gnome-shell` and `~/Projects/mutter`, both **50.3** (the checkouts were moved
up from 50.1 during this port; every citation below was re-read there).

## 1. What the gap actually blocks

| Surface | Blocked on |
|---|---|
| App menu **Open Windows** section (`appMenu.js:262-291`) | per-window identity + title for a running app |
| App menu **Quit** (`appMenu.js:99-100,136-138`) | the same, plus `state == RUNNING` |
| App menu **App Details** (`appMenu.js:84-95`) | an `org.gtk.Actions` call into `org.gnome.Software` |
| Overview preview **icon + caption** (`windowPreview.js:133-191`) | window→app resolution *per preview* (the model has it; the picker doesn't ask) |
| Dash/grid **running dot** for `STARTING` (`appDisplay.js:3007-3012`) | the state machine |
| Launching onto a workspace | currently a bespoke `Synoik::pending_launches` hack, not a startup sequence |
| `Ctrl`/middle-click = new window (`appDisplay.js:3060-3075`) | `state == RUNNING` + `can_open_new_window` |

## 2. The reference model

### 2.1 States — `shell-app.c`

```
SHELL_APP_STATE_STOPPED   no windows, no startup sequence
SHELL_APP_STATE_STARTING  a startup sequence is open for it
SHELL_APP_STATE_RUNNING   >= 1 "interesting" (non-skip-taskbar) window
```

- `shell_app_state_transition` (`shell-app.c:911-924`) forbids exactly one edge: `RUNNING → STARTING`.
  It notifies `AppSystem::app-state-changed`, which the dash (`dash.js:383`), the app menu
  (`appMenu.js:104`) and search (`search.js:978`) all listen to.
- `shell_app_sync_running_state` (`:943-955`) is the window-driven half: **while STARTING it does
  nothing** — a starting app does not fall back to STOPPED just because its window list is empty.
  Otherwise `interesting_windows == 0 ? STOPPED : RUNNING`.
- `_shell_app_handle_startup_sequence` (`:1169-1196`) is the sequence-driven half: sequence opens and
  the app is STOPPED → STARTING (and mutter *unsets input focus*, so the launching app's first window
  isn't stolen from); sequence completes → RUNNING if it has windows, else STOPPED ("application has
  > 1 .desktop file").

### 2.2 Startup notification is compositor-side, not client-side

This is the fact that makes the port cheap. On Wayland mutter does **not** wait for a client to say
anything: `meta_launch_context_get_startup_notify_id` (`meta-launch-context.c:129-186`) mints a
**random UUID** itself, inserts its own `MetaStartupSequence` (carrying application-id, name,
workspace, timestamp) and hands the id to GIO, which exports it to the child as
`XDG_ACTIVATION_TOKEN` / `DESKTOP_STARTUP_ID`.

A sequence ends on any of:
- the client uses the token (`meta-wayland-activation.c:347-374`);
- a window maps carrying that startup id, or — with no id — one whose `WM_CLASS` matches an open
  sequence (`meta_display_apply_startup_properties`, `display.c:2661-2712`); the same path applies the
  sequence's **workspace** to the window (`display.c:2720-2731`);
- `launch_failed`;
- **15 s timeout** (`STARTUP_TIMEOUT_MS`, `startup-notification.c:38`, swept in
  `startup_sequence_timeout`).

Our `Synoik::pending_launches` (`synoik.rs:301-311, 9088-9133`) is already a degenerate version of this —
app-id keyed, 15 s expiry, claimed by the first matching window — with a comment saying so. The port
is to **promote it to a real sequence table** keyed by token, and let the app state read off it.

### 2.3 Quit — `shell_app_request_quit` (`shell-app.c:1210-1243`)

1. Not RUNNING → return false (menu item hidden anyway).
2. If the app exports a parameterless `app.quit` action on the bus, activate it.
3. Otherwise **close every window that `can_close`**.

Step 2 needs the `org.gtk.Application` action muxer (`ShellApp`'s `unique_bus_name` +
`GtkActionMuxer`, `shell-app.c:45-52`), which needs `gtk_shell1`'s
`set_startup_id`/`meta_window_get_gtk_application_object_path` plumbing. We have none of it. Step 3
alone is a faithful *fallback* — GNOME takes it for every app that doesn't export `app.quit`, which
includes every non-GTK app.

### 2.4 The window section (`appMenu.js:262-291`)

Header "Open Windows" + one row per window, shown when the app has ≥ `minWindows` windows
(`showSingleWindows: true` for app-grid/dash icons → 1). Row label is `window.title`, falling back to
the app name. Activating a row is `Main.activateWindow(window)`. Windows are filtered by
`!skip_taskbar`.

### 2.5 Preview icon + caption (`windowPreview.js:24-27, 133-191, 238-266`)

- `ICON_SIZE = 64`, `ICON_OVERLAP = 0.7`, `ICON_TITLE_SPACING = 6`.
- The icon is centred on X over the window container and anchored to its **bottom edge** with pivot
  `y = ICON_OVERLAP`: 70 % of the icon sits inside the preview, 30 % hangs below
  (`chromeHeights` bottom oversize is `(1 - ICON_OVERLAP) * iconHeight` = 19.2 px).
- Style: `.window-icon` is `.icon-dropshadow` only — `icon-shadow: 0 2px 4px rgba(black, 0.4)`
  (`_base.scss:16-18`); no background outside high-contrast (`_window-picker.scss:11-21`).
- The caption is an `St.Label.window-caption`, X-centred, top-aligned to the container's bottom edge
  plus `ICON_SIZE * (1 - ICON_OVERLAP) + ICON_TITLE_SPACING` = **25.2 px**, single-line, ellipsized.
  `%tooltip` (`_common.scss:225-238`): `background-color: rgba(0,0,0,.9)`, `1px solid rgba($light_1,.1)`,
  `color: $light_1`, `border-radius: $forced_circular_radius` (a pill), `padding: $base_padding
  $base_padding*2` = 6px/12px, centred text.
- Caption text is `metaWindow.title`, falling back to the **app name** (`_getCaption`, `:259-266`).
- **Visibility differs between the two.** The icon is always visible in the window picker, and its
  *scale* ramps with the overview adjustment: `1 - |WINDOW_PICKER - currentState|`, and 0 unless the
  transition touches WINDOW_PICKER (`_updateIconScale`, `:238-252`). The caption is part of the
  **overlay** — hidden until hover, faded over `WINDOW_OVERLAY_FADE_TIME` alongside the close button
  (`showOverlay`, `:310-352`), which is the alpha our `PreviewOverlay` already carries.

## 3. Design for the fork

### 3.1 Model — `src/app_system.rs`

`RunningWindow` gains identity and a title; `RunningApp` gains the window list and the state:

```rust
pub struct RunningWindow {
    pub id: MappedId,              // NEW — a stable handle the menu can act on
    pub app_id: Option<String>,
    pub title: Option<String>,     // NEW — the "Open Windows" row label
    pub last_focus: Option<Duration>,
}

pub enum AppState { Stopped, Starting, Running }

pub struct RunningApp {
    pub id: String,
    pub windows: Vec<RunningWindow>,   // replaces n_windows; MRU order
    pub last_focus: Option<Duration>,
}
```

`n_windows()` stays as a method so existing callers don't churn. Windows within an app sort by
`last_focus` descending — `shell_app_compare_windows` (`shell-app.c:692`) reduced the same way
`recompute_running` already reduces `shell_app_compare`.

State comes from one function over two inputs, mirroring §2.1 exactly:

```rust
pub fn app_state(&self, id: &str) -> AppState {
    if self.starting.contains_key(id) { AppState::Starting }   // sequence open
    else if self.is_running(id)       { AppState::Running }
    else                              { AppState::Stopped }
}
```

`starting` is the sequence table: `HashMap<String /*desktop id*/, StartupSequence { token, workspace,
expires }>`. Note the ordering — the sequence wins while it is open, which is precisely
`shell_app_sync_running_state`'s "while STARTING, do nothing".

**Deliberate simplification.** GNOME stores the state on the app object and transitions it
imperatively; we recompute it from the two inputs on read. Same answer, no edge to miss — the same
choice `sync_running_apps` already made against `ShellWindowTracker`. The one thing that buys GNOME
something we lose is the `RUNNING → STARTING` assertion; a re-launch of a running app just stays
RUNNING here, which is what the assertion enforces anyway.

**`skip_taskbar` is not modelled.** xdg-shell has no such hint; the closest is a window with no
`app_id`, which `recompute_running` already drops. `interesting_windows` is therefore the whole list.

### 3.2 Startup sequences — promote `pending_launches`

Move the table from `Synoik` into `AppSystem` (it is app-model state, and the state machine needs it),
keyed by desktop id as today, and additionally carry the **activation token**:

- `AppSystem::begin_startup(id, token, workspace) -> ()` — called by `launch()`.
- `AppSystem::complete_startup_for_window(&RunningWindow) -> Option<WorkspaceId>` — the sequence's
  workspace, matched by token first (`display.c:2661`) then by resolved app id (the wmclass
  fallback, `find_startup_sequence_by_wmclass`).
- `AppSystem::expire_startups(now)` — the 15 s sweep.

`GioLauncher` mints the token: `Synoik::activation_state.create_external_token(None)` (the call
`synoik.rs:9460` already makes for notifications), then `context.setenv("XDG_ACTIVATION_TOKEN", tok)`
and `setenv("DESKTOP_STARTUP_ID", tok)` — GIO's own `get_startup_notify_id` is a no-op on a plain
`GAppLaunchContext`, so we set the env directly, which is what GIO would have done with the id
mutter returns. This is a **new capability**, not just a refactor: today our launched apps get no
token at all, so an app that tries to activate itself is treated as an unsolicited focus request.

The token has to be minted where `activation_state` lives, so `launch()` grows a token-minting
closure argument (or the launcher trait grows a `token: Option<String>` parameter). Prefer the
latter — it keeps the seam plain-data and the recording launcher can assert on it.

### 3.3 Verbs

- **Quit** — `PopoverAction::AppQuit(id)`; the input layer resolves the app's windows through
  `AppSystem::running()` and sends `send_close()` to each, reusing `Action::CloseWindowById`'s body.
  §2.3 step 2 is explicitly out of scope (no action muxer); recorded as a divergence.
- **Activate window** — `PopoverAction::ActivateWindow(MappedId)`; the same thing the alt-tab /
  IPC `focus-window` path does.
- **App Details** — a one-shot zbus call to `org.gnome.Software` `/org/gnome/Software`
  `org.gtk.Actions.Activate("details", [(app_id, "")], {})`, exactly `appMenu.js:86-93`. Visible only
  when `org.gnome.Software.desktop` resolves in the catalog (`_updateDetailsVisibility`, `:182-185`).

### 3.4 Preview icon + caption

`window_preview.rs` gains the icon and caption; the picker must hand it, per preview, the resolved
app icon and the caption string. `PreviewOverlay` grows `icon: Option<AppIconRef>` (resolved through
the existing `AppIconCache`) and `caption: String`; the icon is drawn at `alpha = 1` scaled by the
overview ramp, the caption at the overlay `alpha` that already drives the close button.

The caption pill is a **reusable widget**, not a one-off: `%tooltip` is shared by `.window-caption`,
the dash label and the screenshot UI (`_dash.scss:104`, `_screenshot.scss:200`), so it lands as
`widget::Tooltip` per the toolkit-first tenet.

## 4. Slices

- **L1 — model. ✅ DONE** (`10731236`). `RunningWindow` id + title, `RunningApp.windows`, `AppState`
  derived from the sequence table + the window snapshot, `can_open_new_window`, `shows_running_dot`.
  `sync_running_apps` feeds ids and titles.
- **L2 — startup notification. ✅ DONE** (same commit). Token minted per launch in
  `State::launch_app` and exported by `GioLauncher` as `XDG_ACTIVATION_TOKEN`/`DESKTOP_STARTUP_ID`;
  sequence completed by the mapping window (token first, then app id); `Synoik::pending_launches`
  deleted in favour of the table; expiry swept from `sync_running_apps`.
- **L3 — preview icon + caption. ✅ DONE** (`112fff5c`). `widget::Tooltip`, `Monitor::preview_rects`
  (every drawn preview, not just the hovered ones) + `preview_icon_scale`, the icon uploaded once and
  rescaled on the GPU.
- **L4 — menu verbs. ✅ DONE.** Open Windows section (a labelled separator + one row per window),
  Quit, App Details; `AppMenuContext` carries the app snapshot into the menu.

Each slice: headless corpus tests in `src/tests/gnome.rs` (model, verbs, sequence lifetime). The icon
*scale* ramp and the caption *fade* are animations, so they are live-only
([[headless-animation-clock-trap]]); their endpoints are pinned, and all four slices were eyeballed
on the headless self-iteration harness.

- **L5 — the running dot follows the state. ✅ DONE.** The dash's dot reads
  `shows_running_dot` (state ≠ STOPPED), so a launching favorite lights up before its window
  exists. That needed a state-change signal of its own: a launch touches no window, so
  `sync_running_apps` now also reports `AppSystem::take_state_changed()` — our
  `app-state-changed` (`shell-app.c:921`, consumed at `dash.js:383`).

- **L6 — launch-feedback cursor. ✅ DONE.** See §2.6. Live-checked on the harness: the Adwaita
  `wait` ring shows from the click until the app maps, and the 15 s timeout releases it for a launch
  that never produces a window.

### Still open
- The `RUNNING`+`Ctrl`/middle-click = new window refinement of `AppIcon.activate`
  (`appDisplay.js:3060-3075`) — the dash still always `Activate`s.
- `animateLaunch` (`appDisplay.js:3080`), the icon's zoom-out on launch.

### 2.6 Launch feedback is a compositor-wide cursor override

While any startup sequence is pending, mutter forces `CLUTTER_CURSOR_WAIT` over *everything* —
not on a particular actor, and nothing to do with libstartup-notification or the client:

- `meta_startup_notification_has_pending_sequences` (`startup-notification.c:120-132`) — true when
  any sequence has an `application_id` and is not completed.
- `meta_compositor_update_global_cursor` (`compositor.c:1103-1117`) reads that predicate and stores
  `MetaCompositor:global_cursor`, recomputed from the `MetaStartupNotification::changed` signal.
- `on_override_cursor` (`:1125-1132`) hands it to the backend's `override-cursor` hook, so it wins
  over whatever the pointer is actually over.

`CLUTTER_CURSOR_WAIT` is the xcursor `"wait"` (legacy `"watch"`), *not* `PROGRESS`/`left_ptr_watch`
(`meta-cursor-xcursor.c:114-115,198-201`).

**Ported (L6).** `CursorManager::set_global_override`, recomputed in `sync_running_apps` from
`starting_apps()` rather than at the launch/complete sites, so no path can leave it stuck on.
`application_id` is vacuously true for us — every sequence is keyed by a desktop id. One divergence:
a client that *hid* the pointer keeps it hidden; we do not put a cursor back under it because a
launch is happening elsewhere.

## 5. Accepted divergences

- **No `org.gtk.Application` action muxer.** Quit closes windows; `open_new_window` never uses
  `app.new-window`; busy state (`shell_app_get_busy`) is unmodelled. Needs `gtk_shell1` plumbing.
- **No `skip_taskbar`.** Every resolved window is "interesting" (§3.1).
- **No discrete-GPU item.** `switcheroo-control` is absent on this hardware (already recorded in
  `app_menu.rs`).
- **State is derived, not stored** (§3.1) — the `RUNNING → STARTING` assertion has no analogue.
- **Startup notification does not unset input focus.** `_shell_app_handle_startup_sequence`
  (`shell-app.c:1186`) calls `meta_display_unset_input_focus` so the launching app's window isn't
  raced; our focus policy is niri-derived and this would need its own audit. Deferred, flagged.
