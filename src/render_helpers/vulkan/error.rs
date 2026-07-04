use std::fmt;

use ash::vk;
use smithay::backend::allocator::Fourcc;

/// Error type for the Vulkan renderer. Satisfies Smithay's `RendererSuper::Error:
/// std::error::Error`.
#[derive(Debug)]
pub enum VulkanError {
    /// A raw Vulkan call returned a non-success result.
    Vk(vk::Result),
    /// A DRM format was requested that this (skeleton) renderer does not handle.
    UnsupportedFormat(Fourcc),
    /// A trait method reached a path this Stage 2 skeleton does not implement yet.
    Unsupported(&'static str),
    /// Waiting on a `SyncPoint` was interrupted.
    SyncInterrupted,
    /// Any other failure carried up from the low-level layer.
    Other(String),
}

impl fmt::Display for VulkanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VulkanError::Vk(e) => write!(f, "Vulkan call failed: {e:?}"),
            VulkanError::UnsupportedFormat(fourcc) => write!(f, "unsupported format: {fourcc:?}"),
            VulkanError::Unsupported(what) => write!(f, "unsupported in Vulkan skeleton: {what}"),
            VulkanError::SyncInterrupted => write!(f, "wait on sync point interrupted"),
            VulkanError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for VulkanError {}

impl From<vk::Result> for VulkanError {
    fn from(e: vk::Result) -> Self {
        VulkanError::Vk(e)
    }
}

impl From<smithay::backend::renderer::gles::GlesError> for VulkanError {
    // Required by `NiriRenderer::NiriError: From<GlesError>`. The Vulkan path never actually
    // produces a GLES error; this only satisfies the (GLES-historical) unified error bound.
    fn from(e: smithay::backend::renderer::gles::GlesError) -> Self {
        VulkanError::Other(format!("unexpected GLES error on the Vulkan renderer: {e}"))
    }
}

impl From<anyhow::Error> for VulkanError {
    fn from(e: anyhow::Error) -> Self {
        VulkanError::Other(format!("{e:#}"))
    }
}
