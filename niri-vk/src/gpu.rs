//! Minimal Vulkan (ash) context — the seed of the owned renderer.
//!
//! Stage 0 goal: prove `vkCreateDevice` → `vkQueueSubmit` → readback works on Venus
//! (virtio-gpu, no zink) and on lavapipe (CPU, deterministic test baseline). Nothing here
//! touches a Wayland socket / DRM master / swapchain, so it runs headless from any shell.

use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use ash::vk;

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
    pub instance: ash::Instance,
    // Owns the loaded Vulkan library; must outlive `instance`/`device`.
    #[allow(dead_code)]
    pub entry: ash::Entry,
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
            .application_name(c"niri-vk")
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let create_info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance =
            unsafe { entry.create_instance(&create_info, None) }.context("vkCreateInstance")?;

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

        let priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&enabled_ptrs);
        let device =
            unsafe { instance.create_device(phys, &device_ci, None) }.context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };

        Ok(Gpu {
            device,
            queue,
            queue_family,
            phys,
            mem_props,
            device_name,
            device_type: props.device_type,
            drm_render_node: drm_render_node(&instance, phys),
            enabled_extensions,
            modifier_features: Mutex::new(HashMap::new()),
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

    /// Allocate device memory sized/typed for `req` with the given property `flags`.
    pub fn allocate(
        &self,
        req: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory> {
        let index = self.find_memory_type(req.memory_type_bits, flags)?;
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        unsafe { self.device.allocate_memory(&info, None) }.context("allocate_memory")
    }

    /// Record `record` into a one-time primary command buffer, submit, and block until done.
    pub fn run_commands(
        &self,
        pool: vk::CommandPool,
        record: impl FnOnce(vk::CommandBuffer),
    ) -> Result<()> {
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
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cbuf));
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .context("vkQueueSubmit")?;
            if let Err(e) = self.device.wait_for_fences(&[fence], true, u64::MAX) {
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
}

/// Frees the one-shot command buffer + fence that [`Gpu::run_commands`] allocates, on every exit
/// path (so an error between allocate and the final wait doesn't leak them). `free_command_buffers`
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
            self.device.free_command_buffers(self.pool, &self.cbufs);
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
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
