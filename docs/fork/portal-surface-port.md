# The portal surface — screenshots and screen sharing

Status: **2026-08-03.** Slices 1-3 and 6 landed; slice 4 is *partly* landed (`Stream.Start`/`Stop`
in, `RecordVirtual` not); slice 5 not started. Screenshots are seat-validated — wayshot captures
through the portal. Screencasting is seat-validated too: casts start, and the frames we hand over
are current (see the OBS section below — the one remaining complaint there is a client-side bug
that reproduces under real mutter).

**`f7a629da`: we advertised dmabuf-only formats, so any consumer without dmabuf import could not
start** — seat-validated, OBS and gnome-software now reach `Streaming` where they used to fail with
`res=-32 (Broken pipe): no more input formats`.

Cause, read on both sides. Mutter offers each format **twice** — `build_format_params`
(`meta-screen-cast-stream-src.c:1576-1592`) loops all formats with `with_modifiers=TRUE`, then
again with `FALSE`, and the second pass calls `push_format_object(..., NULL, 0, ...)`
(`:1543-1553`), emitting a format param with **no modifier property at all**. That is the
SHM/MemPtr fallback. `push_format_object` only attaches the modifier when `n_modifiers > 0`
(`:297`). We offered one param per format and *always* attached a `MANDATORY` modifier, and
advertised `DataType::DmaBuf` alone with an `assert!` rejecting anything else. A consumer that
cannot import our modifiers was left with nothing to accept.

The fix needed the memfd path as well as the wider offer, because widening alone would have turned
a negotiation failure into a crash — `dequeue_buffer_and_render` looks the block's fd up in
`inner.dmabufs` and `add_buffer` asserted DmaBuf. What landed:

- `make_video_params` takes `with_modifier`; `false` omits the property entirely and returns
  `None` when a dmabuf offer would have no modifiers to name (mutter's `:1526-1531` early return).
- The offer is built in mutter's order — all formats with modifiers, then all without.
- A `Sink` enum records which world the stream settled on, so the block count, the data type, the
  allocation and the render path cannot disagree. `SPA_PARAM_BUFFERS_dataType` follows it.
- `attach_memfd` allocates/seals/maps the storage exactly as mutter does (`:2318-2358`).
- Rendering goes through `render_and_copy_to_memory`, extracted from `render_to_shm` so the
  Wayland shm path and the PipeWire path share one readback.

Memory frames are queued directly rather than through `queue_after_sync`: the readback has
already synchronized with the GPU, so there is no fence left to wait on.

Still divergent, deliberately: mutter offers `VideoSize` as a `CHOICE_RANGE` where we send a fixed
rectangle. Worth matching, but it is not what broke negotiation.

Worth doing on its own merits, and **not** a bug fix: `render_to_dmabuf`
(`src/render_helpers/mod.rs`) hardcodes age `0` into `render_output_with_states`, and Smithay treats
age 0 as "damage everything" (`damage/mod.rs:747`), so every cast frame that renders at all is a
full repaint. Threading the real per-buffer age *in* — and collapsing the two `damage_output` calls
per frame down to one — would make partial damage actually work. A commit that made only the *skip*
decision use each buffer's real age was written as a fix for the frozen-frame report, changed
nothing, and was dropped from history.

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
| `org.gnome.Shell.Screenshot` | all of it | — |
| `org.gnome.Shell.Introspect` | all of it | — |
| `org.gnome.Mutter.ScreenCast.Session` | `Start`, `Stop`, `RecordMonitor`, `RecordWindow`, `RecordArea`, `Closed` | `RecordVirtual` |
| `org.gnome.Mutter.ScreenCast.Stream` | all of it | — |
| `org.gnome.Mutter.RemoteDesktop` | — | the whole name |

The rows above were the state at scoping; slices 1, 2, 3 and 6 have since closed the first two and
the stream row. `RecordVirtual` and `RemoteDesktop` are the remainder, both scoped as their own
work below.

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

## The OBS frozen-frame hunt, and why it ended outside this repo

A monitor cast delivered one frame to OBS and then appeared to freeze, with pieces of old frames
overlapping. It cost several days. **It is not ours: it reproduces identically under real GNOME
Shell / mutter 50.3 on this same machine.** Log into `gnome.desktop` and try the same capture
before spending anything on a client that misbehaves against us — that control ran in one login and
would have ended the hunt on day one.

The mechanism is client-side. OBS logs `Cannot query the number of formats` just before it creates
its stream: its DMA-BUF format query fails on `Mesa zink -> Venus` (GL is routed through zink
system-wide by `/etc/environment.d/90-limina-zink.conf`). With no dmabuf formats advertised,
negotiation is forced onto OBS's shm branch, which is barely exercised anywhere else because every
other stack hands it dmabufs.

Established about our side, so it need not be re-derived:

- The producer is correct. A digest of the bytes taken *at the instant of the handover* (TRACE, in
  `dequeue_buffer_and_render`) tracks screen content exactly — long runs while the desktop is
  settled, distinct frames through every animation.
- Delivery is correct. A second consumer (host `gst-launch` `pipewiresrc`) on the same node and the
  same link receives the **live** desktop while OBS sits frozen — caught in a single captured frame
  showing the current clock with OBS's stale preview inside it.
- Our memfd allocation matches mutter field-for-field (`meta-screen-cast-stream-src.c:2318-2358`).

Two instrument traps worth keeping: dumping the buffer fds from `/proc` samples them *now*, not at
queue time, so it cannot tell "every queued frame was fresh" from "the buffers change over time" —
that weak inference stood for a day. And `gst-launch -q … | tail` hides a failed pipeline; a
"200 buffers in 0.02s" reading was an error exit, not a rate.

Kept from the hunt: `87e89fa1` calls `pw_stream_trigger_process()` after queueing when
`pw_stream_is_driving()`. It did not fix this symptom, but a `DRIVER` stream owes the graph that
call (`pipewire/stream.h`, and mutter does it at `meta-screen-cast-stream-src.c:995-1008`).
`986c829d` logs the driving state on entry to `Streaming` — **that line has never been read on the
seat**, so whether `is_driving()` is true here is still unknown.

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
