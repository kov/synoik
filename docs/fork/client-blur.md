# Client background blur — what we offer, and how it compares

Status 2026-08-06. Covers `ext-background-effect-v1` as synoik implements it today, measured against
KWin (Plasma 6.7) and macOS's `NSVisualEffectView`. Written to seed the improvement backlog at the
bottom; nothing here is implemented yet beyond what "Today" describes.

## 1. The protocol we speak

`ext_background_effect_manager_v1` version 1, staging
(`wayland-protocols/staging/ext-background-effect/ext-background-effect-v1.xml`). The whole surface
of it:

- `capabilities(flags)` — a bitfield with exactly one bit defined, `blur`.
- `get_background_effect(id, surface)` — one effect object per `wl_surface`; a second one is a
  `background_effect_exists` protocol error.
- `set_blur_region(wl_region | null)` — surface-local, double-buffered, clipped by the compositor to
  the surface. NULL removes it. "The blur algorithm is subject to compositor policies."

That is the entire client-facing vocabulary: **a region, and nothing else.** No radius, no strength,
no tint, no material. Every parameter is the compositor's.

We register the global unconditionally (`synoik.rs:6790`) and always answer `Capability::Blur`
(`src/handlers/background_effect.rs:117`). Smithay (our fork, `0ffb517`) does the double-buffering;
we cache the committed region as non-overlapping surface-local rects and damage the surface's
background effect on commit.

## 2. Today's rendering path

`src/render_helpers/background_effect.rs` decides, per surface, from three inputs: the client's blur
region, the global `Blur` config, and the per-window/per-layer `BackgroundEffect` rule.

```
has_blur_region → blur = true                                (unless a rule says blur: false)
any effect visible && rule.xray is None → xray = true        ← the important line
```

There are two draw paths behind that flag:

| path | source of the backdrop | code |
| --- | --- | --- |
| `FramebufferEffect` (non-xray) | **the real framebuffer** — mid-frame capture of everything drawn behind the surface, then dual-Kawase, then the postprocess/clip draw | `render_helpers/framebuffer_effect.rs`, `vulkan/backdrop_blur.rs` |
| `Xray` | an offscreen holding **only the background-layer surfaces and the GNOME wallpaper** (`Synoik::fill_xray_elements`, `synoik.rs:10446`) | `render_helpers/xray.rs`, `vulkan/effect_blur.rs` |

**Today every client that sets a blur region gets the xray path.** The config file is gone, so no
window or layer rule can set `xray: false`; `rule.xray` is always `None`; the line above therefore
always fires. The consequence is not a quality difference, it is a semantic one: a window with a
blur region shows *the wallpaper, blurred* — the windows stacked behind it are not there. Stack two
blurred terminals and the top one does not see the bottom one.

The real-backdrop path exists, works, and is exercised by tests
(`vulkan_backdrop_effect_roundtrips_under_rotation`); it is simply unreachable from a client.

Blur maths, both paths: dual-Kawase (`synoik-vk/src/blur.rs`), `passes = 3`, `offset = 3.0`, plus
`noise = 0.02` and `saturation = 1.5` — global defaults in `synoik-config::Blur`, identical for
every surface. A separable-gaussian variant exists (`vulkan/gaussian_backdrop.rs`) but is the lock
screen's, specified in source pixels to match `Shell.BlurEffect`.

Region handling: the committed region is flattened to non-overlapping rects, offset to the surface's
geometry, and intersected with the frame's damage so the draw is scissored to it
(`framebuffer_effect.rs:265`). The effect geometry is the *surface* geometry, so the region can't
escape the surface. Corner rounding is applied only when `clip_to_geometry` is set — which, again,
nothing can set today, so a client's rounded window gets a square-cornered blur under its corners
unless its own region excludes them.

Reach: toplevels, their popups, and layer-shell surfaces (`window/mapped.rs`, `layer/mapped.rs`).
**Not subsurfaces** — `layout/tile.rs:1301` says so outright. Not XWayland: we honor no
`_KDE_NET_WM_BLUR_BEHIND_REGION`.

One thing we do that neither reference does: the effect state is **per render target**
(`RenderTarget::COUNT` buffers, `xray.rs:29`), so a screencast that must block out a window cannot
composite from a capture the on-screen target filled. That is a privacy invariant, not a nicety.

## 3. KWin (Plasma 6.7)

Sources: `plasma/kwin/src/plugins/blur/blur.cpp`, MR
[!4890](https://invent.kde.org/plasma/kwin/-/merge_requests/4890) (protocol support), MR
[!6838](https://invent.kde.org/plasma/kwin/-/merge_requests/6838) (contrast merged into blur).

- **Same protocol, same semantics.** 6.7 dropped `org_kde_kwin_blur_manager` and advertises
  `ext_background_effect_manager_v1` instead — which is what broke Ghostty's blur on the 6.7 upgrade.
  It also still honors `_KDE_NET_WM_BLUR_BEHIND_REGION` for X11 clients, and takes a blur region from
  the *decoration* (`KDecoration3::Decoration::blurRegion()`) as well as the surface.
- **Samples the real backdrop**, via `blitFromRenderTarget` of the pixels behind the shape. Per
  window, per view, with cached framebuffers/textures in `BlurRenderData`.
- **Dual-Kawase**, strength 1–15 mapped onto an iteration count and an offset range
  (`blurStrengthValues`, up to 4 iterations, offsets in the 1.0–8.0 band). A single global user
  setting, not per client.
- **Noise** when `m_noiseStrength > 0`, and since the contrast merge, a **saturation/contrast/
  intensity colour matrix** (`colorTransformMatrix()`) — the old BackgroundContrast effect folded in
  as an extra step, with a switch to disable just the contrast half.
- **Rounded corners**: a dedicated rounded blur shader keyed to the window's `borderRadius()`.

So against KWin, the discriminators are: what gets sampled (real backdrop vs. our wallpaper-only),
the rounded-corner shader, decoration/X11 region sources, and the contrast matrix. Our
noise/saturation and Kawase parameters are the same class of knob at similar defaults.

## 4. macOS

macOS is a different shape of answer, and worth stating precisely so we don't cargo-cult it. There
is **no client protocol**. An app instantiates an `NSVisualEffectView`, picks a *material* by
semantic role — `.sidebar`, `.menu`, `.popover`, `.hudWindow`, `.titlebar`, `.underWindowBackground`
— and a `blendingMode` of `.behindWindow` (blur what is behind the window, the WindowServer's job)
or `.withinWindow` (blur this app's own content below the view). The view's *shape*, including its
corner mask, is the region.

What each material carries, that neither Wayland compositor has as a concept:

- **A recipe, not parameters.** Radius, tint colour, saturation boost and grain are baked per
  material and vary with light/dark appearance and with the desktop tint. The app never names a
  radius, so the whole system stays coherent — the thing a "blur strength slider" cannot buy.
- **Vibrancy.** Text and symbols drawn inside the view get a blend mode that keeps them legible
  against whatever the backdrop turned out to be. This is the part that reads as "quality" and it
  is a *foreground* treatment, not a blur.
- **Active/inactive state.** `NSVisualEffectView.State` follows window key state; an inactive window
  falls back toward a flat material instead of staying live-blurred. Cheaper *and* a focus cue.
- **Reduce transparency** (accessibility) collapses every material to an opaque fill, globally.

So "clients cannot tune the blur" is true on all three systems. macOS's real advantages are the
per-material tint/vibrancy recipe, the active/inactive fallback, and the accessibility opt-out.

## 5. Gaps and defects, ranked

Defects first — these are wrong, not merely absent.

1. **Client blur is wallpaper-only.** §2. The fix is to reach the `FramebufferEffect` path for
   client-requested regions; the path already exists. Needs a decision on the default and on what
   replaces the dead rule plumbing.
2. **`blur.off` + a client region turns the window see-through.** With blur disabled, `options.xray`
   is still true and `blur` is false, so the client gets an *unblurred* hole onto the wallpaper —
   strictly worse than doing nothing. `capabilities()` also keeps advertising `Blur` in that state
   instead of clearing the bit (the protocol explicitly supports capabilities changing, and says the
   effect stops being applied when the bit goes away).
3. **No corner rounding for client regions.** `clip_to_geometry` is unreachable, so the blur under a
   rounded CSD corner is square unless the client's own region is rounded. KWin has a shader for
   exactly this.

Then the absent capabilities, in the order I'd take them:

4. **Subsurfaces** (`layout/tile.rs:1301`). A GTK/Qt client that puts its blurred chrome on a
   subsurface gets nothing today.
5. **A contrast/tint step.** We have saturation and noise; KWin's merged contrast matrix and macOS's
   per-material tint are what make light-mode blur legible. Ours is one global recipe with no
   appearance awareness — and the shell plate already follows `color-scheme`
   (`docs/fork/` shell-plate work), so the seam exists.
6. **Active/inactive fallback**, macOS-style: an unfocused window stops paying for a live blur and
   gains a focus cue. Cheapest perf win on the list.
7. **XWayland** `_KDE_NET_WM_BLUR_BEHIND_REGION`.
8. **Occlusion skip** for the real-backdrop path — the xray path already checks whether an opaque
   workspace background fully covers the element; the framebuffer path should not capture at all
   when the surface is fully occluded or the region is empty.
