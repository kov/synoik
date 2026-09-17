# A workspace the switch passes over keeps its size

**Divergence from GNOME, taken deliberately 2026-09-17.** Approved by kov after seeing it on the
seat: the strip visibly wobbles during a keyboard switch that skips workspaces.

## What GNOME does, and what we do

`_updateWorkspacesState` (`js/ui/workspacesView.js:243-266`) scales each workspace by its distance
from the **animated** scroll adjustment:

```js
const distanceToCurrentWorkspace = Math.abs(adj.value - index);
const scale = Util.lerp(WORKSPACE_INACTIVE_SCALE, 1, 1 - clamp(distanceToCurrentWorkspace, 0, 1));
```

We interpolate between the distance at the **start** layout and the distance at the **end** layout
instead, over the switch's own progress — `InactiveDistance` in `src/layout/monitor.rs`.

## Why

gnome-shell's keyboard switch lays out only `[from, to]`, side by side, so its scroll adjustment
never passes over a third workspace and the distance rule is complete for every case it can reach.

Our row is continuous — the approved strip divergence — so a switch from 6 to 1 drags four
intermediate workspaces through the middle of the screen. Under the distance rule each one swells
from 0.94 to 1 and shrinks back as it transits the centre. The slot pitch is fixed, so the swelling
has nowhere to go but the gaps: measured at 1920x1080, the gap between neighbours pumped
**97px → 61px → 97px** while the row flowed past, four times over. The workspace *centres* travel
rigidly (rigid-strip residual under 1px), which is why this reads as the borders breathing rather
than as anything sliding backwards — and why a "does any edge move backwards" check reports it as
clean: the row is travelling 200–300px per frame, so a 73px swell never turns a step negative.

Interpolating the endpoint distances leaves the departing workspace shrinking, the arriving one
growing, and everything merely passed over at inactive size throughout.

## What it costs

Nothing, for the case GNOME actually has. With `from` and `to` one apart, each endpoint distance is
linear in the scroll position across the whole range, so interpolating the two distances and taking
the distance of the interpolated position are the same expression. An adjacent switch — the only
kind gnome-shell's keyboard path produces — is byte-for-byte unchanged. The divergence only applies
where gnome-shell has no equivalent case.

## What is deliberately left alone

A **gesture**, and the fling it is released into. There the row is tracking a finger: the user is
scrolling *through* those workspaces rather than past them, and each one growing as it reaches the
centre is the feedback they are steering by. `workspace_switch_from_gesture` is what separates the
two.

## Pinned by

`src/tests/gnome.rs`:

- `a_switch_that_skips_workspaces_leaves_their_size_alone` — the fix. Every workspace's drawn width
  is monotone across the whole animation, and the ones merely passed over never change at all.
- `an_adjacent_switch_still_trades_size_between_the_two_workspaces` — the fidelity half, so the
  divergence cannot be "flattened the shrink everywhere" instead.
