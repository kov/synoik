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
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Resource, Weak};
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};
use smithay::wayland::compositor::{self, SurfaceData};
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
        // timeline. Gating on the retire poll instead is what made a cached blur's offscreen
        // rebuild itself forever after a wallpaper change —
        // `VulkanRenderer::discount_pending_holds` has the whole story.
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
struct ShmTextureCache(Mutex<HashMap<ContextId<VkTexture>, ShmCacheEntry>>);

struct ShmCacheEntry {
    tex: VkTexture,
    /// Damage smithay already handed us for commits whose pixels never reached `tex` — an
    /// off-thread copy that was discarded because a newer commit overtook it. Smithay reports each
    /// commit's damage once, so the next import owes these rectangles on top of its own.
    carry: Vec<Rectangle<i32, BufferCoord>>,
}

/// A surface's own handle, kept in its `data_map` so an import — which smithay hands only the
/// [`SurfaceData`] — can name the surface an off-thread copy belongs to. Recorded on every commit
/// by [`note_surface_handle`]; weak, so it cannot keep a destroyed surface alive.
struct SurfaceHandle(Weak<WlSurface>);

/// Remember `surface`'s handle for its shm imports. Called from the commit handler, before the
/// commit reaches smithay's buffer handling.
pub fn note_surface_handle(surface: &WlSurface) {
    compositor::with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| SurfaceHandle(surface.downgrade()));
    });
}

/// `surface`'s cached shm texture for renderer context `id`.
pub(super) fn cached_shm_texture(
    surface: &WlSurface,
    id: &ContextId<VkTexture>,
) -> Option<VkTexture> {
    compositor::with_states(surface, |states| {
        let cache = states.data_map.get::<ShmTextureCache>()?;
        let cache = cache.0.lock().unwrap();
        cache.get(id).map(|entry| entry.tex.clone())
    })
}

/// Owe `damage` to `surface`'s next import for context `id` — see [`ShmCacheEntry::carry`].
pub(super) fn carry_shm_damage(
    surface: &WlSurface,
    id: &ContextId<VkTexture>,
    damage: Vec<Rectangle<i32, BufferCoord>>,
) {
    compositor::with_states(surface, |states| {
        if let Some(cache) = states.data_map.get::<ShmTextureCache>() {
            if let Some(entry) = cache.0.lock().unwrap().get_mut(id) {
                entry.carry.extend(damage);
            }
        }
    });
}

impl ImportMemWl for VulkanRenderer {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        // Read the shm pool, validate its geometry, and either refresh the cached image in place
        // or import a new one. The rows are written **into** their destination — the staging
        // mapping on the cache-hit path — rather than repacked into an intermediate `Vec` first:
        // a HiDPI client shipping tens of MiB per commit paid for that `Vec` twice over, once to
        // allocate and fill it and once to copy it in, both into never-touched pages, on the
        // compositor thread, every frame. The per-surface cache below reuses the `VkImage` and
        // its staging so an actively-updating client allocates nothing per commit, and on a hit
        // only the rectangles the client damaged are copied, as mutter does
        // (`process_shm_buffer_damage`): the damage is what changed since the commit this image
        // last took, which is exactly what the cached image is missing.
        //
        // Everything that touches the mapped pool happens inside `with_buffer_contents`, which is
        // the only place smithay guarantees it is valid and SIGBUS-guarded.
        //
        // A commit too big to copy on the frame is handed to the off-thread uploader instead
        // (`shm_upload`), and the frame shows the image's previous contents until it lands.
        let id = self.context_id();
        let cached = surface.and_then(|surface| {
            let cache = surface
                .data_map
                .get_or_insert_threadsafe(ShmTextureCache::default);
            let mut cache = cache.0.lock().unwrap();
            cache
                .get_mut(&id)
                .map(|entry| (entry.tex.clone(), std::mem::take(&mut entry.carry)))
        });
        // Only a surface we can name again when its copy lands can have one off the frame.
        let handle = surface
            .filter(|_| self.shm_uploads_enabled())
            .and_then(|surface| surface.data_map.get::<SurfaceHandle>())
            .map(|handle| handle.0.clone());

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
            if let Some((tex, carry)) = cached {
                if tex.size() == size && tex.format() == Some(fourcc) {
                    let mut damage = damage.to_vec();
                    damage.extend(carry);
                    if let Some(handle) = &handle {
                        // One copy in flight per surface: a commit arriving meanwhile waits
                        // for it, and is the one copied next.
                        if self.shm_upload_in_flight(handle) {
                            self.defer_shm_upload(handle, buffer, size, damage);
                            return Ok(Imported::Reused(tex));
                        }
                        let bytes = match damage_regions(&damage, size) {
                            Some(regions) => regions
                                .iter()
                                .map(|r| u64::from(r.extent.width) * u64::from(r.extent.height) * 4)
                                .sum(),
                            None => rows.packed_len() as u64,
                        };
                        if bytes > super::shm_upload::OFF_THREAD_MIN_BYTES
                            && self.start_shm_upload(handle, buffer, size, damage.clone())
                        {
                            return Ok(Imported::Reused(tex));
                        }
                    }
                    match damage_regions(&damage, size) {
                        // Damage says nothing changed; the image already holds these pixels.
                        Some(regions) if regions.is_empty() => {}
                        Some(regions) => {
                            let partial =
                                self.reupload_shm_regions_with(&tex, regions.clone(), |dst| {
                                    rows.write_regions_into(&regions, dst)
                                })?;
                            if !partial {
                                self.reupload_shm_with(&tex, |dst| rows.write_into(dst))?;
                            }
                        }
                        None => self.reupload_shm_with(&tex, |dst| rows.write_into(dst))?,
                    }
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
        if let Some(handle) = &handle {
            self.forget_deferred_shm_upload(handle);
        }
        cache.insert(
            id,
            ShmCacheEntry {
                tex: tex.clone(),
                carry: Vec::new(),
            },
        );
        Ok(tex)
    }
}

/// Past this many damage rectangles a commit's upload copies their bounding box instead: a region
/// is a `VkBufferImageCopy` and a row loop each, and a client that damages a scatter of glyphs
/// should not turn into hundreds of them. Mutter has no cap; it has no per-region command either.
const MAX_DAMAGE_REGIONS: usize = 32;

/// The rectangles of a `size` buffer an shm re-upload has to copy for `damage`, or `None` for the
/// whole buffer.
///
/// The damage is the client's, so every rectangle is clamped to the buffer and the empty ones
/// dropped — which can leave none at all: a commit that damaged nothing needs no upload. A
/// rectangle covering the whole buffer means a whole upload, which also skips the partial copy's
/// layout round trip. Overlapping rectangles are copied twice rather than merged; the bytes are
/// the same either way.
fn damage_regions(
    damage: &[Rectangle<i32, BufferCoord>],
    size: Size<i32, BufferCoord>,
) -> Option<Vec<ash::vk::Rect2D>> {
    let bounds = Rectangle::from_size(size);
    let clamped: Vec<Rectangle<i32, BufferCoord>> = damage
        .iter()
        .filter_map(|rect| rect.intersection(bounds))
        .filter(|rect| !rect.is_empty())
        .collect();
    if clamped.contains(&bounds) {
        return None;
    }
    let clamped = if clamped.len() > MAX_DAMAGE_REGIONS {
        let bbox = clamped
            .iter()
            .copied()
            .reduce(|a, b| a.merge(b))
            .expect("more than the cap is not empty");
        if bbox == bounds {
            return None;
        }
        vec![bbox]
    } else {
        clamped
    };
    Some(
        clamped
            .into_iter()
            .map(|rect| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: rect.loc.x,
                    y: rect.loc.y,
                },
                extent: ash::vk::Extent2D {
                    width: rect.size.w as u32,
                    height: rect.size.h as u32,
                },
            })
            .collect(),
    )
}

/// A validated view of an shm pool's pixel rows: where each row starts and how many bytes it is.
///
/// Holds the geometry checks in one place so the two consumers — writing straight into a staging
/// mapping, and building a tight `Vec` for a fresh import — cannot disagree about what is in
/// bounds. Constructing one proves every row lies inside `pool`, so writing is infallible.
pub(super) struct ShmRows<'a> {
    pool: &'a [u8],
    offset: usize,
    stride: usize,
    row_bytes: usize,
    height: usize,
}

impl<'a> ShmRows<'a> {
    pub(super) fn new(
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
    pub(super) fn write_into(&self, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), self.packed_len());
        for (row, out) in dst.chunks_exact_mut(self.row_bytes).enumerate() {
            let start = self.offset + row * self.stride;
            out.copy_from_slice(&self.pool[start..start + self.row_bytes]);
        }
    }

    /// Write rows `rows` tightly packed into `dst`, which must be exactly their size — one band of
    /// an off-thread copy, which re-enters the pool band by band.
    pub(super) fn write_rows_into(&self, rows: std::ops::Range<usize>, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), rows.len() * self.row_bytes);
        for (row, out) in rows.zip(dst.chunks_exact_mut(self.row_bytes)) {
            let start = self.offset + row * self.stride;
            out.copy_from_slice(&self.pool[start..start + self.row_bytes]);
        }
    }

    /// Write each of `regions` tightly packed into `dst`, one after another in order — the layout
    /// a partial re-upload's copies read ([`synoik_vk::texture::StagedTexture::record`]). `dst`
    /// must be exactly their total size, and every region inside the buffer, which
    /// [`damage_regions`] guarantees by clamping; a row inside the buffer is inside the pool, so
    /// this cannot read out of bounds.
    fn write_regions_into(&self, regions: &[ash::vk::Rect2D], dst: &mut [u8]) {
        let mut at = 0;
        for r in regions {
            let (x, y) = (r.offset.x as usize, r.offset.y as usize);
            let bytes = r.extent.width as usize * 4;
            for row in y..y + r.extent.height as usize {
                let start = self.offset + row * self.stride + x * 4;
                dst[at..at + bytes].copy_from_slice(&self.pool[start..start + bytes]);
                at += bytes;
            }
        }
        debug_assert_eq!(at, dst.len(), "regions must fill the staging exactly");
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
    use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};

    use super::{damage_regions, ShmRows, MAX_DAMAGE_REGIONS};

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

    fn damage(rects: &[(i32, i32, i32, i32)]) -> Vec<Rectangle<i32, BufferCoord>> {
        rects
            .iter()
            .map(|&(x, y, w, h)| Rectangle::new((x, y).into(), (w, h).into()))
            .collect()
    }

    fn as_tuples(regions: &[ash::vk::Rect2D]) -> Vec<(i32, i32, u32, u32)> {
        regions
            .iter()
            .map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
            .collect()
    }

    /// The damage is the client's to get wrong: a rectangle hanging off the buffer is clamped to
    /// it, one wholly outside it (or empty) is dropped — so a commit can end up needing no upload
    /// at all — and one covering the whole buffer asks for a whole upload.
    #[test]
    fn damage_regions_clamps_the_clients_rectangles() {
        let size = Size::<i32, BufferCoord>::from((100, 80));

        let regions = damage_regions(&damage(&[(90, 70, 50, 50), (-10, 5, 20, 10)]), size);
        assert_eq!(
            as_tuples(&regions.expect("partial")),
            vec![(90, 70, 10, 10), (0, 5, 10, 10)]
        );

        let regions = damage_regions(&damage(&[(200, 200, 10, 10), (5, 5, 0, 10)]), size);
        assert_eq!(regions.map(|r| r.len()), Some(0), "nothing left to copy");

        assert!(damage_regions(&damage(&[(0, 0, i32::MAX, i32::MAX)]), size).is_none());
        assert!(damage_regions(&damage(&[(10, 10, 5, 5), (-5, -5, 200, 200)]), size).is_none());
    }

    /// Past the cap a scatter of damage becomes its bounding box, and a bounding box that is the
    /// whole buffer is a whole upload.
    #[test]
    fn damage_regions_caps_a_scatter_at_its_bounding_box() {
        let size = Size::<i32, BufferCoord>::from((1000, 1000));
        let scatter: Vec<_> = (0..=MAX_DAMAGE_REGIONS as i32)
            .map(|i| (10 + i * 10, 20 + i * 5, 4, 4))
            .collect();
        let regions = damage_regions(&damage(&scatter), size).expect("partial");
        let last = MAX_DAMAGE_REGIONS as i32;
        assert_eq!(
            as_tuples(&regions),
            vec![(10, 20, (last * 10 + 4) as u32, (last * 5 + 4) as u32)]
        );

        let mut corners = scatter.clone();
        corners.push((0, 0, 1, 1));
        corners.push((999, 999, 1, 1));
        assert!(damage_regions(&damage(&corners), size).is_none());
    }

    /// Each region's rows land tightly packed and in order, from the right place in a strided
    /// pool — over a destination pre-filled with a value the source never has, so a byte left
    /// unwritten shows.
    #[test]
    fn write_regions_into_packs_each_region_in_order() {
        // 4x3 pixels, 4 bytes of row padding; pixel (x, y) is [10*y + x, 0, 0, 255].
        let (w, h, stride) = (4usize, 3usize, 20usize);
        let mut pool = vec![0xEE; stride * h];
        for y in 0..h {
            for x in 0..w {
                pool[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[
                    (10 * y + x) as u8,
                    0,
                    0,
                    255,
                ]);
            }
        }
        let rows = ShmRows::new(&pool, 0, stride as i32, w as i32, h as i32).unwrap();
        let regions =
            damage_regions(&damage(&[(1, 1, 2, 2), (3, 0, 1, 1)]), Size::from((4, 3))).unwrap();
        let mut dst = vec![0xCD; (2 * 2 + 1) * 4];
        rows.write_regions_into(&regions, &mut dst);
        let firsts: Vec<u8> = dst.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert_eq!(firsts, vec![11, 12, 21, 22, 3]);
        assert!(dst.as_chunks::<4>().0.iter().all(|p| p[1..] == [0, 0, 255]));
    }
}
