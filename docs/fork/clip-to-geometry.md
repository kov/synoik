<!-- SPDX-License-Identifier: GPL-3.0-only -->

# `clip_to_geometry` — removed

*Inventoried 2026-08-07 while checking whether it was dead code (it was not — see "why it looked
dead"), then removed the same day on Gustavo's call. Kept as the record of what went and why, so
the analysis is not re-derived if the question comes back.*

## What it did

Clipped a window's rendering to its xdg *window geometry*, rounded by the window's
`geometry_corner_radius`. A niri feature: the compositor imposing a shape on the client's buffer.
Inherited, never re-derived against GNOME.

## Why it was removed

**GNOME has no equivalent.** mutter does not clip client windows to a compositor-chosen rounded
geometry; CSD clients round themselves and the shell does not impose a shape. Under the fork tenet
this was niri's way with no GNOME counterpart to replace it with.

**And it was already inert in any live session.** `ResolvedWindowRules.clip_to_geometry` came only
from `WindowRule`s, and the config file and its parser are gone ([[no-config-file]]), so the field
was `None` everywhere outside tests. Three consequences that matter for reading old code:

- the client-blur `clip` branch already resolved to `false`;
- the resize shader's clip branch already never fired;
- **the Firefox mitigation was already unreachable.** `layout/tile.rs` guarded it with
  `fullscreen_progress < 1. && rules.clip_to_geometry == Some(true)`, whose right half could not be
  true live. The comment said it clipped "to help with buggy clients that submit a full-sized buffer
  before acking the fullscreen state (Firefox)"; that help had not existed since the config file
  went. So the removal is not "we accept the Firefox flash" — the flash was already unmitigated, and
  what the removal deletes is the *option* to mitigate it.

Wholesale removal therefore changed **zero** live behavior. It deletes a capability, not a
behavior.

## Why it looked dead, and was not

A grep read exactly like dead code, but `WindowRule` is a live programmatic API and the tests used
it. Stubbing the branch to `false` failed `vulkan_clips_a_window_to_rounded_geometry` and
`vulkan_clipped_tile_pushes_its_rounded_corner_damage`.
`vulkan_clips_a_window_through_the_overview_wrapper` *passed* — because it only asserted "present
and zoomed out", which an unclipped window satisfies too; it was strengthened the same day to assert
the corners are actually cut. **"Unreachable from user config" is not "dead"**, and with no config
file only the test callers tell the two apart.

## What went

| area | what |
|---|---|
| rule | `WindowRule::clip_to_geometry`, `ResolvedWindowRules::clip_to_geometry` and its merge |
| tile | the `clip_to_geometry` local, the Wayland arm of the `clip` closure, the `ClippedSurface` variant of tile's element enum |
| damage | `Tile::rounded_corner_damage` and the whole `RoundedCornerDamage` type — with it goes the workaround for `ClippedSurfaceRenderElement::damage_since` not reporting radius changes (that FIXME still stands; nothing depends on it now) |
| effects | the `clip_to_geometry` parameter through `background_effect::render_for_{tile,surface}` and `LayoutElement::render_background_effect`; the client-blur-region branch is now plainly `clip = false` |
| resize | `ResizeRenderElement::new`'s flag, `ResizePush::clip_to_geometry`, and the clip branch plus the now-uncalled `synoik_rounding_alpha` in `resize.frag`/`resize.vert` |
| **shader ABI** | `CustomResizePush::clip_to_geometry`, its GLSL push-block line, the `#define synoik_clip_to_geometry`, and the clip branch of `RESIZE_EPILOGUE`. `CustomResizePush` is 164 → 160 bytes, `ResizePush` 128 → 124 |
| tests | `clipped_window_fixture` and its three tests, deleted; the two custom/builtin resize ABI tests adapted (they now assert the corner carries the same blend as the interior) |

**What stayed:** `ClippedSurfaceRenderElement` and the whole Vulkan clip-arming path — they are
still used by `render_helpers/window_thumbnail.rs`, which clips independently of this rule. That
path keeps its coverage through `vulkan_postprocess_clips_and_desaturates`.

Also stayed, deliberately: `curr_geo_size`, `corner_radius` and `synoik_scale` in `ResizePush`, now
unused by the builtin fragment stage. Dropping a `vec2`/`vec4` mid-block would shift every later
`vec4` off its std430 16-byte alignment, and `CustomResizePush` still exposes all three to custom
snippets through the prelude. Removing them means re-deriving the block layout, which is a bigger
change than it looks.

Verified with `SYNOIK_VK_VALIDATION=1 cargo test --workspace`: exit 0, no `VULKAN ERROR`.

## Related

`client-blur.md` §5 gap 3 — the client-blur side of the same flag, and why it was *not* what gave a
client's rounded region its rounded blur (the region's own rects do that, exactly).
