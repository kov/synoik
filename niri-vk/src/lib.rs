//! Owned Vulkan (ash) render primitives for the fork's Vulkan render stack.
//!
//! Promoted out of the Stage 0/1 bring-up spike (still shipped as this crate's `niri-vk` binary,
//! `src/main.rs`) so the compositor's `--renderer=vulkan` path can reuse the low-level pieces:
//!
//!   - [`gpu`]     — instance/device bring-up, extension probing (`Gpu`),
//!   - [`render`]  — offscreen render target + solid/SDF/textured quad pipeline,
//!   - [`texture`] — sampled-texture upload,
//!   - [`blur`]    — dual-kawase blur chain,
//!   - [`text`]    — hinted swash glyph-atlas text,
//!   - [`dmabuf`]  — foreign (GBM) dmabuf allocation + import,
//!   - [`probes`]  — forward-looking physical-device capability probes.
//!
//! SPIR-V for the pipelines is compiled at build time (`build.rs`) and `include_bytes!`'d by the
//! modules that need it, so both the library and the binary target share one shader build.

pub mod blur;
pub mod dmabuf;
pub mod gpu;
pub mod probes;
pub mod render;
pub mod shaders;
pub mod stats;
pub mod sync_spike;
pub mod text;
pub mod texture;
