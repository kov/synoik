<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The dash as a dock

**An approved functional divergence from GNOME**, agreed 2026-08-04. gnome-shell's dash exists
only inside the overview; ours also slides up from the bottom edge of the screen when the pointer
pushes into it. GNOME's source is still the reference for everything the dash *is* — this doc
covers only where we depart, and why each choice is the one that was made.

## The trigger

The bottom edge is a [pressure barrier](../../src/input/pressure.rs), the same primitive the hot
corner uses (`PressureBarrier`, `js/ui/layout.js:1267-1408`): 100 px of inward push inside a
second, travel *along* the edge discounted, latched until the pointer leaves.

This is the whole reason the barrier was ported first. A corner survives a naive
enter-to-trigger because it is hard to hit by accident; a full-width bottom edge is not. Every
pointer thrown at a bottom-anchored menu, every overshot scrollbar, every fling toward a corner
crosses it. Pressure is what makes an edge trigger mean *intent*.

The pressure is the motion the output clamp discarded — see `src/input/pressure.rs` for why that
is the honest stand-in for what mutter's blocking barriers report, and for the one case it cannot
serve (absolute pointing devices, which build no pressure at all and so have no dock).

**A fullscreen window owns the bottom edge**, as it already owns the corner (`push_hot_corner`) and
the top strip (`Monitor::panel_hidden_fraction`) — unless the overview is up. Same predicate,
`render_above_top_layer`, so the three cannot drift.

This is a gate rather than a larger threshold, because no threshold can work here. Pressure *is*
sustained inward motion, and a game whose map scrolls when you push the pointer off the edge
generates exactly that, indefinitely. Any finite threshold is crossed by holding against the edge;
raising it only buys seconds. The cost is worse than the interruption, too: the dock draws above
the fullscreen window and outside the panel's hide gate, so a reveal drops the window out of direct
scanout, silently, mid-game.

Refused pressure is dropped rather than banked (`Dock::forget_pressure`), or the dock would spring
out on the first motion after the window left fullscreen.

## Decisions

| | | |
|---|---|---|
| **One dash or two** | One | The dock and the overview share a single `Dash`. Favorites, running dots, hover, context menus and pin/unpin drags are the same code in both places — the only thing that differs is the rect it is drawn in. `Synoik::dash_area` is that rect, and every hit-test, hover and drop-target asks it. |
| **Overview interaction** | It just stays | Opening the overview does not retract the dock: it is the same dash, and the overview's slot is within a few px of the dock's. The overview owns the dash while it is up (`dock_owns_dash` is false), and the dock is still out underneath when it closes. |
| **Reserved space** | None | Pure overlay. No struts, no relayout, no window is aware of it. |
| **Which output** | The one under the pointer | Pressure built against one monitor's edge never opens the dock on another. |
| **Dismissal** | Pointer leaves + 300 ms | Long enough to cross a gap between icons or overshoot the top edge and come back. Activating an icon dismisses it immediately — you asked for a window, and the dock would be in the way. A right-click does not: that opens a menu belonging to an icon still on screen. |
| **Drags** | Hold it open | A pin/unpin drag pins the dock open until the drop, wherever the pointer wanders. Derived from `app_drag` rather than hooked into each of the five paths that end a drag, none of which can then forget. |
| **Locked / screenshot UI** | Never | `dash_area` refuses to place the dash, and the render path asks the same function, so what is drawn and what is clickable cannot come apart. Without this the dock would be an invisible click-eater in front of the shield. |

## What is not built yet

- **Absolute pointing devices build no pressure of their own.** Their position is mapped into the
  output, so the clamp never discards anything. The hot corner falls back to its corner pixel; a
  full bottom edge has no equivalent fallback that isn't a tripwire.

  This is survivable on a seat that has *both* kinds, which the dev VM does: its pointer sends
  absolute events for ordinary movement and switches to relative deltas while the host cursor is
  pinned against a screen edge — so the push that matters is relative and does build pressure.
  What that split broke once (fixed, and pinned by `leaving_by_any_device_re_arms_the_barrier`):
  **a barrier's latch must be released from a path every device reaches.** With the release living
  in the relative-only `push`, the dock fired exactly once per session and every later push landed
  on a latched barrier, because all the *leaving* was absolute. A seat with only an absolute
  pointer still has no dock and needs a different signal — a gesture, or a bindable action.
- **No touch gesture.** A swipe up from the bottom edge is the obvious counterpart and is not
  wired.
- **Bottom hot corners are gone**, removed with the rest of niri's `gestures.hot_corners` when
  hot corners moved to `org.gnome.desktop.interface enable-hot-corners` (GNOME has exactly one
  corner, top-left). So there is no corner-versus-dock conflict to arbitrate at the bottom
  corners today. If bottom corners ever come back, the agreed rule is that **the corner wins**.

## The poke — urgency on the dock

A window demanding attention slides the dock a fraction of the way out (`POKE_PROGRESS = 0.45`)
and draws **only** the icons asking for it: no pill, no blur, no separator, no running dots, no
show-apps button. The icons keep their normal dash x, so pushing the pointer into the bottom edge
to answer the poke lands on the icon you are about to click. Suppressed while a fullscreen window
is focused — poking into a fullscreen video is where "louder than GNOME" becomes "worse than
GNOME".

This has no GNOME counterpart at all: `windowAttentionHandler.js` posts a notification and touches
nothing in the dash.

An urgent icon glows in the system accent — one `drop_shadow`, no offset, baked into a buffer
padded for the 3σ fringe. The glow is drawn **wherever the dash is**, not only during a poke:
pulling the dock the rest of the way out is exactly the moment you are reaching for that icon, and
that is the wrong moment for the only mark identifying it to disappear. It sits above the pill
plate and below the icons, which is what the push order buys — first element pushed is topmost.

### Urgency is per window; an app you are looking at is not urgent

Urgency stays per window, as mutter keeps it: focusing a window unsets its own
`wm_state_demands_attention` and nothing else's (`window.c:5090-5091`), and `Mapped::set_urgent`
refuses to mark the focused window for the same reason.

The dash and the dock, though, aggregate per **app** — so an app that focuses one window on your
desktop and maps a second one elsewhere used to poke at you while you were already working in it,
and only walking over to focus *that* window would stop it. So: **an app with a focused window is
never urgent.** `Synoik::clear_urgency_of_focused_app` enforces it, and it runs at snapshot time
inside `sync_running_apps` rather than on focus changes, because the window that demands attention
arrives by *mapping*, long after focus last moved — a focus hook would fire before the urgent
window existed, and would work or not depending on the order the client happened to map in. App
identity comes from `app_for_window`, the same resolution the dash aggregates on; comparing
`app_id` strings would leave a poke that focusing could never clear.

**Interim, decided 2026-08-08.** Urgency wants a redesign around a less distracting per-window
affordance; until there is one, an app you are looking at does not shout.

One gap left open:

- **Mutter also refuses to flag a window in full view** (`meta_window_set_demands_attention`,
  `window.c:6635-6700`: another workspace or minimized counts as obscured, otherwise it walks the
  stack looking for an overlap). We only test "not on the active workspace".
**What each restore `reason` may do** (decided 2026-08-08, `session-management-port.md` §`reason`):
`launch` takes focus and may mark; **`recover` marks but never focuses** — one app returning from a
crash gets to say where it went, which is a single window's worth of noise; `session_restore` does
neither, because a login restoring everything you had open would have every app shouting at once,
which is no signal at all.

## Tests

- `src/ui/dash.rs` — `a_poke_can_only_be_clicked_on_its_urgent_icons`: what a poke draws is what a
  poke can be clicked on, or pushing into the edge activates an invisible favorite.
- `src/tests/vulkan_render.rs` — `a_poking_dash_draws_glowing_icons_and_no_chrome`, which is also
  the only exercise of the padded glow bake under `SYNOIK_VK_VALIDATION=1`.
- `src/tests/gnome.rs` — `an_app_you_are_looking_at_does_not_demand_attention`.
- `src/ui/dock.rs` — the state machine: pressure, the slide, the grace period, the drag hold.
- `src/tests/gnome.rs` — `the_dock_needs_pressure_on_the_bottom_edge`,
  `a_fullscreen_window_owns_the_bottom_edge`,
  `the_dock_hit_tests_the_same_dash_as_the_overview`, driving real synthetic input against the
  `Fixture`, as the conformance corpus does for GNOME behaviors.
- `src/ui/dock.rs` — `refused_pressure_is_forgotten_rather_than_banked`, which needs the pointer
  held *on* the edge across the refusal; leaving re-arms the barrier by itself, so the integration
  test above cannot show it.
