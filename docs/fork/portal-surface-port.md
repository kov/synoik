# The portal surface — screenshots and screen sharing

Status: **2026-08-02.** Slice 1 landed. Slice 2 is *partly* landed — `ScreenshotArea` and
honouring `filename` are in; `ScreenshotWindow` and `FlashArea` are not. Slice 3 (`SelectArea`) has
not been started. Nothing here is seat-validated yet.

`xdg-desktop-portal-gnome` is the path every Flatpak app, every browser screen-share and every
in-app "take a screenshot" goes through. It is not optional for a daily-driven session, and it is
the largest remaining gap between "looks finished" and "works".

Scope comes from `docs/fork/dbus-surface-audit.md` §2, re-checked against the *installed*
`/usr/libexec/xdg-desktop-portal-gnome` rather than the reference source — what the shipped binary
calls is what we owe. Confirmed call set (a `strings` scan for exact member names):

```
CreateSession  Start  Stop
ScreenshotArea  SelectArea  FlashArea
GetRunningApplications  ScreenSize
RecordMonitor  RecordWindow  RecordArea  RecordVirtual
EnableClipboard
```

## What we serve today

| interface | ours | missing |
| --- | --- | --- |
| `org.gnome.Shell.Screenshot` | `Screenshot`, `PickColor` | `ScreenshotArea`, `SelectArea`, `FlashArea`, `ScreenshotWindow`, `InteractiveScreenshot` |
| `org.gnome.Shell.Introspect` | `GetWindows`, `WindowsChanged` | `GetRunningApplications`, `ScreenSize`, `AnimationsEnabled`, `RunningApplicationsChanged`, `version` |
| `org.gnome.Mutter.ScreenCast.Session` | `Start`, `Stop`, `RecordMonitor`, `RecordWindow`, `RecordArea`, `Closed` | `RecordVirtual` |
| `org.gnome.Mutter.ScreenCast.Stream` | `PipeWireStreamAdded`, `Parameters` | `Start`, `Stop` |
| `org.gnome.Mutter.RemoteDesktop` | — | the whole name |

## Two things found while scoping, both of which change slice 1

**Our `Introspect` has no sender check, and GNOME's does.** `introspect.js:7-11` defines an
allowlist of exactly two senders:

```js
const APP_ALLOWLIST = [
    'org.freedesktop.impl.portal.desktop.gtk',
    'org.freedesktop.impl.portal.desktop.gnome',
];
```

and both `GetRunningApplicationsAsync` (`:124-133`) and `GetWindowsAsync` (`:135-182`) begin with
`await this._senderChecker.checkInvocation(invocation)`. Ours (`src/dbus/gnome_shell_introspect.rs`)
answers anyone on the session bus. **That is a privacy leak we inherited**: any application can
enumerate every window's title and app-id without asking the user. It has to be fixed *before*
`GetRunningApplications` is added, not after, because that method widens the same hole.

**Our `WindowProperties` carries two fields; GNOME sends up to nine.** `introspect.js:163-181`
sends `app-id`, `client-type`, `is-hidden`, `has-focus`, `width`, `height` always, plus `title`,
`wm-class` and `sandboxed-app-id` when available. The portal's window chooser reads them. The
comment on our struct — *"Shell does internal tracking to match Wayland app IDs to desktop files.
We don't do that yet, which is the reason why xdg-desktop-portal-gnome's window list is missing
icons"* — is an **expired premise**: the app-lifecycle port supplies exactly that model
(`app_system.rs:584-606`). See the sweep in `dbus-surface-audit.md` §5; this is a fourth instance
of the same bug class.

## Slices

Ordered by what unblocks the portal soonest, not by interface.

1. **Introspect completion.** — **DONE** (`f7b12a1c`). Allowlist, `GetRunningApplications`, the
   full window field set, `ScreenSize`, `AnimationsEnabled`, `version`, both change signals off the
   `sync_running_apps` seam. Pinned by `the_portal_window_list_carries_what_its_chooser_reads`.
   Original scope follows. The sender allowlist first, then `GetRunningApplications` off
   `AppSystem::running` + the focused app, the full `WindowProperties` field set, `ScreenSize`,
   `AnimationsEnabled`, `version`, and the two change signals. Unblocks the portal's app/window
   chooser, and closes the leak.
2. **Screenshot, non-interactive.** — **PART DONE** (`684626ae`): `ScreenshotArea` and the
   `filename` argument. **Left: `ScreenshotWindow` and `FlashArea`.** `FlashArea` is the one with
   real work behind it — it is a white rectangle that fades, so it needs an overlay element and an
   animation, not just a D-Bus method. Original scope follows. `ScreenshotArea` (a crop of the existing capture),
   `ScreenshotWindow` (`Niri::screenshot_window` already exists), `FlashArea`, and **honouring the
   `filename` argument**, which `Screenshot` currently ignores in favour of its own path.
3. **`SelectArea`.** Interactive: opens the picker and returns the rect. `ui::screenshot_ui` has
   the selection machinery but no "select only, hand back coordinates" mode.
4. **ScreenCast completion.** `Stream.Start`/`Stop`, then `RecordVirtual`.
5. **`org.gnome.Mutter.RemoteDesktop`.** The whole name — screen sharing *with input*, and
   `EnableClipboard`. Largest, and the only one that needs new input-injection plumbing.
6. **`InteractiveScreenshot`.** The shell's own dialog driven over D-Bus. Last: nothing in the
   portal path needs it.

## Open questions

- Whether the allowlist should be exactly GNOME's two names or configurable. GNOME hardcodes it;
  a fork that adds an escape hatch is adding a way to reopen the leak.
- `ScreenSize` on a multi-output session is `global.screen_width/height`, i.e. the union bounding
  box, not a per-output size. Confirm against our output model before implementing.

## Open question raised by slice 1

Under `xdp-gnome-screencast` the window list carries a synthetic **"niri Dynamic Cast Target"**
entry with app id `rs.bxt.niri.desktop`. It is a niri capability rather than a niri *way of doing
a GNOME thing*, so the tenet says keep it — but it shows up in the portal's chooser as a window
called "niri", which is a branding leak in a shell that is meant to be GNOME. Decide whether to
rename it, hide it from `GetWindows`, or leave it.
