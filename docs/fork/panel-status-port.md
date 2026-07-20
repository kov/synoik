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
| C2 | **Notifications dot** (`MessagesIndicator`) | small dot on clock when unread | `dateMenu.js:743,869,883` | `[subsystem]` notifications | ⬜ |
| C3 | **Message-list column** (`CalendarMessageList`) | notifications + media controls + **Clear** button + "No Notifications" placeholder | `js/ui/calendar.js:794`; placeholder `:775,804`; view `:814-821`; clear button `:823-837`; added `dateMenu.js:918-919` | `[subsystem]` `org.freedesktop.Notifications` server (we have none) | ⬜ |
| C4 | **TodayButton header** | day-of-week + full date above the grid | `TodayButton` `dateMenu.js:52,70-77`; added `:942` | `[self]` | ✅ `e5b3ac6c` (header card in `calendar.rs`; click snaps to today; one-surface divergence) |
| C5 | Calendar month grid | 6×7 day grid | `Calendar.Calendar()` `dateMenu.js:899,943` | `[self]` | ✅ (`calendar.rs`) |
| C6 | **Events section** | today's calendar events / "No Events" | `EventsSection` `dateMenu.js:111`; added `:960-961`; placeholder `:289-293` | `[dbus:evolution-data-server]` (CalendarServer) | ⬜ (noted deferred `calendar.rs:11`) |
| C7 | **World clocks section** | clocks from GNOME Clocks | `WorldClocksSection` `dateMenu.js:331`; added `:963-964` | `[dbus]` GNOME Clocks + gsettings | ⬜ |
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
| R5 | **keyboard** (`InputSourceIndicator`) | current input-source short-name/flag label, only when >1 source | source list to switch; modifier-key popup (`InputSourcePopup` `keyboard.js:78`) | `js/ui/status/keyboard.js:874`; container `:834,884-885`; label `:986` | `[self]` xkb + input-source gsettings | 🟡 label done (`33c25f94`), live-validated. **Deferred (later keyboard pass):** (a) clicking it opens no menu — needs the source-switch list + "Show Keyboard Layout" + "Keyboard Settings" items; (b) reads niri's xkb config, not GNOME's `org.gnome.desktop.input-sources` |

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
| Q1 | **system** | battery/power icon + optional % (`system-shutdown-symbolic` w/o battery) | `SystemItem` row: PowerToggle (battery→power settings), Screenshot, Settings, Lock, **Shutdown w/ submenu** (Suspend/Restart/Power Off/Log Out) | `js/ui/status/system.js:308`; icon `:319-360`; row `:263-306`; PowerToggle `:32`; Screenshot `:110`; Settings `:133`; Lock `:240`; Shutdown+submenu `:167` | `[self]`/`[dbus:login1]` | ✅ screenshot/settings/lock + battery pill; **power button now opens the session submenu** (Suspend/Restart…/Power Off…/Log Out…) on the QuickMenuToggle framework, no longer power-offs directly. Switch User deferred (greeter jump). **No user avatar upstream** — don't add one. |
| Q2 | **volume output** | speaker icon | `OutputStreamSlider` + output-device submenu | `OutputIndicator` `js/ui/status/volume.js:468`; icon `:439,487`; slider `:293,491`; device section `:77` | `[pw]` | 🟡 slider ✅ + **device picker ✅** (slider `go-next` arrow when >1 sink → in-menu list of sinks, current default checked, click sets it default via a `default.configured.audio.sink` metadata write). Divergences: sink-level not gvc port-level; no per-row device icon; list capped not scrolled |
| Q3 | **volume input (mic)** | mic privacy icon while recording | `InputStreamSlider` + input-device submenu | `InputIndicator` `volume.js:508`; icon `:544-549`; slider `:367,535` | `[pw]` | ✅ privacy icon (orange while a non-skipped, non-monitor `Stream/Input/Audio` runs; white when source muted; leftmost in cluster); **mic slider + input-device picker ✅** (`7730c6ad`): a second QS slider below the output slider, shown only while recording with a bound source (`stream != null && recording`, `volume.js:429`); icon toggles source mute, track sets source level (sensitivity-graded icon), `go-next` arrow when >1 source → "Sound Input" card listing sources w/ default checked → row sets it default via `default.configured.audio.source`. Divergences (like Q2): source-level not gvc port-level; no slider→0-mute coupling; no unmute-at-25% |
| Q4 | **brightness** | none | `BrightnessItem` slider; multi-monitor submenu | `js/ui/status/brightness.js:94`; slider `:38,99`; submenu `:12` | `[dbus:gsd/logind]` backlight | ⬜ |
| Q5 | **keyboard backlight** | none | `KeyboardBrightnessToggle` (slider or discrete steps) | `js/ui/status/backlight.js:236`; toggle `:159,241`; slider `:21`; steps `:79` | `[dbus:UPower/logind]` | ⬜ |
| Q6 | **network** | status icon(s) | per-device `QuickMenuToggle`s: wired, Wi-Fi (list), modem, BT-tether, VPN — each w/ submenu | `js/ui/status/network.js`; built `panel.js:303-310`; NMToggle `:1381`; Wi-Fi `:1076`; VPN `:1541` | `[nm]` | 🟡 panel icon ✅; tile is now a QuickMenuToggle — arrow opens an in-menu detail card (header + **Network Settings** row); body opens settings. **In-menu enable/disable, Wi-Fi list, VPN ⬜** (need NM writes); no SSID label |
| Q7 | **bluetooth** | bluetooth icon | `BluetoothToggle` + device-list submenu | `js/ui/status/bluetooth.js:442`; built `panel.js:312-319`; icon `:450`; toggle `:273,453`; device item `:201` | `[dbus:bluez]` | ⬜ |
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
  dim-the-rest, per-row hover, split-radius tile look.
- **Shutdown submenu** in the system row (Q1) — ✅ done on the framework above.

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
3. ✅ R5 input-source (keyboard) indicator — done `33c25f94` (xkb layout label; menu deferred).
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
8. Q7 bluetooth (bluez).
9. ✅ Q8 power profiles — done (`70b9ba96` model+watcher+panel icon, `a47fb1de` two-line tile+body
   toggle, `8e0bad97` picker, `69f6a2b0` review): `UPower.PowerProfiles` watcher as a 3rd task on
   the shared system-bus connection (name-owner wake, hidden-on-daemon-death); a two-line Power Mode
   tile (first appended conditional, before Airplane) + a >2-gated profile picker; last-selected via
   `org.gnome.shell last-selected-power-profile`, authoritative on `Niri`. Not live (VM has no ppd).
10. ✅ **QuickMenuToggle detail-view framework** — done (`2f7d2b0a`/`16d81fc9`); consumers so far
    Network tile + Q1 power submenu. Unlocks Q6 Wi-Fi list, Q7 device list, Q2 device picker, Q8
    profiles — each adds a `DetailOwner` arm + its backend.
11. Q4 brightness slider (gsd/logind).
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
    headless harness incl. an action click via injected input). Divergences recorded in the
    module docs: no expand/6-line clamp, no app focus on body click, QS popovers also block,
    only left clicks intercepted. Remaining: calendar message-list column (3a/3b), C2
    indicator.
14. C6 events / C7 world clocks / C8 weather.
15. Q12 location, Q14 thunderbolt, Q17 auto-rotate, Q16 camera, Q19 background apps (as hardware/need arises).
