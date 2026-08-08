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
across restarts. With dynamic workspaces the index routinely no longer exists at restore time;
grow the strip until it does, capped (see slice 4).

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
   `unmapped_windows` / `InitialConfigureState` state). Otherwise set
   `Unmapped::wants_session_restore` — a bare bit, deliberately: the id and name are looked up
   again from the registration at configure time, so nothing is carried across the idle that a
   takeover could make stale.
3. **`send_initial_configure`** (`src/handlers/xdg_shell.rs`) — if the `Unmapped` carries a
   restore request *and* the store still has a record for that name, `resolve_session_restore`
   turns it into seeds:
   - the output comes from the saved rect by largest overlap (mutter:
     `meta_monitor_manager_get_logical_monitor_from_rect`), and is simply not seeded when the rect
     lands on no connected output, so the normal monitor choice takes over;
   - `open_fullscreen` / `open_maximized_to_edges` from the saved state;
   - `default_width`/`default_height` and `default_floating_position` from the saved rect, the
     position folded out of global into the workspace's working area;
   - `workspace_idx` into `PlacementSeeds`, clamped to the monitor's last workspace — this seed
     only picks a size to configure against; the real workspace is decided at map time;
   - `open_focused = false` for every reason but `launch` (see Policy below);
   - the payload is stashed in `InitialConfigureState::Configured` for the map step;
   - `xdg_toplevel_session_v1.restored` goes out **immediately before** `toplevel.send_configure()` —
     the spec pins it to "prior to the first `xdg_toplevel.configure`".

   If the name is *unknown* in the session, the request degrades to `add_toplevel` and **no**
   `restored` event is sent.
4. **On map** — consume the payload: pick the workspace by the saved index, growing the strip if
   it is past the end (`Monitor::ensure_workspace_at`, capped), skip
   GNOME auto-maximize, and seed `Tile::tiled_restore_*` with the saved rect so a window that maps
   straight into maximized has somewhere to unmaximize *to*.

### Save flow

`add_toplevel` / a successful `restore_toplevel` registers `(session, name) → window`. When that
window is unmapped or destroyed — the `remove_window` call at
`src/handlers/compositor.rs:448` — snapshot its rect / state / workspace index into the store and
arm the 3s debounce.

`remove_toplevel(name)` and session `remove` delete from the store; session `destroy` keeps the
data but makes the objects inert.

**Unmap is not the only trigger, and assuming it was is a bug we shipped.** The spec's word for
`xdg_session_v1.destroy` is *"preserving the current state"* — frozen as of the request. A client is
entitled to destroy its session while its windows are still up, and the destructor dropped the
registrations, so anything torn down afterwards had nothing left to look itself up under and kept
whatever record its *previous* run wrote. `destroyed` now snapshots every live registration before
dropping them, via the `save_live_session_toplevels` hook, reusing the sweep shutdown already runs
for the same reason (`save_live_session_toplevels_matching`). Two symptoms fell out of the one
cause, and both looked like placement bugs:

- a client that tears down **session-then-toplevels** loses the state of every window that outlives
  the session — for a partial order, exactly one window comes back on the desktop it was on *two*
  runs ago;
- a client that is **killed outright** saves nothing at all, while still leaving a
  restorable-looking session id behind (the id is `touch`ed, the toplevel map stays empty). The next
  run then restores nothing and every window opens wherever new windows go — which reads as "all my
  windows came back on one desktop".

Found on the live seat, not in the corpus: `tools/session-probe --quit-style` drives all three
teardown orders, and only *toplevels-before-session* used to save. The corpus had modelled the
polite order alone.

---

## Conformance corpus

The corpus is headless, and that is the right home for the rules — but it models the client we
wrote, so it inherits our assumptions about how a client behaves. Both bugs found after slice 4
landed were of that shape: a legal teardown order and a shorter workspace strip, neither of which
any test thought to produce. **`tools/session-probe` is the live counterpart** — it speaks the real
protocol against a running compositor, and its `--quit-style` exists precisely because the order a
client tears down in turned out to be load-bearing. Reach for it when a report comes from the seat:
a rebuilt binary does not reach a running session until that session restarts, so "the tests pass"
and "the desktop is fixed" are different claims.

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
- workspace index that no longer exists grows the strip; a nonsense one is capped
- a set of windows restores onto the right desktops in any order the client asks in
- a session destroyed while its windows are up freezes their *current* state
- a toplevel torn down after its session still saves; the loser of a takeover destroying its inert
  object leaves the winner holding the id

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

1. **Protocol skeleton — DONE.** `resources/xdg-session-management-v1.xml` is vendored (a
   byte-for-byte copy of wayland-protocols 0.32.13, which ships the XML but no bindings module
   yet); `protocols::raw::xdg_session_management` runs the scanner over it, server-side always and
   client-side under `cfg(test)` for the fixture. `protocols::session_management` implements the
   three interfaces with all state in memory: sessions live only while a client holds them, and
   `restore_toplevel` always degrades to `add_toplevel` because there is nothing to restore yet.

   **Inertness is not a flag.** A session object is inert exactly when it is no longer the object
   registered for its id, which covers takeover, `destroy` and `remove` at once with no bit that
   can go stale; toplevel handles follow the same rule one level down. `destroy` and `remove` are
   both destructors and both fall to the same `destroyed` hook today — they stay separate arms
   because slice 2 gives `remove` the extra job of deleting the stored state.

   `already_mapped` needs the *initial commit*, not the initial configure, which is sent from an
   idle a beat later; `Unmapped::had_initial_commit` was added for exactly that distinction.

   An unmapped toplevel is re-inserted as a fresh `Unmapped` (`compositor.rs`, the same site
   slice 3 targets), so the flag is carried across explicitly: `already_mapped` is pinned to the
   first commit after the *toplevel* was created, not after it was last unmapped.

   Landed in `src/tests/gnome.rs`: `basic` (created, and an added toplevel is never restored),
   unknown-id-is-new, `replace` (restored to the taker, replaced to the loser), `in_use`,
   `already_mapped` both fresh and after a remap, restore-of-an-unknown-name-adds-without-restored,
   `name_in_use`, `already_added`, rename-frees-the-old-name, rename-onto-a-taken-name, and
   destroy-goes-inert.
2. **Persistence — DONE.** `src/session_state.rs` is the store: plain data and serde, no Wayland
   types, JSON at `$XDG_DATA_HOME/synoik/session.json`, written through a temp file that is
   `fsync`ed and renamed, so neither a crash nor a power loss mid-write can truncate the previous
   store. Loaded in `State::new`, saved by a 3s
   debounce armed through the new `SessionManagerHandler::schedule_session_save` hook (the protocol
   owns *what* changed, the event loop owns the timer), and flushed once more after
   `event_loop.run` returns so a clean exit never loses it. First-change-wins arming, matching
   mutter (`meta-wayland-xdg-session-manager.c:136-141`).

   With the store in place a session id is *known* if a client holds it **or** the store remembers
   it, so `destroy` now keeps the id restorable while only `remove` forgets it — the difference the
   spec draws between the two, which slice 1 could not express.

   **No tombstones.** The proposal called for them, but the save serializes from live state at the
   moment it fires and only then hands bytes to the filesystem, so there is no window in which a
   removed session could be resurrected by an in-flight write. The mechanism would have guarded a
   hazard this shape does not have.

   **The write is off the compositor thread**, as the settled cadence said. Only the serialize is
   inline — which is exactly what makes tombstones unnecessary — and the bytes go to one long-lived
   worker thread, so the channel gives write ordering for free and coalesces a burst down to the
   newest snapshot. It was briefly synchronous; that was wrong, because the `fsync` measures 9–22 ms
   for a full store on a warm NVMe (worse tail on a busy disk) and the compositor has one thread, so
   it would drop frames on the very interactions that schedule a save. Only the shutdown flush
   blocks, and it writes unconditionally so it doubles as the retry for a queued write that failed.

   A headless test instance gets a store with no path — the suite must neither read nor clobber the
   real session file — using the same `BackendMode::HeadlessTest` gate as the GSettings watcher.

   The [`MAX_SESSIONS`] cap is enforced **at load only**. Evicting mid-run could drop a session a
   client is still holding; a run that creates more than 1000 sessions simply keeps them until the
   next start.

   Store unit tests cover the JSON round trip, the too-new version refusal, an unsupported state
   value (half-tiling) that keeps its record but restores nothing, an unknown-to-us state value
   from a future synoik, MRU eviction, dirty-flag bookkeeping, save-then-load, and that a burst of queued saves coalesces
   to the newest. Conformance
   tests cover restore-from-a-remembered-id, destroy-keeps/remove-forgets, an inert session's
   `remove` being a no-op, and the write being scheduled rather than inline.
3. **Save on unmap — DONE.** `State::save_session_toplevel` writes a registered window's sizing
   mode, saved rect and workspace index into the store when it unmaps or is destroyed, mirroring
   mutter's one and only save trigger, `on_window_unmanaging`
   (`meta-wayland-xdg-session.c:262-276`). It runs **before the unmapping commit is processed** —
   alongside the close-animation snapshot, for the same reason: afterwards the window's size is
   already zero.

   Two things the reference gets for free and we do not:

   - **Shutdown with windows open.** `meta_display_close` unmanages every window (`display.c:1052`)
     before the context's synchronous save (`meta-context-main.c:445`), so mutter's flagship case
     falls out of save-on-unmap. We tear down without unmapping, so `main.rs` sweeps every live
     registration before flushing. Without it, logging out with windows open would save nothing —
     the demo case (close a window, reopen the app) would have worked and the real one would not.
   - **`rename` moves the stored record**, or a window unmapped before the rename would leave its
     state orphaned under a name nothing answers to. Only reachable once there is a record.

   The rect is **global**; the workspace index is **per monitor**. The pair is deliberate: restore
   resolves the output from the rect first, then indexes into that monitor's workspaces.

   Which rect gets saved is the subtle part. In GNOME mode a maximizing tile stays in the floating
   layer, so its pre-maximize geometry goes to `Tile::tiled_restore_*`; `floating_*` is the same
   memory for scrolling mode, where the tile changes layers instead. Between them they are mutter's
   `saved_rect`, and `Workspace::session_snapshot` prefers them in that order, falling back to the
   live floating position. Reading only `floating_*` made a maximized window save its *maximized*
   rect — the exact thing a separate `saved_rect` exists to prevent.

   Everything read is a model value, never a render position: `Tile::window_loc` centres the window
   using animated sizes, so `window_offset` was split out to do it from the model ones. A window
   closed mid-animation must be remembered where the layout has it.

   Conformance tests cover unmap-saves-state, the rect being global and clearing the panel strut,
   a maximized window saving its pre-maximize rect, `remove_toplevel`-then-unmap saving nothing,
   the shutdown sweep, and rename carrying the record.
4. **Restore — DONE.** The `Unmapped` → `send_initial_configure` → map pipeline, built as slice 0
   promised: restore is a **writer of seeds**, not a sixth copy of the placement chain. It fills in
   `ResolvedWindowRules` (`open_fullscreen`, `open_maximized_to_edges`, `default_width`/`_height`,
   `default_floating_position`, `open_focused`) and `PlacementSeeds` (`output`, the new
   `workspace_idx`), and everything downstream is the code every window already took. A window that
   didn't ask to be restored runs an unchanged path.

   **Nothing is trusted across the idle.** The initial configure is queued in a calloop idle, so the
   payload is re-resolved at configure time rather than stashed at `restore_toplevel` time. A
   takeover in between empties the previous holder's registrations, so a stale request simply fails
   to look up — inertness doing the work again, with no staleness bit.

   **The output comes from the saved rect, by largest overlap** (mutter:
   `meta_monitor_manager_get_logical_monitor_from_rect`). No overlap with any connected output —
   the monitor is gone — and the seed is dropped, so the window falls through to the normal choice
   and still maps.

   **The workspace index grows the strip** until it exists (`Monitor::ensure_workspace_at`), capped
   at 36 — the range GNOME declares for `num-workspaces`, and the bound matters because the index
   comes back from a file a user can edit. The index is seeded twice, at configure and at map,
   because the two answer different questions: the configure needs a workspace to *size* against
   and only clamps, the map needs one to *add to* and is where the growth happens.

   **This was originally a clamp, and the clamp was a bug (fixed 2026-08-08).** Restoring a set of
   windows must not depend on the order the client asks in, and clamping made it depend on exactly
   that: three windows saved on desktops 0/1/2 came back correctly in *ascending* order — each
   landing on the trailing empty grew the strip just in time for the next — and collapsed two onto
   one desktop in any other order. A client iterating a hash map draws a different order per run,
   which is what made it present as intermittent. Found by asking what the implemented behavior
   actually was rather than what it was designed to be.

   **Divergence from mutter (approved 2026-08-08).** It appends a *single* workspace
   (`meta_window_change_workspace_by_index(.., append = TRUE)`, `window.c:5557-5565`), which is
   order-independent for a contiguous set but lands a sparse index on the wrong desktop. Growing to
   the index restores what was actually saved, and costs less here than upstream because this fork
   does not reap empty workspaces (`docs/fork/dynamic-workspaces-divergence.md`) — a gap in the
   middle of the strip is already a legal, first-class state.

   Growth is deliberately map-time only: placement resolution stays `&self`, and a window that is
   configured but dies before its first commit would otherwise leave desktops behind for good.

   Two things restore had to teach the layout:

   - **A restored maximized window never floated this run**, so there is no `tiled_restore_*` for
     unmaximize to return to. `Workspace::seed_unmaximize_geometry` writes the saved rect there
     directly — the reverse of the read slice 3 added.
   - **GNOME auto-maximize must skip restored windows.** Its size is remembered, not guessed. Left
     in, it overwrote the seed with its own scaled-down rect, so unmaximizing a restored window
     returned it to a default. The seed is applied *after* the auto-maximize point, not before.

   `restored` is sent immediately before `toplevel.send_configure()`, per the spec's "prior to the
   first `xdg_toplevel.configure`".

   Conformance tests cover the geometry round trip, `restored`-before-configure, state restore,
   the workspace round trip under all three reasons, an index past the end growing the strip, a
   nonsense index being capped, a set restoring in any order, a saved monitor that is gone,
   unmaximize returning to the saved rect, the saved position, the configure being taken against the
   saved workspace, and the auto-maximize exemption.

Slices 1–3 are independently useful and independently testable; slice 4 is where the real risk was,
because it touches the initial-configure path that every window goes through.

---

## Policy (settled 2026-08-07)

### Activation — keyed on `reason`

A restored window activates iff `reason == launch`. `recover` and `session_restore` map without
taking focus, so restoring five windows at login doesn't have each one steal focus in turn, while
a launcher-started app still behaves like any other launch. Mutter says nothing here; this is ours.

**Mapping never moves the user, and that is not a restore rule** (fixed 2026-08-08). Placing a
window used to activate its workspace, so a session coming back walked the user across every
desktop in turn — and the same jump happened for an `open-on-workspace` rule or any app opening a
window elsewhere. GNOME does not: `meta_window_show` focuses but never activates a workspace, the
single activation site in `window.c` (`:3921`) is reached only from an explicit activation, and a
window wanting attention from another workspace gets the pulsing indicator instead
(`window.c:3891-3899`). Mutter's restore agrees by construction — it calls
`meta_window_change_workspace_by_index` and nothing else.

**The exception is the client's lever: a fresh activation token**, gated exactly as mutter gates it
on `allow_workspace_switch = (timestamp != 0)` (`window.c:3866`). An app that wants you looking at a
particular window activates that one, and since a launch mints one token, at most one window can
move you. Tokens are honoured for `launch` only: at login the shell owns where you land, not a queue
of apps each minting one.

Focus travels with the workspace on purpose. Focusing a window the user cannot see would let a
restored window take focus from the one in front of them, and mutter has no such state — focusing a
window elsewhere goes down the activate-the-workspace branch instead.

Restore expresses this in the **activation decision**, not by setting the `open_focused` window
rule. That rule means "the user configured this app not to steal focus"; borrowing it for restore
conflated the two and short-circuited the activation path *before* the token was ever consulted, so
a restored window's token could not work at all. A policy that a client is supposed to be able to
override must not be expressed as a config override.

A knock-on for the corpus: session tests can no longer find their window via `layout.focus()`, since
a restored window on another desktop deliberately does not take it. `session_window(f, name)` goes
through the live registration instead.

This is the *only* thing `reason` is used for. We deliberately do **not** honour the spec's hint
that `launch` might restore size-only while `session_restore` also restores workspace: a launched
app ignoring its saved workspace would read as a bug, not a feature. Add a conformance test that
pins workspace restore under all three reasons so this stays deliberate.

---

### `reason` — we are its only consumer, and a client cannot compute it

Established live on 2026-08-08, chasing "launching ghost from the dash never restores its session".

**Mutter ignores `reason` outright.** `xdg_session_manager_get_session`
(`~/Projects/mutter/src/wayland/meta-wayland-xdg-session-manager.c:215-285`) takes `reason_value` at
`:226` and never reads it again; it is the only occurrence of the word in the xdg-session files. So
the enum has no reference behaviour at all, ours included: keying activation on it (above) is a
**divergence by addition**, not a port. A client that has only ever met mutter has had no way to
learn that the field does anything.

**A client cannot honestly distinguish `launch` from `session_restore` today.** The reason describes
*how the process came to be running*, and two of the three values are knowable only to whoever
started it:

- `recover` an app can determine alone (crash sentinel, or its previous Wayland connection dropping);
- `launch` and `session_restore` are identical from inside the process — same argv, env, cwd,
  parent. Nothing on this desktop marks a restore-initiated start: gnome-session and systemd set no
  such marker, and the compositor never relaunches apps. The value is provisioned for a future
  session manager that replays a saved app list and says so.

The rule a client should follow, therefore: **default to `launch`; send `recover` when you know you
crashed; send `session_restore` only when something external told you it is a restore.** The reason
describes how the process *started*, never what the app then decides to reopen — restoring saved
windows is what an app does *after* a launch, and does not retroactively make the launch a restore.

**The failure mode this invites, observed in ghost** (`ghost-ui/src/lib.rs:3409-3425`): it maps
`Startup::Restore(_) => SessionReason::SessionRestore`, i.e. any launch that finds a non-empty
`windows.toml` — a dash click included — reports `session_restore`. Its own comment names the cause:
*"synoik lets only a `Launch` take focus — so a workspace restore must say so."* The client reached
for `reason` as a **focus lever** because we made it one and mutter made it free. Reported to ghost
on 2026-08-08 as a client bug; the compositor side is unchanged.

**Upstream agrees it is missing, and is building the mechanism** (GUADEC 2026, Adrian Vovk's
save-and-restore talk; LWN 2026-07-22, <https://lwn.net/Articles/1083750/>). The design is three
layers, of which our protocol is the bottom one:

- `xdg_session_management_v1` — compositor-owned window state, merged in mutter. The stated
  motivation includes keeping restore decisions central *so apps cannot force workspace switches*,
  which is the rule we arrived at independently.
- **`org.freedesktop.portal.SaveRestore`** (xdg-desktop-portal PR #1818, design discussion #1698) —
  `Register()` returns an **instance id** plus the app's saved state *and a restore reason*:
  `PRISTINE` (restore nothing) / `LAUNCH` (minimal — size, navigation position) / `RESTORE` (whole
  previous state) / `RECOVER` (restore, and you crashed). The session manager knows because it is
  the thing doing the relaunching. Four values to our three: `PRISTINE` needs no Wayland
  counterpart, since an app with nothing to restore just never asks for a session.
- **`gnome-session`** (MR !158) — tracks apps by app id and relaunches from their `.desktop` files
  rather than saved command lines.

GTK's app-facing API (`gtk!8502`: `GtkApplication:support-save`, `save-state`/`restore-window`,
15 s autosave) is merged, but the plumbing between the layers is not: the MR states the open
question outright — *"how to communicate the restore reason to an activated application. It
currently uses the `GTK_APPLICATION_RESTORE` environment variable, but that is probably not what we
want."* So the correct source for a client's Wayland `reason` is the portal's restore reason, passed
through; until the portal lands, an env var from the launcher is the honest stopgap. None of it
ships in GNOME 51.

**Re-check when 51 reaches the reference checkouts.** "Mutter ignores `reason`" is verified against
50.3 and is the world ghost was written against, but mutter's implementation is reported as mostly
complete for the 51 beta. Re-grep `meta-wayland-xdg-session*.c` before continuing to treat our
reason-keyed activation as having no reference behaviour to compare with.

The lesson for us: acting on a client-chosen descriptive field turns it into an interface clients
will steer with. Our behaviour for a given reason must stay defensible when the reason is *wrong*,
because there is no path by which we can validate it. It currently is — a mis-tagged
`session_restore` costs the app its focus and nothing else — but any future rule keyed on `reason`
has to clear that same bar.

**Settled 2026-08-08 — what each reason may do.** Focus and attention are separate decisions:

| reason | takes focus | marks urgent when it lands elsewhere |
| --- | --- | --- |
| `launch` | yes (or marks, if denied) | yes |
| `recover` | no | **yes** |
| `session_restore` | no | no |

`recover` marks because a single app returning from a crash gets to say where it went — "your app
is back, over there" is one window's worth of noise, and useful. `session_restore` must not: a
login restoring everything you had open would have every app shouting at once, which is no signal
at all. The `recover` arm is deliberately *not* gated on the window having wanted focus, unlike the
launch arm: restore seeds `open_focused = false` for every reason but `launch`, so that test would
make it unreachable. Pinned by
`a_recover_demands_attention_from_another_desktop_but_a_restore_does_not`.

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
