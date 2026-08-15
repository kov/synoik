# D-Bus surface audit — what a GNOME session calls that we do not answer

Status: **2026-08-01, mechanical diff.** Our `#[interface(name = …)]` blocks against the reference
XML in `~/Projects/mutter/data/dbus-interfaces/` and `~/Projects/gnome-shell/data/dbus-interfaces/`,
plus a scan of the *installed* callers (`xdg-desktop-portal-gnome`, `gsd-power`, `gsd-media-keys`)
to tell a real dependency from a theoretical one.

This is the "you only find out when the niche use case needs it" surface. It is ordered by what a
daily-driven session actually hits, not by interface size.

**Method note.** The member diff is generated, so it is only as good as its parsing: it maps our
`fn snake_case` to `PascalCase` and honours explicit `#[zbus(name = "…")]`. Every claim below that
carries a consequence was re-checked by hand against the source file — but if a row looks wrong,
re-run the diff before believing it.

---

## 1. The session cannot lock — `org.gnome.ScreenSaver` is absent

We serve `org.freedesktop.ScreenSaver`, which is only the **inhibit** half (`Inhibit`/`UnInhibit`,
what a video player calls to stop blanking). The half that *locks* is a different name and we do
not serve it in any form:

| what | who owns it in GNOME | us |
| --- | --- | --- |
| `org.gnome.Shell.ScreenShield`, object `/org/gnome/ScreenSaver`, interface `org.gnome.ScreenSaver` | gnome-shell itself | **absent** |
| the activatable `org.gnome.ScreenSaver` name | a thin gjs proxy service in front of the above (`js/dbusServices/screensaver/`) | **absent** |

Members: `Lock`, `GetActive`, `SetActive`, `GetActiveTime`, signals `ActiveChanged`, `WakeUpScreen`.

Callers are dynamic (GDBus proxies), so a `strings` scan understates them, but they include
`gsd-power` (idle lock, lock-on-suspend — its binary carries the `Lock` member name),
`xdg-screensaver`, gnome-control-center, and anything responding to logind's session `Lock` signal.

**Consequence: the screen never locks.** Not a niche case for a machine you walk away from, and
the reason it is first here.

Note the shape — the real implementation is exported on the connection owning
`org.gnome.Shell.ScreenShield`, with the activatable name as a *separate* proxy so the name
resolves when the shell is down. Getting that placement wrong is the failure mode already recorded
in the well-known-name/object-placement note: an object on the wrong connection gives
`UnknownObject` and a silent no-op.

We do have `ext-session-lock` (inherited from niri, for external lockers like swaylock). That is
niri's way, not GNOME's: in a GNOME session the shell *is* the lock screen, and nothing in the
session speaks `ext-session-lock`.

## 2. Screenshots and screen sharing through the portal

Confirmed by scanning the installed `/usr/libexec/xdg-desktop-portal-gnome`: it talks to
`org.gnome.Shell.Screenshot`, `org.gnome.Shell.Introspect`, `org.gnome.Mutter.ScreenCast`,
`org.gnome.Mutter.RemoteDesktop`, `org.gnome.Mutter.InputCapture`, `org.gnome.Mutter.DisplayConfig`
and `org.gnome.Mutter.ServiceChannel`. This is the path every Flatpak app, every browser
screen-share and every in-app "take a screenshot" goes through.

| interface | ours | missing, and wanted by the portal |
| --- | --- | --- |
| `org.gnome.Shell.Screenshot` | 2/7 | `ScreenshotArea`, `SelectArea`, `FlashArea` (portal calls all three); also `ScreenshotWindow`, `InteractiveScreenshot` |
| `org.gnome.Shell.Introspect` | 2/7 | `GetRunningApplications`, `ScreenSize` (portal calls both); also `AnimationsEnabled`, `RunningApplicationsChanged`, `version` |
| `org.gnome.Mutter.ScreenCast.Session` | 6/7 | `RecordVirtual` (portal calls it) |
| `org.gnome.Mutter.ScreenCast.Stream` | 2/4 | `Start`, `Stop` |
| `org.gnome.Mutter.RemoteDesktop` | — | **the whole name** |
| `org.gnome.Mutter.ServiceChannel` | 1/2 | `OpenWaylandConnection` (the portal uses `OpenWaylandServiceConnection`, which we do have) |

`GetRunningApplications` is worth calling out on its own: it was unimplementable when
`Introspect` was written and is not any more — the app-lifecycle port supplies exactly that model.
That is an *expired premise*, see §5.

## 3. `org.gnome.Shell` itself is 7/17

We implement the accelerator-grab quartet, `ShowOSD`, and both accelerator signals. Missing:

- `ShowMonitorLabels` / `HideMonitorLabels` — gnome-control-center's Displays panel draws the
  "1"/"2" overlays with these while you arrange monitors. Directly adjacent to the
  output-scaling/`monitors.xml` work already landed.
- `ShowApplications`, `FocusApp`, `FocusSearch` — external entry points into surfaces we now have.
- Properties `ShellVersion`, `Mode`, `OverviewActive`. `ShellVersion` is what things probe to
  identify the shell at all; `OverviewActive` is readable *and writable*.
- `ScreenTransition` (used for fades over a mode switch).
- `Eval` — deliberately not wanted. It is gated behind unsafe-mode upstream and is a remote-code
  hole; leave it unimplemented and say so, rather than leaving it looking forgotten.

## 4. Whole names we do not serve

Rough triage for a daily-driven session:

**Would be missed:**
- `org.gnome.ScreenSaver` — §1.
- `org.gnome.Mutter.RemoteDesktop` — screen sharing with input, and the portal binds it.
- `org.gnome.Shell.Extensions` — no extension host yet (STRATEGY §4), so this is honest for now,
  but Settings and the Extensions app will report the shell as broken rather than as extensionless.

**Situational:**
- `org.gnome.Mutter.ColorManager` + `DisplayConfig`'s `SetCrtcGamma` / `SetOutputCTM` — night
  light and colour profiles. `gsd-color` drives these.
- `DisplayConfig`'s `Backlight` / `SetBacklight` / `ChangeBacklight` — the newer panel-backlight
  API; `gsd-power` binds `DisplayConfig`. We serve `org.gnome.Shell.Brightness`, which is the
  other half of that story, so this needs a look rather than an assumption.
- `org.gnome.Shell.AudioDeviceSelection` — the "what did you just plug in?" dialog;
  `gsd-media-keys` binds it.
- `org.gnome.Mutter.InputCapture` — the InputCapture portal.
- `org.gnome.Shell.PortalHelper` — captive-portal login.
- `org.freedesktop.impl.portal.Access` — the shell's own portal permission dialogs.

**Probably not worth it here:**
- `org.gnome.Mutter.X11`, `Devkit`, `DebugControl`; `org.gnome.Shell.Wacom.PadOsd`;
  `org.gnome.Shell.ScreenTime`; `org.gnome.Shell.Notifications` (a proxy in front of the
  notification surfaces we already serve directly).

Also absent but *correct*: `org.gnome.Shell.CalendarServer` and `org.gnome.Shell.HotplugSniffer`
are separate binaries in GNOME that we consume rather than serve.

## 5. Expired premises — the other sweep

A recurring bug class rather than a list: a simplification that was true when written, whose
premise later changed, and whose comment kept asserting it. Three landed in one session
(2026-08-01):

- the dash click path: *"All our apps are stopped, so this is a plain `Activate`"* — false since
  the app-lifecycle port. It relaunched running apps (busy cursor) and ignored Ctrl.
- `GnomeKeyAction::SwitchApplications`: *"no app grouping — accepted divergence for now"* — false
  since the switcher port retired `mru.rs`.
- `LaunchMode`: *"activate-existing-window … is slice S6"* — S6's data landed; the branch was
  simply never written.

Still open, and **still true** (checked, not assumed):
- no action muxer, so `open_new_window` via an app's exported `app.new-window` action, and
  `shell_app_request_quit`'s `app.quit`, both take GNOME's fallback branch;
- `can_open_new_window` cannot reach its GtkApplication rung without
  `gtk_shell1.set_dbus_properties` — which is why System Monitor offers a "New Window" it does
  not have;
- window-backed `ShellApp`s are synthesized (`window:<n>`, as GNOME does), but carry no icon of
  their own — the dash draws the `application-x-executable` fallback;
- parental controls (`malcontent`) are not modelled, so that half of the app-grid filter is inert.

The cheap recurring check: grep the tree for simplification notes ("for now", "we have no",
"not modelled", slice references) and re-test each premise rather than the code. The comment is
the artefact that rots, and it rots silently — every one of the three above passed its tests.
