# Panel status — port backlog

**Purpose.** The working plan for finishing the GNOME 50.1 top panel in this fork: every panel
element and status indicator that upstream ships, its **source in the reference checkout**, what it
renders (panel icon vs. menu UI), its external dependency, and our current port status. Pick the next
slice from the prioritized backlog at the bottom.

**Reference-first (see CLAUDE.md).** Ground every port in the actual 50.1 source before implementing —
this doc is a cited cache of that reading, not a substitute. Checkout: `~/Projects/gnome-shell`
(confirmed `version: '50.1'`, `meson.build:1`; mutter API 51). Paths below are relative to that
checkout. **Style** (fonts/colors/radii) comes from `docs/fork/gnome-style-reference.md`; **child
order/placement** comes from the JS `add_child`/`_addItems` sequence cited here — not the SCSS.

**Our code.** The panel lives in `src/ui/panel.rs` (bar + roles + indicator cluster), `src/ui/popover.rs`
(popover framework + contents), `src/ui/quick_settings.rs` (QS menu), `src/ui/calendar.rs` (dateMenu
calendar). Backing models: `src/system_status.rs` (network+battery, feature `dbus`), `src/audio.rs` +
`src/pipewire_audio.rs` (volume, feature `pipewire`), `src/gnome.rs` (gsettings toggles). D-Bus
watchers follow the `src/dbus/system_status.rs` pattern (one `Connection::system()`, a task per
service, `PropertiesChanged` → calloop channel → observable model → panel setter).

**Status legend:** ✅ done · 🟡 partial · ⬜ TODO · 🚫 removed upstream (don't build).
**Dependency tags:** `[self]` compositor/gsettings only · `[pw]` PipeWire (backend already wired) ·
`[nm]` NetworkManager (watcher exists) · `[dbus:X]` a new system-bus watcher on service X ·
`[subsystem]` a whole new subsystem we don't have yet.

---

## Working method (one cycle per slice)

Every backlog item is ported as one **advise → implement → adversarial-review** cycle, one commit at
the end. The advisor/reviewer is a **Fable subagent** (model `fable`) — the advisor tool is down, so we
use a subagent and **continue the same agent** (via SendMessage) into the review so it keeps the plan
context, but the review is deliberately re-framed to *attack* the diff. See
[[panel-status-port-backlog]] for the standing memo.

**1. Advise (Fable).** Prompt Fable for an implementation plan. Every advise prompt must require:
   - **Reference-first, both axes** — cite *where it goes* (JS `add_child`/`_addItems` construction
     order) **and** *how it looks* (SCSS / `gnome-style-reference.md`), from the different files they
     live in. [[reference-first-child-order]]. No design accepted without both citations.
   - **Extension-fitness** — does the seam survive becoming an extension surface later? Flag any choice
     that a future extension would have to fight. Decide the `QuickMenuToggle` detail-view framework
     *before* the first item that needs a submenu, so indicators don't each grow a bespoke popover.
   - **Verifiability, classified per item** — say up front which bucket each behavior is in and what
     stays live-only: *functionality* → headless conformance test (`src/tests/gnome.rs`); *rendering* →
     Vulkan render test (self-skips w/o device) + the `NIRI_VK_VALIDATION=1` grep gate; *animation* →
     largely **not** honestly headless-testable (the clock trap, [[headless-animation-clock-trap]]) —
     some is live-only on gsrs. Do not fake a unit test around an animation and call it pinned.

**2. Implement, test-first.** Write the pinning test(s) from step 1's classification first, then the
   code. `cargo test --workspace` green; run `NIRI_VK_VALIDATION=1 cargo test --workspace` and grep for
   `VULKAN ERROR` (must be empty) after any renderer touch.

**3. Adversarial review (same Fable agent).** Continue the agent via SendMessage; feed it the **actual
   diff + test output + the GNOME 50.1 reference file:lines** and task it to *attack the diff* —
   re-deriving from the 50.1 source, not trusting the plan's own citations. Fix what it finds or record
   why not.

**4. Commit** the slice (one commit), then bring it to Gustavo for the live gsrs pass (he drives the
   restart on the freshly-built `target/debug/niri`).

---

## Panel skeleton (reference)

- Three `St.BoxLayout` boxes: `_leftBox` / `_centerBox` / `_rightBox` — `js/ui/panel.js:446-451`.
- Role→class registry `PANEL_ITEM_IMPLEMENTATIONS` — `js/ui/panel.js:419-428`.
- Boxes populated from the session mode — `js/ui/panel.js:634-655`; `addToStatusArea` box mapping
  `js/ui/panel.js:712-726`.
- **Default `user`-session layout** — `js/ui/sessionMode.js:96-100`:
  - left: `['activities']`
  - center: `['dateMenu']`
  - right: `['screenRecording', 'screenSharing', 'dwellClick', 'a11y', 'keyboard', 'quickSettings']`

Our model exposes exactly three roles (`ROLE_ACTIVITIES`, `ROLE_DATE_MENU`, `ROLE_QUICK_SETTINGS`) in
`panel.rs`; the five standalone right-box indicators between them and quickSettings are all unbuilt.

---

## LEFT box

### activities ✅ `[self]`
- Reference: `ActivitiesButton` — `js/ui/panel.js:198-228`; its only child is a `WorkspaceIndicators()`
  dots actor (`panel.js:211`). No text label, no app menu.
- Ours: `ROLE_ACTIVITIES`, workspace-dot indicator + overview toggle + scroll-to-switch, animated morph
  (`panel.rs`, `draw_workspace_dots`). Matches upstream. **Nothing missing.**

---

## CENTER box — dateMenu

Reference: `DateMenuButton` — `js/ui/dateMenu.js:861`. Panel button = clock label + a notifications
dot; popover = **two columns** (`calendarArea` hbox, `dateMenu.js:893-897`).

| # | Element | Renders | Reference | Dep | Status |
|---|---|---|---|---|---|
| C1 | Clock label | wall-clock text | `dateMenu.js:865,882` | `[self]` | ✅ (`panel.rs` `format_clock`) |
| C2 | **Notifications dot** (`MessagesIndicator`) | small dot on clock when unread | `dateMenu.js:743,869,883` | `[subsystem]` notifications | ✅ `44e1a1c9`+`704f02a6` (slice 4; see item 13) |
| C3 | **Message-list column** (`CalendarMessageList`) | notifications + media controls + **Clear** button + "No Notifications" placeholder | `js/ui/calendar.js:794`; placeholder `:775,804`; view `:814-821`; clear button `:823-837`; added `dateMenu.js:918-919` | `[subsystem]` `org.freedesktop.Notifications` server (we own it) | ✅ slices 3a/3b (see item 13); media controls deferred |
| C4 | **TodayButton header** | day-of-week + full date above the grid | `TodayButton` `dateMenu.js:52,70-77`; added `:942` | `[self]` | ✅ `e5b3ac6c` (header card in `calendar.rs`; click snaps to today; one-surface divergence) |
| C5 | Calendar month grid | 6×7 day grid | `Calendar.Calendar()` `dateMenu.js:899,943` | `[self]` | ✅ (`calendar.rs`) |
| C6 | **Events section** | today's calendar events / "No Events" | `EventsSection` `dateMenu.js:111`; added `:960-961`; placeholder `:289-293` | `[dbus:evolution-data-server]` (CalendarServer) | ✅ 6a `a68a2a49` (data plane: `org.gnome.Shell.CalendarServer` client + store) + 6b `ec70a4db` (UI). ScrollView/reactivity deferred (`calendar.rs:12`); needs live validation with a configured calendar |
| C7 | **World clocks section** | clocks from GNOME Clocks | `WorldClocksSection` `dateMenu.js:331`; added `:963-964` | `[dbus]` GNOME Clocks + gsettings | ✅ 7a `8270ada8` (logic: tzf-rs coord→tz + jiff) + 7b `f3c1acfa` (gsettings read + Clocks D-Bus mirror) + 7c `02a46bde` (UI card). Pure-Rust tz (no libgweather); shown iff `org.gnome.clocks.desktop` installed. Needs live validation with Clocks configured |
| C8 | **Weather section** | forecast | `WeatherSection` `dateMenu.js:543`; added `:966-967` | `[subsystem]` GWeather / GNOME Weather | ⬜ |

Note: the old **calendar "Do Not Disturb" switch is gone** — DND moved to Quick Settings (C-list has no
DND). Don't re-add it to the calendar.

---

## RIGHT box — standalone indicators (before quickSettings)

All five are unbuilt in the fork. Order per `sessionMode.js:96-100`.

| # | Role | Panel renders | Menu | Reference | Dep | Status |
|---|---|---|---|---|---|---|
| R1 | **screenRecording** | red ticking `M:SS` timer + `screencast-stop-symbolic`, only while a screencast is live; click stops it | none | `ScreenRecordingIndicator` `js/ui/status/remoteAccess.js:65`; label/icon `:78-88`; visibility+stop `:90-99` | `[self]` (compositor knows) | ✅ (`914accbb`; `is-recording` ledger → panel pill; triggered by `RecordArea` `a4b8c72c`. See slice-1 follow-ups) |
| R2 | **screenSharing** | `screen-shared-symbolic` + stop icon during remote sharing; click stops | none | `ScreenSharingIndicator` `remoteAccess.js:133`; icons `:146-153` | `[self]`/portal state | ⬜ |
| R3 | **dwellClick** | dwell-click mode icon, only when the a11y feature is on | mode choices | `DwellClickIndicator` `js/ui/status/dwellClick.js:35`; icon `:42`; per-mode `:76-82` | `[self]` a11y gsettings | ⬜ |
| R4 | **a11y** (`ATIndicator`) | `accessibility-menu-symbolic` | toggles: High Contrast, Zoom, Large Text, Screen Reader, Screen Keyboard, Visual Alerts, Sticky/Slow/Bounce Keys | `js/ui/status/accessibility.js:32`; icon `:39`; items `:45-75+` | `[self]` a11y gsettings | ⬜ |
| R5 | **keyboard** (`InputSourceIndicator`) | current input-source short-name/flag label, only when >1 source | source list to switch; modifier-key popup (`InputSourcePopup` `keyboard.js:78`) | `js/ui/status/keyboard.js:874`; container `:834,884-885`; label `:986` | `[self]` xkb + input-source gsettings | ✅ label (`33c25f94`) + **keymap driven by GNOME's `org.gnome.desktop.input-sources`** (`b396e063`; GNOME wins when the schema is present, niri xkb + systemd-localed fallback-only) + **layout popover** (`7c4f1e88`: row/layout w/ active checked + short label, separator, "Show Keyboard Layout"→Tecla, "Keyboard Settings"→control-center; picking switches the xkb group + writes `mru-sources`) + HiDPI glyph fix (`3c7473be`) + startup-seed panic fix (`79e14af5`) + **shares the panel-button hover/checked pill** (`48418eaf`). Live-validated 2026-07-23. Divergences: active row uses a check not a radio dot; IBus unsupported; per-window layout not honored (global); modifier-key `InputSourcePopup` not built. **Cleanup TODO:** the now fallback-only niri `input.keyboard.xkb` knob could be dropped ([[gnome-way-replaces-niri]]) |

---

## RIGHT box — quickSettings

Reference: `QuickSettings` — `js/ui/panel.js:287`. Panel area = a `_indicators` box of small status icons
(`panel.js:291-294`); menu = a `QuickSettingsMenu` grid. All sub-indicators built in `_setupIndicators()`
— `js/ui/panel.js:302-395`.

- **Panel icon order** — `panel.js:339-362`: `remoteAccess, camera, volumeInput, location, brightness,
  thunderbolt, nightLight, network, darkMode, doNotDisturb, backlight, bluetooth, rfkill, autoRotate,
  volumeOutput, unsafeMode, powerProfiles, system`.
- **QS tile/slider order** — `panel.js:364-394`: `system, volumeOutput, volumeInput, brightness, camera,
  remoteAccess, thunderbolt, location, network, bluetooth, powerProfiles, nightLight, darkMode,
  doNotDisturb, backlight, rfkill, autoRotate, unsafeMode, backgroundApps` (last, spans all columns).

Our QS panel cluster currently emits DND, Night Light, Network, Volume, Battery icons; the menu has the
system row, battery pill, volume slider, Network tile, and Dark Style / DND / Night Light toggles.

| # | Indicator | Panel icon | QS menu UI | Reference | Dep | Status |
|---|---|---|---|---|---|---|
| Q1 | **system** | battery/power icon + optional % (`system-shutdown-symbolic` w/o battery) | `SystemItem` row: PowerToggle (battery→power settings), Screenshot, Settings, Lock, **Shutdown w/ submenu** (Suspend/Restart/Power Off/Log Out) | `js/ui/status/system.js:308`; icon `:319-360`; row `:263-306`; PowerToggle `:32`; Screenshot `:110`; Settings `:133`; Lock `:240`; Shutdown+submenu `:167` | `[self]`/`[dbus:login1]` | ✅ screenshot/settings/lock + battery pill; **power button now opens the session submenu** (Suspend/Restart…/Power Off…/Log Out…) on the QuickMenuToggle framework, no longer power-offs directly. Switch User deferred — needs a lock screen + GDM greeter handoff (see backlog item 16). **No user avatar upstream** — don't add one. |
| Q2 | **volume output** | speaker icon | `OutputStreamSlider` + output-device submenu | `OutputIndicator` `js/ui/status/volume.js:468`; icon `:439,487`; slider `:293,491`; device section `:77` | `[pw]` | 🟡 slider ✅ + **device picker ✅** (slider `go-next` arrow when >1 sink → in-menu list of sinks, current default checked, click sets it default via a `default.configured.audio.sink` metadata write). Divergences: sink-level not gvc port-level; no per-row device icon; list capped not scrolled |
| Q3 | **volume input (mic)** | mic privacy icon while recording | `InputStreamSlider` + input-device submenu | `InputIndicator` `volume.js:508`; icon `:544-549`; slider `:367,535` | `[pw]` | ✅ privacy icon (orange while a non-skipped, non-monitor `Stream/Input/Audio` runs; white when source muted; leftmost in cluster); **mic slider + input-device picker ✅** (`7730c6ad`): a second QS slider below the output slider, shown only while recording with a bound source (`stream != null && recording`, `volume.js:429`); icon toggles source mute, track sets source level (sensitivity-graded icon), `go-next` arrow when >1 source → "Sound Input" card listing sources w/ default checked → row sets it default via `default.configured.audio.source`. Divergences (like Q2): source-level not gvc port-level; no slider→0-mute coupling; no unmute-at-25% |
| Q4 | **brightness** | none | `BrightnessItem` slider; multi-monitor submenu | `js/ui/status/brightness.js:94`; slider `:38,99`; submenu `:12`; **manager** `js/misc/brightnessManager.js` (global scale = max of per-monitor scales, scaleFactors, dimming clamp, keybindings, OSD) | **`[subsystem]` compositor backlight** — 50.1 is NOT gsd-power: mutter owns the hardware (`MetaBacklight`, `src/backends/meta-backlight-sysfs.c`: udev-matched `/sys/class/backlight` device per output, writes via `login1.Session.SetBrightness`, udev-watched external changes) and the shell's `BrightnessManager` builds on `monitor.get_backlight()` | 🟡 **Q4a+Q4b+Q4c-1 done** (`c5525544` backlight subsystem: `src/backlight.rs` pure algebra + `src/backend/backlight.rs` udev enumerate/watch + `login1.Session.SetBrightness` writer with mutter's one-write-in-flight serializer; `435f2018` `src/brightness.rs` — the full `BrightnessManager` scale algebra incl. the 3-phase `_sync` order, per-monitor factors, dimming clamp and ab-bias; `65d64d5d` the QS slider row itself, third in gnome-shell's item order, non-reactive icon, optimistic drag; `3a8b98ff` Q4c-2 the per-monitor card — arrow gated on >1 scale, label+slider row pairs, no separator/settings row, plus the `row_shape` → per-row-**kind** framework change and a `SliderId` drag identity). **Remaining: Q4d only** — keybindings + OSD (we have none at all) + the `org.gnome.Shell.Brightness` session object. Divergences D1 (no pkexec helper fallback), D2 (no HDR ref-white), D5 (scale per output, not per logical monitor), plus rounding where GJS truncates. **Live-only: everything** — this VM has no backlight device |
| Q5 | **keyboard backlight** | none | `KeyboardBrightnessToggle` (slider or discrete steps) | `js/ui/status/backlight.js:236`; toggle `:159,241`; slider `:21`; steps `:79` | `[dbus:UPower/logind]` | ⬜ |
| Q6 | **network** | status icon(s) | per-device `QuickMenuToggle`s: wired, Wi-Fi (list), modem, BT-tether, VPN — each w/ submenu | `js/ui/status/network.js`; built `panel.js:303-310`; NMToggle `:1381`; Wi-Fi `:1076`; VPN `:1541` | `[nm]` | 🟡 panel icon ✅; tile is now a QuickMenuToggle — arrow opens an in-menu detail card (header + **Network Settings** row); body opens settings. **In-menu enable/disable, Wi-Fi list, VPN ⬜** (need NM writes); no SSID label |
| Q7 | **bluetooth** | bluetooth icon | `BluetoothToggle` + device-list submenu | `js/ui/status/bluetooth.js:442`; built `panel.js:312-319`; icon `:450`; toggle `:273,453`; device item `:201` | `[dbus:bluez]` | ✅ BlueZ ObjectManager watcher (`src/dbus/bluez.rs`, 4th task on the shared system connection; one PropertiesChanged match rule for all of `/org/bluez`) + the gsd-rfkill Bluetooth trio (tile `available` gate + `BluetoothAirplaneMode` write, `rfkill.rs`). Tile INSERTED at grid slot 1 (after Network, `panel.js:380-383`), subtitle = connected summary, icon from `PowerState` incl. the ported predicted-state override (30 s failsafe); arrow → `DetailOwner::Bluetooth` device list (order frozen at open, newcomers append; trailing Connect/Disconnect; placeholder; settings row); `connectable` = gnome-bluetooth's UUID list; connect = `Device1.Connect/Disconnect` with a busy mark until done. Panel icon between network and rfkill iff any connected device. Headless/unit + render-differential verified; **live-only: everything** (this VM has no BT adapter — needs real hw). Divergences in the module docs: no spinner ("…" busy mark), placeholder as one 36px centered row, no idle-coalescing, plain-Unicode sort, echoed-state toggle target |
| Q8 | **power profiles** | profile icon (when not Balanced) | `PowerProfilesToggle` + profile submenu | `js/ui/status/powerProfiles.js:134`; icon `:139`; toggle `:42,150`; section `:75` | `[dbus:power-profiles-daemon]` | ✅ two-line QS tile ("Power Mode" + active-profile subtitle) with body-toggle (Balanced ↔ last-selected via `org.gnome.shell last-selected-power-profile`, vendor profiles included) + a >2-gated profile picker + panel icon; `UPower.PowerProfiles` system-bus watcher (3rd task, echo-driven write). Headless/unit-verified; not live (this VM has no ppd) |
| Q9 | **night light** | `night-light-symbolic` | `NightLightToggle` (plain) | `js/ui/status/nightLight.js:38`; icon `:43`; toggle `:16-21,46` | `[self]` gsettings | ✅ |
| Q10 | **dark mode** | none | `DarkModeToggle` "Dark Style" (plain) | `js/ui/status/darkMode.js:43`; toggle `:9-13,48` | `[self]` gsettings | ✅ |
| Q11 | **do not disturb** | icon when active | `DoNotDisturbToggle` (plain) | `js/ui/status/doNotDisturb.js:24`; indicator `:32-35`; toggle `:7-12,36` | `[self]` gsettings | ✅ |
| Q12 | **location** | `location-services-active-symbolic` privacy icon | none (owns `GeolocationDialog` prompt `:329`) | `js/ui/status/location.js:211`; icon `:218-219` | `[dbus:geoclue]` | ⬜ |
| Q13 | **rfkill / airplane** | `airplane-mode-symbolic` when airplane on | `RfkillToggle` "Airplane Mode" | `js/ui/status/rfkill.js:114`; icon `:119-120,132`; toggle `:94-98,127` | `[dbus:rfkill]`/`[nm]` | ✅ standalone toggle (5th QS tile, appended when `HasAirplaneMode && ShouldShowAirplaneMode`) + panel icon (sibling of network, not replacing it) from gsd-rfkill session bus; echo-driven D-Bus write; dropped the `NetworkStatus::Airplane` heuristic. Headless/unit-verified; not live (this VM has no rfkill hw) |
| Q14 | **thunderbolt** | `thunderbolt-symbolic` | device-authorize prompts (`AuthRobot` `:129`), no standing toggle | `js/ui/status/thunderbolt.js:216`; icon `:221-222,287-289` | `[dbus:bolt]` | ⬜ |
| Q15 | **remote access** | `media-record-symbolic` privacy icon | indicator only | `RemoteAccessApplet` `js/ui/status/remoteAccess.js:14`; icon `:26-28` | `[self]`/portal | ⬜ |
| Q16 | **camera** | `camera-web-symbolic` privacy icon | indicator only | `js/ui/status/camera.js:6`; icon `:11-17` | `[pw]`/portal | ⬜ |
| Q17 | **auto rotate** | none | `RotationToggle` "Auto Rotate" (only on capable hw) | `js/ui/status/autoRotate.js:38`; toggle `:9,14,21-29,43` | `[dbus:iio-sensor-proxy]` | ⬜ |
| Q18 | **unsafe mode** | `channel-insecure-symbolic` when on | toggle to leave unsafe mode | `UnsafeModeIndicator` `js/ui/panel.js:273`; icon `:277-280` | `[self]` compositor state | ⬜ |
| Q19 | **background apps** | none | `BackgroundAppsToggle` flat-menu list of running background apps | `js/ui/status/backgroundApps.js:251`; toggle `:137-147,256`; item `:35` | `[subsystem]` app/portal tracking | ⬜ |

### QS framework gaps
- **`QuickMenuToggle`** — ✅ **framework landed** (`src/ui/quick_settings.rs`): a menu tile split into
  a toggle-body + expand-arrow whose arrow opens an in-menu **detail view** (a `%card` with a header +
  action rows) pinned below its owner's row, growing the menu and shifting the rows below down. Keyed
  by identity (`DetailOwner`), one open at a time, no new `PopoverAction` (open/close is internal +
  `Consumed`; rows are `Spawn`). Consumers so far: the **Network** tile (arrow → header + Network
  Settings row) and the **power button** (Q1 session submenu). This is the prerequisite for Q6/Q7/Q8's
  in-menu device/profile lists — each now just adds a `DetailOwner` arm (`header`/`rows`/`row_count`/
  `anchor_row_bottom`) + the backend to enumerate/act. Deferred v1 polish: slide-down grow animation,
  dim-the-rest, split-radius tile look.
- **Shutdown submenu** in the system row (Q1) — ✅ done on the framework above.
- **Hover highlighting** — ✅ `ac2d5b7b` + review `6b7d9e24` + notification fix `5642b2b9`. A
  `pointer_hover` route parallels `pointer_click`, and a hover change bumps the widget's texture
  revision to re-bake. Direction is cited per widget, NOT assumed: QS tiles / pill / system buttons /
  slider mute icons / detail rows and calendar day cells / month arrows / today card **lighten**
  (`button(hover)` = `st-lighten(…,4%)` on the dark theme, `_drawing.scss:193`), as an additive
  `≈white@0.10` wash. Notification **cards darken** (`.message`=`%card`: `%card:hover` =
  `lighten($card_bg,4%)` vs `.message`'s normal `+5%` override → ~1% darker), while the card **button**
  under the pointer **lightens** on top (`%notification_button` white@.15→.30). Whole notification card
  darkens on hover (banner + list), not just the buttons. Cursor stays the arrow (GNOME uses none on
  shell widgets); a floating banner now also grabs the pointer so the window under it can't paint its
  own cursor. Divergences: QS tiles light whole (not per body/arrow half); the slider picker arrow and
  the banner's individual buttons aren't separately highlighted. Not live-validated yet
  (unit + Vulkan-differential only).

---

## Slice 1 (R1 + RecordArea) follow-ups

Landed `a4b8c72c` (RecordArea capture) + `914accbb` (R1 indicator); the adversarial review
carved these out as separate work — none block R1 for daily use:

- **Cross-output area casts** — the fork records an area from the single output it overlaps most
  (crop + `warn!` on span); mutter composites every intersecting view (`meta-stream-area.c:164-184`).
  Needs a multi-output accumulation buffer we don't have.
- **Client-vanish session cleanup** — mutter closes a client's sessions when its bus name vanishes
  (`meta-dbus-session-watcher.c:56,74,92`); the fork does not watch names, so a crashed recorder
  leaves a stuck session (R1 recoverable by clicking the pill). Add name-watching for mutter parity.
- **Screenshot-UI record mode** — GNOME's real R1 trigger (`js/ui/screenshot.js`); until then R1 is
  driven by the stock `org.gnome.Shell.Screencast` recorder or a direct `RecordArea` D-Bus call.
- **Q15 `RemoteAccessApplet` privacy dot must exclude these sessions** (mutter excludes the shell's
  own cast, `remoteAccess.js:36-43`) — the `recordings` ledger gives it the handle when Q15 lands.
- **Metadata-cursor offset for area casts** — embedded/hidden cursor handled; Metadata mode's
  location is area-local already, but only the recorder's Hidden/Embedded path is exercised.
- **Accepted divergence:** the R1 pill has no hover-lighten (GNOME filled `panel_button` hovers to
  `lighten($bg,5%)`, `_drawing.scss:418-421`); the static red pill is intentional for this slice.
- **Live-only checks:** the 1 s tick cadence, area-on-rotated-output, and the full D-Bus+PipeWire
  path (recorder → `is-recording` → pill → click → `Closed` → finalized `.webm`).

## Removed upstream in 50.1 — do NOT build
- **App menu** (`AppMenuButton`) — gone; nothing sits left of activities.
- **Calendar DND switch** — moved to the QS DND toggle (Q11), which we already have.
- **User avatar / name** in the QS system row — not present in 50.1; the row is just
  power/screenshot/settings/lock/shutdown.

---

## Prioritized backlog (pick from the top)

**Tier 1 — self-contained, no new daemon, real daily-driver gaps:**
1. ✅ C4 TodayButton header (calendar) — done `e5b3ac6c`.
2. ✅ R1 screen-recording indicator — done (native recorder track).
3. ✅ R5 input-source (keyboard) indicator — **fully done**: label `33c25f94` + GNOME `input-sources`
   keymap `b396e063` + layout popover/switching `7c4f1e88` + shared pill `48418eaf`. Live-validated.
4. Q18 unsafe-mode indicator + toggle — deferred: no unsafe-mode state exists in the fork yet
   (nothing sets/reads it), so the indicator would be inert chrome. Revisit once a privileged
   surface actually gates on it.

**Tier 2 — reuses backends already wired:**
5. ✅ Q3 microphone input slider + input-device picker — done (`7730c6ad`): mic slider (level +
   mute, recording-gated) below the output slider, `DetailOwner::Input` off its arrow, source
   enumeration + a `default.configured.audio.source` metadata write. Generalized the single-slider
   geometry to two stacked sliders (`Sliders`/`Slider`/`slider_row_y`).
6. ✅ Q13 airplane → standalone rfkill toggle + panel icon — done (`9f74f342`, review fixes
   `c0b9fd1e`): gsd-rfkill session-bus watcher (`src/dbus/rfkill.rs`), 5th QS tile appended when
   `HasAirplaneMode && ShouldShowAirplaneMode`, echo-driven `AirplaneMode` write, panel icon as a
   network sibling. Dropped the `NetworkStatus::Airplane` NM heuristic. Not live (VM has no rfkill).
7. ✅ Q2 output-device picker — done (`cae8fa05`): sink enumeration + descriptions + a
   `default.configured.audio.sink` metadata write, `DetailOwner::Output` hung off the slider's
   menu-button.

**Tier 3 — new system-bus watchers (same pattern as `src/dbus/system_status.rs`):**
8. ✅ Q7 bluetooth (bluez) — done (see the Q7 row above). Semantics grounded in gnome-bluetooth
   master (`lib/bluetooth-client.c`/`bluetooth-device.c`, read from GitLab): largest-path default
   adapter, `PowerState` mapping, the connectable-UUID list, plain `Device1.Connect/Disconnect`.
   Needs real hardware for any live validation.
9. ✅ Q8 power profiles — done (`70b9ba96` model+watcher+panel icon, `a47fb1de` two-line tile+body
   toggle, `8e0bad97` picker, `69f6a2b0` review): `UPower.PowerProfiles` watcher as a 3rd task on
   the shared system-bus connection (name-owner wake, hidden-on-daemon-death); a two-line Power Mode
   tile (first appended conditional, before Airplane) + a >2-gated profile picker; last-selected via
   `org.gnome.shell last-selected-power-profile`, authoritative on `Niri`. Not live (VM has no ppd).
10. ✅ **QuickMenuToggle detail-view framework** — done (`2f7d2b0a`/`16d81fc9`); consumers so far
    Network tile + Q1 power submenu. Unlocks Q6 Wi-Fi list, Q7 device list, Q2 device picker, Q8
    profiles — each adds a `DetailOwner` arm + its backend.
11. ✅ Q4 brightness — Q4a backlight subsystem, Q4b manager algebra, Q4c the QS slider + the
    per-monitor card, Q4d keybindings + the `org.gnome.Shell.Brightness` object. **Only the OSD
    is left**, deferred by request: we have no OSD subsystem at all, so it is its own slice (and
    would also serve volume, which currently has none either). Nothing about Q4 is live-validated
    — this VM has no backlight device. Plan file: `~/.claude/plans/q4-brightness-plan.md`.
12. R4 a11y menu (gsettings).

**Tier 4 — larger subsystems:**
13. Notification server (`org.freedesktop.Notifications`) → C2 dot + C3 message list.
    **IN PROGRESS** — full design (Fable-reviewed, approved 2026-07-19) in the plan file
    `~/.claude/plans/rustling-finding-popcorn.md`; slice 1 ✅ (`5ac65744`+`552336b3`: the fdo
    server we own + the `NotificationStore` model in `src/notifications.rs`, sender-tracked
    replace/close, unicast signals, live-validated against a real bus); slice 2 ✅
    (`7c08464b`: banner overlay in `src/ui/notification_banner.rs` — tray timing incl. idle
    gating, transient-destroy-on-hide, popover blocking, close/action/body clicks with real
    XDG activation tokens, untrusted-string/icon-name hardening; live-validated on the
    headless harness incl. an action click via injected input); slice 3a ✅ (`49d6e952`+
    `5abad02d`: the dateMenu popover is the two-column layout — message list first at 29em,
    calendar second (`dateMenu.js:917-940`) — flat cards via the shared card renderer
    (`src/ui/notification_card.rs`), store-order sources (move-to-front on add only),
    ack-on-open exactly once + push-while-open without re-ack (`messageList.js:1193-1199`),
    placeholder + Clear pill, card close/body/Clear clicks (body-activate closes the popover
    like GNOME activation), bundled `no-notifications-symbolic` (gresource-only icon);
    live-validated on the headless harness with injected clicks. Also fixed here: the shared
    card's icons were z-buried under the card texture since slice 2 — output element lists
    are top-to-bottom; icons now precede the card, pinned by a pixel test); message
    expansion ✅ (`7bcf2787`+`ca3befe1`: the shared card is expandable per GNOME —
    collapsed = ONE ellipsized body line + no action row, expanded = wrapped body up to 6
    lines + actions (`LabelExpanderLayout` + action-bin gating,
    `messageList.js:220-275,598-666`); list cards get the header expand caret
    (`notification-expand-symbolic`, bundled + a baked-180° collapse variant; live iff
    ellipsized/has-actions/expanded, `messageList.js:521-538`), per-id expansion state
    surviving snapshot pushes with the line budget clamped to the no-scroll space (falls
    back to collapsed when even 1 line + actions can't fit); expanded list action buttons
    emit token+ActionInvoked, destroy unless resident, close the popover; the banner has NO
    caret (`messageTray.js:1137`) — hover expands once shown incl. hover started mid-slide
    (`:970-996,1102-1105`) with the popped-under-pointer guard (`:978-991`), CRITICAL
    auto-expands at show/replace (`:1170-1174`); this made the slice-2 always-visible
    action row faithful (actions only when expanded). niri-vk grew GPU-free
    `wrap_lines_weighted` (scale-independent breaks; bidi-safe after a Fable-review HIGH —
    visual-order glyph ranges panicked on RTL-in-LTR bodies, now min/max + a bidi test).
    Live-validated on the harness: collapsed/hover-expanded/critical banner shots, banner +
    list action clicks received by `notify-send -A`, caret expand/collapse with the flipped
    chevron; also re-confirmed critical queues behind a showing banner and action-destroyed
    notifications never re-banner. Close button gained its missing 3px margin
    (`_message-list.scss:152-155`)). Divergences recorded in the module docs: instant
    expand (no 200 ms ease), no focus grab on expand, list expansion state dropped with the
    popover, invisible caret not clickable, hover-cycle stands in for GNOME's mouse-away
    timeouts, `forceExpanded` deferred with per-app policies, no app focus on body click,
    QS popovers also block, only left clicks intercepted); grouped card stacks — slice 3b ✅
    (`d5d4cdb0`: notifications from one source group into a `NotificationMessageGroup`
    (`messageList.js:858-949`); the snapshot is now `Vec<CardGroup>` via `message_list_groups`.
    A one-notification group renders as a plain card (unchanged); a larger one fans into a
    collapsed stack — newest card on top over ≤2 darkened peeks (`second/lower-in-stack`,
    `_message-list.scss:89-98`), each inset 6px/side and revealed 10px then ÷1.4
    (`:1314-1350,1370-1404`); urgent groups sort first, criticals lead within a group
    (`:1815-1826`). Clicking a collapsed stack expands it into a header (source title +
    `group-collapse-symbolic`, bundled) over the cards; the header button re-collapses; a
    collapsed close closes the WHOLE group, expanded closes one card (`:1106-1118,1236-1242`);
    one group expanded at a time. Peeks render as cached darkened rounded rects (the peek
    shows only bg); the header is a cached texture + composited chevron; `SourceKey` gained
    `Hash`. Pinned by model/list unit tests, a gnome.rs same-source grouping conformance test
    (real clicks), and a Vulkan render differential (peek + expanded-header chevron).
    Review follow-ups (`bc0e674b`): the collapsed peek z-order was inverted (deepest painted
    over the shallower peek for 3+ groups — the 2-card render test could not catch it; now a
    3-card differential asserts the upper peek band is lighter); the expanded state drops when
    a source shrinks to one and un-expands member bodies on collapse
    (`messageList.js:988,1170-1173`); a click on the expanded header background / inter-card
    gap collapses the group (`messageList.js:879,934-935`).
    Divergences: 200 ms expand animation, the group
    highlight fade, Escape group-collapse (Escape closes the whole popover; the header button,
    header background, and inter-card gap all collapse); the collapse button's click target is
    the 24px visible circle (GNOME's actor is 32px with an 8px transparent border) and the
    header is 36px (GNOME ≈44px); the header source title is not ellipsized (a hostile long app
    name overdraws under the always-on-top chevron); the peek bgs are within a few /255 of the
    exact `darken()` values (style-reference tolerance)); C2 MessagesIndicator — slice 4 ✅ (`44e1a1c9`+`704f02a6`: the
    dateMenu unread dot (`message-indicator-symbolic`, bundled) after the clock with a
    size-matched leading pad so the clock stays centered (`dateMenu.js:871-886`); visible iff
    `show-banners && unseen − queued > 0` (`:787-798`), recomputed on every store mutation, on
    the QS DND-tile flip, and on the gsettings DND change; composited from the icon cache atop
    the bar; hit rect widens to keep the dot clickable while the lit pill stays on the clock
    alone; anchored 2px off the pill edge like GNOME's box spacing. Pinned by panel-geometry,
    gnome.rs unseen/DND, and a Vulkan differential). Message-list scrolling — un-deferred ✅
    (`e1f44f6f` + review follow-ups `e20033fe`: gnome-shell's `St.ScrollView`, `calendar.js:816`.
    The list lays out in content space (never dropping); when it overflows the fixed popover
    height the visible window is baked into a **viewport-sized** texture (content shifted up by
    the scroll offset and clipped by the buffer, so its dimensions stay bounded however many
    notifications there are; cached by scale+revision+scroll) and presented with an overlay
    scrollbar thumb. A wheel/touchpad scroll *over the popover* scrolls the list, or — over the
    calendar column — pages the month (`Calendar.vfunc_scroll_event`, `calendar.js:560-571`); it
    is consumed there, but a scroll over a panel indicator still reaches its own handler (e.g. QS
    volume). An expanded card shows its full ≤`EXPAND_LINES` wrap regardless of height (the
    earlier height-clamp/collapse-fallback is gone — the list scrolls instead). Clicks register
    only inside the viewport. Divergences: no vfade edge gradient (`_scrollbars.scss:4`
    `-st-vfade-offset`), no scrollbar-handle drag (wheel/touchpad only), no scroll-to-focused
    message (`ensureActorVisibleInScrollView`, `calendar.js:845`); `card_rects` reports a
    partially-clipped card as visible though only its in-viewport part is clickable. Pinned by
    scroll-reveal/clamp + calendar-vs-list routing unit tests, a Vulkan differential (clipped
    card + thumb), and updated gnome.rs conformance). **Notifications subsystem COMPLETE**
    through the approved plan (slices 1/2/3a/3b/4 + message expansion + scrolling); deferred items
    (MPRIS media, GtkNotifications, per-app policies, sounds, live time refresh,
    rich `<b>/<i>` spans) remain per the plan.
14. ~~C6 events~~ ✅ (6a `a68a2a49` + 6b `ec70a4db`) / ~~C7 world clocks~~ ✅ (7a `8270ada8` + 7b `f3c1acfa` + 7c `02a46bde`) / C8 weather.
15. Q12 location, Q14 thunderbolt, Q17 auto-rotate, Q16 camera, Q19 background apps (as hardware/need arises).
16. **Lock screen + Switch User** (Q1 shutdown submenu's missing `Switch User…` row). Investigated
    2026-07-23 — this is a Phase-2 subsystem (see STRATEGY §6), not a one-line add. GNOME's
    `SystemActions.activateSwitchUser` (`js/misc/systemActions.js:470`) does two things and gates on a
    third, none of which the fork has yet:
    - **Lock the current session first** — `Main.screenShield.lock(false)`. We have **no screen
      shield / lock screen** at all. This is the load-bearing prerequisite *and* a hard security
      requirement: without it, switching users leaves this session unlocked and visible on its VT.
      The lock screen is its own subsystem (lock UI + PAM/logind unlock auth + sessionMode `isLocked`
      plumbing that also gates the notification list, banners, etc.). Building it unblocks Switch User
      almost for free.
    - **Hand off to a greeter** — `Gdm.goto_login_session_sync(null)` spawns/activates a fresh GDM
      greeter on another VT. That's a libgdm (GObject) call; to stay GObject-free (fork tenet) drive
      GDM's D-Bus (`org.gnome.DisplayManager`) and/or logind (`org.freedesktop.login1`) directly.
    - **Visibility gate** — GNOME shows the row iff `userManager.can_switch() && has_multiple_users`
      (AccountsService, `org.freedesktop.Accounts`) and `!lockdown disable-user-switching`
      (`org.gnome.desktop.lockdown`); hidden entirely on a single-user machine. So we also need an
      AccountsService query to decide whether to render it.

    Plan: do a **lock-screen slice** first (Phase 2), then add Switch User (the GDM handoff) + the
    faithful multi-user/lockdown gate on top. Until then the shutdown submenu's session group is just
    `Log Out…` (the group separator above it is already faithful). `Log Out`/`Restart`/`Power Off`
    already work via our gnome-session `EndSessionDialog` handshake; only Switch User needs this.
