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
- **`ToggleAppExpose`** is an action (config, IPC, and a GNOME-style keybinding slot) so the mode
  is reachable without a touchpad, and so the corpus and a live session can drive it.

## Cycling apps

Tab / Shift+Tab, and a **horizontal** three-finger swipe, move to the next / previous running app
in the switcher's order (most recently used first), re-laying every display out for the new app.
A swipe is one step per lift, decided by its direction once it crosses the same 16 px threshold
that tells vertical from horizontal. Arrow keys move a selection between the previews by geometry,
Enter activates it.

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

1. The **live desktop**, as normally drawn — minus the app's windows on the active workspace,
   which are drawn by step 3 instead.
2. The **blurred wallpaper**, pushed at App Exposé's progress over the live desktop. That fade *is*
   the other windows leaving; they need no animation of their own.
3. The app's **windows at their slots**, with their preview chrome:
   - on the active workspace they interpolate from their live rect to the slot, with the picker's
     own placement function at zoom 1;
   - on any other workspace they have no on-screen origin, so they scale up into the slot while
     fading in.
4. The **panel**, unchanged apart from the overview's background fade.

The overview's chrome block, the thumbnail strip, and the picker's per-workspace wallpaper and
shadows are all gated off. Input to the chrome is gated by the same visibility predicate as the
rendering, so nothing invisible can take a click — the dash's lesson under the lock screen.

## Input

- A keyboard focus of its own (`KeyboardFocus::AppExpose`), so the overview's type-to-search
  path never sees a key.
- A display-level hit test over the new layout, front to back, hovered preview first.
- Click on a preview activates it (switching workspace if needed); click on its close button
  closes the window; click elsewhere leaves.

## Testing

Corpus, in its own `src/tests/app_expose.rs` next to `gnome.rs`, driven the same way: the action
opens and closes it; the grid holds exactly the focused app's windows on that
display's workspaces, minimized included; displays are independent; a display without the app is
empty; activating a window on another workspace switches to it; Escape and swipe-up leave; locked
or unfocused is a no-op; a key never starts a search; Tab cycles apps. The swipe-down-from-desktop
no-op that `touchpad_swipe_walks_the_overview_states` pins changes meaning here.

Render tests: no chrome is drawn; the backdrop is the blurred wallpaper; a preview lands at its
slot.

Live: `synoik msg input` gains a swipe (begin / update / end), alongside the finger-scroll and hold
injection, so the gesture itself is drivable on a running session.
