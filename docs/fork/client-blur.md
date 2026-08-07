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

**Fixed 2026-08-06** (§5 defects 1 and 2): client-requested blur now takes the real-backdrop path,
and blur turned off globally makes the request a no-op rather than a see-through hole. §2 describes
the state before that change and the machinery both paths still share; the resolution rule now reads:

```
has_blur_region → blur = true, xray = false        (unless a rule says otherwise)
shell-requested → blur per rule, xray = true       (unchanged: the cheap path)
blur.off        → nothing renders, capability bit cleared
```

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

1. ~~**Client blur is wallpaper-only.**~~ **DONE 2026-08-06.** `update_render_elements` no longer
   defaults xray on when the region came from the client, so client blur reaches the existing
   `FramebufferEffect` real-backdrop path. An explicit `xray: true` rule still wins; shell-requested
   effects still default to xray, which is the cheaper path and what the overview/shell chrome want.
   Pinned by `a_client_blur_region_blurs_the_real_backdrop` and its three siblings, over the
   resolution rule, and by `src/tests/background_effect.rs` over the protocol seam either side of it
   (a real client binds the manager, sets a region, and the compositor-side cache is read back).
   The draw itself was already pinned: `vulkan_backdrop_blur_honours_the_subregion` renders the
   real-backdrop path with a `set_blur_region` subregion and checks the edge softens inside it and
   stays sharp outside.
   **Cost note:** each blurred surface now pays a mid-frame capture + its own Kawase chain per
   frame, where the xray path shared one buffer per output. Both are cached across frames
   (`BackdropBlur` in the element's `UserDataMap`) and the blur records into the frame's own command
   buffer, so it is not a submit — but it is real per-surface GPU work, and gap 8 (occlusion skip)
   is what bounds it.
2. ~~**`blur.off` + a client region turns the window see-through.**~~ **DONE 2026-08-06.** `off` is
   folded into `options.blur` at resolve time, so a surface whose only effect was that blur stops
   being visible instead of degrading to an unblurred hole, and `capabilities()` clears the blur bit
   to match. The capabilities event is bind-time only; that is honest while `Blur::off` has no
   runtime writer, and the handler says what to do when one appears.
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
8. **Bound the real-backdrop cost.** Originally written as "add an occlusion skip"; that was
   mis-scoped. Read against `smithay/src/backend/renderer/damage/mod.rs`, the damage tracker
   already does all three skips, and does them better than a compositor-side check could:
   - **Occluded or undamaged**: element damage subtracts the opaque regions of everything above,
     and an empty result `continue`s *before* both `capture_framebuffer` and `draw` (`mod.rs:907`).
   - **Nothing below changed**: `needs_capture` is set only when the accumulated damage below the
     effect overlaps it (`mod.rs:689-724`). A static scene captures and blurs nothing at all.
   - **Empty client region**: `render_params_for_tile` already returns `None`.

   What is genuinely left is allocation churn under **animated geometry**. `BackdropBlur::matches`
   keys on the exact intermediate size, so a geometry that moves by one pixel discards the capture
   texture, the whole Kawase chain (a level image and its ping-pong twin per pass, with render
   passes and descriptor sets) and the blurred output, and rebuilds them — every frame of the
   animation. Pinned by `vulkan_backdrop_blur_rebuilds_on_every_size_change`. Two halves:
   - **The stall — DONE.** `SharedBlurChain::drop` called `device_wait_idle`, on the strength of a
     comment saying it "only runs when a chain is rebuilt … never per frame"; under an animating
     geometry that is exactly what it did, once per frame, on the compositor thread. Removed: a
     refcount of zero already implies every submit that recorded the chain has completed, by all
     four release paths (deferred finish → `InFlightSubmit`, drained only past the queue timeline;
     synchronous finish → the frame outlives its own `wait_for_fences`; an errored or abandoned
     frame never submitted; `flush_pending_blurs` → `run_commands`, which waits). Renderer teardown
     is covered by `drain_in_flight()`. The invariant is written out in the `Drop`. Suite clean
     under `SYNOIK_VK_VALIDATION=1` (exit 0, zero `VULKAN ERROR`). **Not yet seat-validated.**
   - **The churn.** Giving the sizing slack (round up, blur the sub-rect) needs `BlurChain::record`
     to take a region, which it does not — synoik-vk shader work, a slice of its own. Worth
     measuring after the stall is gone rather than designing now.

   Note the intermediate size comes from the effect *geometry*, not `dst`, so overview zoom is
   explicitly not a rebuild case; window resize and open/close (`surface_anim_scale`) are.

## 6. The resize-lag report: measured, and not ours

Reported 2026-08-07 against ghost on synoik — dragging a blurred window's edge left the blur behind:
stale backdrop trailing a grow, a glass pane trailing a shrink. Long hunt; the answer came from
`tools/blur-probe/` (see its README), run on the gsrs seat with **neither** `SYNOIK_VK_VALIDATION`
nor `SYNOIK_VK_FULL_DAMAGE` set, i.e. the real partial-damage path.

The probe separates a compositor that re-captures late from a client that respecifies its blur
region late. Method: pulse the window 420x320 ↔ 1300x850 on a 1.6 s cycle, capture bursts with
`grim`, locate the window by its opaque orange frame, and measure high-frequency energy in a band
just inside the growing edge against a band mid-window. A blurred backdrop is smooth; an unblurred
one is not, so the ratio is the signal.

| arm | region behaviour | median edge/middle | max |
|---|---|---|---|
| `whole` | one oversized rect, set **once** — cannot go stale | **1.00** | 1.57 |
| `exact` | `(0,0,w,h)` respecified per configure | **0.99** | 1.40 |
| `lag:4` | the size from 4 configures ago | **2.55** | 4.10 |

`lag:4`'s low frames (0.87, 0.97) are the shrink half, where a stale *larger* region clips to the
surface and looks correct — exactly as predicted, which is what makes the arm trustworthy.

The shrink symptom was tested separately: roughness in the band immediately outside the border
against one further out. A trailing glass pane would leave the inner band smooth, i.e. a ratio well
below 1. Median was 1.03 / 1.06 / 1.07 across the three arms with **no** value below 0.95. Nothing
trails outside the window.

**Conclusion: a correctly-behaving client shows no blur lag on this compositor, on grow or shrink.
The reported symptom is reproduced exactly, and only, by a client whose region is late.** Ghost is
structurally that kind of client: it blurs through a vendored winit whose `reapply_blur_shape`
derives the region from the window size whenever the corner radii are non-zero
(`~/Projects/ghost/vendor/winit/src/platform_impl/linux/wayland/window/state.rs`). winit's own
resize-invariant `WHOLE_SURFACE` rect is used only at radius 0.

Two things this does **not** cover, and either could still be a real difference from ghost:

- **Buffer type.** The probe is `wl_shm`; ghost presents dmabuf. The capture path is shared, but
  commit timing is not obviously identical.
- **Region shape.** The probe's `exact` arm sends **one** rect. Ghost's rounded-corner region is a
  stack of them (`blur_shape_rects`). A multi-rect region respecified per frame is untested here;
  adding `--radius` to the probe would close it.
