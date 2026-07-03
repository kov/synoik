# Running the compositor for manual testing

The headless conformance corpus (`cargo test -p niri`, see `src/tests/gnome.rs`)
is the primary loop. This doc covers running the *real* compositor when you want
to see a change on screen.

## Backends, and which one you get

The backend is chosen automatically at startup (`src/niri.rs`, `State::new`):

| Situation | Backend | What it is |
|---|---|---|
| Started inside a Wayland/X11 session (`WAYLAND_DISPLAY`/`DISPLAY` set) | **Winit** | a single nested window |
| Started on a bare VT (no display) | **Tty** | real KMS/DRM, your actual session |
| `--headless` flag, and tests | **Headless** | no display or input devices |

So from inside your GNOME session, `cargo run` already opens a nested window —
nothing special required.

## Headless (drive it entirely over IPC)

```sh
cargo run -- --headless
```

starts the real compositor with no display and no input devices: one virtual
1920×1080 output, a Wayland socket real clients can connect to, and a
surfaceless-EGL renderer so GL clients actually draw. Everything is driven
over IPC — the same `niri msg` surface as any other instance:

```sh
export NIRI_SOCKET=…   # printed at startup ("IPC listening on: …")
niri msg action spawn -- kitty
niri msg windows
niri msg event-stream
niri msg action quit --skip-confirmation
```

Keyboard-driven paths work too: `niri msg input` injects synthetic events
through the **real input pipeline** (`src/input/synthetic.rs`), so binds,
grabs, modality and focus behave exactly as if the keys were pressed:

```sh
niri msg input key Super          # tap the overlay key → overview toggles
niri msg input key Alt+F2         # combo: press in order, release reversed
niri msg input text 'kitty'       # type into whatever is focused
niri msg input key Return
niri msg input key-press Alt      # hold…
niri msg input key Tab            #   Alt+Tab MRU switch
niri msg input key-release Alt    # …commit on release
niri msg input pointer-motion 200 300; niri msg input click; niri msg input scroll 1
```

Keys are evdev keycodes in decimal (`125`), XKB keysym names
(case-insensitive: `Super_L`, `F2`, `a`), or the shorthands
`alt`/`ctrl`/`shift`/`super`; names resolve through the instance's active
keymap, and unresolvable keys come back as errors. Injection works on any
backend, not just headless — handy for poking a nested instance past the
host compositor's grabs.

This is the way to exercise compositor behavior from a shell (or an agent)
without a Wayland session, a free VT, or fighting the host compositor for
keys. It reads and watches the real GSettings store like any non-test
instance; point `GSETTINGS_BACKEND=keyfile` + a scratch `XDG_CONFIG_HOME` at
it for isolation. Don't pass `--session`.

## Nested (quick, for most things)

```sh
scripts/dev-nested.sh            # debug build, spawns a terminal inside
PROFILE=release scripts/dev-nested.sh
```

or by hand:

```sh
cargo run -- --config resources/default-config.kdl -- kitty
```

Do **not** pass `--session` when nested — it imports env into systemd/D-Bus
globally and will disrupt your real session.

### The Super caveat (important for the overlay key)

When nested under GNOME, **the host GNOME grabs `Super` globally**, so a `Super`
press never reaches the nested window. Two consequences:

- niri already knows this: when nested, the *compositor* mod key defaults to
  `Alt`, not `Super` (`backend/mod.rs`, `mod_key_nested`).
- The GNOME **overlay key** (Super-tap → overview) therefore can't be exercised
  nested under GNOME.

To test the overlay key specifically, use the **TTY** path below, or inject
the key press over IPC — it takes the real overlay-key code path, host grabs
notwithstanding:

```sh
NIRI_SOCKET=$(ls -t $XDG_RUNTIME_DIR/niri.*.sock | head -1) \
  cargo run -- msg input key Super
```

(The running instance also prints its socket path on startup. See the
Headless section for the full `niri msg input` surface.)

## TTY (faithful: real Super, real KMS)

This is the way to test the overlay key and anything input- or display-specific.

1. Build first, from your GNOME session (compiling on the VT is slow/noisy):
   ```sh
   cargo build --release
   ```
2. Switch to a free VT: **Ctrl+Alt+F3** (F3–F6 are usually free; GDM/GNOME sit
   on F1/F2). Log in.
3. Run it:
   ```sh
   cd ~/Projects/gnome-shell-rs
   ./target/release/niri
   ```
   Tap `Super` — the overview should open.
4. Quit the compositor (its configured quit bind, default `Super+Shift+E`), then
   switch back to your GNOME session: **Ctrl+Alt+F1** (or F2).

## Full GNOME session (our compositor in place of gnome-shell)

The closest thing to the endgame: a complete GNOME session — gnome-session,
the `gsd-*` daemons, portals — where only the shell binary is ours. It runs as
a dedicated test user (`gsrs`), so your own session and dconf stay untouched.

How it works: GDM's "GNOME" session starts the systemd user target
`gnome-session@gnome.target`, which requires `org.gnome.Shell@user.service`
(`ExecStart=/usr/bin/gnome-shell --mode=user`, `Type=notify`). A per-user
drop-in overrides `ExecStart` to point **straight at the binary in this repo's
`target/debug`** (set `PROFILE=release` for the release build); `--session`
mode already sd_notifies readiness, so the unit contract holds.

```sh
cargo build
sudo scripts/install-test-session.sh   # one-time: creates user "gsrs" + the override
```

To run: switch user (lock screen → the other user), log in as `gsrs` choosing
the regular **GNOME** session. GDM gives it its own VT; **Ctrl+Alt+F\<n\>**
flips between it and your session. To iterate: just rebuild and log `gsrs` out
and back in — the session always runs whatever is currently in `target/debug`,
no reinstall step.

Leaving the session: quitting the compositor (`Super+Shift+E` or
`Ctrl+Alt+Delete`) **is** logging out — the drop-in adds
`OnSuccess=gnome-session-shutdown.target`, so a clean quit tears the whole
GNOME session down to GDM, the same as `OnFailure=` does when it crashes.
(Without that, a clean exit leaves a headless gnome-session running, and
every later GDM login re-joins it: a black VT.) Your own session is
unaffected either way. Undo everything with
`sudo scripts/install-test-session.sh --uninstall`.

Expectations: this exercises the Phase 1 contract surface (STRATEGY.md §3.8).
Display config and most gsd daemons should come up (niri implements the core
`org.gnome.Mutter.*` names, and `org.gnome.Shell` accelerator grabs — so
gsd-media-keys' volume/brightness/media keys work, though without OSD popups
until `ShowOSD` exists); there is no panel or GNOME chrome. The gaps you hit
here are, in effect, the Phase 1 worklist.

## Windowing mode: floating by default

New windows **open floating with GNOME semantics** (`layout {
windowing-mode "floating" }`, the default): mutter's placement rules
(`src/layout/floating.rs`, ported from mutter `src/core/place.c`) — dialogs
center on their parent (top-biased third), other windows first-fit without
overlap and cascade in 50px steps when nothing fits. Window rules
(`open-floating false`) still override per window.

niri's scrollable tiling is one config line away:

```kdl
layout {
    windowing-mode "scrolling"
}
```

(The matching GNOME-side setting is deferred; the switch lives in the niri
config for now.)

GNOME's window keys work on the floating layer: **Super+Left/Right**
edge-tiles to half the work area and toggles back (`toggle-tiled-left/right`
from `org.gnome.mutter.keybindings`), **Super+Up** maximizes, **Super+Down**
unmaximizes or untiles (`maximize`/`unmaximize` from
`org.gnome.desktop.wm.keybindings`) — restore geometry follows mutter's
`saved_rect` rules, including through tile→maximize chains. Windows covering
more than 80% of the work area **auto-maximize on map** with a clamped
restore size (mutter `place.c`). All of it is IPC-drivable:
`niri msg action toggle-tiled-left` / `maximize` / `unmaximize`.

Dragging works like mutter too (`meta-window-drag.c`): **drop a window in
the 48px band at a side of the work area to tile it** to that half (with a
tile preview while hovering the band), **drop it on the top edge to
maximize**; the restore rect is the pre-drag geometry. Dragging a maximized
window "shakes it loose" only after 48px of vertical movement (an edge-tiled
one after 48px on either axis), popping out at the restore size under the
pointer. Gated on `org.gnome.mutter edge-tiling`, honored live — note the
schema default is `false` and GNOME sessions enable it via a
`[org.gnome.mutter:GNOME]` session override, which we inherit through gio:
outside a GNOME-branded session (e.g. bare headless), set it explicitly or
export `XDG_CURRENT_DESKTOP=GNOME`. To drag over IPC:
`niri msg input button-press left`, `pointer-motion …`, `button-release
left` with `key-press super` held.

Focus follows GNOME's stealing-prevention rules in this mode (mutter
`window.c` / `meta-wayland-activation.c`): a window whose launch — its
xdg-activation token — predates your last interaction with the focused
window does *not* take focus; it opens below the focused window and is
marked urgent (demands attention; visible in Alt-Tab, `is_urgent` over
IPC). Transients of the focused window always take focus, and token-less
windows always may. `org.gnome.desktop.wm.preferences focus-new-windows`
(`smart`/`strict`) is honored live.

## How GNOME settings feed in

On startup the compositor reads the settings it honors from the **same
GSettings/dconf store gnome-shell uses** (`src/gnome.rs`, `GnomeSettings`). So:

```sh
gsettings get org.gnome.mutter overlay-key      # e.g. 'Super'
gsettings set org.gnome.mutter overlay-key 'Menu'
gsettings set org.gnome.desktop.wm.keybindings close "['<Super>q']"
```

changes are picked up **live** by a change subscription
(`gnome::load_and_watch_gsettings`, a dedicated glib-loop thread bridged into
calloop). Note dconf is shared with your real GNOME session, so a change
affects both.

GNOME keybindings (`org.gnome.desktop.wm.keybindings`: close, workspace
switch/move, panel-run-dialog, …) resolve **before** binds from the niri
config file — in a GNOME session, GSettings *is* the keybinding config; the
niri config stays underneath as a fallback. The adopted subset lives in
`gnome::adopted_wm_keybindings`.

## The run dialog (Alt+F2)

**Alt+F2** opens the GNOME run dialog (`src/ui/run_dialog.rs`): type a
command, Enter runs it (shell quoting honored, but no pipes/expansion — it's
an argv split + PATH search, exactly gnome-shell's `trySpawnCommandLine`);
errors show in-dialog and keep it open; Escape closes; Up/Down walk the
history, shared with gnome-shell via `org.gnome.shell command-history`.
`org.gnome.desktop.lockdown disable-command-line` disables it. Not yet ported:
Tab completion, Ctrl+Enter (run in terminal), the open-a-file-path fallback.

Since a nested session can't see Alt+F2 (the host GNOME grabs it — same Super
caveat as above), open it over IPC when nested: `niri msg input key Alt+F2`
(or `niri msg action show-run-dialog`).

## Inspecting / driving a running instance (IPC)

```sh
niri msg outputs           # or: windows, workspaces, overview-state
niri msg action toggle-overview
niri msg action show-run-dialog
niri msg event-stream      # live event feed
```

`niri msg` finds the instance via `$NIRI_SOCKET`; set it to the socket the
instance printed if you have more than one running.
