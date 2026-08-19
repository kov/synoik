<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Workspace peek — the thumbnail strip on the live desktop

**Divergence, deliberate.** gnome-shell reaches the workspace thumbnail strip only through the
overview. This makes the same strip available on the plain desktop, on **Super+Shift**, and gives an
interactive window move a drop target while it is down. There is no reference to port; this document
is the specification.

## The gesture

**Hold the overlay key (`org.gnome.mutter overlay-key`, default `Super_L`) and hit Shift.** Out
come the overview's two pieces of furniture over the live desktop: the workspace thumbnail strip
slides down from the top of the work area, semi-transparent, and the dock slides up from the bottom.
Nothing else changes — no window spread, no search entry, no dimming, and the windows do not move.
**The overlay key is the hold**: releasing it puts both away, whatever Shift is doing. Shift is only
the trigger, and letting go of it changes nothing — but hitting it again **toggles**, so the key
that summoned the strip also dismisses it without letting go of Super. Either Shift triggers: the
gesture is "the hand already on Super, the other thumb on Shift", and which one that is depends on
the hand.

The trigger is a *key*, not a *duration*. A hold cannot be one, because Super held down already
means something: it is the modifier half of Super+drag, of Super+click, and of every Super chord.
An affordance that fires on the passage of time fires in the middle of all of them — dragging a
window to the top edge to maximize it takes longer than any threshold worth having.

| what happens | result |
| --- | --- |
| Shift, with the overlay key held | the strip comes down |
| Shift again, still held | the strip goes away; the next hit brings it back |
| release the overlay key | the strip dismisses; the overview does **not** open |
| release Shift, keep holding | nothing — the strip stays |
| any other key while held | the strip dismisses (if up) and the chord fires |
| an action fires, by any route | the strip dismisses — see below |
| Super+MMB (the resize chord) | the strip dismisses and the resize begins |
| **while the overview is open** | nothing comes down; the strip is already on screen in its own band |
| **over a fullscreen window** | nothing comes down — see §8 |

Shift spends the overlay key's tap on the way in, exactly as any key pressed under Super always
has, so the release that ends a peek was never going to open the overview anyway. The peek claims it
regardless, because it must **forward** that release where the tap swallows it (§4).

A toggle acts on the trigger's **edge**, so a repeat of a Shift already down flips nothing. A
physical keyboard never repeats a press without a release — repeat is generated client-side from the
keymap — but a `zwp_virtual_keyboard` client can send anything, and the old arm was idempotent where
a toggle is not.

The order is part of the gesture: Super, *then* Shift. An overlay key pressed while any other
modifier is already down never arms — mutter's rule for the tap, which the peek inherits by riding
the same press. A client holding a keyboard-shortcuts inhibit prevents arming for the same reason
and at the same point.

The strip comes down on every output at once, as the overview's does. The dock is single-output by
construction (`Dock::show` takes the output whose edge was pushed); a peek shows it on the active
one. It is a pointer affordance: touch has no Super, and no touch path arms it.

## The overlay key's tap has a ceiling

Independent of the peek, and shipped with it: **a hold longer than `OVERLAY_KEY_TAP_LIMIT` (500 ms)
is not a tap.** Its release toggles the overview neither open nor shut.

**Divergence.** mutter fires the overlay key on release however long it was down, so a thumb resting
on Super throws the overview at you on the way off — an accident that happens often enough to be
worth a rule. Nothing is asking for the overview at the end of a long hold.

Nothing *happens* when the limit elapses, so nothing schedules a wake-up for it: it is read once, at
the release, against the instant recorded when the key went down. That instant and the keycode live
in one field (`Synoik::overlay_key_hold`) — two would be a desync waiting for a path that clears only
one, and a stale instant reads every later tap as a long hold, which is the overview becoming
unreachable by tap. With no timer behind it a stale hold does not self-heal, so every path that can
swallow the key's release must reach `end_peek` (the lock/shield teardown does, unconditionally).

## What the strip does while it is down

- **click a thumbnail** — switch to that workspace. The strip stays up and scrolls to follow the new
  active workspace. Clicking the *active* thumbnail does nothing.
- **drag a thumbnail** — reorder the workspaces.
- **drag a window onto a thumbnail** — move it to that workspace, and **stay where you are**:
  sending a window away is filing it, and following it turns one gesture into two. This is the
  existing interactive move; what is new is that the strip is a drop target for it outside the
  overview, and that as the pointer climbs toward the band the dragged window scales down toward
  thumbnail size. That
  feedback lives in the interactive-move render path, lerping the carried window's render scale by
  the pointer's proximity to the band; it is the one piece of this with no existing home.
- **drop into a gap** — open a new workspace there, as on the overview strip.
- **drag a dock icon onto a thumbnail** — launch that app on that workspace. This is why the peek
  brings the dock out and not only the strip: the dock is hidden until the pointer *pushes* into the
  bottom edge (`src/ui/dock.rs`), and that gesture is unreachable while the other hand holds Super
  and the pointer is travelling to the top of the screen. With no dock there is nothing to drag, and
  the strip's app-drag drop target (`Layout::drop_workspace_at`, `src/layout/mod.rs:5934`, reached
  through `insert_position`'s strip branch) would be dead code on the desktop.

The peek takes a **hold** on the dock while it is up, the same one an icon drag and an open context
menu already take (`Dock::set_hold`). Releasing the key releases the hold, which re-arms the dock's
ordinary hide deadline — so a dock the pointer is resting on stays, and one it is not slides away
after the usual grace period. No new dock state, and the drag hold means carrying an icon to the
strip keeps the dock out even if the key comes up mid-drag.

## The shape of the change

Every consumer of the strip — render, hit test, close button, reorder grab, drop target — funnels
through `Monitor::thumbnail_strip()` (`src/layout/monitor.rs:2293`), gated by `thumbnails_visible()`
→ `expose_progress()`. So the peek is **a second visibility source and a second band**:

- `Monitor::peek_progress()` — a `0 → 1` animation of its own, and `thumbnails_visible()` becomes
  "the overview exposes the strip, or the peek does". `expose_progress()` itself is untouched: it
  drives the window spread, which the peek must never do. It is gnome-mode-only
  (`monitor.rs:2266-2268`) and the peek inherits that gate.
- the band: the overview's is `controls_layout().workspace_row`, at the floating search entry's
  midline, well down the canvas. The peek's is full width, pinned just below the top of the work
  area, at the same `small_workspace_height` — computed from the same `start_y`, the work area's
  top, as the overview's — so a thumbnail is the same size in both and the strip does not resize if
  the overview opens over a peek. `lay_strip` (`:2317`) chooses between the two.

Two things are already built and already right. The entrance is `thumbnail_slide_offset` (`:2671`),
`-extent * (1 - progress)` — a slide from off the top edge. The transparency is
`push_group_at_alpha` (`src/synoik.rs:11427`), where the overview passes its search cross-fade and
the peek passes a constant below 1.

Most of the interaction plumbing genuinely does come along from the visibility gate alone: hover
tracking, the reorder grab, phantom slots, `via_strip` insert hints, gap-drops-create-workspace, and
the close button on an empty workspace are none of them overview-gated.

## What does not come for free

### 1. Two accessors that shape the strip also shape the live desktop

They must be **split**, not repointed. Feeding peek progress into either one deforms the desktop
behind the strip:

- `workspace_background_radius()` (`:1557`) is read by the strip *and* by the live workspace bake
  (`ws.update_render_elements(is_active, background_radius)`, `:1623`). At peek, `overview_zoom()` is
  1, so the clamp lands on the full constant and the desktop wallpaper would grow ~30 px rounded
  corners.
- `workspace_inactive_ramp()` (`:2014`) feeds `workspaces_render_geo()` (`:2797`) — the geometry of
  the **real desktop**, not just of thumbnails. With the ramp above zero and a fractional scroll
  position — exactly the click-a-thumbnail-while-peeked case — the live desktop would visibly shrink
  mid-switch, overview-style.

Only `render_thumbnails`' own progress read (`:3165`) is strip-local. The strip needs its own radius
and its own shrink ramp, derived from the peek progress, with the desktop's left reading
`expose_progress()`.

### 2. Inactive workspaces are culled from render updates when the overview is closed

`update_render_elements` iterates `workspaces_with_render_geo_mut(true)` (`:1623`) and the cull drops
every workspace whose geometry does not intersect the output — overview closed, that is all but the
active one. Their thumbnails would draw stale. The cull must admit every workspace while the peek is
up; the cost is bounded to the peek and is work the overview already does continuously. Frame
callbacks are a second question with the same shape: whether a window visible *only* in a thumbnail
gets callbacks and queues damage (`src/synoik.rs:12278`), or animating clients freeze in the strip.

### 3. A window dropped on the strip would edge-tile

`edge_tiling`, `keep_position` and `allow_to_activate_workspace` all key off `self.overview_open`
(`src/layout/mod.rs:3210`, `:4845-4853`). The overview never has strip drops and edge tiling live at
once; the peek is the first context that does. A strip drop computes
`pos_within_workspace = (pointer - thumb.loc).downscale(zoom)` with `zoom` 1 at peek (`:4877`), so
thumbnail-local coordinates near the top of the screen reach `edge_tile_target` and the window
maximizes instead of moving.

The fix is to key those three off *the drop being a strip drop* rather than off the overview being
open — which `insert_position` already knows, since it returns from its strip branch. That also
settles where a strip-dropped window lands: `keep_position`, i.e. it keeps its geometry and only
changes workspace, in the peek exactly as in the overview.

### 4. The trigger cannot ride `overlay_key_armed`

That flag is cleared by **any pointer button, scroll, or touch** (`event_cancels_overlay_key`,
`src/input/mod.rs:9745`, applied at `:488`) and overwritten by any other key press (`:950`). The
first click of any peek interaction disarms it, so it cannot be the record that Super is down.

The peek gets **its own state** — `Synoik::overlay_key_hold`, the keycode that armed it and when —
and is dismissed on that keycode's release, which the keyboard path sees whatever the pointer did in
between.

**Pointer activity does not cancel the peek.** It cancels the *tap*, and must: mutter will not let
Super+click open the overview. The peek has the opposite relationship to the pointer — it is a
pointer affordance, and pointer activity is evidence the user wants it. Reaching for Shift after
grabbing a window is how anyone will actually perform "carry it to the strip", and a peek that died
on the button press could never be summoned mid-drag. So the hold is cleared only by a chord and by
the key's own release, while `:488` keeps cancelling the tap alone. Summoning the strip mid-drag is
deliberate: that is exactly when the drop target becomes useful.

**An action stands the peek down, however it arrived.** A chord's own key press already does it,
but taking it at `do_action` as well covers the actions no key press produced — IPC, a pointer
binding — and, more to the point, stops a peek that fired under a modifier-held UI from swallowing
the release that UI is waiting for. The alt-tab switcher is exactly that: it rides the same held
Super, and a peek eating its release loses the window the user picked.

**The peek forwards its release; the tap swallows its own.** The tap gets away with swallowing
because firing moves focus to the overview, and a keyboard leave releases every key client-side
(`overlay_key_firing_release_is_not_sent_to_the_client`). A peek moves focus nowhere — the client
keeps it throughout — and an intercepted release left the focused client's `mods_depressed` at 64:
Super held down forever, on every peek. The client saw the press, so it sees the release.

Also deliberate: arming is idempotent, so a virtual-keyboard client's key repeat cannot refresh the
hold's instant and make an arbitrarily long hold read as a fresh tap; the release is checked *before*
the `overlay_key_armed` tap path, so a claimed release never reaches `ToggleOverview`; and it does
not touch `overlay_key_last_fired`, which belongs to the app-grid escalation.

### 5. Clicking a thumbnail would open the overview

`ThumbGrab`'s non-drag release calls `activate_overview_workspace_at` → `activate_overview_workspace`
(`src/input/mod.rs:5493`), which sends the **active** workspace down `toggle_overview_to_workspace` —
opening the overview from a closed one. The peek needs its own click action: switch to another
workspace, do nothing on the active one.

### 6. During a peek every click is a Super+click

The strip's press handling (`src/input/mod.rs:7327`) is gated on `is_overview_open` and sits *after*
the modifier-drag gestures. In the overview that ordering was free, because Super is not held there.
**Rule: a press inside the strip's band belongs to the strip, before any modifier gesture is
considered.** Outside the band, Super+drag keeps its meaning — which is what makes "grab a window,
carry it up to the strip" work.

Outside the band the pointer chords split. **Super+MMB, the resize chord, stands the peek down and
then resizes**, exactly as a keyboard chord does. **Super+LMB does not**: carrying a window up to
the strip is the peek's headline gesture, and dismissing on its press would take the target
away.

### 7. Z-order and the top edge

The thumbnails group is pushed *after* the `Top` and `Overlay` layers, and earlier push means higher
z — so layer-shell surfaces draw over the strip, and the peek band sits exactly where client bars
live. A top-anchored `Top` bar therefore covers the peeked strip.

**Deferred, pending a case to ground it against.** The argument for raising the peek above those
layers is that it is a transient affordance the user is actively holding, and one a client bar can
hide is not one that can be aimed at — but nothing on our seats puts a bar there (our own panel is
not layer-shell), so the rule would be written against an imagined client rather than an observed
one. Raise it when a real surface is being covered, and let that surface decide the rule.

Whatever is decided, render and hit test move together: `contents_under` requires its order to match
the render's, and `is_layout_obscured_under` is the half that would have to stop refusing a strip hit
under a layer surface. Raising input alone gives a strip you can click but cannot see.

Same reasoning for the **hot corner**: pressure into the top-left toggles the overview
(`src/input/mod.rs:5055`), and a pointer travelling to the strip's left end would trip it. The hot
corner is suppressed while the peek is up.

### 8. A fullscreen window takes a render branch that has no strip

When `render_above_top_layer()` is true the render takes a branch that never calls
`render_thumbnails`, has no close-button chrome and no alpha group.

**A fullscreen window is left alone: the peek does not engage over one.** `Layout::set_peek`
refuses there as it does in the overview, so Super+Shift keeps exactly the meaning it had before the
peek existed, which is none. That is deliberately a
question the seat can answer rather than a design: whether the strip is *wanted* over fullscreen is
unknown, and an affordance that is half-there teaches nothing. Half-there is the specific risk,
because only the strip is missing from that render branch: the dock would come out on its own, which
is furniture for a gesture that is not happening.

Two guards keep it whole if a window goes fullscreen *during* a peek: `Monitor::strip_progress`
refuses at the strip's one funnel, so nothing draws, nothing is hit and no drop lands; and
`Synoik::peek_is_over_fullscreen` releases the dock's hold.

If the peek is later given a strip over fullscreen, note that clicking a thumbnail flips
`render_above_top_layer()` false the moment the switch starts, swapping render branches
mid-animation; the strip must be continuous across that swap.

### 9. Teardown

Lock, VT switch, suspend, and the session dialogs dismiss the peek and cancel a pending timer. Today
nothing clears `overlay_key_armed` on those paths either (`src/input/mod.rs:2568`), which is a
pre-existing hole the same fix closes.

### 10. Releasing Super mid-drag

If the key comes up while a `ThumbGrab` or a strip-targeted move is in flight, dismissal waits for
the button release that ends the grab, then slides up. This is not a latch — the strip is never
*usable* after the key is up; it is only not torn down while a grab holds its geometry.

### 11. The strip is chrome over a live desktop, so it must take the pointer

Everywhere else the strip appears, what it covers is inert: the overview's windows are drawn as
previews and hit as `HitType::Activate`, never `Input`. A peek is the one place the strip lies over
ordinary interactive windows, and nothing in the hit test knew about it — so the window underneath
kept pointer focus and went on driving the cursor image through the strip, which is a window edge
showing a resize cursor over a thumbnail.

**Rule: where the peek draws, the peek owns the pointer.** `Synoik::is_desktop_chrome_under`
suppresses the window under it in `contents_under`, `window_under` and `workspace_under`, and
`resize_edges_under` finds no edge there. The region is the *row* (`Strip::bounds` ∩ the band),
not the full-width band — the same rectangle that accepts a drop, so what swallows the pointer and
what can be aimed at agree. A band with two thumbnails in it is mostly empty air with the desktop
showing through, and a screen-wide dead zone with nothing drawn in it is worse than the leak.

Losing focus does not reset the cursor image — nothing does — so the motion path also forces the
arrow, as the notification banner already did. That runs on motion, so a strip descending under a
stationary pointer leaves the stale cursor until the pointer next moves.

The **dock** is the same defect on the other screen edge, and the peek brings it out: the dash
suppression in `contents_under` was gated on the overview, so the dock's dash did not take the
pointer from the window under it. It does now, peek or not.

While the fullscreen branch draws no strip (§8), it takes no pointer there either.

## Conformance corpus

In `src/tests/gnome.rs`, driving real input against the `Fixture`. Peek state is observable through
the inspectable model, and the tap limit is measured on the compositor clock, so the harness drives
the hold rather than sleeping:

- Shift under a held overlay key → the strip is visible, the overview is closed, no window has moved
- Shift again → the strip goes; again → it comes back; a repeat with no release in between → nothing
- release the overlay key → the strip is gone, the overview did not open
- release Shift and keep holding → the strip stays; the overlay key is the hold
- an overlay key pressed *under* Shift → never a hold, and a later Shift summons nothing
- a bare tap → the overview opens (existing behavior, pinned against regression)
- a bare hold past the tap limit → the overview neither opens nor closes
- a chord while held → the strip is gone and the chord's action fired
- Super+Tab out of a peek → the release commits the switcher, not the peek
- the release of a peek → the focused client sees it, and its modifier state clears
- the trigger while the overview is open → no peek; the Shift spends the tap, and a later bare
  tap still closes the overview
- the trigger with a shortcuts-inhibiting client focused → no peek
- click a thumbnail → the active workspace changed, the strip is still up
- click the active thumbnail → nothing changed, the overview did not open
- drag a window to a thumbnail → the window is on that workspace, keeps its geometry, is not tiled,
  and the active workspace did not change (the drop names a workspace; it does not go there)
- drag a thumbnail → the workspaces reordered
- click the active thumbnail → nothing changed and the overview did not open
- carry a window toward the strip → it shrinks, and grows again on the way back down
- release the key mid-drag → the strip stays until the button releases, then goes
- drag a window into a gap → a workspace was created there
- the trigger → the dock is out on the active output
- release → the dock's hide deadline is re-armed, and it goes
- drag a dock icon onto a thumbnail → the app launched on that workspace
- the trigger *while already dragging a window* → the strip comes down
- the top-left hot corner while peeked → the overview did not open
- Super+MMB while peeked → the strip is gone, and the release that follows opens nothing
- the trigger over a fullscreen window → no peek, no strip, no dash
- the pointer over a peeked thumbnail → the window under it holds neither surface nor activation,
  and takes both back when the peek goes
- lock while peeked → the strip is gone

Plus render tests for two things state assertions cannot see: that peek thumbnails show their
workspaces' live contents (the cull), and that the desktop behind the strip has neither rounded
corners nor a shrink (the split accessors).

## Reverting

The peek is a visibility source, a band, a dock hold, and the ten items above. The interactions it exposes are the
strip's own, and no overview behavior is rewritten to make room for it.
