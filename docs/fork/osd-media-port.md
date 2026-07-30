# OSD + media milestone

**Status: nothing built.** The fork has no OSD subsystem of any kind — no volume feedback, no
brightness feedback (deliberately deferred at the end of the Q4 brightness port), no media
controls. This doc sequences all of it.

**Reference-first.** Every claim cites GNOME 50.1 in `~/Projects/gnome-shell` /
`~/Projects/mutter`, or a live measurement on this machine. Re-read the cited file before
implementing.

---

## The ground truth that shapes everything

**The OSD is mostly an *inbound contract*, not a feature we drive.** `ShowOSD` is a method
**on** `org.gnome.Shell` (`data/dbus-interfaces/org.gnome.Shell.xml:9-11`), handled at
`js/ui/shellDBus.js:121-153`, restricted to three trusted senders — `org.gnome.Settings`,
`org.gnome.SettingsDaemon.MediaKeys`, `org.freedesktop.impl.portal.desktop.gnome`
(`shellDBus.js:27-31`).

Who triggers what:

| Trigger | Handles the key | Shows the OSD |
|---|---|---|
| Volume / mute / mic-mute keys | **gsd-media-keys** (external) | calls **`ShowOSD`** on us |
| Kbd backlight, rotation lock, touchpad | gsd | calls `ShowOSD` on us |
| **Screen brightness keys** | **us** (ported in Q4) | **direct internal call** — `brightnessManager.js:264-276` |
| QS brightness slider drag | us | direct, same path (`brightnessManager.js:172-178,186,227-239`) |
| Panel volume-indicator scroll, headphone plug | shell | direct (`js/ui/status/volume.js:284-289,347-357,442-464`) |

**Measured on this machine (2026-07-30):** gsd-media-keys 50.1 is running on the gsrs seat
(`org.gnome.SettingsDaemon.MediaKeys`, PID 318791), and `busctl call … ShowOSD` against our live
`org.gnome.Shell` returns **`Unknown method 'ShowOSD'`** — we expose only the accelerator API. So
gsd's OSD requests are being dropped today, which is exactly what `RUNNING.md:160` records
("media keys work, though without OSD popups"). `strings` on `/usr/libexec/gsd-media-keys`
confirms it uses all five dict keys (`connector`, `label`, `level`, `max_level`, `icon`) and ships
`audio-volume-overamplified-symbolic` — i.e. **`max_level > 1` is real** and must render.

Two consequences:
1. **We must NOT handle volume/media keys ourselves.** gsd owns them in GNOME's model and already
   works against our compositor. Volume OSD is delivered by implementing `ShowOSD`, nothing more.
2. **Brightness is the mirror image**: we own those keys, so the OSD is an internal call with no
   D-Bus involved.

`ShowOSD` also lands on the connection that **already owns** `org.gnome.Shell`
(`src/dbus/gnome_shell.rs`), so the well-known-name placement rule is satisfied for free.

---

## Slices

### A — OSD framework + `widget::BarLevel` (the framework; do first)
New `src/ui/osd.rs`: an `OsdManager` with one slot per output. Model it on
`src/ui/notification_banner.rs` — the existing timed, auto-hiding, non-interactive overlay
(its `Hidden/Showing/Shown{deadline}/Hiding` state machine, `next_wakeup`/`advance_animations`/
`render` seams, and the calloop timer wiring in `src/niri.rs`).

Spec, cited:
- **Structure** (`js/ui/osdWindow.js:28-49`): hbox = [icon | vbox[label?, level?]]; the vbox hides
  when both are hidden (`:56-58`); `show()` refuses without an icon (`:90-92`).
- **Placement** (`osdWindow.js:20-21`): horizontally centered, **bottom** of each monitor, lifted
  `margin-bottom: 4em` (`_osd.scss`). One per monitor, rebuilt on monitors-changed (`:143-163`).
- **Style** (`_osd.scss:5-35`, `%osd_panel` `_common.scss:291-299`): pill radius
  (`$forced_circular_radius` → height/2), `$osd_bg_color`, `$osd_fg_color`, 1px border at 2% fg,
  padding 12/18, spacing 12 (icon↔vbox) and 8 (inner), icon 32px, label `%heading` 11pt bold.
  Always dark — no light variant. Resolve exact colors via `docs/fork/gnome-style-reference.md`.
- **Level bar** (`.level`, `_osd.scss:17-29`; `js/ui/barLevel.js:117-220`): min-width 160px, 6px
  tall, rounded ends, track = fg @10%, fill = fg, **overdrive** past `overdrive_start` in
  `$destructive_color` with a 3px gap (`barLevel.js:141-143,211-220`) — that is how amplified
  volume renders.
- **Timing** (`osdWindow.js:10-12,71-84`): 100 ms EASE_OUT_QUAD fade; **1500 ms** hide timeout,
  re-armed on every `show()`; the level value **eases** 100 ms when already visible, snaps when not.
- **Replace-in-place** (`:94-111`): a second OSD swaps content and re-arms the timeout with **no
  re-fade** — the fade only runs on the hidden→visible edge.
- **Per-monitor cancel** (`:114-120,173-182`): `show()` takes a per-monitor level dict; monitors
  *absent* from it are `cancel()`ed (timer killed, fade-out starts now).
- **Never interactive**: nothing in `osdWindow.js` is reactive — no hit-test, no pointer, no focus.
  It raises above all chrome on show (`:98`).
- `hideAll()` — the alt-tab switcher hides every OSD (`switcherPopup.js:178`); wire our MRU
  switcher to match.

**Toolkit-first:** the level bar is GNOME's shared `BarLevel`, so it becomes `widget::BarLevel`,
not a one-off in the OSD. The quick-settings sliders should eventually share it.

**Landed `5bb8c180`** (`src/ui/osd.rs`, `widget::BarLevel` + `Painter::bar_level`). Two things
adversarial review changed, both worth remembering: the hide deadline is armed in `show()`, not at
the fade's end — GNOME's timeout runs concurrently with the fade-in (`osdWindow.js:107-110`), so a
still-fading OSD can expire and a `show()` mid-fade must not restart the fade; and because `show()`
can set that deadline *between* frames, `Niri` re-arms the calloop wake-up against **what the timer
is armed for** (`osd_timer_at`), not against a before/after-`advance_animations` diff, which is
always equal and would let a replaced OSD hang on a damage-free desktop.

Known gap, systemic rather than this slice's: **RTL is unimplemented**. `barLevel.js:121,131-148`
mirrors the whole bar for RTL locales; nothing in `src/ui/` does RTL layout anywhere, so the bar
fills left-to-right regardless. Fixing it belongs to a toolkit-wide RTL pass, not here.

### B — `ShowOSD` on `org.gnome.Shell` ✅ `dd52b347`
Add the method to the existing interface in `src/dbus/gnome_shell.rs`, forwarding a
`ShowOsd { connector, label, level, max_level, icon }` message over the existing channel.
**This single commit lands volume, mute, mic-mute, kbd-backlight and rotation-lock OSDs**, because
gsd supplies the icon, level and stepping.

Details: all params optional; `icon` is a **serialized `GIcon`** (`shellDBus.js:140-142`) — accept
both a bare themed name and the `". GThemedIcon name1 name2 …"` form, resolving through
`IconCache`'s existing candidate-list fallback. `max_level` defaults to 1.0 (`osdWindow.js:86-88`)
and may exceed it. `connector` routes to one output, absent = all.

**Live-validated** 2026-07-30 on an owned headless harness with a real allowlisted caller (a
python process owning `org.gnome.SettingsDaemon.MediaKeys`): the pill, icon, bar, label and the red
overdrive segment past 100% all render; `busctl` (a unique name only) is refused with Access denied.
The cold-icon trap bit again — a *newly requested* icon name misses the first frame after its first
request, so always take a second shot.

**Decision — implement the sender allowlist here.** `src/dbus/gnome_shell.rs:10-14` records "no
sender allowlist" as an existing divergence for the accelerator API; `ShowOSD` is the first method
that lets a caller draw arbitrary text and icons across every monitor, so it gets the check now.
Extending it to the accelerator methods is a follow-up, not this slice.

### C — Brightness OSD (internal)
Close the gap left by Q4. Trigger from the brightness `_sync` equivalent in `src/brightness.rs` so
both key steps and QS slider drags show it, faithfully per `brightnessManager.js:227-239`: the
monitor-scale branch shows only the monitors that changed, the global branch shows all. Icon
`display-brightness-symbolic`, no label, per-monitor `{level}` — **no `max_level`**, so max is 1.0
(`brightnessManager.js:264-276`).

### D — MPRIS model (`src/mpris.rs`, no UI)
Discovery: `ListNames` + `NameOwnerChanged`, prefix `org.mpris.MediaPlayer2.`
(`js/ui/mpris.js:18,189-258`); proxy both `org.mpris.MediaPlayer2` and `…​.Player` at
`/org/mpris/MediaPlayer2` (`:34-39`). A player is exposed **only while `CanPlay`**
(`:206-207,217-223`). Consumed: `PlaybackStatus`, `CanPlay`, `CanGoNext`/`CanGoPrevious`,
`Metadata` → `xesam:title` / `xesam:artist` / `mpris:artUrl`, each **type-validated with a
fallback** (`mpris.js:129-165`); `Identity` + `DesktopEntry` resolve the source name/icon through
our `AppSystem` (`:167-177`). Methods: `PlayPause`, `Next`, `Previous` (`:73-91`); raise = activate
the app, else `Raise()` when `CanRaise` (`:93-100`).

**Untrusted-content seam** (fork rule): titles, artists and `Identity` are app-controlled strings —
the bidi visual-order trap from the notifications port applies. `mpris:artUrl` is an app-controlled
**URI that GNOME loads directly** (`messageList.js:817-820`). Ours accepts **`file://` only**
(divergence: players that publish `http(s)` art — Spotify — fall back to the generic icon),
size-capped, decoded behind a plain-data `MprisSnapshot`, never panicking on a malformed image.

### E — Media card in the message list
Where it goes, from the construction sequence: media messages are inserted at **index 0** of the
dateMenu's `MessageView` — above every notification group, whose indices are offset by the player
count (`js/ui/messageList.js:1780-1784,1826-1832`; mpris is set up before notifications,
`:1516-1518`; the view itself is `js/ui/calendar.js:814`). One card per player, newest on top, **no
most-recently-active resorting** in 50.1.

Card child order (`messageList.js:445-502,770-791`): header (source icon + name; close **hidden** —
`canClose() = false`, `:668-670`), then [album art | content(title = track title, body = artists
joined `', '`, `:825-830`) | controls: prev / play-pause / next (`:778-791`)]. Play-pause icon is
`media-playback-pause-symbolic` iff `PlaybackStatus === 'Playing'` (`:831-835`); prev/next
reactivity follows `CanGoPrevious`/`CanGoNext` (`:837-838`). Style: art radius 8px with
`audio-x-generic-symbolic` fallback at 32px (`_message-list.scss:260-269`); controls padding
`0 18px`, radius 8px (`:226-257`) — use `widget::Button` with a style variant, not bespoke paints.
Body click raises the player and closes the popover (`:799-804`).

### F — Panel volume scroll + headphone plug (optional polish)
Scroll on the panel volume indicator steps volume and shows the OSD (`volume.js:442-464`: skip
pointer-emulated events, honour smooth-scroll deltas); plugging headphones shows one
(`:347-357`, skipping the initial sync). Our panel indicator has no scroll handling today, so this
grows an input seam — schedule separately.

---

## Order and rationale
**A → B → C**, then **D → E**, with F last. A is prerequisite; **B is the cheapest real payoff and
is live-verifiable today** (audio works on this machine); C completes the brightness port; D/E are
independent and could run in parallel.

## Verifiability
- **A**: headless conformance (`src/tests/gnome.rs`) driving `OsdManager` directly — replace-in-place
  re-arms the deadline, 1500 ms expiry, per-monitor cancel, icon-required gate. **Heed
  [[headless-animation-clock-trap]]**: roundtrips clear the lazy clock; settle fades explicitly.
  Plus a Vulkan render test (pill geometry, bar fill) — our own chrome, so headless shots are
  trustworthy.
- **B**: headless via injected channel messages (the pattern the Q4 D-Bus tests use), covering
  connector routing, missing-key defaults and both GIcon serializations. **Live on the seat**:
  press the volume keys, or `busctl call … ShowOSD`.
- **C**: headless — the fixture injects a `BacklightSnapshot` directly (`src/tests/gnome.rs:4524`),
  so no fake sysfs is needed. **Live is hardware-gated** (no backlight on this VM) → record in
  [[pending-live-validation]].
- **D**: unit tests on the validation/fallback logic; integration by owning a fake
  `org.mpris.MediaPlayer2.*` name from the test. Live: real players on the seat.
- **E**: headless popover render + interaction tests against a fake player. The notifications-port
  trap applies — never screenshot between popover-open and click.

## Do NOT build
Pad OSD (tablet subsystem); `ShowMonitorLabels`/`osdMonitorLabeler` (belongs to display config);
resize popup; privacy-screen OSD (a mutter hardware signal we don't surface,
`mutter/src/core/display.c:614-625`); per-app notification policy and lock-screen visibility for
media cards (`messageList.js:806-810,841-853` — both subsystems absent); MPRIS
Seek/Shuffle/Loop/position (GNOME surfaces none of them); `http(s)`/gvfs cover art; and **any
compositor-side volume-key handling**.
