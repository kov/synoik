# Running the compositor for manual testing

The headless conformance corpus (`cargo test -p synoik`, see `src/tests/gnome.rs`)
is the primary loop. This doc covers running the *real* compositor when you want
to see a change on screen.

## Backends, and which one you get

The backend is chosen automatically at startup (`src/synoik.rs`, `State::new`):

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
over IPC — the same `synoik msg` surface as any other instance:

```sh
export SYNOIK_SOCKET=…   # printed at startup ("IPC listening on: …")
synoik msg action spawn -- kitty
synoik msg windows
synoik msg event-stream
synoik msg action quit --skip-confirmation
```

Keyboard-driven paths work too: `synoik msg input` injects synthetic events
through the **real input pipeline** (`src/input/synthetic.rs`), so binds,
grabs, modality and focus behave exactly as if the keys were pressed:

```sh
synoik msg input key Super          # tap the overlay key → overview toggles
synoik msg input key Alt+F2         # combo: press in order, release reversed
synoik msg input text 'kitty'       # type into whatever is focused
synoik msg input key Return
synoik msg input key-press Alt      # hold…
synoik msg input key Tab            #   Alt+Tab MRU switch
synoik msg input key-release Alt    # …commit on release
synoik msg input pointer-motion 200 300; synoik msg input click; synoik msg input scroll 1
```

Keys are XKB keysym names (case-insensitive: `Super_L`, `F2`, `a`, `8`), the shorthands
`alt`/`ctrl`/`shift`/`super`, or `code:N` for a raw evdev keycode (`code:125` =
`KEY_LEFTMETA`); names resolve through the instance's active keymap, and unresolvable keys
come back as errors.

**A bare number is the digit key.** `Super+8` presses `8`, as the accelerator reads. It used to
be parsed as the keycode, and evdev 8 is `KEY_7` — so every digit accelerator silently fired
its neighbour, and `Super+8` "launching the wrong favourite" was investigated as a compositor
bug before the instrument was suspected. Injection works on any
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
cargo run -- -- kitty
```

Do **not** pass `--session` when nested — it imports env into systemd/D-Bus
globally and will disrupt your real session.

### The Super caveat (important for the overlay key)

When nested under GNOME, **the host GNOME grabs `Super` globally**, so a `Super`
press never reaches the nested window. Two consequences:

- synoik already knows this: when nested, the *compositor* mod key defaults to
  `Alt`, not `Super` (`backend/mod.rs`, `mod_key_nested`).
- The GNOME **overlay key** (Super-tap → overview) therefore can't be exercised
  nested under GNOME.

To test the overlay key specifically, use the **TTY** path below, or inject
the key press over IPC — it takes the real overlay-key code path, host grabs
notwithstanding:

```sh
SYNOIK_SOCKET=$(ls -t $XDG_RUNTIME_DIR/synoik.*.sock | head -1) \
  cargo run -- msg input key Super
```

(The running instance also prints its socket path on startup. See the
Headless section for the full `synoik msg input` surface.)

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
   ./target/release/synoik
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
Display config and most gsd daemons should come up (synoik implements the core
`org.gnome.Mutter.*` names, and `org.gnome.Shell` accelerator grabs — so
gsd-media-keys' volume/brightness/media keys work, though without OSD popups
until `ShowOSD` exists); the GNOME top panel is drawn (see below), but the
rest of the GNOME chrome (quick settings, notifications, calendar) is not.
The gaps you hit here are, in effect, the Phase 1 worklist.

## Windowing mode: floating by default

New windows **open floating with GNOME semantics** (`layout {
windowing-mode "floating" }`, the default): mutter's placement rules
(`src/layout/floating.rs`, ported from mutter `src/core/place.c`) — dialogs
center on their parent (top-biased third), other windows first-fit without
overlap and cascade in 50px steps when nothing fits. Window rules
(`open-floating false`) still override per window.

niri's scrollable tiling is still in the tree (`layout.windowing_mode`), but
there is no longer a way to *ask* for it: the config file is gone and the
GNOME-side setting is not ported yet, so a session always comes up floating.

GNOME's window keys work on the floating layer: **Super+Left/Right**
edge-tiles to half the work area and toggles back (`toggle-tiled-left/right`
from `org.gnome.mutter.keybindings`), **Super+Up** maximizes, **Super+Down**
unmaximizes or untiles (`maximize`/`unmaximize` from
`org.gnome.desktop.wm.keybindings`) — restore geometry follows mutter's
`saved_rect` rules, including through tile→maximize chains. Windows covering
more than 80% of the work area **auto-maximize on map** with a clamped
restore size (mutter `place.c`). All of it is IPC-drivable:
`synoik msg action toggle-tiled-left` / `maximize` / `unmaximize`.

Dragging works like mutter too (`meta-window-drag.c`): **drop a window in
the 48px band at a side of the work area to tile it** to that half (with a
tile preview while hovering the band), **drop it on the top edge to
maximize**; the restore rect is the pre-drag geometry. Dragging a maximized
window "shakes it loose" only after 48px of vertical movement (an edge-tiled
one after 48px on either axis), popping out at the restore size under the
pointer. Gated on `org.gnome.mutter edge-tiling`, honored live — note the
schema default is `false` and GNOME sessions enable it via a
`[org.gnome.mutter:GNOME]` session override, which we inherit through gio.
`--session` presents as GNOME (it keeps the `XDG_CURRENT_DESKTOP` the
session manager set, falling back to `GNOME`), so the full-session flow
gets it automatically; bare headless runs have no desktop set, so set the
key explicitly or export `XDG_CURRENT_DESKTOP=GNOME`. To drag over IPC:
`synoik msg input button-press left`, `pointer-motion …`, `button-release
left` with `key-press super` held.

Focus follows GNOME's stealing-prevention rules in this mode (mutter
`window.c` / `meta-wayland-activation.c`): a window whose launch — its
xdg-activation token — predates your last interaction with the focused
window does *not* take focus; it opens below the focused window and is
marked urgent (demands attention; visible in Alt-Tab, `is_urgent` over
IPC). Transients of the focused window always take focus, and token-less
windows always may. `org.gnome.desktop.wm.preferences focus-new-windows`
(`smart`/`strict`) is honored live.

## The overview: GNOME's window picker

In floating (GNOME) windowing mode, the overview spreads each workspace's
windows into **picker slots** instead of showing them at their layout
positions — gnome-shell's `UnalignedLayoutStrategy` ported to
`src/layout/expose.rs` (row packing that keeps previews near their real
windows, small windows enlarged up to 1.5×, everything capped at 95% of
natural size). The spread animates with the overview open/close progress,
clicks hit the slots, and clicking a preview activates that window and
leaves the overview. Scrolling mode keeps niri's zoomed-strip overview
untouched.

The workspaces form a **horizontal row** (GNOME 40+): the active one
centered at 80% of the monitor, and the neighbor workspaces peeking in at
the screen edges (gnome-shell keeps the inter-workspace spacing at its
minimum precisely so the side margins show them). Clicking a neighbor workspace switches to it and stays in
the overview; clicking the empty area of the active workspace leaves it
(gnome-shell's Workspace click rules). The mouse wheel — either axis —
scrolls through workspaces while the overview is open.

**Dragging a preview** is gnome-shell's WindowPreview drag, not a window
move: it picks up immediately (no shake-loose threshold, even for a
maximized window), and dropping it — on its own workspace, a neighbor's
peeking edge, or the gap between workspaces (which inserts a new one) —
only changes which workspace holds the window. The window keeps its
position on the desktop and the overview stays open. The real window is
never touched in flight: a maximized, fullscreen or edge-tiled window
keeps that state (and its restore rect) through the drag — no unmaximize
pop, no configure — and the dragged preview keeps the on-screen footprint
it was picked up at. The source desktop's picker layout freezes for the
duration (gnome-shell's `layout_frozen`), so the other previews hold their
slots instead of shuffling into the gap; the drop lets the layout
recompute. Windows poking past their workspace's edge are clipped to it in
the overview and during workspace switches, so they don't draw over the
neighbor.

The **wallpaper** comes from `org.gnome.desktop.background`: `picture-uri`
(or `picture-uri-dark` when `org.gnome.desktop.interface color-scheme` is
`prefer-dark`) is decoded (PNG/JPEG/WebP via the image crate, GNOME's stock
JPEG XL backgrounds via jxl-oxide) and drawn behind every workspace in GNOME
windowing mode, live-updating with the settings. In the overview the
workspace previews get gnome-shell's 30px rounded corners, growing with the
open transition (`BACKGROUND_CORNER_RADIUS_PIXELS`). Divergences for now:
every `picture-options` mode draws as `zoom` (cover + center crop, the
default), SVG wallpapers aren't decoded, and `primary-color` isn't used as
the no-picture fill — the configured solid background color backs those
cases instead.

The **thumbnails strip** (gnome-shell's ThumbnailsBox) appears above the
workspace row once a second desktop is populated (dynamic workspaces with
more than two workspaces, counting the trailing empty one), sliding in with
the overview transition. Each thumbnail is the workspace at 5% scale —
wallpaper and windows at their real positions — with the active one lifted
by a deeper, wider drop shadow that tracks workspace switches. Clicks
follow the same rules as the workspaces (non-active switches and stays,
active leaves); dragging a window preview onto a thumbnail moves the window
to that workspace, and dropping it into the gap between two thumbnails
inserts a new workspace there — the strip spreads apart around a
translucent placeholder pill while the drag hovers the gap (gnome-shell's
drop placeholder). The between-workspaces drop zone in the main row shows a
matching pill-shaped bar. Holding a dragged window against the left or
right screen edge snaps the row one desktop at a time: the first switch
comes right after the anti-flicker delay, then a 750 ms grace period has
to pass before each further snap while the pointer stays on the edge (our
affordance — continuous panning would make aiming at a desktop
impossible); on the desktop the screen edges keep belonging to edge
tiling. The active thumbnail's shadow is deliberately **not** accent-coloured:
a shadow reads as depth only while it is darker than what it falls on, and a
light accent inverted the cue.

Not yet ported: preview chrome (title, close button, app icon), the app
grid, and search.

## The top panel

In floating (GNOME) windowing mode a **top panel** is drawn in-compositor
(`src/ui/panel.rs`) on every output: a 32px opaque bar (gnome-shell's
`2.2em @ 11pt`) with a left **Activities** button and, at the far right past the
status indicators, the **clock** (local `HH:MM`, ticking on the minute; GNOME centres
it — see the accepted divergence in `docs/fork/panel-status-port.md`). Clicking Activities toggles the
overview — the mouse counterpart of the Super-tap — and highlights while the
overview is open; the panel itself stays put in the overview (in the dark
theme gnome-shell's `:overview` transparency is a visual no-op). It renders
above the windows but below the transient overlays (run dialog, Alt-Tab), and
is hidden on the lock and screenshot screens.

Crucially the panel **reserves a top strut**: the work area
(`layout::workspace::compute_working_area`) insets by the panel height, so
maximize, edge-tiling, floating placement and the overview picker slots all
sit below it — gnome-shell's `set_builtin_struts`. The strut and the drawing
are gated on floating mode, so niri's scrolling mode is unaffected.

Deferred: quick-settings/status indicators, the calendar popover, the
`clock-format`/date/seconds settings, the Activities workspace-dot animation,
and GNOME's primary-monitor-only placement (we currently panel every output).

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

Keys resolve in four tiers: the hardcoded VT/power keys, then everything GNOME itself names,
then external accelerator grabs (gnome-settings-daemon's lock, logout and media keys), and
last the scrolling-layout keys only we have. That last tier yields to gsd on purpose — see
`docs/fork/keybindings-port.md`.

Keybindings live **only** in GSettings — the config file's `binds{}` block is
gone. GNOME's own schemas own everything GNOME names; the scrolling-layout
actions it has no name for are in ours, `org.gnome.shell-rs.keybindings`, which
installs to a private schema dir (see `docs/fork/keybindings-port.md` for the
adopted tables, the divergences and how to read the keys from a shell).

## Configuration (there is no config file)

There is no `config.kdl`, no `--config`, no `synoik validate` and no file watcher.
A session runs on the compiled-in `Config::default()` (`synoik-config/src/lib.rs`)
plus GSettings, which is where everything user-facing is supposed to land — per
the fork tenet, GNOME's settings are the settings. What is still only in
`Config::default()` is work that has not been ported yet, and that is on purpose:
a compiled-in default is visible as a gap, a config knob hides one.

Input devices are the first block to complete that move: touchpad, mouse, trackball,
pointingstick and key repeat all come from `org.gnome.desktop.peripherals.*`, so
**Settings → Mouse & Touchpad** works and takes effect with no restart. Tablet and touchscreen
are not ported yet — see `docs/fork/input-peripherals-port.md`.

The one escape hatch is the debug toggles that used to live in `debug {}`. They
come from the environment now — `SYNOIK_DEBUG_<FIELD_NAME_IN_CAPS>`, the same
idiom as `SYNOIK_VK_VALIDATION`, so a systemd drop-in reaches the live session:

```sh
SYNOIK_DEBUG_DISABLE_TRANSACTIONS=1 cargo run
SYNOIK_DEBUG_PREVIEW_RENDER=screencast cargo run   # or screen-capture
SYNOIK_DEBUG_IGNORE_DRM_DEVICES=/dev/dri/card1:/dev/dri/card2
```

The field list is `Debug` in `synoik-config/src/debug.rs`; every `bool` field is a
flag var (set to anything but `0`/empty).

## The run dialog (Alt+F2)

**Alt+F2** opens the GNOME run dialog (`src/ui/run_dialog.rs`): type a
command, Enter runs it (shell quoting honored, but no pipes/expansion — it's
an argv split + PATH search, exactly gnome-shell's `trySpawnCommandLine`);
errors show in-dialog and keep it open; Escape closes; Up/Down walk the
history, shared with gnome-shell via `org.gnome.shell command-history`.
`org.gnome.desktop.lockdown disable-command-line` disables it. Not yet ported:
Tab completion, Ctrl+Enter (run in terminal), the open-a-file-path fallback.

Since a nested session can't see Alt+F2 (the host GNOME grabs it — same Super
caveat as above), open it over IPC when nested: `synoik msg input key Alt+F2`
(or `synoik msg action show-run-dialog`).

## Inspecting / driving a running instance (IPC)

```sh
synoik msg outputs           # or: windows, workspaces, overview-state
synoik msg action toggle-overview
synoik msg action show-run-dialog
synoik msg event-stream      # live event feed
```

`synoik msg` finds the instance via `$SYNOIK_SOCKET`; set it to the socket the
instance printed if you have more than one running.
