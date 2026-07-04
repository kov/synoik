//! Forward-looking physical-device probes — data for the stages after this spike, not part of the
//! rendering. Informational (printed, never asserted) because the answers are ICD-specific: Venus
//! has real DRM modifiers + external-semaphore support; lavapipe won't.
//!
//! - DRM format modifiers (Stage 1: importing client dmabufs as VkImages with modifiers).
//! - External-semaphore handle types (Stage 3: explicit sync, drm_syncobj <-> VkTimelineSemaphore).

use std::ffi::CStr;

use ash::vk;

use crate::gpu::Gpu;

pub fn report(gpu: &Gpu) {
    eprintln!("vk-spike: --- probes (planning data for later stages) ---");
    drm_modifiers(
        gpu,
        vk::Format::B8G8R8A8_UNORM,
        "B8G8R8A8_UNORM (ARGB/XRGB8888)",
    );
    drm_modifiers(gpu, vk::Format::R8G8B8A8_UNORM, "R8G8B8A8_UNORM (ABGR8888)");
    external_semaphores(gpu);
}

fn supports_extension(gpu: &Gpu, name: &str) -> bool {
    let exts = unsafe { gpu.instance.enumerate_device_extension_properties(gpu.phys) };
    let Ok(exts) = exts else { return false };
    exts.iter().any(|e| {
        unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }
            .to_str()
            .map(|s| s == name)
            .unwrap_or(false)
    })
}

fn drm_modifiers(gpu: &Gpu, format: vk::Format, label: &str) {
    if !supports_extension(gpu, "VK_EXT_image_drm_format_modifier") {
        eprintln!("  {label}: VK_EXT_image_drm_format_modifier unsupported");
        return;
    }

    // Two-call idiom: first query the count, then the entries. push_next only stores a pointer to
    // `list`, so `list` is read/written across both scoped queries.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            gpu.instance
                .get_physical_device_format_properties2(gpu.phys, format, &mut props);
        }
    }
    let count = list.drm_format_modifier_count;
    if count == 0 {
        eprintln!("  {label}: 0 modifiers");
        return;
    }

    let mut buf = vec![vk::DrmFormatModifierPropertiesEXT::default(); count as usize];
    list.p_drm_format_modifier_properties = buf.as_mut_ptr();
    list.drm_format_modifier_count = count;
    {
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            gpu.instance
                .get_physical_device_format_properties2(gpu.phys, format, &mut props);
        }
    }

    eprintln!("  {label}: {count} DRM modifiers");
    for m in &buf {
        let f = m.drm_format_modifier_tiling_features;
        eprintln!(
            "    modifier {:#018x}  planes={}  sampled={} color_attachment={}",
            m.drm_format_modifier,
            m.drm_format_modifier_plane_count,
            f.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE),
            f.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT),
        );
    }
}

fn external_semaphores(gpu: &Gpu) {
    eprintln!("  external semaphore handle types (export/import):");
    let handle_types = [
        (vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD, "OPAQUE_FD"),
        (vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD, "SYNC_FD"),
    ];
    let kinds = [
        (vk::SemaphoreType::BINARY, "binary"),
        (vk::SemaphoreType::TIMELINE, "timeline"),
    ];
    for (handle, hname) in handle_types {
        for (sem_type, tname) in kinds {
            let mut type_info = vk::SemaphoreTypeCreateInfo::default().semaphore_type(sem_type);
            let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(handle)
                .push_next(&mut type_info);
            let mut props = vk::ExternalSemaphoreProperties::default();
            unsafe {
                gpu.instance
                    .get_physical_device_external_semaphore_properties(gpu.phys, &info, &mut props);
            }
            let feats = props.external_semaphore_features;
            eprintln!(
                "    {hname} {tname}: export={} import={}",
                feats.contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE),
                feats.contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE),
            );
        }
    }
}
