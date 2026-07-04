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
    // Device extensions we asked for AND the device advertised (so lavapipe, which lacks the
    // dmabuf ones, still builds a device — the dmabuf demo checks these and skips if absent).
    enabled_extensions: Vec<String>,
    pub instance: ash::Instance,
    // Owns the loaded Vulkan library; must outlive `instance`/`device`.
    #[allow(dead_code)]
    pub entry: ash::Entry,
}

impl Gpu {
    /// Bring up an instance + a single graphics-capable logical device.
    ///
    /// Device selection is deterministic: with `VK_DRIVER_FILES` pinned to one ICD only that
    /// driver's devices enumerate, so the choice is forced. When several are visible we prefer
    /// a real GPU over a CPU one, but log everything so the run is self-documenting.
    pub fn new() -> Result<Self> {
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
        for &pd in &devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = device_name(&props);
            let kind = device_type_str(props.device_type);
            let gfx = graphics_queue_family(&instance, pd);
            eprintln!(
                "  device: {name:?} [{kind}] api {}  graphics-queue: {gfx:?}",
                api_version_str(props.api_version)
            );
            if let Some(qf) = gfx {
                let better = match &best {
                    None => true,
                    Some((_, cur, _)) => rank(props.device_type) > rank(cur.device_type),
                };
                if better {
                    best = Some((pd, props, qf));
                }
            }
        }

        let (phys, props, queue_family) =
            best.ok_or_else(|| anyhow!("no physical device with a graphics queue"))?;
        let device_name = device_name(&props);

        // Enable the external-memory extensions needed to import dmabufs with DRM modifiers, but
        // only those the device actually advertises. (external_memory / bind_memory2 /
        // sampler_ycbcr_conversion / image_format_list are core in 1.1–1.2, so not listed here.)
        let want: [&CStr; 4] = [
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_EXT_image_drm_format_modifier",
            // For acquiring imported content from the FOREIGN (non-Vulkan producer) queue family;
            // if absent we fall back to a plain layout transition (see dmabuf.rs).
            c"VK_EXT_queue_family_foreign",
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
            enabled_extensions,
            instance,
            entry,
        })
    }

    /// Was this device extension enabled at device creation? (Used to gate the dmabuf demo.)
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
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cbuf));
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .context("vkQueueSubmit")?;
            self.device.wait_for_fences(&[fence], true, u64::MAX)?;
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(pool, &cbufs);
        }
        Ok(())
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
