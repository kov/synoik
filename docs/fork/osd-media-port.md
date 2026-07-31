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

### C — Brightness OSD (internal) ✅
Closes the gap left by Q4. Trigger from the brightness `_sync` equivalent in `src/brightness.rs` so
both key steps and QS slider drags show it, faithfully per `brightnessManager.js:227-239`: the
monitor-scale branch shows only the monitors that changed, the global branch shows all. Icon
`display-brightness-symbolic`, no label, per-monitor `{level}` — **no `max_level`**, so max is 1.0
(`brightnessManager.js:264-276`).

Landed as a return value, not a callback: `_sync` reaches out to `Main.osdWindowManager` directly,
but ours returns a `BrightnessUpdate { writes, osd }` so the compositor stays the only thing that
touches either the device or the screen — the same shape the hardware writes already had. The
`showOSD` flag is GNOME's own `_sync({showOSD})` parameter, false only for `_monitorsChanged`
(`:181`). A pass that moves no scale — idle dimming, an auto-brightness target, our own write
echoing back — asks for no OSD at all, which is *not* `hideAll`: anything already on screen expires
on its own deadline.

Live validation is hardware-gated (no backlight on this VM); headless coverage is
`brightness_changes_show_the_osd` in `src/tests/gnome.rs`.

### D — MPRIS model ✅ (`src/mpris.rs` + `src/dbus/mpris.rs`, no UI)
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

**Landed** `b2c9db0c`: the model in `src/mpris.rs` (ungated, so its validation is testable without
a bus) and the watcher in `src/dbus/mpris.rs`. Three shapes worth knowing before slice E builds on
it:
- This is our **first client that watches a set of names** (`ListNames` + a prefix-filtered
  `NameOwnerChanged`); every other client filters on one exact name. Each player then gets its own
  task, which is the **sole writer** for its bus name — it sends both the updates and the removal,
  so a read racing a removal cannot resurrect a player that is gone.
- The store tracks a player from the moment its name appears but only *shows* it while `CanPlay`,
  which is what GNOME's `notify::can-play` → player-added/removed pair means. `MprisStore::visible`
  is what the message list renders.
- `raise()` resolves further than the JS does: `app.activate()` on a *running* app focuses its most
  recently used window, so ours goes through the same window-activation path the app menu's "Open
  Windows" row uses, and only launches when nothing is running.

Art decoding is deliberately **not** here: the model carries the validated local path and slice E
loads it, so nothing is decoded for a card that is never drawn.

### E — Media card in the message list ✅ (art deferred)
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

**Landed** `f7d2063b` as `src/ui/media_card.rs`, plus the list plumbing in `src/ui/calendar.rs`.
What the port turned up:
- `is_empty` and `can_clear` are **different questions**. `MessageView.empty` counts messages and
  `canClear` counts the ones that *can close* (`:1521-1527`); a media card cannot. So a list holding
  only players shows neither the placeholder nor the Clear pill, and the list's own `is_empty` had
  to split in two.
- An **insensitive skip button is `reactive = false`** (`:836-838`), so a click on it passes through
  to the message — which raises the player. Ours falls through the same way rather than swallowing
  it. (Live: clicking a disabled Previous launched the resolved app, as `app.activate()` would.)
- The controls are plain `St.Button`s inside the message, not menu items, so they leave the popover
  open; only the body click closes it.

**Live-validated** 2026-07-31 on an owned headless harness with a real MPRIS player on a private bus
(a ~50-line `Gio.bus_own_name` script exporting both interfaces): the card renders above the
notification, `DesktopEntry` resolves to the installed app's name, and clicking play-pause / next
reached the player over D-Bus. The cold-icon trap bit again — the first frame after the popover
opens has no control glyphs at all; take a second shot.

**Album art: LANDED 2026-07-31.** Players publishing a local `mpris:artUrl` draw the cover.

The deferral above rested on a wrong premise, and reading the reference to its *mechanism* dissolved
most of the work. Two findings, both inverting what the SCSS looks like it says:

- **The art is not rounded.** `.media-message .message-icon { border-radius: 8px !important }`
  (`_message-list.scss:262-263`) reads like rounded art, but St paints a theme node's *background*
  rounded and nothing in St or Clutter clips a child actor's content to a rounded rect — there is no
  such call in `st-icon.c`, `st-widget.c` or `clutter-actor.c`. What the rule actually does is
  reshape the *fallback's* backdrop from `$forced_circular_radius` to 8px. So no rounded-texture
  element and no `Painter` image verb were needed at all.
- **Real art removes the plate.** `Message` toggles `.message-themed-icon` on
  `notify::is-symbolic` (`messageList.js:487-492`), so the backdrop fill and the 32px glyph size
  exist only while the fallback is up. A cover that does not fill the square shows the *card*
  behind it, not a 7% white plate. Pinned by `vulkan_draws_album_art_without_the_themed_plate`,
  which uses a 2:1 cover so the letterbox band must read as card fill — the same assertion also
  fails a stretched or cover-cropped implementation, since the art is aspect-fit
  (`CLUTTER_CONTENT_GRAVITY_RESIZE_ASPECT`, `st-texture-cache.c:1017-1019`, over a loader that has
  already scaled the longest side, `st-icon-theme.c:3354-3372`).

What the slice did add to the toolkit, since the drawing turned out to be free:

- `AppIconCache::image(path, …)` — decode an arbitrary **local image file**, async, cached and
  panic-guarded like an app icon, but with **no themed fallback**: an undecodable file returns
  `None` so the caller can draw its own (`audio-x-generic-symbolic` here). The fallback flag is in
  the cache *key*, so the two entry points can never serve each other's result for one path.
- `AppIconCache::retain_images` — the eviction hook, because this is the one open-ended key space
  either cache has: one entry per cover *played*, versus the bounded installed-app set. Driven from
  `refresh_popover_media` with the covers still on screen.
- `widget::image_element` — the path-addressed sibling of `app_icon_element`; upload slots keyed by
  path as well as owner slot, so an owner reusing a slot cannot serve the previous image.

The one non-obvious wiring: the decode is **async**, and the card bakes the fallback decision into
its texture, while the message list's cache keys are positional and revision-scoped — nothing hashes
the content. So a decode landing has to bump the list revision (`note_art_decoded`, routed from the
`IconDecoded` handler); without it the first frame's fallback stays baked in until something
unrelated moves the revision.

**Remote art: LANDED 2026-07-31.** `mpris:artUrl` is no longer `file://`-only — `http(s)` covers
are fetched, as GNOME does by handing the URI to gvfs. Notes worth keeping:

- **Zero new dependencies.** We already depend on `gio`, so the transport is `gio::File::for_uri`,
  the same call GNOME makes — which also inherits its proxy and authentication integration. The
  whole transport is one function (`fetch_remote`) so the intended own-Rust replacement is a
  single-site swap.
- **Fetching is eager, when the *player* appears** (`refresh_media_art`, off `on_mpris_msg`), not
  when the popover opens. That is what gnome-shell does — it constructs the `MediaMessage` and
  resolves its icon on player add (`js/ui/messageList.js:1780-1784`) — and lazy loading would show
  the themed fallback for a whole round trip on a slow link. Pinned by
  `album_art_is_loaded_when_the_player_appears`, which never opens the popover.
- **Its own worker.** A remote fetch can block for the full timeout, and the app-icon worker must
  never queue behind it: a hung cover server would otherwise stall the dash and app grid, looking
  exactly like a renderer problem. This is why `ImageCache` is a separate type from `AppIconCache`
  rather than another door into it.
- **Guards** (`src/image_source.rs`): a scheme whitelist (`file`/`http`/`https` — gvfs would also
  mount `admin://`, `sftp://`, `dav://`, some carrying the user's stored credentials), a URI length
  cap, no credentials in the authority, an 8 MB streamed response cap, a 15 s watchdog, and a
  refusal to fetch anything resolving to a loopback/private/link-local address.

**Known gaps, both arguing for the own-transport work rather than against this one:**

- **Redirects are gvfs's, so the address guard is best-effort.** A public URL that redirects to a
  private address is not caught — gvfs follows redirects internally with no hook to inspect them.
  Closing it needs a transport whose redirect handling we own.
- **Some CDNs reject gvfs's HTTP client.** Measured on this box: `gio cat` succeeds against
  gnome.org, raw.githubusercontent.com and picsum.photos, and gets `400 Bad Request` from
  upload.wikimedia.org. A player whose art lives behind such a host silently shows the fallback.

**Not yet live-validated**: the async path specifically. Under test there is no worker, so the load
answers inline and the fallback frame never happens — the revision bump is covered by a unit test,
not by pixels. The real remote fetch *is* covered, by an `#[ignore]`d test run by hand
(`cargo test --workspace remote_image -- --ignored`), which passed against a live HTTPS host.

### F — Panel volume scroll + headphone plug (optional polish)
Scroll on the panel volume indicator steps volume and shows the OSD (`volume.js:442-464`: skip
pointer-emulated events, honour smooth-scroll deltas); plugging headphones shows one
(`:347-357`, skipping the initial sync).

**Correction (2026-07-31): "our panel indicator has no scroll handling" was wrong** — it has shipped
since `a3e7c9d9`. A wheel tick over the status area already runs `pw.adjust_volume(±SCROLL_STEP)`
(`src/input/mod.rs:6518-6546`), so there is no input seam to grow. What is actually left splits into
three pieces of very different size:

**(1) and (2) landed 2026-07-31.** What the Fable review caught, all fixed in the same change:
the touchpad step was **6x too coarse** (mutter divides libinput pixels by `DISCRETE_SCROLL_STEP`
= 10, `meta-seat-impl.c:62,1139`, and GNOME reads that `dy` as steps — so 10 px is one step, not
60); the **natural-scroll un-inversion** was missing (mutter tags the event `CLUTTER_SCROLL_INVERTED`
and `volume.js:452-454` flips it back to physical direction, while libinput hands us the inverted
delta untagged); the `item.mapped` short-circuit was missing (with the quick-settings menu open its
slider is on screen, so GNOME shows the OSD and does **not** step, `volume.js:457`); and the OSD
fired even when the volume did not move (at 100%, `slider.step()` returns false and GNOME shows
nothing). A lock-screen/screenshot-UI guard went in too — GNOME has no reachable indicator there.

**Test gap, recorded rather than papered over:** headless has no PipeWire, so nothing pins the
scroll→OSD *wiring* — deleting the `show_volume_osd` call inside `adjust_volume_by_scroll` leaves
the suite green. The decision half is split into a pure `volume_scroll_action` and tested; closing
the rest wants a stub audio backend behind a seam, which is the same refactor the port of the
port-level model (below) will want.

1. **The OSD on our own volume change** — small. Nothing shows one for a change *we* make;
   `on_audio_status` (`src/niri.rs:3527`) only refreshes the panel and popover. `show_osd` is
   reachable from the same `&mut self`, and `audio::volume_icon` already picks the icon. Follow
   `ed26af2e` (brightness): have the audio path *return* an OSD request rather than reach into the
   OSD manager. Note the feature gating — the audio path is `pipewire`, the OSD's caller `dbus`.
2. **Fidelity of the scroll itself** — small. Ours is `AxisSource::Wheel` only, so a touchpad
   two-finger scroll does nothing where GNOME honours smooth deltas; and the whole status cluster is
   one `ROLE_QUICK_SETTINGS` rect (`src/ui/panel.rs:1158-1181`), so we scroll-adjust volume anywhere
   on it, where GNOME's handler is on the volume indicator's own actor. Per-icon hit-testing means
   building an icon-index test alongside `qs_indicator_icons`.
3. **The headphone-plug OSD** — the big one, and *not* a wiring change. Our audio model is
   **sink-level, not port-level**: `AudioStatus` is `{volume, muted}` (`src/audio.rs:26-31`) and
   `pipewire_audio.rs` has no concept of a port at all. GNOME's is port-level (gvc UIDevices —
   "Speakers"/"Headphones" on one card), the divergence already recorded at `src/audio.rs:92-93`.
   This needs new PipeWire plumbing (bind `Device` params, track `Route`/`Props` per card, publish
   an active-port change, suppress the initial sync) before there is any event to show an OSD for.

---

## Order and rationale
Done: **A → B → C → D → E** (E without album art). Left: **F**, plus the art follow-up.

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
