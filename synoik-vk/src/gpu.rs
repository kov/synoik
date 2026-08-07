// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Minimal Vulkan (ash) context — the seed of the owned renderer.
//!
//! Stage 0 goal: prove `vkCreateDevice` → `vkQueueSubmit` → readback works on Venus
//! (virtio-gpu, no zink) and on lavapipe (CPU, deterministic test baseline). Nothing here
//! touches a Wayland socket / DRM master / swapchain, so it runs headless from any shell.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use ash::vk;

/// The Khronos validation layer. Opt-in via `SYNOIK_VK_VALIDATION`; see [`Gpu::with_selector`].
const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

/// Validation **errors** reported in this process, across every [`Gpu`].
///
/// Process-wide rather than per-`Gpu` because the messenger is per-instance while a test run builds
/// well over a hundred of them, and an error is a defect wherever it surfaced.
static VALIDATION_ERRORS: AtomicUsize = AtomicUsize::new(0);

/// How many validation errors the layer has reported so far. Always 0 unless `SYNOIK_VK_VALIDATION`
/// is set, since without it no layer is loaded and nothing reports.
pub fn validation_errors() -> usize {
    VALIDATION_ERRORS.load(Ordering::Relaxed)
}

/// Registers [`fail_on_validation_errors`] once per process.
static REGISTER_EXIT_CHECK: std::sync::Once = std::sync::Once::new();

/// Fail the process at exit if the layer reported any error.
///
/// The layer only *reports*: the callback returns `VK_FALSE` and cannot panic (it runs inside the
/// loader's FFI frame), so nothing here fails a test on its own. Without this hook a validated run
/// prints its violations under a **green** result — which is exactly what happened on the first
/// one: 442 errors, `387 passed; 0 failed`.
///
/// It is an `atexit` handler rather than a `#[test]` because the check is only meaningful once
/// every test has run, and libtest neither orders tests nor offers a teardown hook. A test
/// asserting the count is 0 could instead run *before* the offending test (missing it) or fail
/// while naming an innocent one — the count is process-wide and the suite is parallel. At exit it
/// is final, so this neither misses nor misattributes.
///
/// Aborting is blunt, but an `atexit` handler cannot set the exit code, and a blunt red beats a
/// quiet green. Re-run with `--test-threads=1` to attribute the errors to a test.
extern "C" fn fail_on_validation_errors() {
    let errors = VALIDATION_ERRORS.load(Ordering::Relaxed);
    if errors > 0 {
        eprintln!(
            "\nVULKAN VALIDATION FAILED: the layer reported {errors} error(s) — see the \
             `VULKAN ERROR` lines above. Re-run with `--test-threads=1` to see which test caused \
             them (parallel output interleaves, so nearby test names mean nothing)."
        );
        std::process::abort();
    }
}

/// The validation layer's message sink.
///
/// Called on the offending Vulkan call's own thread, from inside the loader's FFI frame: unwinding
/// out of here would cross an `extern "system"` boundary, so every panic is swallowed. Returning
/// `vk::FALSE` means "do not abort the call" — we record and keep going, so one error does not mask
/// the rest of the run.
unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    let _ = std::panic::catch_unwind(|| {
        if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
            VALIDATION_ERRORS.fetch_add(1, Ordering::Relaxed);
        }

        let message = unsafe { (*data).p_message };
        let message = if message.is_null() {
            Cow::Borrowed("<no message>")
        } else {
            unsafe { CStr::from_ptr(message) }.to_string_lossy()
        };
        eprintln!("VULKAN {severity:?} {types:?}: {message}");
    });

    vk::FALSE
}

/// Whether a `(format, modifier)` pair is one the driver vouches for. See
/// [`Gpu::check_modifier_features`] — both variants mean "go ahead", but [`Self::Unlisted`] means
/// we are proceeding on an assumption we could not check, and the caller should say so in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierSupport {
    /// Enumerated, and it has every feature the caller asked for.
    Ok,
    /// The driver enumerates no modifiers for this format at all, so there was nothing to check
    /// against. Proceeding is what we did before this check existed — see the warning on
    /// [`Gpu::check_modifier_features`]: it is best-effort, and nothing downstream will catch it
    /// if the modifier really is unsupported.
    Unlisted,
}

pub struct Gpu {
    // Field order matters for Drop: device before instance before entry.
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    // Used by the device probes (format-support / DRM-modifier / external-semaphore queries).
    pub phys: vk::PhysicalDevice,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    /// Nanoseconds per timestamp-query tick (`VkPhysicalDeviceLimits::timestampPeriod`),
    /// and how many bits of a timestamp the graphics queue actually fills
    /// (`timestampValidBits`, 0 meaning the queue cannot timestamp at all). Both
    /// are needed to turn a query result into a duration; see
    /// [`Self::timestamps_supported`].
    pub timestamp_period_ns: f32,
    pub timestamp_valid_bits: u32,
    /// Alignment for a `VkBufferImageCopy`'s `bufferOffset`, for staging that packs several
    /// uploads into one buffer ([`crate::staging::StagingPool`]). The spec *requires* a multiple
    /// of the texel block size (4 for every 32-bpp format we upload); `optimalBufferCopyOffset-
    /// Alignment` is what the driver would rather have, so this is the larger of the two.
    pub buffer_copy_offset_alignment: vk::DeviceSize,
    pub device_name: String,
    // CPU vs a real GPU — the dmabuf demo needs a hardware device tied to the DRM node, so it
    // skips on CPU devices (lavapipe).
    pub device_type: vk::PhysicalDeviceType,
    // The DRM render node this device drives, if the driver reports one. `None` on lavapipe.
    drm_render_node: Option<(u32, u32)>,
    // Device extensions we asked for AND the device advertised (so lavapipe, which lacks the
    // dmabuf ones, still builds a device — the dmabuf demo checks these and skips if absent).
    enabled_extensions: Vec<String>,
    // Memoized DRM-modifier tiling features per format. See `modifier_features`.
    modifier_features: Mutex<HashMap<vk::Format, Vec<(u64, vk::FormatFeatureFlags)>>>,
    /// Chains every submit on the queue after the previous one. See [`Self::submit`].
    /// `None` when the device does not support timeline semaphores, in which case
    /// submits are unordered relative to each other and the caller must keep
    /// waiting on each one's fence.
    order: Option<SubmitOrder>,
    /// `VK_KHR_external_fence_fd`, when the device has it: turns a submit's pending
    /// completion into a `sync_file` FD that KMS can take as `IN_FENCE_FD`. Measured
    /// pipelined on Venus — see `sync_spike`.
    external_fence: Option<ash::khr::external_fence_fd::Device>,
    // The validation layer's messenger, when `SYNOIK_VK_VALIDATION` is set. Destroyed before the
    // instance in `Drop`.
    debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    pub instance: ash::Instance,
    // Owns the loaded Vulkan library; must outlive `instance`/`device`.
    #[allow(dead_code)]
    pub entry: ash::Entry,
}

/// A timeline semaphore whose value counts submits on the queue.
///
/// Its whole job is to make GPU execution order equal submission order without
/// blocking the CPU. Today every submit is followed by its own fence wait, so
/// nothing can overlap and this changes nothing observable — but that wait is what
/// we mean to stop paying (`docs/fork/renderer-synchronous-submits.md`), and the
/// moment a submit is left in flight, everything issued after it may execute
/// alongside it. That is not a lifetime problem, which is the easy half: the
/// present-blit shadow is one image shared by consecutive frames and the glyph
/// atlas is uploaded while frames sample it, so overlap is a data race on live
/// pixels. Chaining wait(N) → signal(N+1) keeps the order we already rely on and
/// costs nothing on a stack where the GPU is not the bottleneck.
struct SubmitOrder {
    semaphore: vk::Semaphore,
    /// The value the *next* submit waits on; it signals one past it. Starts at 0,
    /// which the semaphore is created already holding, so the first submit is not
    /// blocked.
    next: AtomicU64,
}

/// Which physical device to bring up.
#[derive(Debug, Clone, Copy, Default)]
pub enum DeviceSelector {
    /// The best device by type rank (discrete > integrated > CPU). For headless and CPU
    /// (lavapipe) use, where no DRM node is in play.
    #[default]
    Best,

    /// The device backing this DRM **render** node.
    ///
    /// A compositor must select this way. It tells clients, through dmabuf feedback, which device
    /// to allocate their buffers for; if we then render on a *different* physical device, the
    /// buffers clients hand us are the ones we're least able to import. On a single-GPU machine
    /// the two coincide by luck, which is exactly why picking by rank looks like it works.
    DrmRenderNode { major: u32, minor: u32 },
}

impl Gpu {
    /// Whether the graphics queue can answer timestamp queries, i.e. whether GPU
    /// pass durations are measurable at all. A tick period of zero means the
    /// device declines to timestamp; zero valid bits means this queue does.
    pub fn timestamps_supported(&self) -> bool {
        self.timestamp_valid_bits > 0 && self.timestamp_period_ns > 0.
    }

    /// Turn a raw tick delta from a timestamp query into a duration.
    pub fn timestamp_delta(&self, ticks: u64) -> std::time::Duration {
        std::time::Duration::from_nanos((ticks as f64 * f64::from(self.timestamp_period_ns)) as u64)
    }

    /// Bring up an instance + a single graphics-capable logical device, picking the best by type
    /// rank. See [`Gpu::with_selector`] — a compositor wants [`DeviceSelector::DrmRenderNode`].
    pub fn new() -> Result<Self> {
        Self::with_selector(DeviceSelector::Best)
    }

    /// Bring up an instance + a single graphics-capable logical device.
    ///
    /// Device selection is deterministic: with `VK_DRIVER_FILES` pinned to one ICD only that
    /// driver's devices enumerate, so the choice is forced. Everything is logged so the run is
    /// self-documenting.
    pub fn with_selector(selector: DeviceSelector) -> Result<Self> {
        let entry =
            unsafe { ash::Entry::load() }.context("loading the Vulkan loader (libvulkan)")?;

        let app = vk::ApplicationInfo::default()
            .application_name(c"synoik-vk")
            .api_version(vk::make_api_version(0, 1, 3, 0));

        // Validation is opt-in: the layer costs real time on every call and is a dev/CI dependency
        // that need not exist on a user's machine, so a session never loads it. Asking for it and
        // not getting it is an error, not a warning — a run that silently validates nothing while
        // you believe it does is worse than no run at all.
        let validate = std::env::var_os("SYNOIK_VK_VALIDATION").is_some();
        let mut layers: Vec<*const c_char> = Vec::new();
        let mut instance_extensions: Vec<*const c_char> = Vec::new();
        if validate {
            let available = unsafe { entry.enumerate_instance_layer_properties() }
                .context("enumerating instance layers")?;
            if !available
                .iter()
                .any(|l| l.layer_name_as_c_str() == Ok(VALIDATION_LAYER))
            {
                return Err(anyhow!(
                    "SYNOIK_VK_VALIDATION is set but the {VALIDATION_LAYER:?} layer is not installed \
                     (dnf install vulkan-validation-layers)"
                ));
            }
            layers.push(VALIDATION_LAYER.as_ptr());
            instance_extensions.push(ash::ext::debug_utils::NAME.as_ptr());
            eprintln!("  validation: {VALIDATION_LAYER:?} enabled");

            // Make the run's exit status depend on what the layer found. See
            // `fail_on_validation_errors`.
            REGISTER_EXIT_CHECK.call_once(|| {
                // SAFETY: `fail_on_validation_errors` is a plain `extern "C" fn()` that only reads
                // an atomic and writes to stderr, and it is registered at most once.
                unsafe { libc::atexit(fail_on_validation_errors) };
            });
        }

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_layer_names(&layers)
            .enabled_extension_names(&instance_extensions);
        let instance =
            unsafe { entry.create_instance(&create_info, None) }.context("vkCreateInstance")?;

        // Must outlive nothing but the instance, and be destroyed before it (see `Drop`).
        let debug = validate
            .then(|| -> Result<_> {
                let loader = ash::ext::debug_utils::Instance::new(&entry, &instance);
                let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                    .message_severity(
                        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                            | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
                    )
                    .message_type(
                        vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                            | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                            | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                    )
                    .pfn_user_callback(Some(debug_callback));
                let handle = unsafe { loader.create_debug_utils_messenger(&info, None) }
                    .context("vkCreateDebugUtilsMessengerEXT")?;
                Ok((loader, handle))
            })
            .transpose()?;

        let devices = unsafe { instance.enumerate_physical_devices() }
            .context("enumerate physical devices")?;
        if devices.is_empty() {
            return Err(anyhow!(
                "no Vulkan physical devices (check VK_DRIVER_FILES / ICDs)"
            ));
        }

        let mut best: Option<(vk::PhysicalDevice, vk::PhysicalDeviceProperties, u32)> = None;
        let mut matched: Option<(vk::PhysicalDevice, vk::PhysicalDeviceProperties, u32)> = None;
        let mut any_drm_props = false;

        for &pd in &devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = device_name(&props);
            let kind = device_type_str(props.device_type);
            let gfx = graphics_queue_family(&instance, pd);
            let drm = drm_render_node(&instance, pd);
            any_drm_props |= drm.is_some();
            eprintln!(
                "  device: {name:?} [{kind}] api {}  graphics-queue: {gfx:?}  render-node: {drm:?}",
                api_version_str(props.api_version)
            );

            let Some(qf) = gfx else { continue };

            if let DeviceSelector::DrmRenderNode { major, minor } = selector {
                if drm == Some((major, minor)) {
                    matched = Some((pd, props, qf));
                }
            }

            let better = match &best {
                None => true,
                Some((_, cur, _)) => rank(props.device_type) > rank(cur.device_type),
            };
            if better {
                best = Some((pd, props, qf));
            }
        }

        let chosen = match selector {
            DeviceSelector::Best => best,
            DeviceSelector::DrmRenderNode { major, minor } => match matched {
                Some(matched) => Some(matched),
                // No device claims this node. If *nothing* reported DRM properties, the driver
                // simply doesn't implement VK_EXT_physical_device_drm (lavapipe, say) and there is
                // no way to correlate — fall back to rank, loudly. But if some device did report
                // and none of them matched, we are about to render on a GPU we did not tell clients
                // about, which is the bug this selector exists to prevent. Fail instead.
                None if any_drm_props => {
                    return Err(anyhow!(
                        "no Vulkan device backs DRM render node {major}:{minor}; refusing to \
                         render on a device clients are not allocating for"
                    ));
                }
                None => {
                    eprintln!(
                        "  warning: no device reports VK_EXT_physical_device_drm; cannot confirm \
                         the render node {major}:{minor}, falling back to the best by type"
                    );
                    best
                }
            },
        };

        let (phys, props, queue_family) =
            chosen.ok_or_else(|| anyhow!("no physical device with a graphics queue"))?;
        let device_name = device_name(&props);

        // Enable the external-memory extensions needed to import dmabufs with DRM modifiers, but
        // only those the device actually advertises. (external_memory / bind_memory2 /
        // sampler_ycbcr_conversion / image_format_list are core in 1.1–1.2, so not listed here.)
        let want: [&CStr; 6] = [
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_EXT_image_drm_format_modifier",
            // For acquiring imported content from the FOREIGN (non-Vulkan producer) queue family;
            // if absent we fall back to a plain layout transition (see dmabuf.rs).
            c"VK_EXT_queue_family_foreign",
            // Explicit sync (Stage 3): export a submit's completion as a binary SYNC_FD, the only
            // usable Vulkan external-sync bridge on Venus/lavapipe (see
            // docs/fork/venus-explicit-sync-gap.md). Both are enable-only — no feature struct —
            // and their base extensions (external_semaphore/fence) are core in 1.1. See
            // sync_spike.
            c"VK_KHR_external_semaphore_fd",
            c"VK_KHR_external_fence_fd",
        ];
        let avail = unsafe { instance.enumerate_device_extension_properties(phys) }
            .context("enumerate device extensions")?;
        let has = |name: &CStr| {
            avail
                .iter()
                .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == name)
        };
        let enabled_cstr: Vec<&CStr> = want.into_iter().filter(|n| has(n)).collect();
        let enabled_ptrs: Vec<*const std::ffi::c_char> =
            enabled_cstr.iter().map(|s| s.as_ptr()).collect();
        let enabled_extensions: Vec<String> = enabled_cstr
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        if !enabled_extensions.is_empty() {
            eprintln!("  enabling device extensions: {enabled_extensions:?}");
        }

        // Timeline semaphores (core in 1.2, still opt-in as a feature) are what lets a submit be
        // left in flight without letting the next one execute alongside it. See [`SubmitOrder`].
        let mut supported12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut supported = vk::PhysicalDeviceFeatures2::default().push_next(&mut supported12);
        unsafe { instance.get_physical_device_features2(phys, &mut supported) };
        let has_timeline = supported12.timeline_semaphore == vk::TRUE;
        if !has_timeline {
            eprintln!(
                "  warning: no timelineSemaphore; submits cannot be ordered without blocking, \
                 so each one keeps its fence wait"
            );
        }

        let priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let mut enable12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
        let mut device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&enabled_ptrs);
        if has_timeline {
            device_ci = device_ci.push_next(&mut enable12);
        }
        let device =
            unsafe { instance.create_device(phys, &device_ci, None) }.context("vkCreateDevice")?;

        let external_fence = enabled_cstr
            .contains(&c"VK_KHR_external_fence_fd")
            .then(|| ash::khr::external_fence_fd::Device::new(&instance, &device));

        let order = has_timeline
            .then(|| {
                let mut type_ci = vk::SemaphoreTypeCreateInfo::default()
                    .semaphore_type(vk::SemaphoreType::TIMELINE)
                    .initial_value(0);
                let ci = vk::SemaphoreCreateInfo::default().push_next(&mut type_ci);
                unsafe { device.create_semaphore(&ci, None) }
                    .context("create timeline semaphore")
                    .map(|semaphore| SubmitOrder {
                        semaphore,
                        next: AtomicU64::new(0),
                    })
            })
            .transpose()?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };

        let timestamp_valid_bits =
            unsafe { instance.get_physical_device_queue_family_properties(phys) }
                .get(queue_family as usize)
                .map_or(0, |f| f.timestamp_valid_bits);

        Ok(Gpu {
            device,
            queue,
            queue_family,
            phys,
            mem_props,
            timestamp_period_ns: props.limits.timestamp_period,
            timestamp_valid_bits,
            buffer_copy_offset_alignment: props.limits.optimal_buffer_copy_offset_alignment.max(4),
            device_name,
            device_type: props.device_type,
            drm_render_node: drm_render_node(&instance, phys),
            enabled_extensions,
            modifier_features: Mutex::new(HashMap::new()),
            order,
            external_fence,
            debug,
            instance,
            entry,
        })
    }

    /// The DRM-modifier tiling features this device reports for `(format, modifier)`, or `None` if
    /// the driver does not enumerate that modifier for that format.
    ///
    /// For an image created with `DRM_FORMAT_MODIFIER_EXT` tiling, *these* — not the linear/optimal
    /// tiling features — are the "format features" every command's VUs are written against. Nothing
    /// in them is mandated by the spec, so an operation that is guaranteed at `OPTIMAL` tiling
    /// (`vkCmdBlitImage` on `R8G8B8A8_UNORM`, say) is pure driver goodwill on a modifier. Use
    /// [`Self::check_modifier_features`] rather than reading this directly.
    ///
    /// Memoized: the list is static per `(physical device, format)`, and importing a dmabuf is not
    /// a per-frame operation but is not rare either.
    pub fn modifier_features(
        &self,
        format: vk::Format,
        modifier: u64,
    ) -> Option<vk::FormatFeatureFlags> {
        self.with_modifier_list(format, |list| {
            list.iter().find(|(m, _)| *m == modifier).map(|(_, f)| *f)
        })
    }

    /// Does this device enumerate *any* DRM modifier for `format`? An empty list means the driver
    /// told us nothing — no `VK_EXT_image_drm_format_modifier`, or it lists none — which is very
    /// different from a list that exists and omits a particular modifier.
    fn enumerates_modifiers(&self, format: vk::Format) -> bool {
        self.with_modifier_list(format, |list| !list.is_empty())
    }

    fn with_modifier_list<R>(
        &self,
        format: vk::Format,
        f: impl FnOnce(&[(u64, vk::FormatFeatureFlags)]) -> R,
    ) -> R {
        let mut cache = self.modifier_features.lock().unwrap();
        let list = cache
            .entry(format)
            .or_insert_with(|| drm_modifier_features(&self.instance, self.phys, format));
        f(list)
    }

    /// Fail closed unless this device can actually perform `required` on an image of
    /// `(format, modifier)`.
    ///
    /// `required` must be derived from the commands we will *record* against the image, never from
    /// its `usage`: the two are independent axes. `TRANSFER_DST` usage is necessary but not
    /// sufficient for `vkCmdBlitImage` (VUID-vkCmdBlitImage-dstImage-02000 additionally demands the
    /// `BLIT_DST` *feature*), and a driver may legally offer the `TRANSFER_DST` feature — copies —
    /// without `BLIT_DST`, or the reverse.
    ///
    /// The three cases, and why:
    ///
    /// - **Enumerated, a required bit missing** → `Err`. The driver has affirmatively told us the
    ///   operation is undefined. Failing means no picture, but nothing works in that world either;
    ///   the difference is that an error naming the format, the modifier and the missing bit is one
    ///   a bug report can act on, where garbage pixels or a device loss ten frames later is not.
    /// - **Enumerated with every bit** → `Ok`.
    /// - **A non-empty list that omits this modifier** → `Err`. The list is the driver's own
    ///   statement of what it supports for the format, so an absence in a list that exists is
    ///   evidence against the modifier, not evidence of nothing. (A dmabuf's modifier comes from
    ///   GBM, which need not be the same driver stack as the ICD — on split render/scanout hardware
    ///   this is how we would find out, and that import would not have worked anyway.)
    /// - **No list at all** — no `VK_EXT_image_drm_format_modifier`, or the driver enumerates none
    ///   → [`ModifierSupport::Unlisted`], for the caller to warn about and proceed. Here we know
    ///   nothing, so refusing would be inventing a failure.
    ///
    /// **`Unlisted` is best-effort, not a safety net.** Importing on an unenumerated modifier
    /// violates VUID-VkImageDrmFormatModifierExplicitCreateInfoEXT-drmFormatModifier-02264 — it is
    /// undefined behavior a driver is under no obligation to diagnose, and *nothing downstream
    /// reliably catches it*. Measured on this VM: for a vendor-`0xff` modifier no device can back,
    /// both Venus and lavapipe report the image as creatable via
    /// `vkGetPhysicalDeviceImageFormatProperties2`, and `vkCreateImage` then creates it happily. So
    /// there is no second query to lean on — this enumeration is the only honest gate, which is
    /// exactly why an absence from a *populated* list is treated as an answer.
    pub fn check_modifier_features(
        &self,
        format: vk::Format,
        modifier: u64,
        required: vk::FormatFeatureFlags,
    ) -> Result<ModifierSupport> {
        let Some(features) = self.modifier_features(format, modifier) else {
            if !self.enumerates_modifiers(format) {
                return Ok(ModifierSupport::Unlisted);
            }
            return Err(anyhow!(
                "this device does not support DRM modifier {modifier:#018x} for {format:?} (it \
                 enumerates others), so an image imported with it would be undefined"
            ));
        };

        let missing = required & !features;
        if !missing.is_empty() {
            return Err(anyhow!(
                "{format:?} with DRM modifier {modifier:#018x} lacks the format features \
                 {missing:?} this image needs (device reports {features:?}) — the operations we \
                 record against it would be undefined"
            ));
        }
        Ok(ModifierSupport::Ok)
    }

    /// Was this device extension enabled at device creation? (Used to gate the dmabuf demo.)
    /// The DRM render node `(major, minor)` this device drives, if the driver reports one
    /// (`VK_EXT_physical_device_drm`). `None` on drivers that don't — lavapipe, notably.
    pub fn drm_render_node(&self) -> Option<(u32, u32)> {
        self.drm_render_node
    }

    pub fn supports(&self, ext: &str) -> bool {
        self.enabled_extensions.iter().any(|e| e == ext)
    }

    /// Pick a memory type satisfying `type_bits` (from a `*MemoryRequirements`) and `flags`.
    pub fn find_memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                (type_bits & (1 << i)) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| anyhow!("no memory type for bits {type_bits:#x} flags {flags:?}"))
    }

    /// The memory type index to import a dmabuf into: the image's own requirements, narrowed by
    /// the driver's answer for this specific fd.
    ///
    /// The narrowing is the whole point — `vkGetMemoryFdPropertiesKHR` is what says which heaps can
    /// actually back *this* handle, and an image's `memory_type_bits` alone does not know the
    /// memory came from outside. A failed query is fatal here, deliberately.
    ///
    /// It did not use to be. Venus answered `memoryTypeBits = 0` for every dmabuf, while
    /// `vkAllocateMemory` imported the same handle happily, so all three import sites treated the
    /// query as best-effort and fell back to the unmasked bits. That was fixed host-side
    /// (`docs/fork/venus-bugs/README.md` issue 1) and the fallback removed — it was only ever safe
    /// because this device exposes exactly one memory type, which makes the mask a no-op. With
    /// several types, falling back would take `trailing_zeros()` of an unfiltered mask and pick a
    /// heap the driver never said could hold this handle.
    pub fn dmabuf_memory_type(
        &self,
        fd: BorrowedFd<'_>,
        req: vk::MemoryRequirements,
    ) -> Result<u32> {
        let ext_fd = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // The query borrows the fd; it does NOT consume it (unlike the import that follows), so
        // pass a plain borrow — duping here would leak one fd per call, and this runs per bind.
        unsafe {
            ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
        }
        .context("vkGetMemoryFdPropertiesKHR")?;

        let type_bits = req.memory_type_bits & fd_props.memory_type_bits;
        anyhow::ensure!(
            type_bits != 0,
            "no importable memory type for the dmabuf: image accepts {:#x}, fd accepts {:#x}",
            req.memory_type_bits,
            fd_props.memory_type_bits,
        );
        Ok(type_bits.trailing_zeros())
    }

    /// Allocate device memory sized/typed for `req` with the given property `flags`.
    ///
    /// `#[track_caller]` so [`crate::devmem`] attributes the block to whoever asked for it rather
    /// than to this line — the site label is the whole value of the accounting.
    #[track_caller]
    pub fn allocate(
        &self,
        req: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory> {
        let index = self.find_memory_type(req.memory_type_bits, flags)?;
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let memory =
            unsafe { self.device.allocate_memory(&info, None) }.context("allocate_memory")?;
        crate::devmem::track(
            memory,
            req.size,
            crate::devmem::Site::Caller(std::panic::Location::caller()),
        );
        Ok(memory)
    }

    /// Submit `cbufs` on the queue, chained after every previous submit, signalling `fence`.
    ///
    /// **Every** submit in this renderer goes through here. The chain is what makes it safe to
    /// leave a submit in flight: without it, the next submit could execute alongside the one
    /// still running and race it on the shared images the renderer reuses across frames (the
    /// present-blit shadow, the glyph atlas). With it, GPU execution order is submission order,
    /// which is the order the code was written for — and the CPU is free to walk away.
    ///
    /// Waiting and signalling the same timeline semaphore in one submit is legal as long as the
    /// signalled value is greater, which it is by construction.
    pub fn submit(&self, cbufs: &[vk::CommandBuffer], fence: vk::Fence) -> Result<Option<u64>> {
        let submit = vk::SubmitInfo::default().command_buffers(cbufs);
        let Some(order) = self.order.as_ref() else {
            unsafe { self.device.queue_submit(self.queue, &[submit], fence) }
                .context("vkQueueSubmit")?;
            return Ok(None);
        };

        // Relaxed: submits are issued from the thread that owns the renderer, so this is not
        // synchronizing anything — it is just handing out consecutive numbers.
        let wait = order.next.fetch_add(1, Ordering::Relaxed);
        let semaphores = [order.semaphore];
        let wait_values = [wait];
        let signal_values = [wait + 1];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let submit = submit
            .wait_semaphores(&semaphores)
            .wait_dst_stage_mask(&stages)
            .signal_semaphores(&semaphores)
            .push_next(&mut timeline);
        unsafe { self.device.queue_submit(self.queue, &[submit], fence) }
            .context("vkQueueSubmit")?;
        Ok(Some(wait + 1))
    }

    /// A fence whose completion can be handed to KMS as a `sync_file`, or `None` when the device
    /// cannot export one. Create the fence this way *before* submitting: the handle types are
    /// fixed at creation.
    pub fn create_exportable_fence(&self) -> Option<Result<vk::Fence>> {
        self.external_fence.as_ref()?;
        let mut export = vk::ExportFenceCreateInfo::default()
            .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
        let ci = vk::FenceCreateInfo::default().push_next(&mut export);
        Some(unsafe { self.device.create_fence(&ci, None) }.context("create exportable fence"))
    }

    /// Export `fence`'s pending completion as a `sync_file` FD. The fence must have been created
    /// by [`Self::create_exportable_fence`] and already submitted — a `SYNC_FD` export needs a
    /// signal operation to be pending or complete.
    pub fn export_fence_sync_fd(&self, fence: vk::Fence) -> Result<OwnedFd> {
        let ext = self
            .external_fence
            .as_ref()
            .ok_or_else(|| anyhow!("no VK_KHR_external_fence_fd"))?;
        let raw = unsafe {
            ext.get_fence_fd(
                &vk::FenceGetFdInfoKHR::default()
                    .fence(fence)
                    .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD),
            )
        }
        .context("vkGetFenceFdKHR")?;
        if raw < 0 {
            return Err(anyhow!("fence export returned an already-signalled stub"));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    /// Whether submits are ordered against each other, which is what makes it safe to leave one
    /// in flight. See [`SubmitOrder`].
    pub fn orders_submits(&self) -> bool {
        self.order.is_some()
    }

    /// How many submits the queue has *completed*, or `None` when the device cannot order
    /// submits. Test-only observability: the counter advances once per submit, so comparing it
    /// against [`crate::stats::submits`] catches a `vkQueueSubmit` that bypassed [`Self::submit`]
    /// and is therefore unordered against everything else.
    pub fn submit_order_value(&self) -> Option<u64> {
        let order = self.order.as_ref()?;
        unsafe { self.device.get_semaphore_counter_value(order.semaphore) }.ok()
    }

    /// Record `record` into a one-time primary command buffer, submit, and block until done.
    pub fn run_commands(
        &self,
        pool: vk::CommandPool,
        site: crate::stats::SubmitSite,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> Result<()> {
        let submitted = self.submit_commands(pool, site, record)?;
        // Everything below this point is the *wait* — the only difference from
        // [`Self::submit_commands`]'s caller-owned form. A guard re-takes ownership so the command
        // buffer and fence are freed on every exit, including the error path.
        let _guard = RunGuard {
            device: &self.device,
            pool,
            cbufs: vec![submitted.cbuf],
            fence: submitted.fence,
        };
        unsafe {
            let _timed = crate::stats::retire(site);
            if let Err(e) = self
                .device
                .wait_for_fences(&[submitted.fence], true, u64::MAX)
            {
                // The submit already succeeded, so the command buffer and the copy's source/dest
                // (staging/image, freed by the caller's UploadGuard) may still be in flight. Drain
                // the device — or confirm it is lost — before any guard frees those resources on
                // unwind, so we never free memory an outstanding submission still references. (A
                // successful wait is the common case; this only runs on a wait error, ~always
                // DEVICE_LOST, where a drain returns immediately.)
                let _ = self.device.device_wait_idle();
                return Err(e).context("vkWaitForFences");
            }
        }
        Ok(())
    }

    /// As [`Self::run_commands`], but stop at the submit and hand the caller the pieces.
    ///
    /// The caller then owns the command buffer and the fence and **must** free both once the GPU
    /// is done with them — normally by putting the [`Submitted`] into a list retired against
    /// [`Self::submit_order_value`]. Only sound when the device orders submits
    /// ([`Self::orders_submits`]); a caller that cannot guarantee that should use `run_commands`
    /// and wait.
    pub fn submit_commands(
        &self,
        pool: vk::CommandPool,
        site: crate::stats::SubmitSite,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> Result<Submitted> {
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cbufs = unsafe { self.device.allocate_command_buffers(&alloc) }?;
        let cbuf = cbufs[0];
        // Free the command buffer + fence on every exit — including an early `?` (begin/end/submit/
        // wait failure) that would otherwise leak the fence until device teardown and hold the cbuf
        // until the pool is destroyed. Replaces the old manual cleanup on the success path.
        let mut guard = RunGuard {
            device: &self.device,
            pool,
            cbufs,
            fence: vk::Fence::null(),
        };

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device.begin_command_buffer(cbuf, &begin)?;
        }
        record(cbuf);
        unsafe {
            self.device.end_command_buffer(cbuf)?;
        }

        let fence = unsafe {
            self.device
                .create_fence(&vk::FenceCreateInfo::default(), None)?
        };
        guard.fence = fence;
        let timeline = {
            let _timed = crate::stats::submit(site);
            self.submit(std::slice::from_ref(&cbuf), fence)?
        };

        // Submitted: the handles are the caller's problem now, so the guard must not free them.
        guard.cbufs.clear();
        guard.fence = vk::Fence::null();
        Ok(Submitted {
            cbuf,
            fence,
            timeline,
        })
    }
}

/// A submitted one-shot command buffer whose completion nobody is waiting on yet. See
/// [`Gpu::submit_commands`] — the handles are the caller's to free.
#[derive(Debug, Clone, Copy)]
pub struct Submitted {
    pub cbuf: vk::CommandBuffer,
    pub fence: vk::Fence,
    /// The queue-timeline value this submit signals, or `None` on a device that cannot order
    /// submits — in which case there is nothing to retire against and the caller must wait.
    pub timeline: Option<u64>,
}

/// Frees the one-shot command buffer + fence that [`Gpu::submit_commands`] allocates, on every exit
/// path (so an error between allocate and the submit doesn't leak them). `free_command_buffers`
/// is valid for a cbuf in any state; the fence is skipped while still null.
struct RunGuard<'a> {
    device: &'a ash::Device,
    pool: vk::CommandPool,
    cbufs: Vec<vk::CommandBuffer>,
    fence: vk::Fence,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if self.fence != vk::Fence::null() {
                self.device.destroy_fence(self.fence, None);
            }
            // Empty is how [`Gpu::submit_commands`] disarms the guard on success, and
            // `commandBufferCount` must be greater than zero — an empty call is invalid usage, not
            // a no-op.
            if !self.cbufs.is_empty() {
                self.device.free_command_buffers(self.pool, &self.cbufs);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(order) = self.order.as_ref() {
                self.device.destroy_semaphore(order.semaphore, None);
            }
            self.device.destroy_device(None);
            // Before the instance it was created from, and after the device so it can still report
            // on teardown.
            if let Some((loader, handle)) = &self.debug {
                loader.destroy_debug_utils_messenger(*handle, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// The DRM render node `(major, minor)` this physical device drives, via
/// `VK_EXT_physical_device_drm`. `None` if the driver doesn't implement that extension (lavapipe)
/// or the device has no render node.
///
/// Properties-only extension: it needs no `vkCreateDevice` enablement, just
/// `vkGetPhysicalDeviceProperties2` (core since 1.1), so it can be queried during selection.
fn drm_render_node(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<(u32, u32)> {
    let supported = unsafe { instance.enumerate_device_extension_properties(pd) }
        .ok()?
        .iter()
        .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == ash::ext::physical_device_drm::NAME);
    if !supported {
        return None;
    }

    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe { instance.get_physical_device_properties2(pd, &mut props) };

    if drm.has_render == vk::FALSE {
        return None;
    }
    // The spec types these as i64 to leave room, but they are Linux dev_t major/minor.
    Some((drm.render_major as u32, drm.render_minor as u32))
}

/// Every DRM modifier this device enumerates for `format`, with its tiling features. Empty if the
/// driver has no `VK_EXT_image_drm_format_modifier` (lavapipe builds without it in some configs) or
/// simply lists none. Two-call idiom: query the count, then the entries.
fn drm_modifier_features(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<(u64, vk::FormatFeatureFlags)> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { instance.get_physical_device_format_properties2(pd, format, &mut props) };
    }
    let count = list.drm_format_modifier_count;
    if count == 0 {
        return Vec::new();
    }

    // push_next only stored a pointer to `list`, so the second query writes through it into `buf`.
    let mut buf = vec![vk::DrmFormatModifierPropertiesEXT::default(); count as usize];
    list.p_drm_format_modifier_properties = buf.as_mut_ptr();
    list.drm_format_modifier_count = count;
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { instance.get_physical_device_format_properties2(pd, format, &mut props) };
    }

    buf.iter()
        .map(|m| (m.drm_format_modifier, m.drm_format_modifier_tiling_features))
        .collect()
}

fn graphics_queue_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|i| i as u32)
}

fn device_name(props: &vk::PhysicalDeviceProperties) -> String {
    unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn device_type_str(t: vk::PhysicalDeviceType) -> &'static str {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => "discrete",
        vk::PhysicalDeviceType::INTEGRATED_GPU => "integrated",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual",
        vk::PhysicalDeviceType::CPU => "cpu",
        _ => "other",
    }
}

/// Preference when several devices are visible: real GPUs over a software rasterizer.
fn rank(t: vk::PhysicalDeviceType) -> u32 {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => 5,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 4,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 3,
        vk::PhysicalDeviceType::CPU => 2,
        _ => 1,
    }
}

fn api_version_str(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// We must render on the device backing the DRM render node we advertise to clients in dmabuf
    /// feedback. Ask for the node the default device itself reports, and check we land back on a
    /// device that really drives it.
    #[test]
    fn selects_the_device_backing_a_drm_render_node() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping: no Vulkan device");
            return;
        };
        let Some((major, minor)) = gpu.drm_render_node() else {
            eprintln!("skipping: driver reports no DRM node (lavapipe?)");
            return;
        };

        let picked = Gpu::with_selector(DeviceSelector::DrmRenderNode { major, minor })
            .expect("the node a device reports for itself must be selectable");
        assert_eq!(
            picked.drm_render_node(),
            Some((major, minor)),
            "selected a device that does not drive the requested render node",
        );
    }

    /// Fail closed. A render node no device backs must be an error, not a quiet fallback to some
    /// other GPU: clients allocate their buffers for the node we advertised, so rendering elsewhere
    /// hands us buffers we are least able to import. This is the bug the selector exists to stop.
    #[test]
    fn refuses_a_drm_render_node_no_device_backs() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping: no Vulkan device");
            return;
        };
        if gpu.drm_render_node().is_none() {
            eprintln!("skipping: driver reports no DRM node, so nothing to correlate against");
            return;
        }

        // 226 is the DRM major; minor 9999 is not a node anything can back.
        let res = Gpu::with_selector(DeviceSelector::DrmRenderNode {
            major: 226,
            minor: 9999,
        });
        assert!(
            res.is_err(),
            "selected a device for a render node nothing backs",
        );
    }

    const LINEAR: u64 = 0;

    /// The present-blit path blits into a modifier-tiled `B8G8R8A8_UNORM` scanout buffer, and the
    /// direct path renders into one and reads it back. Both rest on features the spec does not
    /// mandate for a modifier, so pin that the drivers we ship on really do report them — if this
    /// ever fails, the scanout path was undefined behavior that happened to work.
    #[test]
    fn the_scanout_modifier_supports_what_the_scanout_path_records() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping: no Vulkan device");
            return;
        };
        if gpu
            .modifier_features(vk::Format::B8G8R8A8_UNORM, LINEAR)
            .is_none()
        {
            eprintln!("skipping: driver enumerates no LINEAR modifier for B8G8R8A8_UNORM");
            return;
        }

        gpu.check_modifier_features(
            vk::Format::B8G8R8A8_UNORM,
            LINEAR,
            vk::FormatFeatureFlags::BLIT_DST
                | vk::FormatFeatureFlags::TRANSFER_SRC
                | vk::FormatFeatureFlags::BLIT_SRC,
        )
        .expect("the present-blit target's operations must be defined on the LINEAR modifier");

        gpu.check_modifier_features(
            vk::Format::R8G8B8A8_UNORM,
            LINEAR,
            vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::TRANSFER_SRC
                | vk::FormatFeatureFlags::BLIT_SRC,
        )
        .expect("the direct scanout target's operations must be defined on the LINEAR modifier");
    }

    /// Fail closed when the driver says the operation is undefined. Ask an enumerated modifier for
    /// a feature no 8-bit color format has (`DISJOINT` is a multi-planar YCbCr bit) and require
    /// an error — a check that cannot fail would pin nothing.
    #[test]
    fn refuses_a_modifier_missing_a_required_feature() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping: no Vulkan device");
            return;
        };
        let Some(features) = gpu.modifier_features(vk::Format::B8G8R8A8_UNORM, LINEAR) else {
            eprintln!("skipping: driver enumerates no LINEAR modifier for B8G8R8A8_UNORM");
            return;
        };
        assert!(
            !features.contains(vk::FormatFeatureFlags::DISJOINT),
            "DISJOINT was picked as a feature B8G8R8A8_UNORM cannot have; pick another",
        );

        let res = gpu.check_modifier_features(
            vk::Format::B8G8R8A8_UNORM,
            LINEAR,
            vk::FormatFeatureFlags::BLIT_DST | vk::FormatFeatureFlags::DISJOINT,
        );
        assert!(
            res.is_err(),
            "imported a modifier the driver says cannot support the commands we record",
        );
    }

    /// A modifier the driver neither enumerates nor can create an image with is a hard error, not
    /// an `Unlisted` wave-through: the wave-through exists for incomplete enumeration, not for
    /// a modifier that genuinely is not there.
    #[test]
    fn refuses_a_modifier_that_does_not_exist() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping: no Vulkan device");
            return;
        };
        if gpu
            .modifier_features(vk::Format::B8G8R8A8_UNORM, LINEAR)
            .is_none()
        {
            eprintln!("skipping: driver enumerates no modifiers at all, so nothing to contrast");
            return;
        }

        // Vendor 0xff is unassigned, so no driver backs this.
        let bogus = 0xff00_0000_0000_0001;
        assert!(
            gpu.modifier_features(vk::Format::B8G8R8A8_UNORM, bogus)
                .is_none(),
            "the bogus modifier turned out to be real; pick another",
        );

        let res = gpu.check_modifier_features(
            vk::Format::B8G8R8A8_UNORM,
            bogus,
            vk::FormatFeatureFlags::BLIT_DST,
        );
        assert!(res.is_err(), "accepted a modifier no device can import");
    }
}
