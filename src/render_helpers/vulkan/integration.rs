// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The synoik-side renderer trait impls for [`VulkanRenderer`]: the client buffer imports
//! ([`ImportMemWl`]/[`ImportDma`]) and dmabuf-target [`Bind`].
//!
//! shm ([`ImportMemWl`]) and single-plane LINEAR dmabuf ([`ImportDma`]) client buffers import for
//! real; clients use dmabuf/shm on this stack. There is no `ImportEgl` impl because smithay only
//! folds that trait into [`ImportAll`] when built with `backend_egl` + `use_system_lib`, and this
//! build has no EGL at all — a `wl_drm` buffer can never arrive, since advertising that global is
//! itself an EGL-backend job. The dmabuf-target [`Bind`] (KMS scanout) lives in `renderer.rs`.
//!
//! [`ImportAll`]: smithay::backend::renderer::ImportAll

use std::collections::HashMap;
use std::sync::Mutex;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{
    ContextId, ImportDma, ImportDmaWl, ImportMem, ImportMemWl, Renderer, Texture,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::shm::with_buffer_contents;

use super::error::VulkanError;
use super::types::VkTexture;
use super::VulkanRenderer;
use crate::render_helpers::renderer::OffscreenRenderer;

impl OffscreenRenderer for VulkanRenderer {
    fn make_offscreen_sampleable(&mut self, texture: &VkTexture) -> anyhow::Result<()> {
        // Transition the just-rendered offscreen from TRANSFER_SRC_OPTIMAL to SHADER_READ_ONLY so a
        // later draw can sample it (the sampleable-offscreen bridge).
        self.make_sampleable(texture).map_err(Into::into)
    }

    fn offscreen_is_reusable(&mut self, texture: &mut VkTexture) -> bool {
        // Retire first, so a submit the GPU has finished stops holding anything at all. It is only
        // a poll, and nothing here may depend on it having succeeded — see below.
        self.retire_completed();

        // Then discount every reference that is ours. A pending blur, a pending layout transition
        // and an in-flight submit each hold the texture so it outlives the submit that will name
        // it — our keep-alive, not a foreign owner. Counting them is answering "not unique" about
        // ourselves, and the caller's answer to "not unique" is to throw the texture away and
        // allocate a new one: per frame, per blurred window, along with its blur chain. That is
        // the per-frame host allocation this path exists to avoid (host time and pool pressure —
        // `VulkanRenderer::readback_staging_buffer` for why it is no longer an abort), and it
        // re-queues the blur on the fresh chain each time — four full-output blurs in a frame that
        // needed one.
        //
        // Both are safe to re-render into: a queued blur has not been recorded yet, so it simply
        // blurs the new contents, and a recorded one is ordered ahead of this render on the queue
        // timeline. Gating on the retire poll instead is what made the xray buffer rebuild itself
        // forever after a wallpaper change — `VulkanRenderer::discount_pending_holds` has the
        // whole story.
        self.discount_pending_holds(texture)
    }
}

/// Per-surface cache of the shm-imported [`VkTexture`], keyed by renderer context id, stored in the
/// surface's `data_map` (freed on surface destroy). Mirrors the GLES renderer's shm texture cache
/// (`Arc<Mutex<HashMap<ContextId, ..>>>`); it lets `import_shm_buffer` reuse the same `VkImage`
/// across commits instead of re-allocating. `Mutex` because `data_map` values must be `Send +
/// Sync`. An entry keyed by a now-dead renderer `ContextId` (e.g. after a device re-add) keeps its
/// `VkImage` — and the whole `Arc<Gpu>` it holds — alive until the surface is destroyed; that is a
/// bounded, surface-lifetime retention, not a growing leak.
#[derive(Default)]
struct ShmTextureCache(Mutex<HashMap<ContextId<VkTexture>, VkTexture>>);

impl ImportMemWl for VulkanRenderer {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        // Read the shm pool, validate its geometry, and either refresh the cached image in place
        // or import a new one. The rows are written **into** their destination — the staging
        // mapping on the cache-hit path — rather than repacked into an intermediate `Vec` first:
        // a HiDPI client shipping tens of MiB per commit paid for that `Vec` twice over, once to
        // allocate and fill it and once to copy it in, both into never-touched pages, on the
        // compositor thread, every frame. (`_damage` is still ignored — damage-based partial
        // upload is a bandwidth follow-up. The per-surface cache below is the win that matters:
        // it reuses the `VkImage` and its staging so an actively-updating client allocates
        // nothing per commit.)
        //
        // Everything that touches the mapped pool happens inside `with_buffer_contents`, which is
        // the only place smithay guarantees it is valid and SIGBUS-guarded.
        let id = self.context_id();
        let cached = surface.and_then(|surface| {
            let cache = surface
                .data_map
                .get_or_insert_threadsafe(ShmTextureCache::default);
            let cache = cache.0.lock().unwrap();
            cache.get(&id).cloned()
        });

        enum Imported {
            /// The cached image was refreshed in place; nothing more to do.
            Reused(VkTexture),
            /// No usable cache entry: the tight pixels, to import as a new image.
            Fresh(Vec<u8>, Fourcc, Size<i32, BufferCoord>),
        }

        let imported = with_buffer_contents(buffer, |ptr, len, data| {
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
            let size = Size::<i32, BufferCoord>::from((data.width, data.height));
            // SAFETY: Smithay documents `ptr..ptr+len` as the valid, SIGBUS-guarded mapped pool
            // region for the duration of this callback. The slice never escapes it: both arms
            // below finish copying out before returning, because client mutation of the shared
            // memory makes a longer-lived borrow unsound.
            let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
            let rows = ShmRows::new(pool, data.offset, data.stride, data.width, data.height)
                .map_err(VulkanError::Other)?;

            // Reuse keys on `Fourcc`, not VkFormat: Argb/Xrgb8888 share `B8G8R8A8_UNORM` but
            // differ in the view's alpha swizzle, so a same-size fourcc switch must re-import.
            if let Some(tex) = cached {
                if tex.size() == size && tex.format() == Some(fourcc) {
                    self.reupload_shm_with(&tex, |dst| rows.write_into(dst))?;
                    return Ok(Imported::Reused(tex));
                }
            }
            Ok(Imported::Fresh(rows.to_packed(), fourcc, size))
        });

        let imported =
            imported.map_err(|e| VulkanError::Other(format!("shm buffer access: {e}")))??;

        let (packed, fourcc, size) = match imported {
            Imported::Reused(tex) => return Ok(tex),
            Imported::Fresh(packed, fourcc, size) => (packed, fourcc, size),
        };

        let Some(surface) = surface else {
            // No surface to hang the cache on (e.g. non-surface internal imports): keep the old
            // uncached behavior.
            return self.import_memory(&packed, fourcc, size, false);
        };
        let cache = surface
            .data_map
            .get_or_insert_threadsafe(ShmTextureCache::default);
        let mut cache = cache.0.lock().unwrap();
        let tex = self.import_memory(&packed, fourcc, size, false)?;
        cache.insert(id, tex.clone());
        Ok(tex)
    }
}

/// Copy the `width`×`height` 32-bpp region at `offset` (rows `stride` bytes apart) out of an shm
/// `pool` into a tight `width*height*4` buffer. Every access is bounds-checked against `pool` with
/// checked arithmetic — a malicious or buggy client controls these numbers. Pure and slice-based so
/// the stride/offset/bounds logic is unit-testable without a live wl_shm buffer.
/// A validated view of an shm pool's pixel rows: where each row starts and how many bytes it is.
///
/// Holds the geometry checks in one place so the two consumers — writing straight into a staging
/// mapping, and building a tight `Vec` for a fresh import — cannot disagree about what is in
/// bounds. Constructing one proves every row lies inside `pool`, so writing is infallible.
struct ShmRows<'a> {
    pool: &'a [u8],
    offset: usize,
    stride: usize,
    row_bytes: usize,
    height: usize,
}

impl<'a> ShmRows<'a> {
    fn new(
        pool: &'a [u8],
        offset: i32,
        stride: i32,
        width: i32,
        height: i32,
    ) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err(format!("shm buffer has non-positive size {width}x{height}"));
        }
        if stride < width * 4 || offset < 0 {
            return Err(format!(
                "shm buffer geometry: stride {stride}, offset {offset}, width {width}"
            ));
        }
        let (offset, stride) = (offset as usize, stride as usize);
        let (row_bytes, height) = (width as usize * 4, height as usize);
        // Check every row up front: the last one is the furthest into the pool, but the arithmetic
        // that finds it can overflow, so walk them rather than reasoning about the maximum.
        for row in 0..height {
            let start = offset
                .checked_add(row.checked_mul(stride).ok_or("shm geometry overflow")?)
                .ok_or("shm geometry overflow")?;
            let end = start
                .checked_add(row_bytes)
                .ok_or("shm geometry overflow")?;
            if end > pool.len() {
                return Err(format!(
                    "shm row {row} spans {start}..{end}, past pool len {}",
                    pool.len()
                ));
            }
        }
        Ok(Self {
            pool,
            offset,
            stride,
            row_bytes,
            height,
        })
    }

    /// Total bytes the tightly-packed pixels occupy.
    fn packed_len(&self) -> usize {
        self.row_bytes * self.height
    }

    /// Write the rows tightly packed into `dst`, which must be [`Self::packed_len`] bytes.
    ///
    /// The whole point of the type: this is the *only* copy of the pixels on the re-upload path,
    /// straight into the staging mapping.
    fn write_into(&self, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), self.packed_len());
        for (row, out) in dst.chunks_exact_mut(self.row_bytes).enumerate() {
            let start = self.offset + row * self.stride;
            out.copy_from_slice(&self.pool[start..start + self.row_bytes]);
        }
    }

    /// The rows as a fresh tight buffer, for the import path — which allocates an image anyway, so
    /// there is nothing yet to write into.
    fn to_packed(&self) -> Vec<u8> {
        let mut packed = vec![0u8; self.packed_len()];
        self.write_into(&mut packed);
        packed
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

#[cfg(test)]
mod tests {
    use super::ShmRows;

    /// `to_packed`, for the assertions below. The two producers share `write_into`, so exercising
    /// either exercises the packing; this is simply the one that returns something to compare.
    fn repack(
        pool: &[u8],
        offset: i32,
        stride: i32,
        width: i32,
        height: i32,
    ) -> Result<Vec<u8>, String> {
        ShmRows::new(pool, offset, stride, width, height).map(|rows| rows.to_packed())
    }

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
        let out = repack(&src, 0, 8, 2, 2).unwrap();
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
        let out = repack(&padded, 0, 12, 2, 2).unwrap();
        assert_eq!(out, tight, "row padding must be stripped");
    }

    #[test]
    fn repack_honors_offset() {
        let tight = tight_2x2();
        let mut with_prefix = vec![0xAA; 5]; // leading bytes before the image
        with_prefix.extend_from_slice(&tight);
        let out = repack(&with_prefix, 5, 8, 2, 2).unwrap();
        assert_eq!(out, tight);
    }

    #[test]
    fn repack_rejects_bad_geometry_and_bounds() {
        // stride < width*4
        assert!(repack(&[0; 64], 0, 4, 2, 2).is_err());
        // negative offset / size
        assert!(repack(&[0; 64], -1, 8, 2, 2).is_err());
        assert!(repack(&[0; 64], 0, 8, 0, 2).is_err());
        // last row runs past the pool
        assert!(repack(&[0; 8], 0, 8, 2, 2).is_err());
    }

    /// The re-upload path writes into a caller-owned destination rather than returning a `Vec`,
    /// and that destination is a staging mapping holding whatever the *last* upload left. So a
    /// byte `write_into` fails to write is not zero, it is a stale pixel — this pins that it
    /// covers the whole extent, over a destination pre-filled with a value the source never has.
    #[test]
    fn write_into_covers_every_byte_of_a_dirty_destination() {
        let tight = tight_2x2();
        let mut padded = Vec::new();
        for row in 0..2 {
            padded.extend_from_slice(&tight[row * 8..row * 8 + 8]);
            padded.extend_from_slice(&[0xEE; 4]);
        }
        let rows = ShmRows::new(&padded, 0, 12, 2, 2).unwrap();
        let mut dst = vec![0xCD; rows.packed_len()];
        rows.write_into(&mut dst);
        assert_eq!(
            dst, tight,
            "every byte must be written, not just the changed ones"
        );
    }
}
