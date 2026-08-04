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

- **Absolute pointing devices have no dock.** Their position is mapped into the output, so the
  clamp never discards anything and no pressure accumulates. The hot corner falls back to its
  corner pixel; a full bottom edge has no equivalent fallback that isn't a tripwire. If the seat
  turns out to use an absolute pointer, this needs a different signal — a gesture, or a bindable
  action.
- **No touch gesture.** A swipe up from the bottom edge is the obvious counterpart and is not
  wired.
- **Bottom hot corners are gone**, removed with the rest of niri's `gestures.hot_corners` when
  hot corners moved to `org.gnome.desktop.interface enable-hot-corners` (GNOME has exactly one
  corner, top-left). So there is no corner-versus-dock conflict to arbitrate at the bottom
  corners today. If bottom corners ever come back, the agreed rule is that **the corner wins**.

## Tests

- `src/ui/dock.rs` — the state machine: pressure, the slide, the grace period, the drag hold.
- `src/tests/gnome.rs` — `the_dock_needs_pressure_on_the_bottom_edge`,
  `the_dock_hit_tests_the_same_dash_as_the_overview`, driving real synthetic input against the
  `Fixture`, as the conformance corpus does for GNOME behaviors.
