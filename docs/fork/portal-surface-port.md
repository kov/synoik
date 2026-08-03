# The portal surface — screenshots and screen sharing

Status: **2026-08-02.** Slices 1-3 landed; 4-6 not started. Not yet seat-validated — that is the
next step, and is what slices 1-3 were sequenced to make possible.

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
2. **Screenshot, non-interactive.** — **DONE** (`684626ae`, `8e583d9d`). `ScreenshotArea` as a
   crop of the existing capture, `ScreenshotWindow` on the focused window, `FlashArea` as
   `ui::flashspot`, and the `filename` argument honoured. `ScreenshotWindow` deliberately bypasses
   `save_screenshot`, which also replaces the clipboard and raises a notification — those belong to
   a keypress, not to a portal call the user never sees.
3. **`SelectArea`.** — **DONE**. The picker opens with `Niri::select_area_reply` armed; confirming
   answers with the rect in global logical coordinates and saves nothing, and **every** close
   answers, because a D-Bus caller that is not answered hangs until its timeout rather than
   failing. That is why all the `ScreenshotUi::close` call sites now route through
   `Niri::close_screenshot_ui`, and why it answers unconditionally rather than only when the picker
   was open. Pinned by `select_area_always_answers_its_caller`.
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

## The dynamic-cast pseudo-window — kept, relabelled

Under `xdp-gnome-screencast` the window list carries a synthetic entry that is not a window. Picking
it in the portal's share dialog starts a cast with **no target yet** (`screencasting/mod.rs:422-433`
parks it in `pending_dynamic_casts`); the target is then chosen and *changed live* with
`SetDynamicCastWindow` / `SetDynamicCastWindowById` / `SetDynamicCastMonitor` /
`ClearDynamicCastTarget` (`input/mod.rs:3622-3650`), without reopening the share dialog. Share once
at the start of a call, then flip what is shared with a keybind.

GNOME has no equivalent — mutter binds a stream to its target at `RecordWindow`/`RecordMonitor`
time. So by the tenet this is an **additional capability**, kept.

Decided 2026-08-02: the visible label is **"Dynamic Target"** — it says what the entry does rather
than who built it, since it sits beside the user's real windows in a shell presenting itself as
GNOME. The **app id is still `rs.bxt.niri.desktop`**, which resolves to nothing and so draws with no
icon; that waits on the wider naming decision (neither "niri" nor "gnome-shell-rs" is the intended
product name). Fix the app id, the desktop file and the icon together when that lands.

## Note for the seat validation

The picker cannot open in the headless corpus — it has to freeze the screen through the renderer
first — so the conformance test covers the *refusal* and *dismissal* exits only. The happy path,
where a real selection comes back as a rectangle, has no automated cover and is exactly what the
seat run has to exercise: open a browser's screen-share or a Flatpak screenshot and check the
returned area matches what was dragged.
