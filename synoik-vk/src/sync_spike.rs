// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Stage 3 de-risking spike: does host-GPU work completion actually propagate to a guest
//! `sync_file` / `drm_syncobj` on this VM?
//!
//! `docs/fork/venus-explicit-sync-gap.md` establishes the *API surface*: only **binary `SYNC_FD`**
//! external semaphores/fences export here (no `OPAQUE_FD`, no timeline export), and the kernel
//! advertises `drm_syncobj` timeline caps. The portable explicit-sync path we intend to build
//! (Wayland `linux-drm-syncobj-v1` + KMS `IN_FENCE`/`OUT_FENCE`) is: kernel timeline syncobj point
//! ⟷ `sync_file` ⟷ binary VkSemaphore/VkFence via `SYNC_FD`.
//!
//! A capability bit cannot prove the one thing that would sink that design: that a guest fence is
//! signalled by **host-GPU completion**, not merely by a CPU `SIGNAL` ioctl or (the dual failure)
//! signalled *immediately* at submit. Nor can it distinguish a real pipelined fence from a driver
//! that emulates `SYNC_FD` export by **blocking until completion and then returning `-1`** — which
//! is exactly what lavapipe does, and would silently serialize every frame.
//!
//! So this measures behaviour, offscreen, on the render node (`renderD128` — every syncobj ioctl
//! is `DRM_RENDER_ALLOW`, no DRM master needed):
//!
//!  1. Enqueue calibrated GPU busy-work of measured duration `D` (dependent buffer copies — command
//!     buffer commands are never dead-code-eliminated, unlike a compute spin loop).
//!  2. Export the still-pending completion as a `sync_file` (binary semaphore **and** fence paths).
//!     A *trustworthy* export is: returns a real FD (`>= 0`) in `≪ D` (non-blocking), the
//!     `sync_file` is **unsignalled at export** (`SYNC_IOC_FILE_INFO` status `== 0`), and it names
//!     a real kernel `dma_fence` (`driver_name`, expected `virtio_gpu`).
//!  3. Then `poll(2)` / `SYNCOBJ_WAIT` / `TIMELINE_WAIT` must block `≈ D` (not ~0, not forever).
//!
//! A driver is classified **Pipelined** only if all of (2)+(3) hold; anything else is **Emulated**.
//! lavapipe is the built-in negative control and must come back *not* Pipelined — that is what
//! proves the test has teeth rather than rubber-stamping.
//!
//! ## Measured result (Venus on this VM, 2026-07-05)
//!
//! A **fence-vs-semaphore asymmetry** the gap doc's §5 did not anticipate (it assumed the semaphore
//! path): **`VkFence` `SYNC_FD` export is real and pipelined** (export in ~0.01 ms, `virtio_gpu`
//! dma_fence, unsignalled at export, downstream wait blocks `≈ D`), and that fence carries all the
//! way through a kernel `drm_syncobj` — `IMPORT_SYNC_FILE` → `SYNCOBJ_WAIT`, and
//! `TRANSFER` binary→timeline → `TIMELINE_WAIT` (the exact client-release shape) — plus
//! `SYNCOBJ_EVENTFD` (Smithay's gate) fires. But **`VkSemaphore` `SYNC_FD` export is emulated**:
//! Venus blocks on the CPU until completion, then hands back an already-signalled `detached-driver`
//! stub. So host-GPU completion **does** propagate here, and **the KMS/Wayland explicit-sync design
//! is buildable — but it must rest on VkFence export, not VkSemaphore export.** lavapipe emulates
//! both, as expected. See `docs/fork/venus-explicit-sync-gap.md` §6 (now answered).
//!
//! This touches no live render/present path; it only extends the shared device with the two
//! `SYNC_FD` extensions (see `gpu.rs`) that the real renderer will need anyway.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ash::vk;
use drm_ffi::syncobj;

use crate::gpu::Gpu;

/// Device-local scratch buffer size for one copy in the busy-work chain.
const BUSY_BUF: vk::DeviceSize = 16 << 20; // 16 MiB
/// Target calibrated busy-work duration; big enough that VM scheduling jitter is noise and the
/// "export in ≪ D" margin is decisive.
const TARGET_D: Duration = Duration::from_millis(80);
/// Upper bound on the copy-chain length so a fast GPU doesn't blow up wall-clock during
/// calibration.
const MAX_ITERS: u32 = 8192;

/// DRM ioctl magic (`'d'`) and `SYNC_IOC_MAGIC` (`'>'`).
const DRM_IOCTL_MAGIC: u32 = 0x64;
const SYNC_IOC_MAGIC: u32 = 0x3E;
/// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` command number.
const SYNCOBJ_FD_TO_HANDLE_NR: u32 = 0xC2;
/// `SYNC_IOC_FILE_INFO` command number.
const SYNC_FILE_INFO_NR: u32 = 4;

// ---- report types ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Export non-blocking + unsignalled at export + real dma_fence, wait blocked ≈ D: host-GPU
    /// completion propagates through the `sync_file`. The explicit-sync design is buildable here.
    Pipelined,
    /// Export blocked ~D and/or returned -1 and/or was already signalled: the driver serializes
    /// (lavapipe-style emulation). The `sync_file` carries no useful pending fence.
    Emulated,
    /// Neither pattern cleanly held (e.g. export failed or timings were ambiguous).
    Inconclusive,
}

/// One measured export→wait path.
#[derive(Debug, Clone)]
pub struct StageResult {
    pub name: String,
    /// Calibrated busy-work duration this stage raced against.
    pub d: Duration,
    /// Wall-time of the FD export call itself (want ≪ D).
    pub export_latency: Duration,
    /// Export returned a real FD (`>= 0`) rather than -1 (already-signalled emulation).
    pub fd_valid: bool,
    /// `sync_file` status sampled right after export: 0 = active/unsignalled (want), 1 =
    /// signalled.
    pub status_at_export: Option<i32>,
    /// Per-fence `driver_name` from `SYNC_IOC_FILE_INFO` (expected `virtio_gpu` on Venus).
    pub driver_name: Option<String>,
    /// Wall-time the downstream wait blocked (want ≈ D).
    pub wait: Option<Duration>,
    pub verdict: Verdict,
    /// Set when the stage could not run to completion (e.g. syncobj import failed); the run still
    /// continues so a bridge-stage bug never masks the core Stage-0 answer.
    pub note: Option<String>,
}

impl StageResult {
    fn classify(
        d: Duration,
        export_latency: Duration,
        fd_valid: bool,
        status: Option<i32>,
        wait: Option<Duration>,
    ) -> Verdict {
        let fast_export = export_latency < d / 2;
        let unsignalled = status == Some(0);
        let blocked = wait.is_some_and(|w| w >= d / 2);
        if fd_valid && unsignalled && fast_export && blocked {
            Verdict::Pipelined
        } else if !fd_valid || !unsignalled || !fast_export {
            Verdict::Emulated
        } else {
            Verdict::Inconclusive
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncSpikeReport {
    pub device_name: String,
    /// CPU device (lavapipe) — the negative control; expected *not* Pipelined.
    pub device_is_cpu: bool,
    pub calibrated_d: Duration,
    pub stages: Vec<StageResult>,
    /// A never-signalled timeline point blocked and timed out (wait machinery works).
    pub neg_control_timed_out: bool,
    /// A CPU-`SIGNAL`'d syncobj woke the wait immediately (distinguishes a hang from broken
    /// waits).
    pub pos_control_signaled: bool,
    /// `DRM_IOCTL_SYNCOBJ_EVENTFD` accepted a registration (Smithay's explicit-sync gate).
    pub eventfd_supported: bool,
    /// The registered eventfd actually fired on GPU completion.
    pub eventfd_fired: bool,
}

impl SyncSpikeReport {
    pub fn stage(&self, name: &str) -> Option<&StageResult> {
        self.stages.iter().find(|s| s.name == name)
    }

    /// The headline: does host-GPU completion propagate through the explicit-sync bridge we will
    /// actually build? That bridge rests on **VkFence** `SYNC_FD` export (Venus emulates the
    /// *semaphore* path, so it is deliberately excluded here) feeding a kernel `drm_syncobj` —
    /// exactly the `fence→sync_file` core stage plus the two syncobj bridge stages.
    pub fn bridge_pipelined(&self) -> bool {
        let names = [
            "fence→sync_file",
            "syncobj→wait",
            "syncobj→transfer→timeline_wait",
        ];
        names
            .iter()
            .all(|n| self.stage(n).map(|s| s.verdict) == Some(Verdict::Pipelined))
    }

    pub fn print(&self) {
        eprintln!("synoik-vk: --- explicit-sync bridge spike ---");
        eprintln!(
            "  device: {} ({})",
            self.device_name,
            if self.device_is_cpu {
                "CPU baseline"
            } else {
                "GPU"
            }
        );
        eprintln!(
            "  calibrated busy-work D = {:.1} ms",
            self.calibrated_d.as_secs_f64() * 1e3
        );
        for s in &self.stages {
            eprintln!(
                "  [{:?}] {}: export {:.2} ms (fd_valid={}), status_at_export={:?}, driver={:?}, wait={}",
                s.verdict,
                s.name,
                s.export_latency.as_secs_f64() * 1e3,
                s.fd_valid,
                s.status_at_export,
                s.driver_name.as_deref(),
                s.wait.map(|w| format!("{:.1} ms", w.as_secs_f64() * 1e3)).unwrap_or_else(|| "-".into()),
            );
            if let Some(note) = &s.note {
                eprintln!("      note: {note}");
            }
        }
        eprintln!(
            "  controls: neg(unsignalled)_timed_out={} pos(cpu-signal)_woke={}",
            self.neg_control_timed_out, self.pos_control_signaled
        );
        eprintln!(
            "  syncobj eventfd: supported={} fired={}",
            self.eventfd_supported, self.eventfd_fired
        );
        eprintln!(
            "  VERDICT: explicit-sync bridge is {} on this device",
            if self.bridge_pipelined() {
                "PIPELINED (host-GPU completion propagates — design is buildable)"
            } else {
                "NOT pipelined (emulated/serialized)"
            }
        );
    }
}

// ---- entry point -----------------------------------------------------------------------------

pub fn run_sync_spike(gpu: &Gpu) -> Result<SyncSpikeReport> {
    if !gpu.supports("VK_KHR_external_semaphore_fd") || !gpu.supports("VK_KHR_external_fence_fd") {
        return Err(anyhow!(
            "device does not expose SYNC_FD external semaphore/fence extensions — cannot bridge sync"
        ));
    }

    let drm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .context("open /dev/dri/renderD128")?;
    let drm = drm.as_fd();

    let ext_sem = ash::khr::external_semaphore_fd::Device::new(&gpu.instance, &gpu.device);
    let ext_fence = ash::khr::external_fence_fd::Device::new(&gpu.instance, &gpu.device);

    let busy = BusyWork::new(gpu)?;
    let d = busy.calibrate()?;

    // Stage order matters only for readability; each submits its own busy-work.
    let stages = vec![
        stage_semaphore(gpu, &busy, &ext_sem, d)?,
        stage_fence(gpu, &busy, &ext_fence, d)?,
        stage_syncobj_wait(gpu, &busy, &ext_fence, drm, d)?,
        stage_timeline_transfer(gpu, &busy, &ext_fence, drm, d)?,
    ];

    let (neg_control_timed_out, pos_control_signaled) = controls(drm)?;
    let (eventfd_supported, eventfd_fired) = eventfd_probe(gpu, &busy, &ext_fence, drm, d)?;

    let device_is_cpu = gpu.device_type == vk::PhysicalDeviceType::CPU;
    Ok(SyncSpikeReport {
        device_name: gpu.device_name.clone(),
        device_is_cpu,
        calibrated_d: d,
        stages,
        neg_control_timed_out,
        pos_control_signaled,
        eventfd_supported,
        eventfd_fired,
    })
}

// ---- calibrated GPU busy-work ----------------------------------------------------------------

/// Two device-local buffers and a reusable command buffer that ping-pongs a large copy between
/// them with a serializing barrier per hop, so a submit takes a measurable, tunable wall-time.
struct BusyWork<'g> {
    gpu: &'g Gpu,
    pool: vk::CommandPool,
    cbuf: vk::CommandBuffer,
    buf_a: vk::Buffer,
    mem_a: vk::DeviceMemory,
    buf_b: vk::Buffer,
    mem_b: vk::DeviceMemory,
}

impl<'g> BusyWork<'g> {
    fn new(gpu: &'g Gpu) -> Result<Self> {
        let device = &gpu.device;
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool =
            unsafe { device.create_command_pool(&pool_ci, None) }.context("busy-work pool")?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cbuf = unsafe { device.allocate_command_buffers(&alloc) }.context("busy-work cbuf")?[0];

        let (buf_a, mem_a) = make_buffer(gpu, BUSY_BUF)?;
        let (buf_b, mem_b) = make_buffer(gpu, BUSY_BUF)?;
        Ok(Self {
            gpu,
            pool,
            cbuf,
            buf_a,
            mem_a,
            buf_b,
            mem_b,
        })
    }

    /// (Re)record the copy chain with `iters` round trips. Reusable (no ONE_TIME_SUBMIT), so the
    /// same recording is submitted for calibration and every stage.
    fn record(&self, iters: u32) -> Result<()> {
        let device = &self.gpu.device;
        unsafe {
            device
                .reset_command_buffer(self.cbuf, vk::CommandBufferResetFlags::empty())
                .context("reset busy cbuf")?;
            device
                .begin_command_buffer(self.cbuf, &vk::CommandBufferBeginInfo::default())
                .context("begin busy cbuf")?;
            let region = vk::BufferCopy::default().size(BUSY_BUF);
            let barrier = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE);
            for _ in 0..iters {
                device.cmd_copy_buffer(self.cbuf, self.buf_a, self.buf_b, &[region]);
                device.cmd_pipeline_barrier(
                    self.cbuf,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[barrier],
                    &[],
                    &[],
                );
                device.cmd_copy_buffer(self.cbuf, self.buf_b, self.buf_a, &[region]);
                device.cmd_pipeline_barrier(
                    self.cbuf,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[barrier],
                    &[],
                    &[],
                );
            }
            device
                .end_command_buffer(self.cbuf)
                .context("end busy cbuf")?;
        }
        Ok(())
    }

    /// Submit the recorded chain, optionally signalling `signal` and/or `fence`.
    fn submit(&self, signal: &[vk::Semaphore], fence: vk::Fence) -> Result<()> {
        let cbufs = [self.cbuf];
        let submit = vk::SubmitInfo::default()
            .command_buffers(&cbufs)
            .signal_semaphores(signal);
        unsafe {
            self.gpu
                .device
                .queue_submit(self.gpu.queue, &[submit], fence)
                .context("busy-work submit")
        }
    }

    /// Size the copy chain so one submit takes ≈ `TARGET_D`, then return the measured duration.
    fn calibrate(&self) -> Result<Duration> {
        let device = &self.gpu.device;
        let mut iters = 64u32;
        let mut d;
        loop {
            self.record(iters)?;
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None)? };
            let t = Instant::now();
            self.submit(&[], fence)?;
            unsafe {
                device.wait_for_fences(&[fence], true, u64::MAX)?;
                device.destroy_fence(fence, None);
            }
            d = t.elapsed();
            if d >= TARGET_D || iters >= MAX_ITERS {
                break;
            }
            iters = (iters * 4).min(MAX_ITERS);
        }
        Ok(d)
    }
}

impl Drop for BusyWork<'_> {
    fn drop(&mut self) {
        let device = &self.gpu.device;
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_buffer(self.buf_a, None);
            device.free_memory(self.mem_a, None);
            device.destroy_buffer(self.buf_b, None);
            device.free_memory(self.mem_b, None);
            device.destroy_command_pool(self.pool, None);
        }
    }
}

fn make_buffer(gpu: &Gpu, size: vk::DeviceSize) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let device = &gpu.device;
    let ci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf = unsafe { device.create_buffer(&ci, None) }.context("busy buffer")?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mem = gpu.allocate(req, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    unsafe { device.bind_buffer_memory(buf, mem, 0) }.context("bind busy buffer")?;
    Ok((buf, mem))
}

// ---- stages ----------------------------------------------------------------------------------

/// Core answer: export a submit's completion semaphore as a `sync_file`, then `poll(2)` it.
fn stage_semaphore(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_sem: &ash::khr::external_semaphore_fd::Device,
    d: Duration,
) -> Result<StageResult> {
    let device = &gpu.device;
    let mut export = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let sem = unsafe {
        device.create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut export),
            None,
        )?
    };
    let done = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None)? };

    busy.submit(&[sem], done)?;
    let t = Instant::now();
    let raw = unsafe {
        ext_sem.get_semaphore_fd(
            &vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(sem)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
        )?
    };
    let export_latency = t.elapsed();
    let (fd_valid, status, driver, wait) = probe_sync_file(raw, d);

    unsafe {
        let _ = device.wait_for_fences(&[done], true, u64::MAX);
        device.destroy_fence(done, None);
        device.destroy_semaphore(sem, None);
    }
    let verdict = StageResult::classify(d, export_latency, fd_valid, status, wait);
    Ok(StageResult {
        name: "semaphore→sync_file".into(),
        d,
        export_latency,
        fd_valid,
        status_at_export: status,
        driver_name: driver,
        wait,
        verdict,
        note: None,
    })
}

/// Path B: the same via a VkFence — the exact object KMS `OUT_FENCE` / CPU-side waits use.
fn stage_fence(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_fence: &ash::khr::external_fence_fd::Device,
    d: Duration,
) -> Result<StageResult> {
    let device = &gpu.device;
    let mut export = vk::ExportFenceCreateInfo::default()
        .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
    let fence = unsafe {
        device.create_fence(&vk::FenceCreateInfo::default().push_next(&mut export), None)?
    };

    busy.submit(&[], fence)?;
    let t = Instant::now();
    let raw = unsafe {
        ext_fence.get_fence_fd(
            &vk::FenceGetFdInfoKHR::default()
                .fence(fence)
                .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD),
        )?
    };
    let export_latency = t.elapsed();
    let (fd_valid, status, driver, wait) = probe_sync_file(raw, d);

    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_fence(fence, None);
    }
    let verdict = StageResult::classify(d, export_latency, fd_valid, status, wait);
    Ok(StageResult {
        name: "fence→sync_file".into(),
        d,
        export_latency,
        fd_valid,
        status_at_export: status,
        driver_name: driver,
        wait,
        verdict,
        note: None,
    })
}

/// Bridge stage 1: import the pending `sync_file` into a binary syncobj, then `SYNCOBJ_WAIT`.
/// Best-effort — a failure here is recorded and does not abort the run (Stage 0 already answered
/// the core question). Skipped cleanly when export yields no FD (the emulated path).
fn stage_syncobj_wait(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_fence: &ash::khr::external_fence_fd::Device,
    drm: BorrowedFd,
    d: Duration,
) -> Result<StageResult> {
    let name = "syncobj→wait".to_string();
    let (fence, sync_file, export_latency) = match export_pending(gpu, busy, ext_fence)? {
        Some(v) => v,
        None => {
            return Ok(skipped_stage(
                name,
                d,
                "no pending FD export (emulated path)",
            ))
        }
    };

    let result = (|| -> Result<Duration> {
        let obj = syncobj::create(drm, false)?.handle;
        let r = import_sync_file(drm, obj, sync_file.as_fd());
        if r.is_err() {
            let _ = syncobj::destroy(drm, obj);
        }
        r?;
        let t = Instant::now();
        let wait = syncobj::wait(
            drm,
            &[obj],
            deadline_in(d * 4 + Duration::from_secs(2)),
            false,
            false,
        );
        let elapsed = t.elapsed();
        let _ = syncobj::destroy(drm, obj);
        wait?;
        Ok(elapsed)
    })();

    finish_pending(gpu, fence);
    stage_from_wait(name, d, export_latency, result)
}

/// Bridge stage 2: import → `SYNCOBJ_TRANSFER` binary point into a timeline point →
/// `TIMELINE_WAIT`. This is the exact shape the client-release path needs. Best-effort, same as
/// stage 1.
fn stage_timeline_transfer(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_fence: &ash::khr::external_fence_fd::Device,
    drm: BorrowedFd,
    d: Duration,
) -> Result<StageResult> {
    let name = "syncobj→transfer→timeline_wait".to_string();
    let (fence, sync_file, export_latency) = match export_pending(gpu, busy, ext_fence)? {
        Some(v) => v,
        None => {
            return Ok(skipped_stage(
                name,
                d,
                "no pending FD export (emulated path)",
            ))
        }
    };

    let result = (|| -> Result<Duration> {
        let binary = syncobj::create(drm, false)?.handle;
        let timeline = syncobj::create(drm, false)?.handle;
        let cleanup = |drm: BorrowedFd| {
            let _ = syncobj::destroy(drm, binary);
            let _ = syncobj::destroy(drm, timeline);
        };
        if let Err(e) = import_sync_file(drm, binary, sync_file.as_fd()) {
            cleanup(drm);
            return Err(e.into());
        }
        if let Err(e) = syncobj::transfer(drm, binary, timeline, 0, 1) {
            cleanup(drm);
            return Err(e.into());
        }
        let t = Instant::now();
        let wait = syncobj::timeline_wait(
            drm,
            &[timeline],
            &[1],
            deadline_in(d * 4 + Duration::from_secs(2)),
            false,
            false,
            false,
        );
        let elapsed = t.elapsed();
        cleanup(drm);
        wait?;
        Ok(elapsed)
    })();

    finish_pending(gpu, fence);
    stage_from_wait(name, d, export_latency, result)
}

// ---- controls & eventfd ----------------------------------------------------------------------

/// (negative: unsignalled point times out, positive: CPU-signalled point wakes immediately).
fn controls(drm: BorrowedFd) -> Result<(bool, bool)> {
    // Negative: a timeline point that is never submitted must block and time out (ETIME), proving
    // the wait actually waits.
    let neg = syncobj::create(drm, false)?.handle;
    let neg_res = syncobj::timeline_wait(
        drm,
        &[neg],
        &[1],
        deadline_in(Duration::from_millis(150)),
        false,
        true,
        false,
    );
    let neg_timed_out = matches!(&neg_res, Err(e) if e.raw_os_error() == Some(libc::ETIME));
    let _ = syncobj::destroy(drm, neg);

    // Positive: a CPU SIGNAL must wake a wait immediately, distinguishing a genuine block from
    // broken wait machinery.
    let pos = syncobj::create(drm, false)?.handle;
    syncobj::signal(drm, &[pos])?;
    let pos_res = syncobj::wait(
        drm,
        &[pos],
        deadline_in(Duration::from_millis(150)),
        false,
        false,
    );
    let _ = syncobj::destroy(drm, pos);
    Ok((neg_timed_out, pos_res.is_ok()))
}

/// Probe `DRM_IOCTL_SYNCOBJ_EVENTFD` (Smithay gates its explicit-sync support on this) and confirm
/// the eventfd fires on GPU completion. Best-effort.
fn eventfd_probe(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_fence: &ash::khr::external_fence_fd::Device,
    drm: BorrowedFd,
    d: Duration,
) -> Result<(bool, bool)> {
    let (fence, sync_file, _lat) = match export_pending(gpu, busy, ext_fence)? {
        Some(v) => v,
        None => return Ok((false, false)),
    };

    let (supported, fired) = (|| -> (bool, bool) {
        let obj = match syncobj::create(drm, false) {
            Ok(o) => o.handle,
            Err(_) => return (false, false),
        };
        if import_sync_file(drm, obj, sync_file.as_fd()).is_err() {
            let _ = syncobj::destroy(drm, obj);
            return (false, false);
        }
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if efd < 0 {
            let _ = syncobj::destroy(drm, obj);
            return (false, false);
        }
        let efd = unsafe { OwnedFd::from_raw_fd(efd) };
        let supported = syncobj::eventfd(drm, obj, 0, efd.as_fd(), false).is_ok();
        let fired = supported && poll_fd(efd.as_fd(), poll_ms(d)).unwrap_or(false);
        let _ = syncobj::destroy(drm, obj);
        (supported, fired)
    })();

    finish_pending(gpu, fence);
    Ok((supported, fired))
}

// ---- shared plumbing -------------------------------------------------------------------------

/// Submit busy-work, export a *pending* completion `sync_file` via a **VkFence** (no wait before
/// export, so the fence is still in flight when the caller imports it). The fence path is used
/// rather than the semaphore path because Venus pipelines fence `SYNC_FD` export but *emulates*
/// semaphore export (blocks then hands back an already-signalled stub) — see the two core stages.
/// Returns `None` on an emulated export (no usable pending FD). Retire the fence via
/// [`finish_pending`].
fn export_pending(
    gpu: &Gpu,
    busy: &BusyWork,
    ext_fence: &ash::khr::external_fence_fd::Device,
) -> Result<Option<(vk::Fence, OwnedFd, Duration)>> {
    let device = &gpu.device;
    let mut export = vk::ExportFenceCreateInfo::default()
        .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
    let fence = unsafe {
        device.create_fence(&vk::FenceCreateInfo::default().push_next(&mut export), None)?
    };
    busy.submit(&[], fence)?;
    let t = Instant::now();
    let raw = unsafe {
        ext_fence.get_fence_fd(
            &vk::FenceGetFdInfoKHR::default()
                .fence(fence)
                .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD),
        )?
    };
    let export_latency = t.elapsed();
    if raw < 0 {
        finish_pending(gpu, fence);
        return Ok(None);
    }
    // Guard against the other emulation shape: a valid FD that is already signalled at export
    // (driver waited on the CPU). Such an FD can't exercise the pending-fence bridge, so treat it
    // as "no usable pending export" and let the stage record the emulated path.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    if sync_file_status(owned.as_fd()).ok() != Some(0) {
        finish_pending(gpu, fence);
        return Ok(None);
    }
    Ok(Some((fence, owned, export_latency)))
}

fn finish_pending(gpu: &Gpu, fence: vk::Fence) {
    let device = &gpu.device;
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_fence(fence, None);
    }
}

/// Inspect a freshly-exported `sync_file` FD: validity, status-at-export, driver name, and how long
/// a `poll(2)` blocks before it signals. `raw < 0` is the emulated (already-signalled, -1) path.
fn probe_sync_file(raw: i32, d: Duration) -> (bool, Option<i32>, Option<String>, Option<Duration>) {
    if raw < 0 {
        return (false, None, None, None);
    }
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let status = sync_file_status(owned.as_fd()).ok();
    let driver = sync_file_driver(owned.as_fd());
    let t = Instant::now();
    let ready = poll_fd(owned.as_fd(), poll_ms(d)).unwrap_or(false);
    let wait = ready.then(|| t.elapsed());
    (true, status, driver, wait)
}

fn skipped_stage(name: String, d: Duration, note: &str) -> StageResult {
    StageResult {
        name,
        d,
        export_latency: Duration::ZERO,
        fd_valid: false,
        status_at_export: None,
        driver_name: None,
        wait: None,
        verdict: Verdict::Emulated,
        note: Some(note.into()),
    }
}

fn stage_from_wait(
    name: String,
    d: Duration,
    export_latency: Duration,
    wait: Result<Duration>,
) -> Result<StageResult> {
    Ok(match wait {
        Ok(elapsed) => {
            let verdict = StageResult::classify(d, export_latency, true, Some(0), Some(elapsed));
            StageResult {
                name,
                d,
                export_latency,
                fd_valid: true,
                status_at_export: Some(0),
                driver_name: None,
                wait: Some(elapsed),
                verdict,
                note: None,
            }
        }
        Err(e) => StageResult {
            name,
            d,
            export_latency,
            fd_valid: true,
            status_at_export: Some(0),
            driver_name: None,
            wait: None,
            verdict: Verdict::Inconclusive,
            note: Some(format!("{e:#}")),
        },
    })
}

// ---- low-level fd / ioctl helpers ------------------------------------------------------------

fn poll_ms(d: Duration) -> i32 {
    ((d.as_millis() as i64 * 4).clamp(2000, 30_000)) as i32
}

/// `poll(2)` for POLLIN with a millisecond timeout; true iff readable within the window.
fn poll_fd(fd: BorrowedFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r > 0 && pfd.revents & libc::POLLIN != 0)
}

/// `_IOWR(magic, nr, size)` — Linux ioctl request encoding (dir=3 in the top two bits).
fn iowr(magic: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((3u32 << 30) | ((size as u32) << 16) | (magic << 8) | nr) as libc::c_ulong
}

#[repr(C)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SyncFenceInfo {
    obj_name: [u8; 32],
    driver_name: [u8; 32],
    status: i32,
    flags: u32,
    timestamp_ns: u64,
}

fn ioctl_file_info(fd: BorrowedFd, info: &mut SyncFileInfo) -> io::Result<()> {
    let req = iowr(
        SYNC_IOC_MAGIC,
        SYNC_FILE_INFO_NR,
        std::mem::size_of::<SyncFileInfo>(),
    );
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req, info as *mut SyncFileInfo) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `sync_file` status: 0 = active/unsignalled, 1 = signalled, <0 = error. A single call with
/// `num_fences == 0` fills status without needing the per-fence buffer.
fn sync_file_status(fd: BorrowedFd) -> io::Result<i32> {
    let mut info: SyncFileInfo = unsafe { std::mem::zeroed() };
    ioctl_file_info(fd, &mut info)?;
    Ok(info.status)
}

/// Per-fence `driver_name` (two-call: first learns the fence count, second fills the buffer).
fn sync_file_driver(fd: BorrowedFd) -> Option<String> {
    let mut info: SyncFileInfo = unsafe { std::mem::zeroed() };
    ioctl_file_info(fd, &mut info).ok()?;
    let n = info.num_fences;
    if n == 0 {
        return None;
    }
    let mut fences: Vec<SyncFenceInfo> = vec![unsafe { std::mem::zeroed() }; n as usize];
    info.sync_fence_info = fences.as_mut_ptr() as u64;
    info.num_fences = n;
    ioctl_file_info(fd, &mut info).ok()?;
    Some(cstr_field(&fences[0].driver_name))
}

fn cstr_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Import a `sync_file` into an existing syncobj. drm-ffi's public `fd_to_handle` hardcodes
/// `handle = 0`, but `IMPORT_SYNC_FILE` installs the fence into the syncobj named by `handle`, so
/// the ioctl is issued directly (drm-ffi's `ioctl` module is private). The request size is taken
/// from the type so the encoding is robust to the struct's exact fields.
fn import_sync_file(drm: BorrowedFd, syncobj_handle: u32, sync_file: BorrowedFd) -> io::Result<()> {
    let mut args: drm_ffi::drm_syncobj_handle = unsafe { std::mem::zeroed() };
    args.handle = syncobj_handle;
    args.fd = sync_file.as_raw_fd();
    args.flags = drm_ffi::DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE;
    let req = iowr(
        DRM_IOCTL_MAGIC,
        SYNCOBJ_FD_TO_HANDLE_NR,
        std::mem::size_of::<drm_ffi::drm_syncobj_handle>(),
    );
    let r = unsafe {
        libc::ioctl(
            drm.as_raw_fd(),
            req,
            &mut args as *mut drm_ffi::drm_syncobj_handle,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Absolute CLOCK_MONOTONIC deadline `after` from now, in ns — the form `DRM_IOCTL_SYNCOBJ_*WAIT`
/// expects (it treats `timeout_nsec` as an absolute monotonic timestamp, not a relative timeout).
fn deadline_in(after: Duration) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    // On aarch64 Linux tv_sec (time_t) and tv_nsec (c_long) are both i64.
    let now = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
    now.saturating_add(after.as_nanos() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_sync_bridge() -> Result<()> {
        let gpu = Gpu::new()?;
        let report = run_sync_spike(&gpu)?;
        report.print();

        // Universal invariants: the wait machinery must both block and wake.
        assert!(
            report.neg_control_timed_out,
            "an unsignalled syncobj point must time out (wait must actually wait)"
        );
        assert!(
            report.pos_control_signaled,
            "a CPU-signalled syncobj must wake the wait (distinguishes a hang from broken waits)"
        );

        let fence = report
            .stage("fence→sync_file")
            .expect("fence stage recorded");
        if report.device_is_cpu {
            // lavapipe is the negative control: fence export must NOT masquerade as pipelined,
            // which is what proves the classifier tells real host-GPU propagation from emulation.
            assert_ne!(
                fence.verdict,
                Verdict::Pipelined,
                "CPU baseline classified Pipelined — the test has no teeth"
            );
        } else {
            // The headline Stage-3 result: on a real GPU, host completion must propagate through
            // the fence-exported sync_file and on into a kernel syncobj (import/wait +
            // transfer/timeline), or the KMS/Wayland explicit-sync design is
            // unbuildable on this VM.
            assert!(
                report.bridge_pipelined(),
                "host-GPU completion did NOT pipeline through the fence→sync_file→syncobj bridge"
            );
        }
        Ok(())
    }
}
