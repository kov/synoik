<!-- SPDX-License-Identifier: GPL-3.0-only -->

# GNOME 51 beta — inventory of upstream changes that touch us

Reference points (all four checkouts under `~/Projects`, fetched; **HEADs stay at 50.x** — read 51
with `git show 51.beta:<path>` or a throwaway worktree, never by switching the checkout):

| project | our reference | upstream anchor | range |
| --- | --- | --- | --- |
| mutter | 50.3 | `51.beta` (2026-08-02) | `50.3..51.beta` — 389 commits, 619 files |
| gnome-shell | 50.3 | `51.beta` (2026-08-02) | `50.3..51.beta` — 304 commits, 435 files |
| gnome-session | 50.1 | `51.beta` | 38 files, +807/−975 |
| gnome-control-center | 50.3 | `51.beta` | 706 files (mostly Adwaita port + po) |

`51.alpha` (2026-06-29) predates `50.3` (2026-07-03), and the ranges above exclude by *ancestry*
only — a fix cherry-picked onto `gnome-50` keeps a different SHA on `main`, so it still shows up.
The ranges therefore over-include in the same direction NEWS does. Before spending reconcile time on
any **[R]** item, confirm it is not already in our reference with
`git log --cherry-pick --right-only --no-merges 50.3...51.beta -- <path>` (three dots). Feature-sized
items below cannot be in 50.3; the exposure is bugfix-shaped ones from 51.alpha.

Post-freeze on `main`, not in `51.beta`: mutter 123, gnome-shell 41, g-c-c 35 commits. Re-check
before acting on anything here close to 51.0.

Each item is tagged:
**[R]** reconcile — behaviour we already ported changed upstream ·
**[N]** new capability we don't have ·
**[D]** touches something we deliberately diverged on — kov's call, not a TODO.

---

## 1. Client background blur — upstream now implements it

The largest single overlap. mutter grew a real implementation of the same protocol we already
serve.

- **[R] mutter implements `ext-background-effect-v1`** (`c07922770a`,
  `src/wayland/meta-wayland-background-effect.c`, `META_EXT_BACKGROUND_EFFECT_V1_VERSION 1`).
  We already expose this via smithay (`src/handlers/background_effect.rs`), so this is not new
  surface — it is the first time there is a *reference implementation* to conform to.
- **[R] mutter's blur constants are fixed, not client- or config-driven**
  (`src/compositor/meta-surface-actor.c`): radius `24.0`, saturation `1.25`, noise `0.015`. Ours
  (`synoik-config` `Blur::default()` + `blur::client_finish`) carries niri's optional
  `noise`/`saturation` knobs and adds two terms mutter has none of. Side by side:

  | | mutter 51 | synoik default |
  | --- | --- | --- |
  | strength | gaussian **σ = 12 logical px**, × `view_scale` at paint | dual-Kawase `passes=3, offset=3` → **σ ≈ 19 physical px**, no scale factor |
  | saturation | `1.25`, `mix(luma, color, s)`, Rec.601 luma | `1.5`, same `mix`, Rec.709 luma |
  | noise | `0.015`, `(hash(gl_FragCoord) − 0.5) · noise` | `0.02`, identical formula |
  | tint | none | 20%-alpha wash, white or near-black by color-scheme |
  | contrast | none | `+0.06` about mid-grey |

  `BACKGROUND_EFFECT_BLUR_RADIUS` is **2σ**, not σ: `clutter_blur_new` sets
  `blur->sigma = radius / 2.0f` (`clutter/clutter/clutter-blur.c`). Our σ is a variance-matched
  estimate from the tap kernels (down `0.5·d²`, up `1.33·d²`, `d = 0.5·2ⁱ·offset`), not a
  measurement — pin it with `blur-probe` before treating it as exact.

  The divergence that matters is **units, not magnitude**: mutter's radius is logical and gets
  `× view_scale`, ours is fixed in physical pixels (`offset` reaches `record_blur` untouched; the
  rescale at `frame.rs:1490` compensates the intermediate ladder, not output scale). So we are
  ~1.6× *wider* than GNOME at 1× and narrower at 2×, and our blur changes strength when a window
  moves between monitors of different scale. Theirs does not.

  The tint and contrast are the larger *visible* divergence, and deliberate: `client_finish`
  documents them as a legibility measure, since the protocol lets a client ask for blur without
  saying anything about how it should look. GNOME 51 answers that question differently — it adds
  nothing and lets the client own its own contrast.
- **[R] sample-region rule**: mutter pads the blur region by `ceil(radius * 2)` on every side
  (`calculate_blur_sample_padding`) and keeps that padded region as a separate
  `background_blur_sample_region`, used both for damage and for the source read.
- **[R] damage/redraw-clip expansion is a stage-level filter**
  (`meta_stage_add_redraw_clip_filter`, `a28572caff`): whenever a redraw clip intersects a blurred
  surface's sample region, the clip is *unioned* with it. That is the upstream answer to
  "what must be redrawn when something behind a blurred surface changes". Compare against ours.
- **[R] blur is skipped in clone paint** (`clutter_actor_is_in_clone_paint`) — i.e. no blur in
  window previews/overview clones. Check what we do in the overview.
- **[N] `clutter_blur_node_new_from_framebuffer(fb, x, y, w, h, radius, saturation, noise, opacity)`**
  — a single paint node that reads the *stage view framebuffer* as the source, at
  `radius * view_scale`. Confirms the "blur samples the already-composited scene, not a
  separate pass" model.

Owner doc: `docs/fork/client-blur.md` (gaps 4–7 were open).

## 2. Session management protocol — upstream caught up to where we already are

- **[R] mutter switched from its in-tree `session-management-v1.xml` (the `xx_session_manager_v1`
  names) to `xdg-session-management-v1` from wayland-protocols** (`10043bf533`). We implemented the
  merged `xdg_session_management_v1` names directly (`src/protocols/session_management.rs`), so we
  were ahead; now the naming matches and mutter is a usable behavioural reference again.
- **[N] `MetaSessionState.has_window` vmethod** (`8366f50891`, `e14fc49862`) — mutter can now ask a
  session state whether a given toplevel is known to it. Relevant to our restore seeds
  (`docs/fork/session-management-port.md`).
- **[R] the debug-control key for session management was dropped** (`81875dabd4`) — session restore
  is no longer gated behind `org.gnome.Mutter.DebugControl`.
- **[D] gnome-session still hides save/restore in the UI** (50.rc decision, unchanged in 51) — our
  restore work stays ahead of what upstream exposes.

## 3. Authentication / lock screen / GDM — near-total rewrite

`js/gdm/util.js` (892 lines) is **gone**, split into `userVerifier.js`, `authServices.js`,
`authServicesLegacy.js`, `authServicesSSSDSwitchable.js`, `authMenuButton.js`, `webLogin.js`,
`fido2TokenManager.js`, `fingerprintManager.js`, `conflictingSessionDialog.js`, `smartcardManager.js`
(moved from `js/misc/`). Every line number cited in `docs/fork/lock-screen-port.md` against
`js/gdm/util.js` is dead.

- **[R] auth services are now objects with roles and mechanisms**, not a flat list of PAM service
  names (`a70f185dd`, `1cfc5a394` "multiple mechanisms per role", `f82082aa7` role properties on the
  base class, `50ef14265` a *driver* service constrains which services are active).
- **[R] fingerprint got a dedicated `FingerprintManager` plus a "ready" state that delays showing
  the icon** (`bad0ff3ac`, `ecc49a0b7`). Our finding that arming rides the page change stays valid;
  the icon-timing behaviour is new.
- **[N] Web Login** (`js/gdm/webLogin.js`, `516015051`) and the **QR code widget**
  (`js/ui/qrCode.js`, `6e2722b1c`, `widgets/_qr-code.scss`).
- **[N] `AuthMenuButton`** (`a4ad55f15`) — the session picker and the new login-options menu are now
  one control; `unlockDialog` grew `_authMenuButton` + `_authIndicatorButton` (`2b9c203dd`).
- **[N] conflicting-session dialog on the login screen** (`js/gdm/conflictingSessionDialog.js`).
- **[R] authPrompt UX**: preemptive input is captured *before* the entry is sensitive
  (`ac213dc9c`), preemptive input is allowed again after `verificationFailed` (`fa0f5cc6e`),
  `isprint()` replaces `isgraph()` for what counts as preemptive input (`8caa35b7c`), the back
  button returns to step 1 instead of a full reset (`878968f8e`), reset is skipped after successful
  verification (`ffa78371a`), the spinner is no longer delayed (`b8700d3f9`), `_entryArea` fades in
  rather than appearing (`07bc0a159`), and both dialogs vertically centre using a fixed height
  (`c0dd8c300`, `5ffb16019`).
- **[R] `unlockDialog` waits for authPrompt destruction before switching VT** (`5f7eb7603`).
- **[R] `widgets/_login-lock.scss` +290 lines** and a new `login_dialog_item_button()` mixin in
  `_common.scss` (always-dark buttons over `$system_base_color`). `docs/fork/gnome-style-reference.md`
  is stale for this surface.
- **[N] greeter D-Bus calls converted sync → async** (`0cd0d774d`).

## 4. Input methods, OSK, text input

- **[N] "input panel" actor group** (mutter `87d6a350e8`, shell `7705c4fe0`,
  `global.compositor.get_input_panel_group()`). It sits near `keyboardBox` in stacking and is
  **implicitly subscribed as grab chrome on the stage**, so OSK and IBus candidate popups stay
  interactive under a grab that would otherwise swallow input. The candidate popup and its dummy
  actors moved into it (`8b3f2d60e`), as did the OSK (`5e5b0f49d`). This is the upstream answer to
  the CJK-candidate-popup gap in `docs/fork/input-method-port.md`.
- **[R] `text_input_v3` version 2** (mutter `f24e857bb2`, `META_ZWP_TEXT_INPUT_V3_VERSION 2`; shell
  `154b3a5bc` pre-edit style hints; clutter `a2fba02155`). mutter also had to **restore the OSK
  trigger for version-1 clients** (`62b67096d6`) after the bump — a regression worth pinning if we
  bump.
- **[R] content hints**: `NO_EMOJI` is forwarded to IBus and hides the emoji key (`79b947a9a`,
  `c4738ee1e`, `7dcbc100a`); `INHIBIT_OSK` is honoured (`f8523e6f9`).
- **[R] OSK popups use a new `NoGrabPopup` class** instead of a grabbing popup (`e38fc66e0`,
  `6b42eff85`).
- **[R] `ibusManager` no longer reuses a renewed cancellable across async calls** (`ecbfbcb78`) —
  same shape as our ibus worker starvation bug.

## 5. Input peripherals and shortcuts

- **[N] `disable-while-typing` now honours a *timeout* setting** (mutter `f63ac7cdcb`).
- **[N] button scrolling on mice** (mutter `240e033acd`) — scroll-on-button-hold for non-trackpoint
  mice.
- **[N] global shortcut inhibitors may inhibit a11y shortcuts** (mutter `85b246486d`).
- **[N] g-c-c exposes `send-events = disabled-on-external-mouse`** (`0b933753c`) — mutter has
  supported it since 3.16; only the UI is new, but it means users will start setting it.
- **[N] g-c-c `GlobalShortcutsProvider.ConfigureShortcuts(app_id, parent_window)`** (`c9de56daf`).
- **[N] g-c-c "Auto Rotate" row bound to
  `org.gnome.settings-daemon.peripherals.touchscreen orientation-lock`** (`b8bbb3f0d`).

## 6. Accessibility and motion

- **[N] `org.gnome.desktop.a11y.interface reduced-motion`** — an *enum*
  (`no-preference` / `reduce`), exposed as `St.Settings.reducedMotion` (`ccc73e9c3`). Explicitly
  **not** "disable all animations": upstream applies it in `boxpointer` (menu open/close),
  `screenShield`, `unlockDialog`, and `windowManager` (map/minimize/unminimize/destroy/size-change).
  Everything else keeps animating.
- **[N] `org.gnome.desktop.a11y.interface keyboard-focus-visible-timeout`** (int; `0` = never hide
  the focus ring) — g-c-c `8a00d2b55`, backed by gsettings-desktop-schemas !127 and GTK !10089.
- **[R] a11y roles/labels added across the app grid, folders, search results and the QS volume
  slider** (`067bf30e7`, `e5da40aa6`, `a6bb705d7`, `2e9ba91f1`). Feeds `docs/fork/a11y-port.md`.

## 7. Toolkit (St) — things our widget layer should mirror

- **[N] `text-align: start | end`** (`c712d247a`, `3c1f53423` — START is now the default,
  `822261385` decouples `StTextAlign` from Pango values, `ea9b322d6` justified text aligns to
  start). Upstream is already replacing `&:ltr {text-align: right}` / `&:rtl` pairs with
  `text-align: end` (`_calendar.scss`, `_message-list.scss`).
- **[R] `StEntry` supports the COPY/CUT/PASTE action keys** (`9975d7098`), and entry keybindings
  moved to a binding pool (`b2a78bf5c`, `2d460562a`). Feeds `ui::text_edit::TextEdit`.
- **[R] hint-text margin changed**: `_entries.scss` `margin-left: $base_margin * 0.5` → `* 2`.
- **[N] `StScrollView` supports touch scrolling** (`0cfdd9f1a`), gated by a setting
  (`d4af38315`), and ad-hoc touch handling was removed from search (`dc27d75e6`).
- **[N] SVG cursors**: `St` gained `st-cursor.c` (`ca479a211`) and gnome-shell ships a whole
  **scalable Adwaita cursor theme** in `data/theme/Adwaita/cursors_scalable/` +
  `gnome-shell-cursor-theme.gresource.xml` (`32f768792`). mutter gained
  `meta-cursor-theme.c` and "allow external cursor implementations" (`!5104`), with
  `meta-cursor-xcursor.c` shrinking by 233 lines. Relevant to `docs/fork/` cursor work and to the
  software-cursor interim.
- **[R] `St.ButtonMask` values renamed** (`a2979bb9c`, `c3b936e4d`) — cosmetic for us.
- **[N] `ShellGLSLEffect` deleted**, effects ported to `ClutterShaderEffect` (`0c86a7b35`,
  `709d0634e`); `st-scroll-view-fade.glsl` is gone, inlined via `Cogl.Snippet` (`89b599cb4`).
- **[R] libcroco was heavily pruned** (fonts, prop-list, encoding handling; UTF-8 only). Relevant
  only as a signal for the planned cssparser cascade (B1) — upstream is shrinking, not fixing, its
  CSS engine.

## 8. Event handling: raw handlers → controllers and gestures

A pervasive refactor (`!4248`, `!3912`) that changes *behaviour at the edges*, not just structure.
Everywhere below, a `Clutter.MotionController` / `ScrollController` / `KeyController` /
`BindingPool` replaced hand-written `button-press-event` / `key-press-event` / `scroll-event`
handlers:

`boxpointer` (input muting, `0518eab50`), `switcherPopup` (hover, scroll, keys — `b6bb62cf3`,
`fbfe5aab7`, `3862afb13`), `windowPreview` (hover + activation — `303eac4c8`, `314f90e07`),
`appDisplay` (pagination + scroll — `621c96e7d`, `b335bc355`), `searchController`
(`b4d872780`, `ff727b589`, `95b7b722e`), `screenShield`/`unlockDialog`/`loginDialog`
(`e2885f9d1`, `d63ae4494`, `93d01f5c9`, `9c14c8748`), `slider` (`48f58214f`, `2433479f1`),
`calendar` (month switch by scroll, `328857cc4`), `messageList`/`messageTray`, `grabHelper` (Esc),
`endSessionDialog` (Alt for boot options), `runDialog`, `shellMountOperation`, `panel`, `keyboard`.

**[R] concrete behaviour changes buried in it:**
- `search`: Ctrl+Enter is handled in the search entry (`c0e84e177`).
- `quickSettings`: the slider itself takes keyboard focus and the row mirrors its focused state,
  instead of the row focusing and forwarding events (`2e9ba91f1`).
- `st`: widgets subscribe a key controller for shortcuts automatically (`329e67488`).

## 9. Overview, workspaces, window management

- **[R] `workspaceAnimation` no longer excludes minimized windows** (`620efa31c`) — it tracks them
  hidden so a window minimized on another workspace doesn't pop in after the switch.
- **[R] workspace updates are blocked during a switch** (`eaa4e0577`, plus
  `WorkspaceTracker.blockUpdates`/`unblockUpdates` exposed in `217b15020`) — a workspace
  added/removed mid-animation used to land the animation on the wrong index. Directly relevant to
  our **mac-style dynamic workspaces** divergence, where workspaces churn more than upstream's.
- **[R] restacking records are sorted when restacking** (`e7e6de300`).
- **[R] app grid label expansion animates the whole icon tile and clips to allocation**
  (`32171c0be`, `60ec1fae0`).
- **[R] mutter window/focus behaviour**: desktop windows are de-prioritised as focus candidates
  (`58d8a94b48`), default focus-candidate filtering was unified (`3e2cfc993d`), focus is adjusted on
  window *type* change (`8256cf78cb`), fullscreen windows stay fullscreen when maximized flags are
  removed (`32f280b9f7`), a window's target monitor refreshes on monitor changes (`371973f3d0`).
  All are conformance-corpus shaped.
- **[R] `MetaWindowConfig` now carries state changes** (`9577c3b58f`, `569ac139d8`, `321d3ff9a0`,
  `9ddf9bd54b` saved-rect) — maximize/fullscreen state transitions go through a transient config
  object. Structural upstream, but it is the new place to read maximize/restore semantics from
  (`docs/fork/window-placement.md`, and the "floating owns maximize/fullscreen" rule).
- **[R] stack-tracker refactor incl. a real fix**: "Fix managed windows restack" (`66881c1b8b`),
  `keep_override_redirect_on_top` and `restack_at_bottom` reworked.
- **[N] `grab-op-begin` now carries the sprite object** (`6aa7eef2d6`); clutter gained private API
  to mark actors as grab "chrome" (`9bcaf865ba`).

## 10. Screenshot UI

Our port is complete against 50.x; 51 adds a full keyboard story to area selection (`!4013`,
`!4158`) plus HIG wording.

- **[R] area-selection keyboard model** (`js/ui/screenshot.js`, binding pool at ~line 1256):
  arrows move the *cursor*; `Ctrl`/`Shift`+arrows resize the selection at different increments;
  `Alt`+arrows move the selection (with `Ctrl`/`Shift` variants); `R` resets the selection
  (`b78ee79ba`); all X/Y are clamped in bounds (`0fd424242`); the pointer is warped to follow
  keyboard move/resize (`2b8592268`) and to the wrapped-around side (`8d7ec470d`).
- **[R] top-level shortcuts**: `s` selection, `c` screen, `w` window, `v` screencast, `p` pointer,
  Return/KP_Enter/ISO_Enter/space activate.
- **[R] `UIAreaSelector` drag is a pan gesture; pixel picking and cursor updates use
  `Clutter.MotionController`** (`7ee04782a`, `54e6a7e8b`, `0b653e2c4`).
- **[R] message text moved to title case, subtitle periods removed** (`617e7fd09`).
- **[R] `shell_screenshot` / `WindowActor.get_image` API changed and a pixel format was renamed**
  (`e7a5d3836`, `372a7dda1`); mutter "avoid using cairo in public screenshot API" (`!4172`).

## 11. Audio

- **[R] the QS volume slider must not push redundant volumes** (`bd5f0a10d`): use the boolean
  returned by `set_volume()` and only push when the effective channel map actually changed —
  otherwise a PipeWire DSP filter-sink graph crackles while dragging. We drive PipeWire directly,
  so the equivalent check is "did the route volume actually change" before writing.
- **[R] the input (microphone) indicator shows only when a stream is `running`** (`4ffbca673`),
  ignoring passive streams like echo-cancellation and denoise filters; it listens to
  `stream-changed` because streams move between idle and running.
- **[N] gnome-shell ships its own sound effects** in `data/sounds/` (`audio-volume-change`,
  `complete`, `device-added`, `device-removed`, `screen-capture`) instead of using the sound theme
  (`3e671f9f9`).

## 12. Notifications and calendar

- **[N] calendar-server grew a `ReminderWatcher`** (655 lines, `b0fcbf466` and follow-ups) — event
  reminder notifications with **Dismiss** (`6bb5356dc`) and **Snooze** (`6f69eb0c9`) buttons, a
  GApplication-based server (`99c9398d2`), a `org.gnome.Shell.CalendarServer.desktop` file, and
  clicking the notification launches the default calendar app (`8d9cc038e`, `5d4d371c9`). Our
  notifications subsystem is marked complete; this is a new producer to consider.
- **[R] `_message-list.scss` app-name label uses `text-align: end`** instead of ltr/rtl pairs.

## 13. Renderer, frame scheduling, screencast

- **[R] frame-clock scheduling was substantially reworked** (`!5138`, `!5131`, `!5183`, `!5167`,
  `!5156`): a **sliding estimate of maximum update duration** (`25551e5cc9`), a **dynamic deadline
  evasion margin** (`3c46c41515`, `3e415ceff2`), **dispatch lateness folded into the margin**
  (`931ce84c5b`, `59323c7ba6`), **minimum update duration tracking** (`8797d941cb`, `aee5396ece`),
  deadline evasion excluded from the update-duration estimate (`42488cac85`), and always trying the
  deadline timer with VRR (`9e46d65316`). This is the closest upstream analogue to our pacing work
  in `docs/fork/foundation.md`; read it before the next pacing change.
- **[N] `clutter_stage_paint_to_framebuffer_clipped`** (`ca44c59f4f`) and
  **`cogl_framebuffer_blit_region`** (`753805bcca`) — the primitives behind "minimize stage paints
  and buffer copies in screencasts" (`!5046`, `!5119`).
- **[R] the whole screencast source layer was renamed and generalised**: `meta-screen-cast-*-stream-src`
  → `meta-stream-source-{area,monitor,virtual,window}` + `meta-stream*`. Every citation in
  `docs/fork/portal-surface-port.md` / `custom-recorder` notes against those filenames is dead.
- **[N] screencast device-ID negotiation** (`607f6edc00`) — the consumer can learn the DRM device
  backing the dmabufs from the stream instead of guessing.
- **[N] `org.gnome.Mutter.Clipboard` D-Bus interface** (142 lines, `c366b31168`) — clipboard
  handling was lifted out of `RemoteDesktop` into its own interface so input-capture sessions can
  use it too (`Enable`/`Disable`/`SetSelection`/`SelectionRead`/`SelectionWrite`/
  `SelectionWriteDone` + `SelectionOwnerChanged`/`SelectionTransfer`). Directly relevant to the
  RemoteDesktop half of `docs/fork/portal-surface-port.md`.
- **[N] clutter colour management rewritten** into `ClutterColorOp` / `ClutterColorPipeline` /
  `ClutterColorTransform` (~5k lines new, `ClutterColorManager` removed, `07fca6532b`), plus HDR
  mastering-display metadata (`!5199`). Nothing to do yet; it is where to look when we do colour.
- **[N] EGLStream / EGLDevice page flipping removed** (`2842e9beaa`), legacy NVIDIA support removed
  (`!5079`), `CoglProgram`/`CoglShader` removed (`!5094`), logind is now a required build option
  (`302e9c39ac`) — relevant to `docs/fork/system-compositor.md`'s VT-less logind premise.
- **[R] fractional-viewport compensation in the projection and in framebuffer captures**
  (`a79b894122`, `308a0fccb0`) — the shape of bug we hit with rounding location and size apart.
- **[R] blurred rendering with non-pixel-aligned monitors was fixed** (`!5115`) and shader snippets
  are reapplied after resize/scale (`!5165`).

## 14. gnome-session

The whole diff is 38 files; read it if touching session end.

- **[N] `SessionIsLocked` (b) and `SessionClass` (s: `user`/`greeter`/`lock-screen`/`background`)
  properties on `org.gnome.SessionManager`** (`7db54e2c`, backed by `e2ef1772`, `453f67f6`).
- **[R] `GsmSystem` became an abstract class** with `GsmSystemNull` split out (`f1611d69`,
  `f1ae0cca`), and `gsm-systemd.c` now uses the **logind session proxy** for properties and methods
  (`dc497507`, −459/+ lines). Reinforces our "`session/auto` is not ours — use `session_path()`"
  rule.
- **[R] a screen-blanking / idle-handling regression was fixed** in beta (NEWS).
- **[N] oo7-portal is the Secret portal implementation** (`3a77ebd2`).
- Already in our 50.1 baseline, restated for completeness: `Suspend()`/`CanSuspend()`,
  `CanReboot()`, richer `CanShutdown()`, the sleep inhibitor downgraded to `block-weak`, and manual
  suspend ignoring inhibitors.

## 15. gnome-control-center

Almost entirely an Adwaita/blueprint port plus translations — no behaviour we consume. The only
items that reach us are the settings keys already listed in §5 and §6. Two more worth knowing:

- **[N] "Remote Login" supports both the `sshd` service and socket units** — affects nothing of ours
  today.
- **[R] About panel no longer shows "Windowing System" and now reports **"GNOME Shell Version"**
  rather than "GNOME Version"** — i.e. what a user reads back to us when reporting a synoik bug.

---

## What is *not* here

- mutter's X11 backend removal, Xwayland internals, EGLStream/NVIDIA legacy, and the g-c-c widget
  port: no surface for us.
- The `libcroco` pruning and `extensions-app` split (`!4267`, now its own repository): structural
  upstream only.
- mutter dropped the `experimental-features` flags key for a
  `org.gnome.mutter.experimental` schema with per-feature booleans (`kms-modifiers`,
  `autoclose-xwayland`). We read neither.
