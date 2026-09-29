// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Big shm commits copied into staging **off the render thread**.
//!
//! An shm re-upload has two halves: the host copy out of the client's pool into mapped staging,
//! and the `vkCmdCopyBufferToImage` that rides the frame's submit. The first is the whole CPU cost,
//! and for a HiDPI client it is large — Firefox repaints its full-window chrome surface on every
//! keyboard-focus change, 40–80 MiB that took 6–24 ms on the frame that opened the overview. Moved
//! here, the frame that imports such a commit only reserves staging and hands the copy to a worker,
//! and keeps showing the image's previous contents until the copy **lands**: the worker wakes the
//! event loop, [`VulkanRenderer::land_shm_uploads`] queues the GPU copy for the next frame and
//! damages the surface so that frame repaints it. One frame or two of old contents on a window
//! that just changed focus is invisible; a stalled frame in an animation is not.
//!
//! The rules that keep it correct:
//!
//! - **The client cannot write what the worker reads.** A buffer is only released — and so only
//!   writable again — when a later commit replaces it or the surface goes away, and both clear the
//!   surface's textures in smithay. A copy therefore lands only if the surface still has its
//!   texture and still has the buffer the copy read, and no newer import is waiting; otherwise it
//!   is discarded, however cleanly it finished.
//! - **Discarded work loses no damage.** Smithay reports each commit's damage once. A discarded
//!   copy's rectangles go to the next copy, or are owed to the surface's next import
//!   ([`super::integration::carry_shm_damage`]).
//! - **One copy in flight per surface.** A commit that arrives meanwhile is remembered, not
//!   started, and is the one copied next; later ones overwrite it, their damage merged.
//! - **Only copies too big for the frame come here** ([`OFF_THREAD_MIN_BYTES`]), and they are
//!   whole-buffer: a small commit gains nothing from a frame of lag, and a whole-buffer copy keeps
//!   the upload queue's superseding rule simple.
//!
//! Protocol-visible consequence: frame callbacks and presentation feedback fire for the frame that
//! still showed the previous buffer. A synchronized subsurface can likewise show its new contents a
//! frame apart from its parent's.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::JoinHandle;

use ash::vk;
use calloop::ping::Ping;
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::backend::renderer::{Renderer as _, Texture as _};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Weak;
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};
use smithay::wayland::shm::with_buffer_contents;
use synoik_vk::staging::StagingReservation;

use super::integration::{cached_shm_texture, carry_shm_damage, ShmRows};
use super::VulkanRenderer;

/// A commit whose copy would move more than this goes off the frame. The staging pool's shared
/// chunk size, so an off-thread copy always sits in a chunk of its own and never holds the shared
/// one back from rewinding while it runs; it is also about where a copy starts to cost a
/// noticeable slice of a frame (~1–3 ms at the 5–14 GB/s the seat measures).
pub(super) const OFF_THREAD_MIN_BYTES: u64 = synoik_vk::staging::MAX_POOLED_CHUNK;

/// Rows the worker copies per visit to the pool. Each visit holds the pool's lock, and a client
/// resizing its pool waits on that lock on the compositor thread — so the copy re-enters in bands
/// of about this many bytes rather than holding it for the whole buffer.
const BAND_BYTES: usize = 4 << 20;

/// The off-thread uploader a [`VulkanRenderer`] gets from
/// [`VulkanRenderer::enable_async_shm_uploads`]. Without one, every shm copy stays on the frame.
pub(super) struct ShmUploader {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Done>,
    worker: Option<JoinHandle<()>>,
    /// Keyed by the surface, which has at most one.
    in_flight: HashMap<ObjectId, InFlight>,
    #[cfg(test)]
    gate: std::sync::Arc<Gate>,
}

/// What the worker is sent: copy `buffer`'s pixels, tightly packed, into `reservation`.
struct Job {
    surface: ObjectId,
    buffer: WlBuffer,
    reservation: StagingReservation,
}

/// What the worker sends back: the filled reservation, or why it could not be filled.
struct Done {
    surface: ObjectId,
    result: Result<StagingReservation, String>,
}

/// The render thread's record of a surface's copy while the worker has it.
struct InFlight {
    surface: Weak<WlSurface>,
    /// The buffer being read. Landing requires the surface to still have it — see the module docs.
    buffer: WlBuffer,
    size: Size<i32, BufferCoord>,
    /// Every rectangle smithay reported that this copy is the first to deliver.
    damage: Vec<Rectangle<i32, BufferCoord>>,
    /// The newest commit that arrived while this one was copying.
    next: Option<Deferred>,
}

struct Deferred {
    buffer: WlBuffer,
    size: Size<i32, BufferCoord>,
    damage: Vec<Rectangle<i32, BufferCoord>>,
}

/// Tests only: holds the worker before each copy, so a test can see the frame that still shows the
/// previous contents without racing the worker to it.
#[cfg(test)]
#[derive(Default)]
struct Gate {
    held: std::sync::Mutex<bool>,
    released: std::sync::Condvar,
}

impl ShmUploader {
    fn spawn(waker: Ping) -> std::io::Result<Self> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel::<Done>();
        #[cfg(test)]
        let gate = std::sync::Arc::new(Gate::default());
        #[cfg(test)]
        let worker_gate = gate.clone();
        let worker = std::thread::Builder::new()
            .name("synoik-shm-upload".to_owned())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    #[cfg(test)]
                    {
                        let mut held = worker_gate.held.lock().unwrap();
                        while *held {
                            held = worker_gate.released.wait(held).unwrap();
                        }
                    }
                    let result = copy_out(&job.buffer, job.reservation);
                    if done_tx
                        .send(Done {
                            surface: job.surface,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    waker.ping();
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            done,
            worker: Some(worker),
            in_flight: HashMap::new(),
            #[cfg(test)]
            gate,
        })
    }
}

impl Drop for ShmUploader {
    fn drop(&mut self) {
        // Closing the job channel ends the worker's loop; joining waits out at most the copy it
        // is on, so no reservation outlives the renderer's teardown on another thread.
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker's half: `buffer`'s rows, tightly packed, into `reservation`, a band at a time.
fn copy_out(
    buffer: &WlBuffer,
    mut reservation: StagingReservation,
) -> Result<StagingReservation, String> {
    let dst = reservation.bytes_mut();
    let mut row = 0;
    loop {
        let band = with_buffer_contents(buffer, |ptr, len, data| {
            // SAFETY: as in `import_shm_buffer` — smithay documents `ptr..ptr+len` as the valid,
            // SIGBUS-guarded pool for the duration of this callback, on this thread, and the slice
            // does not escape it.
            let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
            let rows = ShmRows::new(pool, data.offset, data.stride, data.width, data.height)?;
            let row_bytes = data.width as usize * 4;
            let height = data.height as usize;
            if dst.len() != row_bytes * height {
                return Err(format!(
                    "shm buffer is {}x{}, staging holds {} bytes",
                    data.width,
                    data.height,
                    dst.len()
                ));
            }
            let end = height.min(row + (BAND_BYTES / row_bytes).max(1));
            rows.write_rows_into(row..end, &mut dst[row * row_bytes..end * row_bytes]);
            Ok((end, height))
        })
        .map_err(|e| format!("shm buffer access: {e}"))??;
        row = band.0;
        if row == band.1 {
            return Ok(reservation);
        }
    }
}

impl VulkanRenderer {
    /// Copy big shm commits off the render thread from now on, waking the event loop through
    /// `waker` when one finishes — whose handler must call [`Self::land_shm_uploads`].
    pub fn enable_async_shm_uploads(&mut self, waker: Ping) {
        if self.shm_uploads.is_some() {
            return;
        }
        match ShmUploader::spawn(waker) {
            Ok(uploader) => self.shm_uploads = Some(uploader),
            Err(err) => warn!("could not spawn the shm upload thread: {err}; copying on the frame"),
        }
    }

    pub(super) fn shm_uploads_enabled(&self) -> bool {
        self.shm_uploads.is_some()
    }

    pub(super) fn shm_upload_in_flight(&self, surface: &Weak<WlSurface>) -> bool {
        self.shm_uploads
            .as_ref()
            .is_some_and(|up| up.in_flight.contains_key(&surface.id()))
    }

    /// Remember a commit that arrived while `surface`'s copy is in flight, to copy once it is done.
    /// It supersedes any commit already waiting, whose damage it inherits.
    pub(super) fn defer_shm_upload(
        &mut self,
        surface: &Weak<WlSurface>,
        buffer: &WlBuffer,
        size: Size<i32, BufferCoord>,
        mut damage: Vec<Rectangle<i32, BufferCoord>>,
    ) {
        let Some(flight) = self
            .shm_uploads
            .as_mut()
            .and_then(|up| up.in_flight.get_mut(&surface.id()))
        else {
            return;
        };
        if let Some(waiting) = flight.next.take() {
            damage.extend(waiting.damage);
        }
        flight.next = Some(Deferred {
            buffer: buffer.clone(),
            size,
            damage,
        });
    }

    /// Start copying `buffer` for `surface` off the thread. `false` if it could not be started
    /// (no staging, or the worker is gone), in which case the caller copies it on the frame.
    pub(super) fn start_shm_upload(
        &mut self,
        surface: &Weak<WlSurface>,
        buffer: &WlBuffer,
        size: Size<i32, BufferCoord>,
        damage: Vec<Rectangle<i32, BufferCoord>>,
    ) -> bool {
        let len = size.w as vk::DeviceSize * size.h as vk::DeviceSize * 4;
        if self.shm_uploads.is_none() || len == 0 {
            return false;
        }
        let reservation = match self.staging_pool.reserve(&self.gpu, len) {
            Ok(reservation) => reservation,
            Err(err) => {
                warn!("reserving staging for an off-thread shm copy: {err:#}");
                return false;
            }
        };
        let up = self.shm_uploads.as_mut().expect("checked above");
        let key = surface.id();
        let job = Job {
            surface: key.clone(),
            buffer: buffer.clone(),
            reservation,
        };
        if up.jobs.as_ref().is_none_or(|jobs| jobs.send(job).is_err()) {
            return false;
        }
        up.in_flight.insert(
            key,
            InFlight {
                surface: surface.clone(),
                buffer: buffer.clone(),
                size,
                damage,
                next: None,
            },
        );
        true
    }

    /// Queue every finished off-thread copy that is still current, and damage its surface so the
    /// next frame repaints it. Returns whether anything landed, i.e. whether a redraw is due.
    ///
    /// Called from the event loop when the worker wakes it — between frames, so the copy is
    /// queued before the next frame's `begin` records the queue.
    pub fn land_shm_uploads(&mut self) -> bool {
        let Some(up) = self.shm_uploads.as_mut() else {
            return false;
        };
        let finished: Vec<Done> = up.done.try_iter().collect();
        let id = self.context_id();
        let mut landed = false;
        for done in finished {
            let Some(flight) = self
                .shm_uploads
                .as_mut()
                .and_then(|up| up.in_flight.remove(&done.surface))
            else {
                continue;
            };
            // Gone: its copy, and anything waiting behind it, have nowhere to land.
            let Ok(surface) = flight.surface.upgrade() else {
                continue;
            };
            let current = flight.next.is_none()
                && with_renderer_surface_state(&surface, |state| {
                    state.texture(id.clone()).is_some()
                        && state.buffer().is_some_and(|b| *b == flight.buffer)
                })
                .unwrap_or(false);
            match done.result {
                Ok(reservation) if current => {
                    let tex = cached_shm_texture(&surface, &id).filter(|t| t.size() == flight.size);
                    if let Some(tex) = tex {
                        match tex.stage_reupload_shm_reserved(reservation) {
                            Ok(staged) => {
                                self.queue_texture_upload(&tex, staged);
                                with_renderer_surface_state(&surface, |state| {
                                    state.add_damage(flight.damage.iter().copied())
                                });
                                landed = true;
                                continue;
                            }
                            Err(err) => warn!("landing an off-thread shm copy: {err:#}"),
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => debug!("off-thread shm copy failed: {err}"),
            }
            // Not landed: the damage is still owed, to the commit waiting behind this one or to
            // the surface's next import.
            let mut damage = flight.damage;
            match flight.next {
                Some(next) => {
                    damage.extend(next.damage);
                    let handle = flight.surface;
                    if !self.start_shm_upload(&handle, &next.buffer, next.size, damage.clone()) {
                        // Nothing will import this commit again — smithay already has its
                        // texture — so copy it now, on the frame, rather than leave it stale.
                        if self.copy_shm_now(&surface, &next.buffer, next.size) {
                            with_renderer_surface_state(&surface, |state| {
                                state.add_damage(damage.iter().copied())
                            });
                            landed = true;
                        } else {
                            carry_shm_damage(&surface, &id, damage);
                        }
                    }
                }
                None => carry_shm_damage(&surface, &id, damage),
            }
        }
        landed
    }

    /// Copy `buffer` whole into `surface`'s cached texture on this thread — the fallback when an
    /// off-thread copy cannot be started for a commit nothing else will import again.
    fn copy_shm_now(
        &mut self,
        surface: &WlSurface,
        buffer: &WlBuffer,
        size: Size<i32, BufferCoord>,
    ) -> bool {
        let id = self.context_id();
        let Some(tex) = cached_shm_texture(surface, &id).filter(|t| t.size() == size) else {
            return false;
        };
        let copied = with_buffer_contents(buffer, |ptr, len, data| {
            // SAFETY: see `copy_out`.
            let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
            let rows = ShmRows::new(pool, data.offset, data.stride, data.width, data.height)?;
            if data.width != size.w || data.height != size.h {
                return Err("shm buffer changed size".to_owned());
            }
            self.reupload_shm_with(&tex, |dst| rows.write_into(dst))
                .map_err(|e| e.to_string())
        });
        matches!(copied, Ok(Ok(())))
    }

    /// Forget any commit waiting behind `surface`'s copy: a fresh import just copied a newer
    /// buffer whole, so there is nothing left for it to deliver.
    pub(super) fn forget_deferred_shm_upload(&mut self, surface: &Weak<WlSurface>) {
        if let Some(flight) = self
            .shm_uploads
            .as_mut()
            .and_then(|up| up.in_flight.get_mut(&surface.id()))
        {
            flight.next = None;
        }
    }

    /// Tests only: hold the worker before its next copy until [`Self::release_shm_uploads`].
    #[cfg(test)]
    pub(crate) fn hold_shm_uploads(&mut self) {
        if let Some(up) = &self.shm_uploads {
            *up.gate.held.lock().unwrap() = true;
        }
    }

    /// Tests only: let a held worker go.
    #[cfg(test)]
    pub(crate) fn release_shm_uploads(&mut self) {
        if let Some(up) = &self.shm_uploads {
            *up.gate.held.lock().unwrap() = false;
            up.gate.released.notify_all();
        }
    }

    /// Tests only: copies the worker has not handed back and landed yet.
    #[cfg(test)]
    pub(crate) fn shm_uploads_in_flight(&self) -> usize {
        self.shm_uploads.as_ref().map_or(0, |up| up.in_flight.len())
    }
}
