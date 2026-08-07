<!-- SPDX-License-Identifier: GPL-3.0-only -->

# `clip_to_geometry` — inventory, and a decision deferred

*Written 2026-08-07 while checking whether this was dead code. It is not, and the removal that was
almost done on that basis would have been a regression. Recording the analysis so the actual
decision — which is a **product** decision, not a cleanup — can be taken later on facts.*

**Provisional direction (Gustavo, 2026-08-07): most likely rip the whole thing out, as it is
probably a feature we do not want. Deferred deliberately; nothing below is actioned.**

## What it does

Clips a window's rendering to its xdg *window geometry*, rounded by the window's
`geometry_corner_radius`. A niri feature: the compositor imposes a shape on the client's buffer.
Inherited, never re-derived against GNOME.

Its one documented non-cosmetic use is a workaround, at `layout/tile.rs:1136`:

> Clip to geometry including during the fullscreen animation to help with buggy clients that submit
> a full-sized buffer before acking the fullscreen state (Firefox).

That is worth weighing separately from the cosmetic case: if it goes, that Firefox mitigation goes
with it.

## Why it looks dead, and is not

`ResolvedWindowRules.clip_to_geometry` is `Option<bool>`, resolved from `WindowRule`s
(`window/mod.rs:293`). The **config file and its parser are gone** — deleted 2026-08, knuffel with
them — so no user can set it, and a grep reads exactly like dead code.

But `WindowRule` is a **live programmatic API**, and the tests use it. Two set this flag
(`tests/vulkan_render.rs`, via `clipped_window_fixture`). Stubbing the branch to `false` fails:

- `vulkan_clips_a_window_to_rounded_geometry` — FAILED
- `vulkan_clipped_tile_pushes_its_rounded_corner_damage` — FAILED
- `vulkan_clips_a_window_through_the_overview_wrapper` — passed, **because the test was weak**; it
  asserted only "present and zoomed out", which an unclipped window satisfies just as well.
  Strengthened the same day to assert the corners are actually cut, and it now fails with the stub
  like its siblings. Note the green bbox is *identical* clipped or not — only the corner pixel count
  changes — which is why the original assertions could not see it.

**"Unreachable from user config" is not "dead".** With no config file those two look the same from a
grep, and only the test callers tell them apart.

## Everything it touches

| area | sites |
|---|---|
| rule plumbing | `window/mod.rs:112,293`, `window/mapped.rs:554,823,835`, `layout/mod.rs:226`, `layout/tile.rs:1138,1223,1263,1275,1311,1391` |
| effect resolution | `render_helpers/background_effect.rs:229,246,374,405` — a client blur region is clipped to geometry only when this is on |
| resize material | `render_helpers/resize.rs:154,173,188,201` |
| **shader ABI** | `render_helpers/vulkan/custom.rs:77,240,284,304` — `synoik_clip_to_geometry` is a push constant **exposed in the custom-shader prelude**, with its own tests (`vulkan/tests.rs:1299,1836,1896`) |
| tests | `tests/vulkan_render.rs` (3), `vulkan/tests.rs` (3) |

The shader row is the one that makes this more than a rule removal: dropping the push constant
changes the interface custom render materials are written against.

## What GNOME does

**Nothing equivalent.** GNOME/mutter does not clip client windows to a compositor-chosen rounded
geometry; CSD clients round themselves, and the shell does not impose a shape. So under the fork
tenet this is niri's way with no GNOME counterpart to replace it with — which is the case for
removal, not merely for leaving it unreachable.

Against removal: it is an *additional capability* rather than a differently-spelled GNOME behavior,
and the tenet says to keep those, re-homed. It is also the only thing standing between us and the
Firefox pre-ack fullscreen flash.

## If it is removed

Take the whole column, not the visible half — leaving `Option<bool>` plumbing behind with no
consumer is worse than either end state. Specifically: the rule field and its merge, the `Tile`
threading, the `background_effect` clip branch (which changes what a *client blur region* is clipped
to, so re-read `client-blur.md` §5 gap 3 first), the resize material's flag, the shader push
constant and its prelude `#define`, and the six tests. The Firefox mitigation needs a replacement or
an explicit "we accept this" note.

## Related

`client-blur.md` §5 gap 3 — the client-blur side of the same flag, and why it is *not* what gives a
client's rounded region its rounded blur (the region's own rects do that, exactly).
