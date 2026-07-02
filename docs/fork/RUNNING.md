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
| Tests | **Headless** | no GPU, no display |

So from inside your GNOME session, `cargo run` already opens a nested window —
nothing special required.

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

To test the overlay key specifically, use the **TTY** path below. To test the
overview *behavior* while nested, drive it over IPC instead of the keyboard:

```sh
NIRI_SOCKET=$(ls -t $XDG_RUNTIME_DIR/niri.*.sock | head -1) \
  cargo run -- msg action toggle-overview
```

(The running instance also prints its socket path on startup.)

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
drop-in overrides `ExecStart` to our binary; `--session` mode already
sd_notifies readiness, so the unit contract holds.

```sh
cargo build --release
sudo scripts/install-test-session.sh   # one-time: creates user "gsrs", installs
                                       # /usr/local/bin/gnome-shell-rs + the override
```

To run: switch user (lock screen → the other user), log in as `gsrs` choosing
the regular **GNOME** session. GDM gives it its own VT; **Ctrl+Alt+F\<n\>**
flips between it and your session. To iterate: rebuild, rerun the script, log
`gsrs` out and back in.

If the compositor dies, the unit's `OnFailure=` drops that session back to
GDM; yours is unaffected. Undo with
`sudo scripts/install-test-session.sh --uninstall`.

Expectations: this exercises the Phase 1 contract surface (STRATEGY.md §3.8).
Display config and most gsd daemons should come up (niri implements the core
`org.gnome.Mutter.*` names); media keys won't work yet (gsd-media-keys needs
`org.gnome.Shell` `GrabAccelerator`); there is no panel or GNOME chrome. The
gaps you hit here are, in effect, the Phase 1 worklist.

## How GNOME settings feed in

On startup the compositor reads the settings it honors from the **same
GSettings/dconf store gnome-shell uses** (`src/gnome.rs`, `GnomeSettings`). So:

```sh
gsettings get org.gnome.mutter overlay-key      # e.g. 'Super'
gsettings set org.gnome.mutter overlay-key 'Menu'
```

changes are picked up on the **next start** of the compositor (live
change-signal subscription is a TODO). Note dconf is shared with your real GNOME
session, so a change affects both.

## Inspecting / driving a running instance (IPC)

```sh
niri msg outputs           # or: windows, workspaces, overview-state
niri msg action toggle-overview
niri msg event-stream      # live event feed
```

`niri msg` finds the instance via `$NIRI_SOCKET`; set it to the socket the
instance printed if you have more than one running.
