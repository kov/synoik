# Keybindings port — GSettings replaces the KDL `binds{}`

Keybindings are a dogfood blocker. The fork currently carries **two** keybinding systems:

1. **GNOME's GSettings keybindings** — `src/gnome.rs` holds a port of mutter's accelerator
   parser (`parse_accelerator`, with `Above_Tab`, the implicit `XF86` retry and `0x` keycode
   forms), the `GnomeKeybinding { action, accels }` model, the adopted tables, and the live
   re-read that swaps `Niri::gnome_settings` wholesale (`src/niri.rs:1633`) so a
   `gsettings set` takes effect with no restart.
2. **niri's KDL `binds{}`** — `resources/default-config.kdl`, consulted **last** by
   `find_bind` (`src/input/mod.rs`), so GNOME already wins every conflict.

The endgame is one model. GNOME's schemas own every key GNOME owns; a schema of ours owns the
scrolling-WM actions GNOME has no equivalent for; `binds{}` goes away. Per the fork tenet,
where niri and GNOME merely do the same thing differently, GNOME wins.

## Precedence (`find_bind`)

1. Hardcoded `XF86Switch_VT_1..12` → `ChangeVt`, `XF86PowerOff` → `Suspend`. Never inhibited,
   keymap-independent. **This path stays forever** — it is the recovery hatch out of a
   misbehaving shortcuts-inhibitor.
2. `find_gnome_bind` — the GSettings keybindings.
3. `find_accel_grab_bind` — external `org.gnome.Shell` `GrabAccelerator` grabs
   (gnome-settings-daemon's media keys, the GlobalShortcuts portal, Settings' custom shortcuts).
4. `find_configured_bind` — the KDL binds, on their way out.

An adopted key with no `action_for_gnome` mapping returns `None` and **falls through**. Benign
while the KDL layer catches it; after the prune it means the key reaches the client. The prune
slice must audit that every adopted entry maps to `Some`.

## Ownership split (GNOME 50.3)

- **mutter**: window management, workspaces, tab switchers, tiling, monitor switch/rotate, VT
  switching, `restore-shortcuts`, `cancel-input-capture`.
- **gnome-shell**, in-process: overview, app grid, message tray, quick settings,
  `switch-to-application-N`, all screenshot/screencast keys, screen brightness, the run dialog;
  it also *replaces* mutter's handlers for alt-tab and workspace switching.
- **gnome-settings-daemon**, over `org.gnome.Shell.GrabAccelerator`: volume, mic-mute, media
  transport, launchers, power/suspend, keyboard backlight, rfkill, logout/reboot/shutdown/lock,
  a11y toggles, and the user's custom shortcuts. We already serve that D-Bus API, so these need
  no table entry of ours.

Note screenshot and screen brightness moved **into gnome-shell** in 50.x; they are no longer
gsd media keys.

## Adopted

`org.gnome.desktop.wm.keybindings` — `panel-run-dialog`, `close`, `toggle-fullscreen`,
`maximize`, `unmaximize`, `switch-to-workspace-{left,right,up,down,1..12}`,
`move-to-workspace-{left,right,up,down,1..12}`, `switch-windows(-backward)`,
`switch-group(-backward)`, `cycle-windows(-backward)`, `cycle-group(-backward)`,
`switch-applications(-backward)`, `toggle-maximized`, `switch-to-workspace-last`,
`move-to-workspace-last`, `move-to-monitor-{left,right,up,down}`,
`switch-input-source(-backward)`.

`switch-input-source` is a **divergence**: gnome-shell puts up an input-source switcher popup
for the duration of the modifier hold, and we switch straight away. That popup is the same
shape as the alt-tab switchers and belongs with them.

`toggle-maximized` maps onto the existing `Action::MaximizeWindowToEdges`, which already calls
`layout.toggle_maximized` — mutter's `handle_toggle_maximized` exactly (maximized →
unmaximize, anything else → maximize; an edge-tiled window is not `maximized_vertically &&
horizontally`, so it maximizes). Only the *name* is niri's, and it reaches into niri-ipc, so
renaming it is left for whenever that surface is revisited rather than done here.

`org.gnome.mutter.keybindings` — `toggle-tiled-left`, `toggle-tiled-right`.

`org.gnome.mutter.wayland.keybindings` — `restore-shortcuts`, `switch-to-session-1..12`. Both
are `META_KEY_BINDING_NON_MASKABLE`: they resolve *through* a client's shortcuts inhibitor,
because they are the keys that get you back out of one. `GnomeKeyAction::is_non_maskable`
derives that from the action rather than storing it per keybinding, so it cannot drift from
what the action does. `restore-shortcuts` maps to `Action::RestoreKeyboardShortcuts`, which
only ever restores — mutter's handler bails when nothing is inhibiting
(`meta_wayland_compositor_restore_shortcuts`, `meta-wayland.c:1155`), and a *toggle* on a
recovery key could arm the very thing it exists to undo.

`org.gnome.mutter` — `overlay-key` (the Super tap, with mutter's arm/disarm state machine).

`org.gnome.shell.keybindings` — `switch-to-application-1..9`,
`open-new-window-application-1..9`, `toggle-overview`, `toggle-application-view`,
`toggle-message-tray`, `toggle-quick-settings`, `show-screenshot-ui`, `screenshot`,
`screenshot-window`, `show-screen-recording-ui`,
`screen-brightness-{up,down,cycle}[-monitor]`.

`switch-to-application-N` indexes the **resolved** favourites — `AppFavorites.getFavorites()`,
which is what the dash draws — not the raw `favorite-apps` strv. A stored id whose app isn't
installed drops out of the resolved list, and `<Super>N` has to keep meaning "the Nth tile".

`toggle-message-tray` and `toggle-quick-settings` are registered by gnome-shell with
`ShellActionMode.POPUP` (`windowManager.js:747-760`), so they keep resolving while a panel menu
holds its modal grab. Our popover grab swallowed every key but Escape and the VT chord, which
made the menus one-way — the key that opened one could not close it. `allowed_during_popup`
is the allowlist that fixes it, in the same shape as `allowed_when_locked`.

### The fallback defaults are the session's defaults

The compiled-in defaults in `adopted_*_keybindings()` only apply where the schema isn't
installed — but that includes **the whole test corpus**, since `Fixture` runs on
`Config::default()` and no schema. So an *accidental* invention there hides on a GNOME box
while silently defining what the conformance tests assert. A *deliberate* difference is fine,
but it must be labelled at the key and carried into the `.gschema.override` we install, so the
table, the override and the seat all agree.

Three divergences from upstream have been found:

- `switch-to-workspace-1..4` claimed `<Super>1..4`. Accidental, and wrong: `<Super>N` is
  `switch-to-application-N` (`gnome-shell/data/org.gnome.shell.gschema.xml.in:193+`).
  `switch-to-workspace-1` is `<Super>Home`, 2..12 are `[]`. **Fixed.**
- `show-screenshot-ui` and `screenshot` had their accelerators swapped: we claimed
  `<Shift>Print` opens the picker and plain `Print` writes a file, where GNOME does the
  reverse (`org.gnome.shell.gschema.xml.in`). Accidental. **Fixed.**
- `switch-windows = ['<Alt>Tab']` with `switch-applications = ['<Super>Tab']`, where upstream
  leaves `switch-windows` empty and gives Alt+Tab to `switch-applications`. Arrived by
  accident, **kept deliberately**: Alt+Tab is our window switcher, Super+Tab our application
  switcher. Now labelled as ours at the key; to be backed by the override once S6 lands the
  schema-dir plumbing.

## Our own schema

`org.gnome.shell-rs.keybindings`, path `/org/gnome/shell-rs/keybindings/`, source in
`resources/org.gnome.shell-rs.keybindings.gschema.xml` — the scrolling-window-manager actions
GNOME has no key for: column focus and movement, monitor focus, consume/expel, the preset
width and height cycles, centring, tabbed display, floating, and the session keys.

`GnomeKeybinding.action` is a `KeybindingAction { Gnome(GnomeKeyAction), Niri(Action) }` rather
than a `GnomeKeyAction` grown into a mirror of niri's ~200 actions. `read_keybinding_table` is
generic over `Into<KeybindingAction>`, so the GNOME tables were not touched by the change.

**No arrow keys.** `<Super>` plus an arrow is GNOME's four times over — `toggle-tiled-left`,
`toggle-tiled-right`, `maximize`, `unmaximize` — and `<Super><Shift>` plus an arrow is
`move-to-monitor-*`. So this half of the model is hjkl. Likewise `<Super>v` and `<Super>m` are
the message tray, which is why floating is `<Super>g`.

Three unit tests keep it honest, each covering a failure that is otherwise silent:

- `niri_accels_do_not_collide_with_gnome` — no accelerator of ours may take a chord we adopt
  from GNOME. This is the fork tenet made mechanical: GNOME wins, so ours must not ask. A
  collision would leave a settings key that changes nothing, which is worse than one that
  isn't there.
- `our_schema_matches_the_table` — the XML and `adopted_niri_keybindings()` are two hand-written
  copies of one list (the table runs where the schema isn't installed, the XML is what
  `gsettings` and Settings see). Drift shows up as a key nobody reads.
- `our_defaults_all_parse` — `parse_accels` is deliberately forgiving, mirroring mutter's
  `update_binding`, so a typo in a keysym name yields a binding with no accelerators rather than
  an error. Our own defaults are not user input.

### The scroll bindings

mutter's accelerators are keys only, so the trigger names (`WheelScrollDown`, `MouseMiddle`,
`TabletStylusButton1`, …) are an extension of ours, spelled by `Trigger::from_name` — one
parser shared with the KDL key syntax, so `<Super>WheelScrollDown` in the schema and
`Mod+WheelScrollDown` in a config file cannot drift apart.

They are named for the trigger (`scroll-focus-workspace-down`) rather than the action, because
several bind an action that already has a key of its own and a settings key cannot appear twice.
The workspace ones carry a compiled-in cooldown, which is *not* a settings key: a wheel detent
is not a keypress, and a flick of a free-spinning wheel would otherwise cross several
workspaces.

**The fast-path trap.** The pointer, wheel, touchpad and stylus handlers gate on
`mods_with_*_binds` — sets of the modifier combinations any binding uses — before doing any
lookup, so an unmodified scroll costs nothing. They were built from `config.binds` alone, which
means a binding in the settings model was not merely slow to find but *never found at all*.
`Niri::refresh_mods_with_binds` now rebuilds them from both sources, and runs wherever either
changes: config reload, the initial settings read, and every live settings change. Construction
builds them from the compiled-in model, which is what the headless tests run on.

### Packaging

The schema installs to a **private** directory, never `/usr/share/glib-2.0/schemas`:
`%{_datadir}/gnome-shell-rs/glib-2.0/schemas` from the RPM,
`/usr/local/share/gnome-shell-rs/glib-2.0/schemas` from `scripts/install-test-session.sh`. The
session finds it through `GSETTINGS_SCHEMA_DIR` (`resources/niri.service`, and the systemd
drop-in the test-session script writes), which is searched ahead of the system dir and is
inherited by everything the session launches — so gnome-control-center sees those keys too.

To read or write them from a shell, set the same variable:

```
GSETTINGS_SCHEMA_DIR=/usr/local/share/gnome-shell-rs/glib-2.0/schemas \
    gsettings list-recursively org.gnome.shell-rs.keybindings
```

## Schemas: who ships what, and what a replacement costs

We ship our own schema and **no override yet** — everything GNOME names is read from what the
system has installed, falling back to the tables above.

| Schema | Shipped by | Survives replacing mutter + gnome-shell? |
|---|---|---|
| `org.gnome.desktop.wm.keybindings`, `.wm.preferences`, `.interface`, `.input-sources`, `.a11y.*` | gsettings-desktop-schemas | **yes** — shared with GTK apps, gsd, control-center |
| `org.gnome.mutter*` | mutter | no |
| `org.gnome.shell*` | gnome-shell | no |

Three glib behaviours, all verified with `glib-compile-schemas`, that constrain the options:

1. **An override cannot stand alone.** In a directory with no matching schema,
   `glib-compile-schemas` prints *"No schema files found: doing nothing"* and produces nothing.
   An override only tunes a schema installed **in the same directory** — it is a tuning
   mechanism, never a replacement one.
2. **Duplicate schema ids in one directory silently lose.** Two files declaring the same id
   give `<schema id='…'> already specified. This entire file has been ignored.` — and
   `glib-compile-schemas` **still exits 0**. So our copies of `org.gnome.shell.*` /
   `org.gnome.mutter.*` must never go into `/usr/share/glib-2.0/schemas` on a box that also has
   the real ones, which is exactly this dev VM (real GNOME is the control session).
3. **A private dir on `GSETTINGS_SCHEMA_DIR` wins over the system dir.** That is the safe home
   for anything of ours, and it lets us co-exist with a real GNOME install.

If the mutter/shell schemas are absent altogether, the compositor still runs — on the fallback
tables — but there is no *editable* keybinding config at all: `gsettings set` fails,
dconf-editor shows nothing, and gnome-control-center's Keyboard panel cannot enumerate
shortcuts. That is the cost that S10 buys back.

## Deferred, with reasons

No backing implementation; nearly all default to `[]`, so deferring costs nothing.

| Key | Why |
|---|---|
| `minimize` (`<Super>h`) | no minimized-window state in layout |
| `begin-move` / `begin-resize` | no keyboard interactive-grab machinery |
| `activate-window-menu` | no window menu UI |
| `show-desktop` | not implemented |
| `raise` / `lower` / `raise-or-lower`, `always-on-top` / `toggle-above` | no stacking model |
| `maximize-vertically` / `-horizontally`, `move-to-corner-*`, `move-to-side-*`, `move-to-center` | no half-max / floating-placement infra |
| `toggle-on-all-workspaces` | no sticky windows |
| `switch-panels` / `cycle-panels` | no keyboard navigation between shell surfaces |
| `switch-monitor` / `rotate-monitor` | no display-config switcher OSD |
| `cancel-input-capture` | no input-capture protocol support |
| `set-spew-mark` | mutter debug hook; never |
| `focus-active-notification` (`<Super>n`) | banner exists, no keyboard-focus path — belongs to a notifications slice |
| `shift-overview-up` / `-down` | overview lacks the staged state machine |

## Slices

| # | Slice | Status |
|---|---|---|
| S1 | Fix the invented fallback defaults for `switch-to-workspace-N` | **done** |
| S2 | `non_maskable` flag + `restore-shortcuts` + `switch-to-session-N` | **done** |
| S3 | Shell UI toggles: overview, app grid, quick settings, message tray | **done** |
| S4 | wm/mutter window + monitor keys: `toggle-maximized`, `move-to-monitor-*`, `*-workspace-last`, `switch-input-source` | **done** |
| S5 | `switch-to-application-N` + `open-new-window-application-N` | **done** |
| S6 | Our own schema for the niri actions, plus its packaging | **done** |
| S7 | Route the non-keyboard triggers (mouse, wheel, touchpad, tablet) through the model | **done** |
| S8 | Prune and then delete the KDL `binds{}` | |
| S9 | Re-source the hotkey overlay from the settings model | **done** |
| S10 | Vendor `org.gnome.shell.*` / `org.gnome.mutter.*` into our private schema dir, plus the `.gschema.override` for our differing defaults | after S6 |

### Ordering hazards

- **Jailing.** Handled in S2: `find_gnome_bind` used to hardcode `allow_inhibiting: true`, so
  adopting the two NON_MASKABLE keys before that would have handed an inhibiting client the
  power to lock the user out of both un-inhibiting and VT switching. Both new tests were run
  against a disabled flag to confirm they fail without it. The hardcoded `XF86Switch_VT` path
  stays regardless — on a normal keymap `<Ctrl><Alt>Fn` arrives as `XF86Switch_VT_n` and never
  reaches the settings at all, which is precisely why it is the hatch that cannot be
  misconfigured.
- **Escape-hatch loss.** S8 before S2 removes the only never-inhibited un-inhibit key
  (`Mod+Escape`). S8 before seat-verifying that gsd delivers `logout` (`<Ctrl><Alt>Delete`)
  and `screensaver` (`<Super>l`) removes quit and lock outright.
- **Dead number row.** S8 before S5 leaves `<Super>1..9` doing nothing, since GNOME's
  `switch-to-workspace-N` defaults are empty.
- **Empty cheat sheet.** S8 before S9 empties the hotkey overlay: it read `config.binds` and
  nothing else, so every GNOME-sourced binding was already invisible to it and deleting the KDL
  defaults would have left it blank. Closed by S9, which reads both sources — a config bind
  first (it is the user's override), the settings model otherwise. `hide-not-bound` now asks
  "bound by *either* source", and `hide_not_bound_still_sees_the_settings_model` pins that an
  empty model is what actually empties it.
- **Silent fallback drift.** Closed by S6, which shipped the packaging in the same slice: code
  without it would leave the session on compiled-in defaults while the user believed they were
  editing settings.

### S10 vendoring

Depends on S6, which has to build the plumbing anyway: a private schema dir,
`glib-compile-schemas` at install time, and `GSETTINGS_SCHEMA_DIR` in the session environment.
Once that exists, vendoring is incremental — drop our copies of the mutter and gnome-shell
schemas beside our own, and the `.gschema.override` carrying every default where we knowingly
differ from upstream (starting with Alt+Tab). Never into the shared system dir: see hazard 2
above.

### S6 schema design

Id `org.gnome.shell-rs.keybindings`, path `/org/gnome/shell-rs/keybindings/`, every key type
`as` — mutter-style, so `gsettings`, dconf-editor and the existing parse/watch machinery work
unchanged. Key names are the kebab-case niri action names.

`GnomeKeybinding.action` becomes `enum KeybindingAction { Gnome(GnomeKeyAction), Niri(Action) }`
rather than growing `GnomeKeyAction` into a mirror of niri's ~200 actions; `action_for_gnome`
maps `Niri(a) => Some(a)` and `SwitcherGrab::resolves` only inspects the `Gnome` arm. The table
is curated by hand, not reflected off the `Action` enum: argument-carrying actions can't be
encoded in an `as` key, the XML has to enumerate keys anyway, and the curation *is* the
filtering the fork tenet asks for.

`spawn` / `spawn-sh` get no keys at all. GNOME's way is Settings → Keyboard → Custom Shortcuts,
which reaches us through the `GrabAccelerator` path we already serve — that is the migration
story for `Mod+T` and `Mod+D`.
