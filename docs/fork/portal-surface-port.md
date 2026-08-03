# The portal surface — screenshots and screen sharing

Status: **2026-08-02.** Slices 1-3 and 6 landed; slice 4 is *partly* landed (`Stream.Start`/`Stop`
in, `RecordVirtual` not); slice 5 not started. Screenshots are seat-validated — wayshot captures
through the portal.

**The slice order was wrong, and a live run found it.** Slice 6 (`InteractiveScreenshot`) was put
last on the reasoning that "nothing in the portal path needs it" — drawn from a `strings` scan that
found `ScreenshotArea`/`SelectArea`/`FlashArea` in the shipped binary. Those symbols exist, but
xdg-desktop-portal-gnome 50.0's Screenshot backend calls **`InteractiveScreenshot` first** and only
falls back to the piecemeal methods. The journal said so in one line:
`InteractiveScreenshot failed: ... Unknown method 'InteractiveScreenshot'`. A symbol being present
in a binary says nothing about which call path is taken; the lesson is to read the caller's code or
watch the bus, not to grep for names.

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
4. **ScreenCast completion.** — **PART DONE.** `Stream.Start`/`Stop` are in. `Stream.Stop` is
   deliberately *not* a wrapper around session stop: one session can carry several streams (a
   browser sharing two monitors), so tearing the session down when one stream ends would kill the
   others and close the session object out from under the caller. That is why `Niri::stop_stream`
   exists beside `stop_cast`.

   **Left: `RecordVirtual`**, and it is bigger than it looks. It asks the compositor to *create a
   virtual monitor* to record — the remote-desktop "connect to a headless screen" case. The only
   thing resembling that today is `backend/headless.rs`'s `add_output`, which is test scaffolding,
   not a runtime capability. Scope it as its own piece of work rather than as a D-Bus method.
5. **`org.gnome.Mutter.RemoteDesktop`.** The whole name — screen sharing *with input*, and
   `EnableClipboard`. Largest, and the only one that needs new input-injection plumbing.
6. **`InteractiveScreenshot`.** — **DONE**, and it should have been first. The shell's own picker
   driven over the bus. Its dismissal is `(false, "")`, *not* an error (`screenshot.js:2742-2745`) —
   the opposite convention from `SelectArea` next door, matched per-caller rather than unified.

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

## Two access-control gaps, both closed

`org.gnome.Shell.Screenshot` has its own sender allowlist in GNOME (`screenshot.js:2489-2492`) and
we had none, so any application on the session bus could capture the screen to a path of its
choosing. It is a *different* list from `Introspect`'s — `org.gnome.SettingsDaemon.MediaKeys` (which
owns Print Screen) and `org.freedesktop.impl.portal.desktop.gnome`, with no GTK portal. The check
itself is now shared (`dbus::check_sender`); only the list belongs to each interface.

Consequence worth knowing: **`gdbus call` against these interfaces from a terminal is now refused**,
by design. Test through the portal, or from an allowlisted peer.

## Known gaps in cover

- The **save/close race on the confirm path** has no headless test. `save_screenshot` takes the
  `InteractiveScreenshot` reply with it precisely so the close that follows cannot answer the caller
  as a dismissal first — get that wrong and *every* interactive screenshot reports cancelled. It
  needs a real capture to reach, and the corpus has no renderer.
- The **happy path of `SelectArea`** — a real drag coming back as the right rectangle — likewise.
- **`stop_stream` has no test at all.** A `Cast` owns a live PipeWire stream and a `PendingCast`
  holds a `SignalEmitter`, so neither can be built without a bus and a running PipeWire; a test that
  hand-rolled fakes for them would not fail for the mistakes that matter. Exercise it by sharing two
  monitors in one session and stopping one.
- `version` was declared `i` where the XML says `u`. xdg-desktop-portal-gnome logged
  `Received property version with type i does not match expected type u` at startup and its
  Introspect proxy was no use afterwards; "Could not get window list" downstream is the likely
  consequence. Re-check that line on the next seat run before assuming it is gone.
