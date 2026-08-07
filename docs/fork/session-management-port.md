# Session management port (`xdg_session_management_v1`)

Proposal for supporting client-driven toplevel session restore: an app asks the compositor
to remember where its windows were, and on the next run asks for that state back.

Our first client is **ghost**, which speaks the published staging protocol.

Reference: mutter 50.3 implements this in
`src/wayland/meta-wayland-xdg-session{,-manager,-state}.c` (1519 lines total) plus
`src/core/meta-session-manager.c` for the on-disk store. Protocol XML:
`/usr/share/wayland-protocols/staging/xdg-session-management/xdg-session-management-v1.xml`.

---

## Decisions (settled 2026-08-07)

### D1. Which dialect — staging `xdg_*` or mutter's `xx_*`? → **staging `xdg_*`**

**Decided: the published staging protocol.** Our first client is ghost, which speaks that
dialect, so there is no reason to carry mutter's pre-merge snapshot. An `xx_` alias global is not
planned; add one only if some third-party client forces it.

The comparison that led there:

Mutter 50.3 ships its **own pre-merge snapshot** of this protocol in
`src/wayland/protocol/session-management-v1.xml`, under the `xx_session_manager_v1` name.
The upstream-merged staging protocol installed on this machine is `xdg_session_management_v1`.
They are the same design; diffing them (modulo the `xx_`→`xdg_` rename) the deltas are:

| staging `xdg_*` | mutter's `xx_*` |
| --- | --- |
| `xdg_session_v1.remove_toplevel(name)` | `xx_toplevel_session_v1.remove` (destructor) |
| `xdg_toplevel_session_v1.rename(name)` | — |
| `restored` event has no args | `restored` carries the `xdg_toplevel` |
| errors `invalid_name`, `already_added` | error `invalid_restore` |
| manager errors `invalid_session_id`, `invalid_reason` | — |
| `get_session` arg `session_id` | arg `session` |

No installed client speaks either dialect today — `strings libgtk-4.so.1` has no hit for either.
This is the one place where "GNOME's way" and "upstream's way" differ only by *snapshot age*, not
by design, which is why we diverge from the reference here rather than mirror it.

### D2. Off by default, like GNOME, or always on? → **always on**

Mutter gates the global behind `MetaDebugControl`'s `session-management-protocol` property, which
is **disabled by default in 50.3** (`meta-wayland-xdg-session-manager.c:337-367`). GNOME does not
ship this protocol live yet.

**Decided: no gate — advertise the global unconditionally.** We simply don't deploy to the daily
seat while this is work in progress, which removes the reason for a switch.

### D3. When does state hit disk? → **mutter's cadence**

**Decided: mirror mutter.** Its cadence:

- In-memory toplevel state is refreshed **only when a window is unmanaged**
  (`on_window_unmanaging` → `save-toplevel`), *not* continuously as the window moves. The XML
  prose says state "is automatically updated by the compositor when changed"; mutter's actual
  behaviour is save-on-close, and that's the behaviour clients are written against.
- That schedules a **3-second debounced async write** of the whole store
  (`TIMEOUT_DELAY_SECONDS`, `meta-wayland-xdg-session-manager.c:33,136-141`).
- A final **synchronous** save happens at context teardown (`meta-context-main.c:445`).

The teardown save is the awkward one for us: `docs/fork/session-end.md` records session-end drain
as parked, with the socket outliving `SIGTERM` as the open deadline problem. **We do the sync save
from the same shutdown path that already runs on `SIGTERM`, and accept that a `SIGKILL` loses up
to 3 seconds of state** — same as mutter.

---

## What we can actually represent

Mutter's per-toplevel record (`meta-wayland-xdg-session-state.c:32-57`):

```
window_state: none | floating | maximized | tiled-left | tiled-right | fullscreen
floating.rect  /  tiled.rect   (MtkRectangle)
is_minimized: bool
workspace_idx: int
```

Against synoik today:

| field | synoik status |
| --- | --- |
| floating rect | ✅ `Tile::floating_pos` + window size |
| maximized | ✅ (`Floating` owns maximize/fullscreen in GNOME mode) |
| fullscreen | ✅ |
| tiled-left / tiled-right | ❌ **edge half-tiling is not ported** — no `MetaTileMode` equivalent; planned, unreachable for now |
| is_minimized | ❌ **no minimize** — `synoik.rs:4712` says so outright; planned, unreachable for now |
| workspace index | ✅ `Monitor::workspaces` is an ordered `Vec` |

Minimize and edge half-tiling are both intended for synoik eventually; they are simply
unreachable today. So: the in-memory enum carries only `Floating { rect }`, `Maximized { rect }`,
`Fullscreen { rect }`, but the *serialized* format keeps mutter's numeric `state` values
(1=floating, 2=maximized, 3=tiled-left, 4=tiled-right, 5=fullscreen) and both rect keys, so that
tiled-left/right and `is-minimized` slot in unchanged when they land, and a file written by a
future synoik still parses today. Unknown state values parse as "no restore".

Workspaces persist **by index**, like mutter — `WorkspaceId(u64)` is runtime-only and meaningless
across restarts. With dynamic workspaces the index may no longer exist at restore time; clamp to
the last real workspace rather than creating up to the saved index.

> **Do not port this bug.** `meta-wayland-xdg-session-state.c:304` reads
> `is_minimized` but tests `state` (a `"u"`) against `G_VARIANT_TYPE ("b")`, so the condition is
> always false and mutter never restores minimized-ness from disk. Our version should test the
> right variant. Flagging it here so nobody later "fixes" us back to reference parity.

---

## Architecture

The one structural mismatch with mutter: at `restore_toplevel` time mutter already has a
`MetaWindow` and can call `meta_window_move_resize` / `meta_window_maximize` directly. Synoik has
only an `Unmapped` — the real window doesn't exist until it maps. So restore has to be **staged
across the initial-configure path** rather than applied inline.

### New modules

- `resources/xdg-session-management-v1.xml` — copied from wayland-protocols, scanned via the
  existing `src/protocols/raw.rs` pattern (`mutter_x11_interop` is the template).
- `src/protocols/session_management.rs` — the three interfaces, `GlobalDispatch`/`Dispatch`
  impls, and a `SessionManagerHandler` trait. Modelled on `src/protocols/ext_workspace.rs`, which
  is the closest existing hand-rolled protocol with per-client object graphs.
- `src/session_state.rs` — the persistent store: load, lookup, mutate, debounced save. No
  Wayland types; plain data, serde, testable standalone.

### Storage

gvdb is a glib implementation detail, not observable behaviour. Proposal: serde (RON or JSON —
JSON unless there's a preference) at `$XDG_DATA_HOME/synoik/session.json`, mirroring mutter's
structure so the concepts line up:

```
version: 1
sessions:
  "<uuid>":
    last-used: <unix micros>
    toplevels:
      "<client-chosen name>":
        state: 1
        floating-rect: [x, y, w, h]
        workspace: 0
```

A `version` greater than ours makes the load fail closed (mutter does the same,
`meta-wayland-xdg-session-state.c:257-266`). Deleted sessions are tombstoned in memory until the
next write so a concurrent `remove` isn't resurrected by a pending save.

### Restore flow

1. **`get_session(reason, session_id)`** — unknown id is treated as NULL (spec + mutter). New id:
   generate a UUID, send `created`. Known id: send `restored`. Same client already holds it →
   `in_use` error. *Different* client holds it → send `replaced` to the old one, make it inert,
   hand the state to the new one. Invalid `reason` → `invalid_reason`.
2. **`restore_toplevel(toplevel, name)`** — if the surface already had its initial commit, raise
   `already_mapped` (mutter's `meta_wayland_surface_has_initial_commit` check maps onto our
   `unmapped_windows` / `InitialConfigureState` state). Otherwise stash
   `restore: Option<(SessionId, String)>` on the `Unmapped`.
3. **`send_initial_configure`** (`src/handlers/xdg_shell.rs:1047`) — if the `Unmapped` carries a
   restore request *and* the store has a record for that name:
   - pick the output from the saved rect via `global_space` (mutter:
     `meta_monitor_manager_get_logical_monitor_from_rect`), falling back to the normal monitor
     choice when the rect lands on no connected output;
   - set pending `Fullscreen`/`Maximized` toplevel states from the saved record;
   - configure the saved size instead of the resolved default width/height;
   - store the restore payload in `InitialConfigureState::Configured` for the map step;
   - send `xdg_toplevel_session_v1.restored` **immediately before** `toplevel.send_configure()` —
     the spec pins it to "prior to the first `xdg_toplevel.configure`".

   If the name is *unknown* in the session, the request degrades to `add_toplevel` and **no**
   `restored` event is sent.
4. **On map** — consume the payload: `Layout::add_window` with
   `AddWindowTarget::Workspace(saved_idx)` and the saved `floating_pos`, marking the window as
   already placed so the placement cascade (`docs/fork/` window-placement work) doesn't re-centre it.

### Save flow

`add_toplevel` / a successful `restore_toplevel` registers `(session, name) → window`. When that
window is unmapped or destroyed — the `remove_window` call at
`src/handlers/compositor.rs:448` — snapshot its rect / state / workspace index into the store and
arm the 3s debounce.

`remove_toplevel(name)` and session `remove` delete from the store; session `destroy` keeps the
data but makes the objects inert.

---

## Conformance corpus

Mutter's tests (`src/tests/wayland-xdg-session-management-tests.c`, plus three test clients) name
exactly the behaviours to pin in `src/tests/gnome.rs`:

- `basic` — create a session, get an id back via `created`
- `replace` — a second client taking over an id gets the state; the first gets `replaced` and goes inert
- `restore` — floating geometry round-trips across a "restart"
- `restore-maximized`, `restore-fullscreen`, `restore-tiled`, `restore-tiled-fraction`
- `restore-fullscreen-monitor-removed` — saved output is gone; window must land somewhere sane

Plus, from the staging spec text and not covered by mutter's suite:

- unknown `session_id` behaves as NULL (no `restored`, a fresh `created`)
- `in_use` when the *same* client re-requests a live session
- `restore_toplevel` with an unknown name → behaves as `add_toplevel`, **no** `restored` event
- `restore_toplevel` after the first commit → `already_mapped`
- `name_in_use` on duplicate name; `already_added` on double-add
- `rename` preserves the toplevel's saved state
- workspace index that no longer exists clamps instead of panicking

And pinning our own policy calls (see below), which no reference covers:

- a restored window activates under `reason = launch`, and does *not* under `recover` or
  `session_restore`
- workspace restore happens under **all three** reasons — this is a deliberate divergence from the
  spec's hint, so it needs a test or it will get "fixed"

The store itself (`src/session_state.rs`) gets plain unit tests for parse/serialize round-trip,
too-new `version` rejection, and the tombstone-vs-pending-save interaction.

---

## Slices

0. **Placement seam — DONE (`d2645598`).** `layout::placement` now owns the monitor/workspace
   resolution chain that the xdg-shell handlers had open-coded at five sites (four with a
   `FIXME: deduplicate`). Callers pass `PlacementSeeds`; the order lives in one place. Restore
   becomes a *writer* of seeds in slice 4 rather than a sixth copy of the chain.

   Two per-site differences were load-bearing and survive as seeds: only the initial configure
   consults the pointer (so a window can't hop monitors after being configured), and a
   parent-inherited monitor is not pinned (so dialogs re-fetch at map time). A third is drift —
   maximize/fullscreen don't seed the stored workspace name while unmaximize/unfullscreen do —
   carried over unchanged and documented at the call site; see the open question below.

1. **Protocol skeleton** — XML, scanner wiring, the three interfaces, the global, all state in
   memory only, no restore. Lands `basic`, `replace`, `in_use`, unknown-id, and the error tests.
2. **Persistence** — `src/session_state.rs`, load at startup, debounced + shutdown save.
   Store unit tests.
3. **Save on unmap** — snapshot geometry/state/workspace when a registered window goes away.
4. **Restore** — the `Unmapped` → `send_initial_configure` → map pipeline. Lands the `restore-*`
   tests.

Slices 1–3 are independently useful and independently testable; slice 4 is where the real risk is,
because it touches the initial-configure path that every window goes through.

---

## Policy (settled 2026-08-07)

### Activation — keyed on `reason`

A restored window activates iff `reason == launch`. `recover` and `session_restore` map without
taking focus, so restoring five windows at login doesn't have each one steal focus in turn, while
a launcher-started app still behaves like any other launch. Mutter says nothing here; this is ours.

This is the *only* thing `reason` is used for. We deliberately do **not** honour the spec's hint
that `launch` might restore size-only while `session_restore` also restores workspace: a launched
app ignoring its saved workspace would read as a bug, not a feature. Add a conformance test that
pins workspace restore under all three reasons so this stays deliberate.

---

### The maximize/unmaximize workspace-seed drift — settled, seed it always

Surfaced by slice 0: `maximize_request` and `fullscreen_request` did not seed the stored
`workspace_name` when re-resolving a not-yet-mapped window's monitor, while `unmaximize_request`
and `unfullscreen_request` did. A window with an `open-on-workspace` rule that maximized before
mapping was therefore sized against its monitor's *active* workspace instead of its own.

That asymmetry is niri's, and it only made sense there: niri gives maximized and fullscreen
windows workspaces of their own, so losing the window's workspace on the way in costs nothing.
We do not do that — a maximized window stays on its workspace — so all four requests now seed
`workspace_name`. Restore inherits the fixed behaviour.

One exception, at `fullscreen_request`: when the client names an output, the workspace seed is
dropped, because a named workspace pins the monitor and would otherwise let the workspace we
resolved earlier veto the output the client just asked for. The mapped path honours
`requested_output` unconditionally; the unmapped path now matches it.

Pinned by `tests::window_opening::maximize_after_the_initial_configure_keeps_the_windows_workspace`.

## Open questions

### Eviction — cap at 1000 sessions, most-recently-used

Mutter records `last-used` but no code acts on it, so its store grows forever. We record it too
and, at load time, keep the 1000 most-recently-used sessions and drop the rest. Bounding by count
rather than by age means a rarely-used app doesn't lose its state merely for being rarely used;
entries are tiny, so the cap is generous on purpose. The protocol declares eviction a compositor
implementation detail, so this is free to change later.
