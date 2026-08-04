// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Minimal, self-contained reproducer for a Venus (Mesa `venus`) driver inconsistency:
//!
//!   vkGetMemoryFdPropertiesKHR returns VK_ERROR_INVALID_EXTERNAL_HANDLE for a dmabuf FD
//!   that vkAllocateMemory (import) then accepts and imports SUCCESSFULLY.
//!
//! The query is supposed to tell you which memory types a given external handle can be
//! imported into. On Venus it rejects a perfectly importable LINEAR dmabuf outright, so a
//! renderer that trusts the query (and masks `memory_type_bits` with its result) would refuse
//! a buffer the very next call happily imports and binds.
//!
//! Path: gbm allocates a LINEAR ARGB8888 buffer on the render node and exports it as a dmabuf;
//! we query it (fails) and then import + bind it (succeeds) on the Venus device.

use std::ffi::CStr;
use std::fs::File;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

use ash::vk;
use gbm::{BufferObjectFlags, Device as GbmDevice, Format, Modifier};

const RENDER_NODE: &str = "/dev/dri/renderD128";
/// ARGB8888 (DRM) is [B,G,R,A] in memory → VK_FORMAT_B8G8R8A8_UNORM. DRM_FORMAT_MOD_LINEAR = 0.
const IMPORT_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- 1. gbm: allocate a LINEAR ARGB8888 buffer and export it as a dmabuf. -----------------
    let file = File::options().read(true).write(true).open(RENDER_NODE)?;
    let gbm = GbmDevice::new(file)?;
    let bo = gbm.create_buffer_object_with_modifiers2::<()>(
        64,
        64,
        Format::Argb8888,
        [Modifier::Linear].into_iter(),
        BufferObjectFlags::empty(),
    )?;
    let fd: OwnedFd = bo.fd()?;
    let stride = bo.stride();
    let offset = bo.offset(0);
    println!("gbm: LINEAR ARGB8888 64x64, stride={stride} offset={offset} fd={}", fd.as_raw_fd());

    // --- 2. ash: instance + a non-CPU physical device + logical device with the 3 extensions. --
    let entry = unsafe { ash::Entry::load()? };
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let instance =
        unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None)? };

    let phys_devices = unsafe { instance.enumerate_physical_devices()? };
    let (phys, queue_family) = phys_devices
        .iter()
        .find_map(|&pd| {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let is_gpu = matches!(
                props.device_type,
                vk::PhysicalDeviceType::INTEGRATED_GPU | vk::PhysicalDeviceType::DISCRETE_GPU
            );
            let qf = unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|i| i as u32);
            match (is_gpu, qf) {
                (true, Some(qf)) => Some((pd, qf)),
                _ => None,
            }
        })
        .ok_or("no INTEGRATED/DISCRETE physical device with a graphics queue (Venus not the ICD?)")?;

    let props = unsafe { instance.get_physical_device_properties(phys) };
    let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    unsafe { instance.get_physical_device_properties2(phys, &mut props2) };
    let driver_name = unsafe { CStr::from_ptr(driver.driver_name.as_ptr()) }.to_string_lossy();
    println!("vk: device={name:?} driver={driver_name:?} queue_family={queue_family}");

    let ext_names = [
        c"VK_KHR_external_memory_fd".as_ptr(),
        c"VK_EXT_external_memory_dma_buf".as_ptr(),
        c"VK_EXT_image_drm_format_modifier".as_ptr(),
    ];
    let priorities = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&priorities);
    let device = unsafe {
        instance.create_device(
            phys,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_info))
                .enabled_extension_names(&ext_names),
            None,
        )?
    };

    // --- 3. Create the DRM-format-modifier VkImage matching the dmabuf's explicit plane layout. -
    let plane_layout = vk::SubresourceLayout {
        offset: offset as u64,
        size: 0,
        row_pitch: stride as u64,
        array_pitch: 0,
        depth_pitch: 0,
    };
    let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(0)
        .plane_layouts(std::slice::from_ref(&plane_layout));
    let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(IMPORT_FORMAT)
        .extent(vk::Extent3D { width: 64, height: 64, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_info)
        .push_next(&mut mod_info);
    let image = unsafe { device.create_image(&image_ci, None)? };
    let mem_req = unsafe { device.get_image_memory_requirements(image) };

    // --- 4. THE QUERY: does Venus think this FD can back memory? (expected: it errors) ----------
    let ext = ash::khr::external_memory_fd::Device::new(&instance, &device);
    let query = unsafe {
        ext.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut vk::MemoryFdPropertiesKHR::default(),
        )
    };
    println!("QUERY  vkGetMemoryFdPropertiesKHR -> {query:?}");

    // --- 5. THE CONTRAST: import the SAME buffer via vkAllocateMemory (expected: succeeds). -----
    let raw = fd.try_clone()?.into_raw_fd();
    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(raw);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_req.size)
        .memory_type_index(mem_req.memory_type_bits.trailing_zeros())
        .push_next(&mut import_info)
        .push_next(&mut dedicated);
    let memory = unsafe { device.allocate_memory(&alloc_info, None) };
    println!("IMPORT vkAllocateMemory(ImportMemoryFdInfoKHR) -> {:?}", memory.map(|_| "Ok"));
    let mem = memory?;
    let bind = unsafe { device.bind_image_memory(image, mem, 0) };
    println!("BIND   vkBindImageMemory -> {bind:?}");
    bind?;

    // --- 6. Verdict. --------------------------------------------------------------------------
    //
    // Conditional, because this reproducer outlived the bug: the host-side fix landed in our
    // `virglrenderer` fork on 2026-07-04 (`patches/virglrenderer/0024`), and on a stack carrying it
    // the query agrees with the import. Printing the failure verdict unconditionally meant a
    // *passing* run reported itself as broken — which matters now that this run is the gate for
    // dropping the guest-side fallback (see `../README.md`, "What the guest does when it lands").
    match query {
        Err(e) => println!(
            "CONCLUSION: query={e:?} but import=SUCCESS → inconsistent (the reported bug \
             reproduces on this stack)"
        ),
        Ok(()) => println!(
            "CONCLUSION: query and import agree → FIXED on this stack. The \
             `image_bits & fd_props_bits` masking pattern is safe here."
        ),
    }

    unsafe {
        device.free_memory(mem, None);
        device.destroy_image(image, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    Ok(())
}
