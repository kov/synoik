<!-- SPDX-License-Identifier: GPL-3.0-only -->

# App Exposé

**An approved functional divergence from GNOME**, agreed 2026-10-01. gnome-shell has no
per-application window view; macOS calls this App Exposé. Three fingers *up* opens the overview,
as in GNOME; three fingers *down* from the desktop opens this instead. GNOME's source is still the
reference for every piece it reuses — the blurred backdrop, the picker's grid, the preview chrome —
this doc covers only what is new.

## What it is

- **Which app:** the app of the focused window, resolved through `AppSystem` the way the app
  switcher's `switch_group` does. No focused window, or a session that is locked: the swipe is
  consumed and nothing happens. No affordance.
- **What each display shows:** the blurred wallpaper (the overview's radius and brightness) and
  one grid of that app's windows that live on **any** of that display's workspaces, minimized ones
  included. A display with none of them shows the blurred wallpaper alone.
- **What it does not show:** the thumbnail strip, the dash (in either of its homes — the dock is
  hidden too), the search entry, the app grid. Type-to-search is off. The top panel stays, with
  its background faded exactly as the overview fades it.
- **What each window keeps:** the per-window preview chrome — hover outline, caption, close
  button — because that is the window, not the shell.

## Entering and leaving

- **Swipe three fingers down** while the overview is closed. It tracks the fingers, and the
  overview's release rule decides whether it commits (`overview_gesture_target`, the 0.6 px/ms
  flick threshold, one snap point either way). A swipe never crosses from the overview into App
  Exposé or back: swiping down out of the overview still stops at the desktop.
- **Leave** by swiping up, pressing Escape, or clicking empty space — all three leave the desktop
  as it was.
- **Click a window** to activate it. If it lives on another workspace, that workspace becomes its
  display's active one first, and the leave animation lands the window where it now is.
- **`ToggleAppExpose`** is an action (config and IPC) so the mode is reachable without a
  touchpad, and so the corpus and a live session can drive it. It has no default key.

## Cycling apps

Tab / Shift+Tab, and a **horizontal** three-finger swipe, move to the next / previous running app
in the switcher's order (most recently used first), re-laying every display out for the new app
at once, with no transition between the two grids. A swipe is one step per lift, taken as soon as
it crosses the same 16 px threshold that tells vertical from horizontal.

The arrow keys pick a preview by geometry — the first press the focused window's — and Enter goes
to it. The pick shows as the hover overlay until the pointer moves, which takes the overlay back.

## Model

**A parallel state, not a mode of the overview.** The overview's progress is the master gate for
nearly everything the overview draws: picker, dash, strip, backdrop, panel fade, per-workspace
wallpaper and shadows, and the zoom derived from the chrome layout. App Exposé turns off more of
that list than it shares, so threading a mode bit through the overview would put a check on every
one of those sites, and miss one. Instead `Layout` holds

```text
app_expose: Option<AppExpose { windows: Vec<W::Id>, open: bool, progress: OverviewProgress-like }>
```

next to `peek_open`/`peek_progress` (the precedent for a second overview-shaped state), stamped
onto every monitor alongside the overview's. The only thing the two share at runtime is the
blurred backdrop, which reads the larger of the two progresses. The overview and App Exposé are
mutually exclusive: opening either one closes the other.

Negative values on the overview's existing 0..2 state axis were rejected: they look tidy, and turn
every reader of that axis into a sign check.

**The window set is decided by `Synoik`, not `Layout`.** `Layout` knows nothing about apps, so the
window ids are computed at open (and at every cycle) and handed in. A window of the app that maps
while App Exposé is up is not added; one that closes leaves the grid.

**Focus decides, deliberately.** `docs/fork/multi-display.md` §5 says the pointer decides and focus
is the keyboard's fallback. "This app" can only mean the focused one, so this is the one gesture
that is focus-scoped by nature — a known exception, not drift.

## Layout

**One grid per display, spanning its workspaces.** The picker lays out one workspace at a time
(`Workspace::expose_layout`), so it cannot be reused whole; `Monitor` gathers the app's tiles from
all its workspaces and feeds `expose::compute_slots` directly, as the screenshot UI does. Every
workspace on a display shares its view size, so the workspace-local rects are already in one frame.

- **Inputs** are the settled rects (never the animated ones — see `expose_live_inputs` for why)
  in stable-sequence order, minimized tiles included.
- **Area** is the display's working area with the picker's symmetric strut inset
  (`Workspace::expose_area`), at zoom 1: there is no chrome to make room for.
- **Retention** follows `docs/fork/picker-layout-retention.md`: the grid decision is held and
  re-packed while the inputs are unchanged, so a window closing does not re-seat the others.

## Rendering

Back to front:

1. The **live desktop**, as normally drawn — minus every window on the active workspace, which
   step 3 draws instead.
2. The **blurred wallpaper**, pushed at App Exposé's progress. Unlike the overview's backdrop this
   one must cover, so with no blur to be had the solid backdrop stands in.
3. The active workspace's windows **in their stack order**, so a window of another app between
   two of the app's stays between their previews the whole way rather than popping into place
   when the desktop takes over:
   - the app's windows interpolate from their live rect to their slot, with the picker's own
     placement function at zoom 1, and draw with their preview chrome; the hovered one draws
     on top;
   - every other window stays in its place and fades out at `1 − progress`, each through an
     offscreen of its own so it fades as one picture — one per window, only while the
     animation runs (none at either end). The fade over the backdrop *is* those windows leaving.

   The app's windows on any other workspace have no on-screen origin, so they scale up into the
   slot while fading in — composited as one group, since their slots never overlap.
4. The **panel**, unchanged apart from the overview's background fade.

The overview's chrome block, the thumbnail strip, and the picker's per-workspace wallpaper and
shadows are all gated off. Input to the chrome is gated by the same visibility predicate as the
rendering, so nothing invisible can take a click — the dash's lesson under the lock screen.

## Input

- A keyboard focus of its own (`KeyboardFocus::AppExpose`), so the overview's type-to-search
  path never sees a key.
- Hover, the overlay, the close button and the hit tests are the picker's own: App Exposé's
  previews feed `Monitor::preview_rects` and `Monitor::window_under`, and the overlay is enabled
  only once it is fully up, as the picker's is only at state 1.
- Click on a preview activates it (switching workspace if needed); click on its close button
  closes the window; click anywhere else leaves. The panel stays clickable over it.

## Testing

Corpus, in `src/tests/app_expose.rs`, driven like `gnome.rs`: which windows the grid holds
(every workspace, minimized included, per display), the no-ops (nothing focused, over the
overview), locking, the dock, every key, clicks, hover, and the swipes — down, up, turned back,
over nothing focused, out of the overview, sideways. Render tests in `vulkan_render.rs`: a
preview lands at its slot with its live copy gone, another app's window is covered, and a window
from another workspace is blended part-way up.

Live: `synoik msg input swipe-begin` / `swipe-update DX DY` / `swipe-end` drive the gesture on a
running session (synthetic swipes have natural scrolling off: negative `dy` is towards App Exposé).
