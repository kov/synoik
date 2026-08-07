<!-- SPDX-License-Identifier: GPL-3.0-only -->

# blur-probe

A minimal `ext-background-effect-v1` client, for telling a **compositor-side** blur bug apart from a
**client-side** one.

The bug it was written for: dragging a blurred window's edge leaves the blur behind — stale backdrop
trailing on grow, a glass pane trailing on shrink. Reported against ghost on synoik. Two very
different faults look identical from the outside:

- the compositor re-captures the backdrop too late, or
- the client respecifies its blur region too late.

Ghost blurs through a vendored winit, which derives its region from the window size whenever the
corner radii are non-zero (`vendor/winit/.../wayland/window/state.rs`, `reapply_blur_shape`). So it
is the *second* kind of client, and cannot rule itself out.

## The discriminator

```
blur-probe --region whole --pulse    # a stale region is impossible
blur-probe --region exact --pulse    # respecified every configure, like ghost
blur-probe --region lag:2 --pulse    # a deliberately late client, for comparison
blur-probe --region none  --pulse    # no blur at all; the control
```

`whole` sets one rect far larger than any surface, **once**. The protocol clips the region to the
surface, so it means "all of it" at every size and can never be stale.

| `whole` | `exact` | reading |
|---|---|---|
| lags | lags | the compositor is late re-capturing — ours |
| clean | lags | the region-respecify path — winit/ghost, or how we handle a re-specified region |
| clean | clean | not reproduced; the difference from ghost is elsewhere |

Always run `lag:2` at least once. It is what a known-late client looks like, and it is worth knowing
whether the reported symptom actually matches that before blaming anything for it.

## Driving it

`--pulse` resizes the window from inside the client, so no mouse and no human are needed — which is
what makes this runnable on a headless harness. It sweeps `--min` to `--max` and back over
`--period` ms.

A compositor-sent size is obeyed only when the configure's *state* makes it binding (maximized,
fullscreen, tiled, or mid-interactive-resize). Otherwise it is a suggestion, per xdg-shell — without
that distinction the compositor echoes our own size back, the probe obeys, and the pulse never
leaves its starting size.

Every frame prints one line, so two runs diff cleanly:

```
frame 42 configure=Some((480, 360)) buffer=705x520 region=[(0, 0, 705, 520)]
```

## Reading the window

The surface is translucent (`--alpha`, default 90/255) with a **fully opaque 3px orange frame** and
a white grid. The frame is there because the bug is about edges: if the blur is a size behind, it
stops short of the frame on grow, or runs past it on shrink. Either is visible without measuring.

Do not run it `--opaque` expecting to see anything — that flag exists to demonstrate the opposite.
A fully opaque surface occludes the effect beneath it and the compositor culls it before the
framebuffer-effect pass runs, so the blur vanishes for a reason unrelated to any bug.

## Building and running

Not a member of the synoik workspace, on purpose: the dev loop is `cargo test --workspace` and a
diagnostic client has no business slowing it down.

```
cargo build --manifest-path tools/blur-probe/Cargo.toml
tools/blur-probe/target/debug/blur-probe --region whole --pulse
```

## What it cannot tell you

**Not on the headless harness.** `backend/headless.rs`'s `render` is bookkeeping only and never
builds an `OutputDamageTracker`, so the corpus composites every frame from scratch and a
partial-damage staleness bug cannot appear there. Headless is good for checking that the probe maps,
blurs and logs; the actual verdict has to come from a real seat.

Buffers are rotated across three arenas and only reused after `wl_buffer.release`. That is not
incidental: the first version painted in place into one arena, the compositor tore mid-resize, and
on a probe built to judge whether a blur is one frame behind, a tear reads as exactly that bug.
