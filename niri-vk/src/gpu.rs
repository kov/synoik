//! Minimal Vulkan (ash) context — the seed of the owned renderer.
//!
//! Stage 0 goal: prove `vkCreateDevice` → `vkQueueSubmit` → readback works on Venus
//! (virtio-gpu, no zink) and on lavapipe (CPU, deterministic test baseline). Nothing here
//! touches a Wayland socket / DRM master / swapchain, so it runs headless from any shell.

use std::ffi::CStr;

use anyhow::{anyhow, Context, Result};
use ash::vk;

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
            instance,
            entry,
        })
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
}
