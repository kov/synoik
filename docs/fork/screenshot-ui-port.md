# The screenshot UI — porting GNOME's control panel

Status: **2026-08-03, all five slices landed and checked on the headless harness.** The help text
is gone; the panel is GNOME's control panel — type row, shot/cast pill, capture button,
show-pointer toggle, close button and tooltips. All three capture types work, the delay is armed
from a fourth round button beside show-pointer, and the cast segment records to WebM through our
own recorder.

Reference: `js/ui/screenshot.js` (50.3) and `_screenshot.scss`. The visual spec is already cached
and cited in `docs/fork/gnome-style-reference.md` §screenshot (every class, colour, radius and
padding below comes from that table — re-grep the SCSS only if a row looks wrong).

## What GNOME builds, in construction order

Child order comes from the JS, never from what looks right (`screenshot.js:1226-1445`):

```
ScreenshotUI
├── _areaSelector            shade quadrants, selection outline, 4 corner handles
├── _primaryMonitorBin
│   ├── _panel               vertical, y_align END, %osd_panel
│   │   ├── _typeButtonContainer     [ Selection | Screen | Window ]   spacing 12, homogeneous
│   │   └── _bottomRowContainer      (BinLayout — all three overlap, aligned)
│   │       ├── _shotCastContainer   x_align START   [ shot | cast ]
│   │       ├── _captureButton       centred
│   │       └── _showPointerButtonContainer  x_align END  [ show-pointer ]
│   └── _closeButton         constrained to the panel, outside it
└── Tooltips                 added to the ROOT, so they draw above the panel
```

Three details that are easy to get backwards: the bottom row is a **BinLayout**, so the capture
button is centred on the panel rather than placed after the shot/cast pair; the tooltips are
children of the root widget, not of the buttons they describe; and the close button is **not fixed
to one corner** — it has `.left`/`.right` variants (`_screenshot.scss:22-23`) and an X-align
constraint GNOME flips as the selection moves (`_closeButtonXAlignConstraint`,
`screenshot.js:1262-1267`).

## What we have today

`src/ui/screenshot_ui.rs`. Area selection on a single output, drag/move/resize by pointer and by
keyboard, the shade and selection chrome, and GNOME's control panel: the three type buttons
(Window built but not offered), the shot segment, the capture button, the show-pointer toggle and
the close button, all hover/checked/active-styled and hit-tested off one shared `PanelLayout`. `P`
still toggles the pointer and `Space` still saves. No cast control, no tooltips, no window selector.

Two things the panel's shape now fixes in place, worth knowing before touching it:

- **One `PanelLayout` feeds the bake, the glyph placement and the hit test.** The panel is a single
  baked texture, so nothing structural ties a control's pixels to its clickable rect — that layout
  is the only thing that does, and `vulkan_screenshot_ui_type_buttons_take_clicks_where_they_are_drawn`
  is the test that fails if they ever drift apart.
- **Icons are render elements, not paint verbs**, so they ride *on top* of the bake and fade with
  it. They also arrive a frame late (the symbolic worker rasterizes off-thread and queues a redraw
  on arrival), which is why a first-frame capture of the panel can be glyphless and mean nothing.

## Slices

1. **DONE.** The panel became GNOME's, with Selection and Screen. `Window` is built but hidden —
   **our scaffolding, not GNOME's practice**: GNOME never hides `_windowButton`. (It does hide
   `_castButton`, but on a runtime capability — `visible = this._screencastSupported`,
   `screenshot.js:1524` — which is the parallel to slice 4, not to this.) `CaptureType::is_available`
   is that debt, labelled as such; the row still sizes itself homogeneously over Window's caption so
   unhiding it does not resize the panel. Two divergences taken deliberately: the close button is
   always on the right (`Meta.prefs_get_button_layout()` is unported, as in `window_preview`), and
   in Screen mode the keyboard selection actions are inert rather than silently breaking the
   whole-output invariant.
2. **DONE.** Window mode. Every window on every output's active workspace is frozen at open
   (`capture_screenshot_window_neutrals`) and the selector picks from those copies, not from live
   windows — the difference `screenshot_window` does not make. Slots come from the same exposé
   layout the overview picker uses (`UIWindowSelectorLayout extends WorkspaceLayout`), over the
   monitor less GNOME's 100px margin and 200px at the bottom. The Window button is now
   **insensitive, not hidden**, which is what GNOME does — `CaptureType::is_available` is gone.

   Two things to know before touching it. The per-window capture takes **no pointer**: it is
   composited at save time from the output's own pointer neutral, so the show-pointer toggle still
   works after the freeze rather than being baked in. And a window's *buffer* is bigger than the
   frame rect its slot was sized from (shadows, CSD margins), so the thumbnail scales by the
   slot/frame ratio and is centred on the slot — anchoring at the slot corner offsets every window
   by its own shadow.
3. **DONE.** Delayed capture — our divergence, see below. The button cycles off → 3s → 10s and
   reads as `:checked` when armed, carrying its own baked number in place of `alarm-symbolic`.
   Arming lifts the whole capture out of the picker into `Niri::pending_capture` and dismisses the
   picker, because the delay only means anything with the shell out of the way; the shot is then
   taken from the **live** screen by `screenshot_area`/`screenshot_window`.

   All six design points below are handled. The two worth restating: the D-Bus reply is **taken**
   out of `Niri` as the capture arms, so `close_screenshot_ui`'s unconditional `None` cannot tell a
   still-waiting caller it was cancelled; and the countdown card refuses every `RenderTarget` but
   `Output` inside `Countdown::element`, so there is no way to draw it into a shot, a cast or a
   portal capture. A one-second timer drives it, but the *clock* decides when it fires — a
   coalesced wakeup shortens the last tick rather than the delay.

   Two calls this slice made, both deliberate: a `SelectArea` caller is answered immediately and
   the delay is ignored, because it wants coordinates and already has them; and the countdown is
   pushed *after* the lock branch's early return, so a lock that lands between two ticks cannot get
   a countdown drawn over its surface even for the one frame before the tick cancels.
4. **DONE.** Cast mode. The segmented control has both halves, the capture button's inner circle
   goes `$red_4` under `:cast`, and clicking it starts a native recording of the selected geometry
   (whole output in Screen mode, which passes **no crop** rather than a whole-output one — the crop
   path relocates every frame into a smaller buffer for nothing). Stopping it notifies with a way
   into Files, the same shape as the screenshot notification. This is the real trigger for the
   panel's recording indicator (`docs/fork/panel-status-port.md` R1), which no longer needs a
   direct `RecordArea` call to exercise.

   The thing that made this more than a mode flag: **in Shot mode the frozen screenshot *is* the
   desktop.** `render_inner` returns early when the picker is open, because the still stands in for
   the scene. Cast mode has no still — a recording is of the live screen, and showing a photograph
   of the moment the picker opened while claiming to record the present is a lie — so it must fall
   through and let the real scene draw underneath. Dropping the still without lifting that early
   return leaves a void, which is exactly what the first run produced.

   Window mode is refused while casting, as in GNOME (`_startScreencast` returns early on
   `_windowButton.checked`); `window_enabled()` is now the single authority the bake, the hover
   filter and `set_capture_type` all read, so there is one place that can be wrong.

   The recording notification comes from `State::stop_screen_recordings`, not from
   `Niri::stop_screen_recordings`, so a `org.gnome.Shell.Screencast.StopScreencast` caller does not
   get one — in GNOME the shell UI notifies and the recorder service does not.
5. **DONE.** Tooltips. `widget::Tooltip` and `Painter::tooltip` already existed; what this added
   is the timing (300ms delay, then a 150ms fade) and root-level placement — centred on the
   control, clamped into the output, 24px *above* it, and pushed before the panel so it draws over
   it. The delay is the feature: without it a pointer crossing the row strobes every tip on its
   way past. Two consequences worth keeping: a pending tip has no animation yet but must still
   keep the redraw loop alive, or it never becomes due; and an insensitive control takes no hover
   at all, which is what stops the Window button lighting up and advertising a mode it refuses to
   enter.

## Approved divergence: delayed capture

Requested 2026-08-03. GNOME's shell UI has no delay (gnome-screenshot had one; the shell dropped
it). We want it, and it is not a button we can bolt on at the end, because of what the picker is.

**The picker shows a frozen screen.** Opening it captures the output through the renderer
(`capture_screenshot_neutrals`, `src/niri.rs:3416-3423`) and draws that texture; `Space` saves by
cropping the frozen buffer (`confirm_screenshot` → `capture_from_neutral`, `src/niri.rs:3480`).
GNOME's model is the same (`_stageScreenshot`, `screenshot.js:1211-1216`). A delayed shot is
therefore *not* a delayed read of that texture — the point is to capture something that has not
happened yet. The sequence has to be: arm and dismiss the UI → unfreeze → count down visibly →
capture the **live** screen → save through the ordinary path (clipboard, notification,
`Screenshots/`).

Six things that must be designed in slice 1, not discovered later:

- **Arming must not answer the D-Bus caller.** `close_screenshot_ui` answers `SelectArea` and
  `InteractiveScreenshot` with `None` **unconditionally, by design** (`src/niri.rs:10685-10691`) —
  precisely so a dismissal can never leave a caller blocked until its timeout. Arming a delay goes
  straight through that path, so a delayed `InteractiveScreenshot` would be told "cancelled" before
  the countdown even starts. The pending capture must **take** the reply sender (and the `path`
  from the `Open` variant, `screenshot_ui.rs:81`) as it arms, so the answer travels with the
  pending capture and is delivered at fire time. This is the single change slice 1 most needs to
  get right.
- **Where the state lives.** Not in `ScreenshotUi::Open` — it outlives the picker. It belongs
  beside `select_area_reply`/`interactive_screenshot_reply` on `Niri` (`src/niri.rs:824-829`),
  driven by a loop timer. It carries: mode, output, rect-or-window-id, `show_pointer`, the taken
  reply sender, and the target `path`.
- **Hold the output weakly.** `Closed::last_selection` already uses `WeakOutput`
  (`screenshot_ui.rs:69`) for exactly this reason, and the picker itself closes on output change
  (`src/niri.rs:6639-6647`). Unplug mid-countdown cancels.
- **A lock mid-countdown must cancel it, fail-closed.** `open_screenshot_ui` refuses while locked
  (`src/niri.rs:3402`), but nothing stops a lock landing *during* a countdown, and a live capture
  then photographs the lock screen — or races `WaitingForSurfaces` and captures pre-lock content.
  That is the ClosingWindow fail-closed rule in a new place: when in doubt, capture nothing.
- **The countdown must not appear in the shot.** It is on screen at fire time by definition. Either
  hide it for the capture frame or give it a render-target block, the way the capture targets
  already distinguish output/screencast/screen-capture — otherwise the default outcome is a "1"
  burned into every delayed screenshot.
- **Escape must cancel, with the picker closed.** Today Escape reaches the picker only while it is
  open (`src/niri.rs:2551`); a countdown needs its own route.

Cast mode compounds it: a delayed *recording* start ends the countdown by starting a stream, not by
taking a frame. So the fire action is an indirection from the start, not a call to
`confirm_screenshot`.

Where the control goes: the bottom row already ends with show-pointer, and a delay is the same kind
of persistent capture option, so it belongs beside it — keeping the type row (*what* to capture)
and the bottom row (*how* to capture it) separated the way GNOME has them.

## The cursor

GNOME sets the crosshair on **`_areaSelector`**, not on the picker
(`set_cursor_type`, `js/ui/screenshot.js:448`), so the panel's buttons are siblings that inherit the
default, and leaving Selection mode resets it outright (`:1792`). We set it once for the whole
picker and never changed it, so it stayed a crosshair over every button — reported 2026-08-03,
fixed the same day. It is now derived in `update_hover` (same hit test as the hover) and applied
through `State::handle_screenshot_ui_motion`, one funnel for all four motion call sites, plus a
re-sync on click so switching to Screen mode drops the crosshair without the pointer moving.

**The rest of `_computeCursorType` landed 2026-08-03**, with the interaction it needs. Ported whole
(`js/ui/screenshot.js:354-398`): four corner handles hit-tested as **circles** of their own radius,
then edge bands `10 * scale` wide that sit *outside* the rectangle, then `MOVE` for anything
strictly inside. `area_target` answers both questions at once — what the cursor is and what a press
would grab — because a cursor promising a drag the press does not start is the bug this replaced.

Three things worth keeping:

- **The grab is stored as axes, not as a cursor.** GNOME keeps `_dragCursor` and *rewrites* it on
  every motion when a handle crosses the far side (`:672-709`). We store which sides the pointer
  drives and derive the cursor from where the moving corner currently is, so the two cannot
  disagree. The flip falls out for free.
- **The press warps the pointer**, as GNOME's does (`:519`), through `State::move_cursor` — the same
  path `warp-mouse-to-focus` uses. Not cosmetic: an edge is grabbable from 10px outside it, so
  without the warp every later delta would be measured from a pointer sitting beside the thing it
  drags. `pointer_down` therefore returns `PointerDown`, and `State::handle_screenshot_ui_pointer_down`
  is the one funnel that performs it — the same shape motion and release already had.
- **The cursor is set at three moments, and all three are needed.** On press (taking a handle is a
  cursor change with no motion behind it, `:465`), *after* applying a motion (before it, and a
  flipped handle advertises its old corner one motion too long), and on release (`:578-581` — the
  rectangle under the pointer is no longer the one the drag began on).

**Found while doing it, and fixed:** a trip Selection → Screen → Selection destroyed the area. Ours
reuses `selection` for Screen mode, widening it to the whole output so the capture path needs no
special case; that overwrote the rectangle. It is now put aside on the way out and handed back on
the way in. Nothing can lose it in GNOME — Screen mode draws a different widget.

**Still not ported:** the click-without-dragging fallback expands by `20 * scale` in GNOME
(`stopDrag`, `:412-441`) where ours expands by a flat 16px, and only when the result is empty or
1×1 rather than when the pointer never moved. Both produce "a larger selection to reduce confusion";
the constants differ.

## Toolkit first

Checked against `src/ui/widget.rs` rather than assumed:

- **Already exists, use it:** `widget::IconButton` (`widget.rs:2212`) is the circular one-glyph
  button — the close button, the show-pointer toggle and the two shot/cast segments are all that
  shape. `widget::Tooltip` + `Painter::tooltip` (`widget.rs:638-670`) cover slice 5.
- **Genuinely new:** the **icon-over-label button** (32px icon above a 9pt caption) for the three
  type buttons. It needs a checked state over `%osd_button_flat` that `ButtonStyle` does not have
  (`widget.rs:2126-2138`). Note it does *not* recur in GNOME — `IconLabelButton` is defined inside
  `screenshot.js:53-73` and used only here — so it earns `widget::` on the checked-state gap and on
  our own tenet, not on a false recurrence claim.
- **Genuinely new:** the **segmented toggle** (a pill of translucent white holding buttons whose
  `:checked` inverts to white-on-dark). Also unique in 50.3 (`_screenshot.scss:83,95`); the
  quick-settings split toggle is a *different* construct (`_quick-settings.scss:57-77`). Build it
  as a widget for the composition, not because it recurs today.

## Icons

`preview-close-symbolic` is already vendored in `resources/icons/`. Needed from the reference
checkout (`gnome-shell/data/icons/scalable/actions/`, GPL, same practice as the icons we already
carry): `screenshot-ui-area-symbolic`, `screenshot-ui-display-symbolic`,
`screenshot-ui-window-symbolic`, `screenshot-ui-show-pointer-symbolic`. The shot/cast buttons use
`camera-photo-symbolic` and `camera-web-symbolic` from the icon theme.

## Cover

The corpus can open the picker only through the renderer (it has to freeze the screen first), so
the Vulkan tests in `src/tests/vulkan_render.rs` are where this is pinned —
`vulkan_screenshot_ui_draws_the_help_panel` (`:413`) is the test that has to change shape first,
since the help panel it asserts on is what slice 1 deletes.

The picker's cover is split by what actually needs a device, not by which file was convenient.

**The corpus drives real controls with no Vulkan device.** Two things used to require one, and
neither had to. A `ScreenshotNeutral` is plain `MemoryBuffer` pixels, so `open_screenshot_ui` was
split at exactly the renderer boundary — `open_screenshot_ui_with` takes neutrals that have already
been captured, and the corpus hand-builds them. And `PanelLayout` is arithmetic over measured
captions, so the measuring was lifted out of the bake into `CaptionMetrics` and
`ScreenshotUi::lay_out_panels` installs a layout without one. `open_picker_headless` in
`src/tests/gnome.rs` does both; everything downstream — the hit test, `activate`, the D-Bus
contract, the cancellation rules — is the production path. What the corpus does *not* get is pixels:
with no texture the panel draws nothing, so any claim about how something **looks** belongs in
`vulkan_render.rs`.

A related redundancy went with it: `PanelCache::size` read the panel's physical size back off the
*texture*, when `generate_panel` sizes that texture as exactly `physical_size(scale, layout.size)`.
Two sources for one number, one of which does not exist until a renderer has baked. It now comes
from the layout.

In the corpus (`src/tests/gnome.rs`), device-free, beside `select_area_always_answers_its_caller`:

- `arming_a_delayed_capture_does_not_answer_its_caller` — the cycle through the three stops, and the
  D-Bus contract on both sides: arming stays silent, cancelling answers.
- `a_lock_mid_countdown_cancels_the_delayed_capture` — with a tick *before* the lock asserted to
  keep counting, so it cannot pass for the wrong reason.
- `cast_mode_refuses_window_capture` — cast takes Window mode away, refuses a click that reaches the
  insensitive button anyway, and gives it back on the way out.

In `src/tests/vulkan_render.rs`, where skipping without a device is the file's stated contract:

- `vulkan_screenshot_ui_a_delayed_capture_shoots_the_live_screen` — the window is recoloured during
  the countdown and the saved PNG must not contain a pixel of the old colour. This is the whole
  divergence in one assertion.
- `vulkan_screenshot_ui_countdown_cannot_reach_a_capture` — the same pixel rendered at `Output` and
  at `ScreenCapture`, against a reference capture taken before any of it.
- `vulkan_screenshot_ui_cast_mode_drops_the_frozen_screen` — recolours the live window *after*
  switching to cast, which is the only way to tell "the still is gone" from "the still happens to
  match".
- `vulkan_screenshot_ui_cast_mode_capture_starts_a_recording` — here despite having no pixel claim,
  because starting the recorder spawns a real ffmpeg and the corpus should not need one.

One trap for anything else that wants to read a screenshot back in a test: `save_screenshot`
answers its D-Bus reply from an **event-loop source**, so `recv_blocking` on that channel deadlocks
the loop that would answer it. Pass an explicit path and wait for the file instead.
