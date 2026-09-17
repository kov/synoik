# Workspace switch motion blur

**Status:** landed. **Divergence from gnome-shell, approved 2026-09-17.**

## What GNOME does, and why we don't

A keyboard workspace switch in gnome-shell never scrolls past the workspaces in between.
`WorkspaceAnimationController.animateSwitch` builds `workspaceIndices = [from, to]` — exactly two —
and hands only those to `_prepareWorkspaceSwitch` (`js/ui/workspaceAnimation.js:436-459`).
`MonitorGroup._init` then lays them out at *consecutive* slots (`x += this.baseDistance`,
`:213-236`), so seven-to-one slides one workspace-width over `WINDOW_ANIMATION_TIME`, exactly as
far as two-to-one. The full strip (`workspaceIndices = [...Array(nWorkspaces).keys()]`, `:392-393`)
is built only for the swipe gesture, where the user really is dragging through them.

We keep the continuous strip inherited from niri, because sweeping past the workspaces in between
is what tells you *where* you went — the spatial sense GNOME trades away. The cost of keeping it is
what this document is about.

## The problem the blur solves

The switch is a critically damped spring at stiffness 1000 (`WorkspaceSwitchAnim::default`,
`synoik-config/src/animations.rs:142-153`), so with `β = ω₀ = √1000 ≈ 31.62`:

    x(t) = to + e^(-βt)·(x₀ + βx₀t)        v(t) = -β²·x₀·t·e^(-βt)        |v|ₘₐₓ = β·x₀/e  at t = 1/β

A seven-to-one jump (`x₀ = 6`) peaks at **≈ 70 workspaces per second** — about **1250 px between
two 60 Hz frames** on a 1080-tall output, with the whole switch over in ~290 ms.

That is not a fast animation, it is a **strobe**: consecutive frames share no content at all. It
read as "busy and unfinished" because nothing connects one frame to the next. No tasteful 32-pixel
smear fixes that; the blur has to span the inter-frame travel to read as motion, so the radius
*is* the travel.

## How it works

`Monitor::workspace_switch_motion` reports the travel in logical pixels along the strip axis, and
returns `None` — no blur — in three cases:

- the switch came from a gesture (a swipe under the finger, or the fling it was released into):
  the user is steering it, and it tracks the hand exactly;
- a switch running *with* an overview zoom, where `workspace_render_idx` corrects one animation
  against the other and this animation alone no longer describes what moves;
- travel under `MOTION_BLUR_MIN_TRAVEL`, the last few frames of every switch.

Travel is read off the animation curve across one fixed **exposure** either side of now
(`MOTION_BLUR_EXPOSURE`, a shutter time — deliberately not the refresh interval, so a 144 Hz screen
does not get a sharper picture of the same gesture), never differenced between frames: a dropped
frame must not change how the next one is blurred.

The render side composites everything that slides into a per-output offscreen and smears it —
`MotionBlurSlot::render`, `src/render_helpers/vulkan/motion_blur.rs`. The smear is
`BlurChain::record_directional`: GNOME's gaussian shader and sigma, but a pyramid that halves
**only the axis of travel** and a single blur pass instead of the separable pair. Full resolution
across the motion is what keeps the strip legible rather than a wash, and the one-axis pyramid is
what makes a 1250 px radius affordable — five rungs down it is sigma ≈ 19 on a thirty-second of
the texels.

The taps stay symmetric (`±offset`). That is correct, not a shortcut: a frame samples the animation
at one instant *inside* the exposure it stands for, so the light it represents arrived both before
and after that instant. A one-sided trail would model a shutter that opened where the frame landed.

## Rules that are easy to break

- **The blurred element reports full damage, and so does the first unblurred frame after it.** The
  offscreen's own damage tracker sees only the sliding elements; it knows nothing about a radius
  that changes every frame. Dropping the trailing full-damage frame leaves the last smear on
  screen wherever the settled workspace does not repaint — and a capture cannot show a missing
  repaint, so it would not be caught by looking.
- **The chain is built once per switch at full depth**, and the radius picks how far down it
  descends (`record_directional` clamps `k` to the rungs it has). Sizing the chain to the current
  radius would rebuild it almost every frame, which is the cache churn `BackdropBlur`'s size key
  already cost us once.
- **Per output.** Both the offscreen and the chain are sized to one output, and two monitors can be
  mid-switch at once; one shared pair fails the offscreen's uniqueness check and reallocates every
  frame that both ask for it.
- **The exposure is wall-clock; the curve is sampled in clock time.** `org.synoik.animations speed`
  scales the animation clock, so the sampling window is scaled by `Clock::rate` before it is used.
  An unscaled window asks the curve how far it travels in 16.67 ms *of animation*, which at half
  speed is two real frames — double the smear, in the one mode where someone is watching closely
  enough to have slowed it down.

## Cost

On the desktop the strip is normally pushed straight through — `push_group_at_alpha` only routes
through an offscreen at partial alpha — so during a non-gesture switch this adds one full-output
composite plus the smear, for the fast part of ~290 ms. `OffscreenRenderElement` declares no opaque
regions, so the backdrop below is filled for those frames too.

## Not done

Nothing here shortens the animation. If long-distance switches still feel rushed, the other lever
is the spring: a distance-dependent duration would keep the strip and slow the sweep. That is a
separate change and is not implemented.
