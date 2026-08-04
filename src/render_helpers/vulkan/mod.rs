//! An owned Vulkan (ash) renderer that implements Smithay's renderer trait family, so synoik's
//! `RenderElement`s can draw through it exactly as they do through GLES.
//!
//! This is the Stage 2 "walking skeleton" (see `docs/fork/STRATEGY.md` §3.10): it handles only
//! what the two simplest elements need — a solid quad ([`Frame::draw_solid`]) and a
//! full-`src`, `Transform::Normal`, arbitrary-`dst`-scale memory texture
//! ([`Frame::render_texture_from_to`]) — plus offscreen [`Bind`]/[`Offscreen`] targets and
//! [`ExportMem`] readback so it can be A/B'd against a reference renderer headlessly. Everything
//! else (dmabuf import, non-Normal transforms, sub-`src` sampling, flipped textures, the custom
//! niri effects) is deliberately unimplemented and returns an error rather than a wrong picture.
//!
//! The low-level Vulkan pieces (device bring-up, pipelines, texture upload, SPIR-V) live in the
//! [`synoik_vk`] workspace library; this module is only the Smithay-trait glue, templated
//! method-for-method on Smithay's `PixmanRenderer`.

mod backdrop_blur;
mod blur_chain;
mod custom;
mod effect_blur;
mod error;
mod fence;
mod frame;
mod gaussian_backdrop;
mod integration;
mod renderer;
#[cfg(test)]
mod tests;
mod types;

pub(crate) use backdrop_blur::BackdropBlur;
pub(crate) use custom::{pack_affine, CustomAnimPush, CustomResizePush, CustomShaderType};
pub(crate) use effect_blur::EffectBlur;
pub use error::VulkanError;
pub use frame::VulkanFrame;
pub(crate) use frame::{pack_mat3, premultiply, ClipParams};
pub(crate) use gaussian_backdrop::GaussianBackdrop;
pub use renderer::{dmabuf_formats, VulkanRenderer};
pub(crate) use types::GlyphRun;
pub use types::{VkMapping, VkTexture, NATIVE_FOURCC};
