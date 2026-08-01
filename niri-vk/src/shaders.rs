//! SPIR-V blobs for the quad-family pipelines, compiled at build time (`build.rs` → `OUT_DIR`).
//!
//! Exposed from the library so both the bring-up binary and the compositor-side Vulkan renderer
//! (`niri`'s `render_helpers::vulkan`) build their pipelines from one shared shader build — the
//! `.spv` files live in this crate's `OUT_DIR`, which only this crate can `include_bytes!`.

/// Unit-quad vertex stage (positions + UVs from `gl_VertexIndex`, rect via push constants).
pub const QUAD_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
/// Solid-fill fragment stage (straight through to the push-constant color).
pub const SOLID_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/solid.frag.spv"));
/// SDF rounded-rectangle fragment stage (niri's corner shader).
pub const SDF_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdf_rect.frag.spv"));
/// SDF isoceles-triangle fragment stage (GNOME's `SwitcherPopup.drawArrow` — the app switcher's
/// multi-window chevron and the switcher's scroll arrows). Reads the apex side at the shared push
/// block's `cutoff` offset.
pub const SDF_TRIANGLE_FRAG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/sdf_triangle.frag.spv"));
/// Textured fragment stage (sample a bound `sampler2D`, tinted by the push-constant color).
pub const TEX_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/texture.frag.spv"));
/// Rounded-texture fragment stage (sample a `sampler2D`, then cut the corners with the SDF
/// rounded-rect coverage — niri's `RoundedTextureRenderElement`).
pub const ROUNDED_TEX_FRAG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/rounded_texture.frag.spv"));
/// Gradient-fade fragment stage (sample a `sampler2D`, then fade the alpha out horizontally across
/// a cutoff band — niri's `GradientFadeTextureRenderElement`).
pub const GRADIENT_FADE_FRAG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gradient_fade.frag.spv"));
/// Clipped-surface fragment stage (sample a `sampler2D` via the folded `tex_transform`, then clip
/// to a rounded rectangle in a general `input_to_geo` geometry space — niri's
/// `ClippedSurfaceRenderElement`, the window rounded-corner / clip-to-geometry path). Declares the
/// `ClippedTexturePush` block; blends straight-alpha (attenuates only the alpha). Unlike
/// `POSTPROCESS_FRAG`, sampling goes through `tex_transform`, so a y-flipped or buffer-transformed
/// client surface samples correctly.
pub const CLIPPED_TEX_FRAG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/clipped_texture.frag.spv"));
/// Border material vertex + fragment stages (angled gradient clipped to a rounded-rect ring —
/// niri's `BorderRenderElement`). Both stages declare the `BorderPush` block; the fragment outputs
/// premultiplied color.
pub const BORDER_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/border.vert.spv"));
pub const BORDER_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/border.frag.spv"));
/// Shadow material vertex + fragment stages (gaussian-blurred rounded rect with an optional window
/// cutout — niri's `ShadowRenderElement`). Both stages declare `ShadowPush`; frag outputs
/// premultiplied color.
pub const SHADOW_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.vert.spv"));
pub const SHADOW_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.frag.spv"));
/// Postprocess-and-clip material vertex + fragment stages (sample a texture, apply
/// saturation/noise/bg, then clip to a rounded rect via a general `input_to_geo` mapping — niri's
/// `ClippedSurfaceRenderElement` / `FramebufferEffectElement` postprocess). Both stages declare the
/// `PostprocessPush` block; the fragment samples `set 0` and outputs premultiplied color.
pub const POSTPROCESS_VERT: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/postprocess.vert.spv"));
pub const POSTPROCESS_FRAG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/postprocess.frag.spv"));
/// Resize cross-fade material vertex + fragment stages (blend two window snapshots bound at set 0 /
/// set 1 by progress, then optionally clip/round to the current geometry — niri's
/// `ResizeRenderElement`). Both stages declare `ResizePush`; the fragment outputs premultiplied
/// color.
pub const RESIZE_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/resize.vert.spv"));
pub const RESIZE_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/resize.frag.spv"));
/// Glyph-quad vertex + fragment stages (place a glyph quad, sample its slot in an R8 coverage
/// atlas, and tint by the push-constant color — the owned text path). Both stages declare the
/// [`crate::render::TextPush`] block; the fragment outputs straight-alpha (coverage modulates the
/// text color's alpha). Offscreen-only: the vertex stage has no output-transform `proj`, so it is
/// correct only for an identity-transform target (a UI-chrome offscreen), which is then composited
/// through the transform-aware texture material.
pub const TEXT_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.vert.spv"));
pub const TEXT_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.frag.spv"));
