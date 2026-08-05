# Input method (IBus) port

**Status: dead keys work, in apps and in the compositor's own entries.** Slices 1–6 and 8 landed
(2026-08-05), seat-validated. What is left is CJK: a composition shows, but there is no candidate
popup to pick from — see the backlog.

**Reference-first.** Every claim cites GNOME 50.3 in `~/Projects/gnome-shell` / `~/Projects/mutter`,
GTK 4.22, or a live measurement on this machine (2026-08-04). Re-read the cited file before
implementing.

---

## The bug this exists to fix

Dead keys and Compose do not work in GTK apps under synoik — epiphany, ghostty, gnome-terminal —
with `us+intl` configured. **The cause is ours.** Two facts combine:

1. We create `TextInputManagerState` (`src/synoik.rs:6571`) with nothing behind it.
   `handlers/mod.rs` has a bare `delegate_text_input_manager!` and no input method, so smithay
   never sends `enter` and every request is discarded.
2. GTK selects its IM backend on the **mere existence** of the global. `gtk/gtkimmodule.c`,
   `match_backend`:

   ```c
   return GDK_IS_WAYLAND_DISPLAY (display) &&
          gdk_wayland_display_query_registry (display, "zwp_text_input_manager_v3");
   ```

   and `GtkIMContextWayland` inherits plain `GTK_TYPE_IM_CONTEXT` — *not* `GtkIMContextSimple` —
   with a `filter_keypress` that is nothing but `gdk_keyval_to_unicode` + commit. No compose
   table, no dead-key state, no chain-up.

So advertising the protocol **removes** GTK's own composition and supplies no replacement.
`dead_acute` yields no Unicode, is dropped, and the next letter commits bare.

The keymap is not at fault. Compiled through our own `xkb_from_input_sources` with
`sources = [us+intl, us]` and `xkb-options = [lv3:ralt_switch, compose:rctrl]`: group 0 is
"English (US, intl., with dead keys)", `<AC11>` is `0xfe51 dead_acute` / `0xfe57 dead_diaeresis`,
and `Multi_key` (`0xff20`) is present. Clients receive all of it.

**Two exits.** Drop the global (a two-line stopgap; GTK falls back to `gtk-im-context-simple`,
which has the full compose table), or become the input method. Gustavo chose the latter, so the
global stays advertised and dead keys stay broken until this port delivers composed text.

---

## GNOME's architecture

**There is no `MetaInputMethod`.** `ClutterInputMethod` is abstract and the concrete subclass
lives *outside* mutter: gnome-shell's `js/misc/inputMethod.js:24`,
`class InputMethod extends Clutter.InputMethod`, installed via
`clutter_backend_set_input_method()` (`clutter/clutter/clutter-backend.c:522`) from
`js/ui/main.js:240-241`.

```
wayland client (zwp_text_input_v3)
  ↕  src/wayland/meta-wayland-text-input.c        ← protocol state machine
ClutterInputFocus  ↕  ClutterInputMethod          ← abstract seam
  ↕  js/misc/inputMethod.js                       ← IBus.InputContext over D-Bus
ibus-daemon → engine (ibus-keyboard, libpinyin, …)
```

The same `ClutterInputMethod` serves gnome-shell's *own* StEntry widgets, which is why dead keys
work in GNOME's search box too. One mechanism, both surfaces.

### ★ Plain xkb layouts go through IBus

The single most important finding, and the one that rules out every shortcut.
`js/ui/status/keyboard.js:510-528`:

```js
activateInputSource(is, interactive) {
    this._keyboardManager.apply(is.xkbId);

    let engine;
    if (is.type === INPUT_SOURCE_TYPE_IBUS) {
        engine = is.id;
    } else {
        const [name, variant = ''] = is.id.split('+');
        const [lang = 'eng'] = this._xkbInfo.get_languages_for_layout(is.id);
        engine = `xkb:${name}:${variant}:${lang}`;
    }

    this._ibusManager.setEngine(engine);
    ...
```

An `xkb` source is **both** applied to the keymap **and** turned into a synthetic
`xkb:<layout>:<variant>:<lang3>` engine — `us+intl` → `xkb:us:intl:eng`. Those engines are real
and registered by `ibus-keyboard`. The reciprocal half, `js/misc/inputMethod.js:331-336`, gates
`filter_key_event` only on "have a context" and "have a source" — the source's *type* is never
checked.

**Dead keys and Compose are implemented by the IBus engine, not by the keymap.** There is no
"xkb needs no IM" fast path; taking one is a behavior regression.

---

## What landed

| Slice | Commit | What |
|---|---|---|
| 1 — IBus client | `419962c2` | `src/dbus/ibus.rs`: address discovery, bus + input-context proxies, `IBusText` decoding, engine-name synthesis. `examples/ibus_probe.rs` drives a live daemon. |
| 2 — Wayland seam | smithay `0ffb5170`, ours `f093ed60` | `TextInputHandle::set_internal_input_method`; test client speaks `zwp_text_input_v3`. |
| 3 — IM model | `e64ecf45` | `src/input_method/mod.rs`: engine output → client, byte↔char conversion, preedit cleared before commit. |
| 4 — Key round trip | `02a58520`, `2709fc15`, `df081c2f` | Keys held for the engine; the worker thread; forwarded keys. |
| 5 — Engine selection | `2709fc15` | `SetGlobalEngine` from `mru-sources`, re-sent on layout change. Folded into 4 because the round trip is inert without it. |
| 8 — Content type | `2a1e023d` | Purpose + hint mapping. Pulled ahead of 6: routing a password entry through an engine that has not been told it is a password is not a papercut. |
| 6 — Shell entries | `ce52d477`, `0720bc37` | All five compositor entries compose; `ImFocus`; preedit in `TextEdit`. |

Slice 1 is verified end to end: `dead_acute` then `a` returns a committed `"á"`.
Slice 2 is pinned by a differential pair in `src/tests/gnome.rs` — a text input hears *nothing*
without an input method, and gets `enter` + `Enabled`/`SurroundingText`/`Done` with one.

**Slice 4 is seat-validated, and the differential is the proof the feature works.** Same
`target/debug/synoik --headless`, same `zenity --entry` client, same `us+intl` layout, same
injected `apostrophe` then `a` — the only variable is whether `IBUS_ADDRESS` points at a live
daemon:

| Typed | IBus connected | IBus unreachable |
|---|---|---|
| `'` `a` | `á` (`c3 a1`) | `a` — the dead key vanished |
| `~` space | `~` (`7e`) | — |
| `~` `n` | `ñ` (`c3 b1`) | — |

The `~`-then-space row is worth keeping: it is the *escape* path rather than the composing one,
so it shows the engine really is running the compose state machine — holding the dead key, seeing
a follow-up that composes with nothing, and emitting the bare accent instead of dropping it.

The second row *is* the reported bug, reproduced on demand: `GtkIMContextWayland` has no compose
table, so the dead acute produces nothing at all. Reproducing it that cheaply is worth keeping —
the harness is `IBUS_ADDRESS=unix:path=/tmp/no-such.sock`, and any regression in the key path
shows up as the client going back to bare `a`.

Run the isolated instance under `dbus-run-session` **and** a private `XDG_CONFIG_HOME`: the
active input source comes from live dconf, and neither isolates gsettings on its own. Point
`IBUS_ADDRESS` at your own daemon on a short socket path — `SetGlobalEngine` is bus-wide, so
using the session's daemon changes the input source for everything else on it.

Slice 6 is validated the same way, on the compositor's *own* entries, which need no client at
all: open the overview, inject `apostrophe` `a`, and `grim` the search entry. It reads `á`, and
`Shift+grave` `space` `Shift+grave` `n` reads `~ñ`. Unlike a client window, compositor chrome
renders fine headless — the blank-GPU-client caveat does not apply to it.

### Where a key can go

| Focus | Offered to the engine | Delivered by |
|---|---|---|
| `ImFocus::Client` | every key that reached the forward point | `input_forward` |
| `ImFocus::Shell(_)` | presses carrying text **or beginning a composition**, plus any key during one | the entry's own handler |
| `ImFocus::None` | nothing | unchanged |

The shell path is narrower on purpose. Those entries sit at the bottom of ladders that fall
through — the overview search gives way to grid navigation, then to hardcoded binds — and a
fall-through has to produce a `FilterResult` synchronously. Deferring a key that *might* fall
through would mean reimplementing each ladder in the delivery path. Deferring only what the entry
is certain to consume costs nothing, because an engine has no use for the rest.

**`dead_acute.key_char()` is `None`.** Gating on "carries text" therefore drops the very key a
dead-key sequence starts with, which is why `begins_composition` (xkb's contiguous `dead_*` block
plus `Multi_key`) is not optional. This shipped broken once: the lock screen and polkit entries
did not compose, while the overview search *appeared* to work because a key with no text falls
past the search block and reaches the generic client-forward path, which offers everything. An
entry that intercepts unconditionally has no such accident to fall back on.

The corpus missed it too, because the `Fixture` keymap is plain `us` — where the apostrophe key
is an ordinary character, so a dead-key test quietly becomes a plain-typing one. Use
`use_us_intl(&mut f)` and assert the keysym, or the test is not testing what it says.

Also settled while chasing it: **IBus composes normally under `PASSWORD` and `PIN` content
types** — announcing the password purpose does not disable the engine. `examples/ibus_password_probe.rs`
is that experiment, kept because it was the first (wrong) theory and is cheap to re-run.

`ImFocus` exists because a modal dialog can open over a client whose text input is still enabled.
GNOME has the same single-focus rule for free: `ClutterInputMethod` holds one
`ClutterInputFocus` (`clutter-input-method.c:544`) and a shell entry taking key focus focuses out
whatever had it. Switching flushes held keys and drops the composition first, so a half-finished
character cannot land in the entry it was not meant for.

### The smithay patch

Stock smithay gates all text-input activity on a `zwp_input_method_v2` **client** existing, which
rules out GNOME's architecture outright. The patch adds

```rust
pub fn set_internal_input_method(&self, sink: Option<InternalInputMethod>)
pub type InternalInputMethod = Arc<dyn Fn(TextInputEvent) + Send + Sync>;
```

and counts an internal IM alongside a Wayland one in the three gates: `enter` on bind
(`text_input/mod.rs`), `enter`/`leave` on keyboard focus (`seat/keyboard.rs`), and the request
filter (`text_input_handle.rs`).

A **callback, not a handler trait**: an internal IM is always talking off-thread, so the sink is a
channel send; a trait taking `&mut D` would bound every `Dispatch` impl for nothing. The internal
sink is notified from the same commit path as the Wayland objects so the two cannot drift.

Note the `leave` split — stock smithay had `deactivate_input_method` and `text_input.leave()`
under one `has_instance()` check. Whoever was told `enter` must be told `leave`, or a client is
left believing it still holds focus.

**Keys need no smithay patch.** Intercept in the existing `KeyboardHandle::input` filter closure
at `src/input/mod.rs:684`, returning `Intercept` for what IBus consumes.

---

## Reference: mutter's protocol state machine

`src/wayland/meta-wayland-text-input.c`. Reproduce these rules exactly.

- **Pending state is per-seat; serials are per-resource.** One shared `pending_state` bitmask and
  one `pending_surrounding` (`:58-125`), but a `resource_serials` hash keyed by `wl_resource`
  (`:70`).
- **`increment_serial` runs at the top of `commit`, before the focus check** (`:856`), so it counts
  every commit even from an unfocused client. `done` echoes that resource's own count — two
  resources of one client can get different serials in the same flush.
- **Only `done` is batched.** `commit_string`, `delete_surrounding_text` and `action` hit the wire
  eagerly; `preedit_string` goes out *only* inside the deferred flush, on a
  `CLUTTER_PRIORITY_EVENTS + 1` idle (`:244-266`). Three synchronous flush points: an unfiltered
  key (`:1192`), a click on the focused surface (`:1244`), and a focus reset (`:543`).
- **Focus is exactly keyboard focus**, set in the same call right after the keyboard
  (`meta-wayland-seat.c:228-248`). `leave` then `enter` inside one call.
- **Requests are focus-gated** (`client_matches_focus`, `:636-644`) — the spec's "ignore requests
  after leave until the next enter".
- **The cursor rect is deferred** to the surface's `pre-state-applied` signal (`:928-951`), not
  sent at commit time, so it lands in step with the surface's own commit.

### Byte↔char conversions — every boundary

The bug class that only shows up on the accented text this port exists for.

| Where | On the wire | In the IM |
|---|---|---|
| `set_surrounding_text` | bytes | chars (`g_utf8_strlen`, `:906-926`) |
| preedit cursor/anchor | bytes | chars (`:331`) |
| preedit hint spans | bytes | chars (`:367-370`) |
| `delete_surrounding_text` | before/after byte **lengths** | char offset + length (`:282-323`) |

`delete_surrounding_text` also clamps with `MIN (offset, 0)` — a positive offset becomes 0 — and
dereferences the stored surrounding text without a NULL check.

### Enum traps

- **`content_purpose` loses `pin` on GNOME — twice.** `ClutterInputContentPurpose` has no `PIN`
  (`clutter-enums.h:1087-1099`), so mutter's `translate_purpose` has no case for the protocol's
  `pin`, hits `g_warn_if_reached()` and returns NORMAL (`:746-781`). A PIN entry therefore reaches
  the engine as ordinary free-form text. Separately, Clutter *has* `DATE`/`TIME`/`DATETIME` and
  IBus does not, and gnome-shell's `else if` chain has no branch for them, so they fall out as
  `FREE_FORM` (`inputMethod.js:302-328`).
  **Both stages map by name, not by value**, so there is no off-by-one — an earlier revision of
  this doc claimed one, wrongly. But the numbers *do* diverge (protocol `pin`=9 where IBus counts
  on to `terminal`=10), so a numeric cast would be a real bug. `content_purpose_to_ibus` maps by
  name and passes `pin` through, since IBus has it at 9 — we are deliberately better than GNOME
  here, and `assert_ne!(ContentPurpose::Terminal as u32, purpose::TERMINAL)` pins the trap.
- **`preedit_shown` is not a hint.** It is diverted into `can_show_preedit` (`:876-882`).
- `on_screen_input_provided` maps to `INHIBIT_OSK`. Hints 0x1–0x200 are bit-for-bit identical.
- `set_text_change_cause` is stored and never read.

### A mutter bug — implement the intent

`clutter_input_method_set_handled_actions` (`clutter-input-method.c:563-581`) early-returns on
unchanged but **never stores the value**, so `trigger_action`'s `handled_actions` guard always
rejects. Implement what it meant, not what it does.

---

## Reference: the IBus D-Bus surface

ibus-daemon runs **its own message bus** on a private AF_UNIX socket — not the session bus, but a
real bus (it implements `org.freedesktop.DBus`). Unlike [`gdm`](../../src/dbus/gdm.rs)'s bare peer
channel, `Hello`/`RequestName`/`AddMatch` all work, so ordinary zbus proxies are fine.

**Address discovery** (libibus order): `$IBUS_ADDRESS` → `$IBUS_ADDRESS_FILE` →
`$XDG_CONFIG_HOME/ibus/bus/<machine-id>-<host>-<display>`.

### `org.freedesktop.IBus` at `/org/freedesktop/IBus`

| libibus call | Wire |
|---|---|
| `create_input_context_async(name)` | method `CreateInputContext(s) → o` |
| `list_engines_async()` | **property** `Engines` (`av`) — `ListEngines()` is deprecated |
| `get_global_engine_async()` | **property** `GlobalEngine` (`v`) |
| `set_global_engine_async(id)` | method `SetGlobalEngine(s)`, 4000 ms timeout |
| `preload_engines_async(ids)` | **write-only property** `PreloadEngines` (`as`) |

### `org.freedesktop.IBus.InputContext` at the returned path

Methods we need: `ProcessKeyEvent(uuu) → b`, `FocusIn`, `FocusOut`, `Reset`,
`SetCapabilities(u)`, `SetCursorLocation(iiii)`, `SetSurroundingText(vuu)`.
Properties (tuple-typed — the variant must be the struct): `ContentType` `(uu)`,
`ClientCommitPreedit` `(b)`.

Signals gnome-shell subscribes to (`inputMethod.js:85-95`): `CommitText`,
`UpdatePreeditTextWithMode`, `ShowPreeditText`, `HidePreeditText`, `ForwardKeyEvent`,
`DeleteSurroundingText`, `RequireSurroundingText`.

**`DeleteSurroundingText` and `RequireSurroundingText` are not in the daemon's introspection XML**
but are emitted — the names exist only as strings inside `libibus`. Subscribe by name.

Capabilities (`IBus.Capabilite`): `PREEDIT_TEXT`=1, `AUXILIARY_TEXT`=2, `LOOKUP_TABLE`=4,
`FOCUS`=8, `PROPERTY`=16, `SURROUNDING_TEXT`=32, `OSK`=64, `SYNC_PROCESS_KEY`=128. gnome-shell's
baseline is `PREEDIT_TEXT|FOCUS` = 9. It does *not* claim auxiliary text / lookup table /
property — those arrive on the **panel** interface instead.

### The key round trip

`inputMethod.js:331-361`. `vfunc_filter_key_event` returns `true` meaning *"I took it, don't
deliver it yet"*, then issues an async `ProcessKeyEvent`. The reply's `handled` boolean decides:
`true` ⇒ drop, `false` ⇒ replay to the client with an `INPUT_METHOD` flag that makes the second
pass skip the IM. **One D-Bus round trip per keystroke** — never block the compositor thread on it.

- `IGNORED_MASK` (1<<25) short-circuit is **mandatory** — without it, forwarded events loop.
- `RELEASE_MASK` (1<<30) marks a release; `ProcessKeyEvent` has no press/release argument.
- Keycodes: xkb − 8 outbound, evdev + 8 inbound.
- Ordering: `SetCapabilities` **before** `SetSurroundingText`. On focus-out:
  `ContentType(0,0)` → `SetCursorLocation(0,0,0,0)` → `Reset` → `FocusOut`.

### Daemon lifecycle

`ibusManager.js:84-135`. Command line is exactly `ibus-daemon --panel disable [extra]`; restart
prepends `-r`. `--panel disable` is essential — gnome-shell *is* the panel and owns
`org.freedesktop.IBus.Panel`. Spawn as a direct child (`DO_NOT_REAP_CHILD`): ibus-daemon refuses
to start with init as its parent. If the systemd unit
`org.freedesktop.IBus.session.GNOME.service` exists, systemd owns the daemon and the shell never
spawns it. Xwayland start ⇒ restart with `--xim`; Xwayland stop ⇒ restart without
(`windowManager.js:892,901`).

---

## Traps found the hard way

- **`WAYLAND_DISPLAY` beats `DISPLAY`** when naming the address file. A GNOME session runs
  Xwayland, so `DISPLAY=:0` is *also* set; preferring it picks a file the live daemon never wrote.
  Verified: pid 2880 wrote `…-unix-wayland-1` while `…-unix-0` was a corpse from 2023. Pinned by
  a test.
- **Address files are never cleaned up** — this machine has some from 2021. Check
  `IBUS_DAEMON_PID` liveness before dialing, and treat `EPERM` from `kill(pid, 0)` as *alive*.
- **`use-global-engine` is on by default**, so per-context `SetEngine` fails with
  `Cannot set engines when use-global-engine is enabled`. gnome-shell's `SetGlobalEngine` is the
  only call that lands on a stock daemon. It is bus-wide, so **never point a probe at the session
  daemon** — spawn your own.
- **Probe socket paths must be short.** `sockaddr_un` truncates at 108 bytes and the scratchpad
  path silently lands the daemon on a *different* socket. Use `/tmp/ibusprobe.sock`.
  `ibus-daemon` also refuses to start when the session already has one unless
  `IBUS_ADDRESS_FILE` points elsewhere. See `examples/ibus_probe.rs`.
- **gnome-shell keeps two separate bus connections** (`ibusManager.js:74`, `inputMethod.js:38`):
  one owns the Panel name, the other owns the input context. That split is what makes its
  `/InputContext_1` focus filter meaningful.

---

## Backlog

| Slice | What | Notes |
|---|---|---|
| **7** | Daemon lifecycle | Spawning a daemon we did not find (GNOME's `ibusManager.js` launches one), systemd-unit check. Reconnect *is* done: the worker retries on a backoff and replays focus + surrounding text. |
| **A** | Candidate popup | The IBus **Panel** interface: lookup table, auxiliary text, property list. Nothing CJK is usable without it — the composition shows, but there is no way to pick among candidates. The biggest remaining gap. |
| **B** | Cursor rectangle | `set_cursor_location`, so a candidate popup can sit under the caret. Needed by A, useless before it. |
| **C** | Engine language | `engine_name` hardcodes `eng`. Deriving it properly needs IBus's own layout→language table. |
| **Deferred** | OSK integration, surrounding text from our own entries | |
