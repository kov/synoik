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
