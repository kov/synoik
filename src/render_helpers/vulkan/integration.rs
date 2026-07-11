//! The niri-side renderer trait impls that make [`VulkanRenderer`] a [`NiriRenderer`]: the client
//! buffer imports ([`ImportMemWl`]/[`ImportEgl`]/[`ImportDma`]) and dmabuf-target [`Bind`], plus
//! the fallible GLES access ([`AsGlesRenderer`]).
//!
//! shm ([`ImportMemWl`]) and single-plane LINEAR dmabuf ([`ImportDma`]) client buffers import for
//! real; EGL client-buffer import ([`ImportEgl`]) still returns an error (clients use dmabuf/shm on
//! this stack). The dmabuf-target [`Bind`] (KMS scanout) lives in `renderer.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::display::EGLBufferReader;
use smithay::backend::egl::Error as EglError;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{
    ContextId, ImportDma, ImportDmaWl, ImportEgl, ImportMem, ImportMemWl, Renderer, Texture,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::shm::with_buffer_contents;

use super::error::VulkanError;
use super::types::VkTexture;
use super::VulkanRenderer;
use crate::render_helpers::renderer::{AsGlesRenderer, OffscreenRenderer};

impl AsGlesRenderer for VulkanRenderer {
    fn try_as_gles_renderer(&mut self) -> Option<&mut GlesRenderer> {
        None
    }
}

impl OffscreenRenderer for VulkanRenderer {
    fn make_offscreen_sampleable(&self, texture: &VkTexture) -> anyhow::Result<()> {
        // Transition the just-rendered offscreen from TRANSFER_SRC_OPTIMAL to SHADER_READ_ONLY so a
        // later draw can sample it (the sampleable-offscreen bridge).
        self.make_sampleable(texture).map_err(Into::into)
    }

    fn offscreen_is_reusable(&self, texture: &mut VkTexture) -> bool {
        texture.is_unique_reference()
    }
}

/// Per-surface cache of the shm-imported [`VkTexture`], keyed by renderer context id, stored in the
/// surface's `data_map` (freed on surface destroy). Mirrors the GLES renderer's shm texture cache
/// (`Arc<Mutex<HashMap<ContextId, ..>>>`); it lets `import_shm_buffer` reuse the same `VkImage`
/// across commits instead of re-allocating. `Mutex` because `data_map` values must be `Send +
/// Sync`.
#[derive(Default)]
struct ShmTextureCache(Mutex<HashMap<ContextId<VkTexture>, VkTexture>>);

impl ImportMemWl for VulkanRenderer {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        // Read the shm pool and repack the (possibly strided, offset) rows into a tight w*h*4
        // buffer. We upload the whole buffer (`_damage` is ignored — damage-based partial
        // upload is a bandwidth follow-up); the important win is the per-surface cache
        // below, which reuses the VkImage + staging so an actively-updating client doesn't
        // churn allocations every commit.
        let prepared = with_buffer_contents(buffer, |ptr, len, data| {
            let fourcc = match data.format {
                wl_shm::Format::Argb8888 => Fourcc::Argb8888,
                wl_shm::Format::Xrgb8888 => Fourcc::Xrgb8888,
                wl_shm::Format::Abgr8888 => Fourcc::Abgr8888,
                wl_shm::Format::Xbgr8888 => Fourcc::Xbgr8888,
                other => {
                    return Err(VulkanError::Other(format!(
                        "unsupported shm format: {other:?}"
                    )))
                }
            };
            // SAFETY: Smithay documents `ptr..ptr+len` as the valid, SIGBUS-guarded mapped pool
            // region for the duration of this callback. We copy out immediately (client mutation of
            // the shared memory makes a longer-lived borrow unsound), so the slice never escapes.
            let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
            let packed = repack_shm(pool, data.offset, data.stride, data.width, data.height)
                .map_err(VulkanError::Other)?;
            Ok((
                packed,
                fourcc,
                Size::<i32, BufferCoord>::from((data.width, data.height)),
            ))
        });

        let (packed, fourcc, size) =
            prepared.map_err(|e| VulkanError::Other(format!("shm buffer access: {e}")))??;

        // smithay re-calls this on every new-buffer commit, and the old code allocated a fresh
        // device image + a fresh HOST_VISIBLE staging buffer each time — a per-commit churn of the
        // mappable-blob type that pressures the Venus host. Reuse the cached VkImage in place when
        // the (size, fourcc) match, so an actively-updating shm client allocates nothing per frame.
        // Reuse keys on `Fourcc`, not VkFormat: Argb/Xrgb8888 share `B8G8R8A8_UNORM` but differ in
        // the view's alpha swizzle, so a same-size fourcc switch must re-import.
        let Some(surface) = surface else {
            // No surface to hang the cache on (e.g. non-surface internal imports): keep the old
            // uncached behavior.
            return self.import_memory(&packed, fourcc, size, false);
        };
        let cache = surface
            .data_map
            .get_or_insert_threadsafe(ShmTextureCache::default);
        let mut cache = cache.0.lock().unwrap();
        let id = self.context_id();
        if let Some(existing) = cache.get(&id) {
            if existing.size() == size && existing.format() == Some(fourcc) {
                let tex = existing.clone();
                drop(cache);
                self.reupload_shm(&tex, &packed)?;
                return Ok(tex);
            }
        }
        let tex = self.import_memory(&packed, fourcc, size, false)?;
        cache.insert(id, tex.clone());
        Ok(tex)
    }
}

/// Copy the `width`×`height` 32-bpp region at `offset` (rows `stride` bytes apart) out of an shm
/// `pool` into a tight `width*height*4` buffer. Every access is bounds-checked against `pool` with
/// checked arithmetic — a malicious or buggy client controls these numbers. Pure and slice-based so
/// the stride/offset/bounds logic is unit-testable without a live wl_shm buffer.
fn repack_shm(
    pool: &[u8],
    offset: i32,
    stride: i32,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, String> {
    if width <= 0 || height <= 0 {
        return Err(format!("shm buffer has non-positive size {width}x{height}"));
    }
    if stride < width * 4 || offset < 0 {
        return Err(format!(
            "shm buffer geometry: stride {stride}, offset {offset}, width {width}"
        ));
    }
    let (offset, stride) = (offset as usize, stride as usize);
    let row_bytes = width as usize * 4;
    let mut packed = vec![0u8; row_bytes * height as usize];
    for row in 0..height as usize {
        let start = offset
            .checked_add(row.checked_mul(stride).ok_or("shm geometry overflow")?)
            .ok_or("shm geometry overflow")?;
        let end = start
            .checked_add(row_bytes)
            .ok_or("shm geometry overflow")?;
        let src = pool.get(start..end).ok_or_else(|| {
            format!(
                "shm row {row} spans {start}..{end}, past pool len {}",
                pool.len()
            )
        })?;
        packed[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(src);
    }
    Ok(packed)
}

impl ImportEgl for VulkanRenderer {
    fn bind_wl_display(&mut self, _display: &DisplayHandle) -> Result<(), EglError> {
        Err(EglError::NoEGLDisplayBound)
    }

    fn unbind_wl_display(&mut self) {}

    fn egl_reader(&self) -> Option<&EGLBufferReader> {
        None
    }

    fn import_egl_buffer(
        &mut self,
        _buffer: &WlBuffer,
        _surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        Err(VulkanError::Unsupported("egl client-buffer import"))
    }
}

impl ImportDma for VulkanRenderer {
    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        _damage: Option<&[Rectangle<i32, BufferCoord>]>,
    ) -> Result<VkTexture, VulkanError> {
        // Damage is ignored: smithay caches the imported texture per (buffer, renderer) and only
        // re-imports on a new commit, so each import is a fresh full acquire of the client buffer.
        self.import_dmabuf_as_texture(dmabuf)
    }

    fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        super::renderer::dmabuf_formats()
    }
}

impl ImportDmaWl for VulkanRenderer {}

/// Compile-time proof that [`VulkanRenderer`] now satisfies the (blanket) [`NiriRenderer`] core, so
/// it can flow through the generic render-element path.
const _: fn() = || {
    fn assert_niri_renderer<R: crate::render_helpers::renderer::NiriRenderer>() {}
    assert_niri_renderer::<VulkanRenderer>();
};

#[cfg(test)]
mod tests {
    use super::repack_shm;

    // A 2x2 image where each pixel byte is (10*row + col)*10 + channel, so mispacking is obvious.
    fn tight_2x2() -> Vec<u8> {
        let mut v = Vec::new();
        for row in 0..2 {
            for col in 0..2 {
                for ch in 0..4 {
                    v.push((row * 20 + col * 10 + ch) as u8);
                }
            }
        }
        v
    }

    #[test]
    fn repack_tight_is_verbatim() {
        let src = tight_2x2();
        let out = repack_shm(&src, 0, 8, 2, 2).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn repack_strips_row_padding() {
        // stride 12 = 8 bytes of pixels + 4 bytes of padding per row.
        let tight = tight_2x2();
        let mut padded = Vec::new();
        for row in 0..2 {
            padded.extend_from_slice(&tight[row * 8..row * 8 + 8]);
            padded.extend_from_slice(&[0xEE; 4]); // padding that must be dropped
        }
        let out = repack_shm(&padded, 0, 12, 2, 2).unwrap();
        assert_eq!(out, tight, "row padding must be stripped");
    }

    #[test]
    fn repack_honors_offset() {
        let tight = tight_2x2();
        let mut with_prefix = vec![0xAA; 5]; // leading bytes before the image
        with_prefix.extend_from_slice(&tight);
        let out = repack_shm(&with_prefix, 5, 8, 2, 2).unwrap();
        assert_eq!(out, tight);
    }

    #[test]
    fn repack_rejects_bad_geometry_and_bounds() {
        // stride < width*4
        assert!(repack_shm(&[0; 64], 0, 4, 2, 2).is_err());
        // negative offset / size
        assert!(repack_shm(&[0; 64], -1, 8, 2, 2).is_err());
        assert!(repack_shm(&[0; 64], 0, 8, 0, 2).is_err());
        // last row runs past the pool
        assert!(repack_shm(&[0; 8], 0, 8, 2, 2).is_err());
    }
}
