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
`switch-applications(-backward)`.

`org.gnome.mutter.keybindings` — `toggle-tiled-left`, `toggle-tiled-right`.
`org.gnome.mutter` — `overlay-key` (the Super tap, with mutter's arm/disarm state machine).

`org.gnome.shell.keybindings` — `show-screenshot-ui`, `screenshot`, `screenshot-window`,
`show-screen-recording-ui`, `screen-brightness-{up,down,cycle}[-monitor]`.

### The fallback defaults are the schema's, verbatim

The compiled-in defaults in `adopted_*_keybindings()` only apply where the schema isn't
installed — but that includes **the whole test corpus**, since `Fixture` runs on
`Config::default()` and no schema. Inventing a default there is a divergence that hides on a
GNOME box and silently defines what the conformance tests assert.

Two such inventions have been found:

- `switch-to-workspace-1..4` claimed `<Super>1..4`. Wrong: `<Super>N` is
  `switch-to-application-N` (`gnome-shell/data/org.gnome.shell.gschema.xml.in:193+`).
  `switch-to-workspace-1` is `<Super>Home`, 2..12 are `[]`. **Fixed.**
- `switch-windows` claims `<Alt>Tab` and `switch-applications` only `<Super>Tab`. The schema
  gives `switch-windows = []` and `switch-applications = ['<Super>Tab','<Alt>Tab']`, i.e. on a
  real GNOME session Alt+Tab is the *application* switcher. **Open** — see below.

## Open questions

**Alt+Tab.** On the seat, where the schema is installed, `<Alt>Tab` already resolves as
`switch-applications`; only the fallback table (and therefore the test corpus) says otherwise.
So the tests currently pin a behavior the live session does not have. Correcting the fallback
is the tenet-correct move, but it re-points a large part of the alt-tab corpus at the app
switcher and touches a completed, seat-validated port — so it is called out rather than done
in passing.

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
| S2 | `non_maskable` flag + `restore-shortcuts` + `switch-to-session-N` | |
| S3 | Shell UI toggles: overview, app grid, quick settings, message tray | |
| S4 | wm/mutter window + monitor keys: `toggle-maximized`, `move-to-monitor-*`, `*-workspace-last`, `switch-input-source` | |
| S5 | `switch-to-application-N` + `open-new-window-application-N`, on a real focus-or-launch path | |
| S6 | Our own schema for the niri actions, plus its packaging | |
| S7 | Route the non-keyboard triggers (mouse, wheel, touchpad, tablet) through the model | |
| S8 | Prune and then delete the KDL `binds{}` | |
| S9 | Re-source the hotkey overlay from the settings model | |

### Ordering hazards

- **Jailing.** `restore-shortcuts` / `switch-to-session-N` are `META_KEY_BINDING_NON_MASKABLE`
  in mutter, but `find_gnome_bind` hardcodes `allow_inhibiting: true`. Adopting them before S2
  hands an inhibiting client the power to lock the user out of both un-inhibiting and VT
  switching. The hardcoded VT path is what makes this survivable — never remove it.
- **Escape-hatch loss.** S8 before S2 removes the only never-inhibited un-inhibit key
  (`Mod+Escape`). S8 before seat-verifying that gsd delivers `logout` (`<Ctrl><Alt>Delete`)
  and `screensaver` (`<Super>l`) removes quit and lock outright.
- **Dead number row.** S8 before S5 leaves `<Super>1..9` doing nothing, since GNOME's
  `switch-to-workspace-N` defaults are empty.
- **Silent fallback drift.** S6's code without S6's packaging leaves the session on
  compiled-in defaults while the user believes they are editing settings.

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
