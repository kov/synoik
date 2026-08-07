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

**Measure the right edges.** The region is anchored at (0, 0), so a late one is short on the
**right and bottom** and covers the top and left completely. An instrument that samples a band
inside the top border reads a perfect 1.00 for a client that is visibly lagging.

**And do not measure roughness over a soft wallpaper.** A gradient looks the same blurred or not, so
the edge-vs-middle ratio has nothing to work with; worse, the probe's own white 64px grid lands
inside a sampling band or not depending on the window size, which swings a *mean* roughness between
~12 and ~40 for reasons unrelated to the blur and makes `whole` look as bad as `lag:4`. On a
backdrop like that use `--subsurface` (below) instead: it supplies its own high-frequency pattern,
so the signal is a 16x-19x contrast collapse rather than a ratio hovering around 1.

## Rounded regions

`--radius N` turns the region into a scanline stack of rects, which is the only way this protocol
can express a curve and what a rounded-corner client sends. It affects `exact` and `lag` only —
`whole` is resize-invariant by construction, and a rounded rect is size-derived.

Two things it answers. Whether the compositor masks the blur **per-rect or to the bounding box**:
diff the same window at radius 0 against radius N, then split the corner band by distance from the
corner circle's centre. Inside the quarter-disc must be zero. And whether a **multi-rect region
respecified every configure** lags, which is the shape ghost actually sends.

## Subsurfaces

`--subsurface` puts the blur on a subsurface inset inside the toplevel, and makes the toplevel
**opaque** and finely striped. That is the point: a subsurface's backdrop is everything below it,
its own parent surface included, so a correct implementation turns the parent's 6px diagonal stripes
into a flat wash *inside the child and nowhere else*. Any real blur radius averages that pattern
out, which makes the answer visible at a glance and measurable as a collapse in local contrast.

```
blur-probe --subsurface --region exact
```

On the headless harness after the fix: local contrast 413 in the parent on all three sides of the
child, 3.4 inside it. Before the fix the flat patch sat in the parent's bottom-right corner instead
— the effect was placed a whole subsurface-offset past the subsurface. Worth knowing what a
misplacement looks like, because the blur is the right size and blurs the right content; only its
position is wrong, and every self-relative check still passes.

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
