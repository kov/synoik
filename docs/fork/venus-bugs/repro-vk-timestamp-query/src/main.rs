//! Minimal, self-contained reproducer for a Venus (Mesa `venus`) timestamp-query gap.
//!
//! The device advertises timestamp queries in full — `timestampComputeAndGraphics = true`,
//! `timestampValidBits = 64` on the graphics queue, `timestampPeriod = 1` — but
//! `vkCmdWriteTimestamp` never writes a value. After a submit and a fence wait the queries come
//! back **available (`availability = 1`) with a value of `0`**.
//!
//! That combination is the whole problem. The queries are not "not ready" and the calls do not
//! fail: `vkGetQueryPoolResults` correctly reports success for a resolved query, and hands the
//! caller a zero. There is nothing in the API surface that distinguishes "this GPU pass took no
//! measurable time" from "this driver does not implement the feature it advertises", so a profiler
//! that trusts the result reports a confident, wrong `0ns` for every pass.
//!
//! The device clock itself is fine: `vkGetCalibratedTimestampsEXT` returns a live `DEVICE`
//! timestamp on the same device, advancing at one tick per nanosecond (so `timestampPeriod = 1` is
//! accurate). The gap is the query-pool write/resolve path specifically, not a missing GPU clock.
//!
//! Run with Venus selected (`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
//! cargo run`) and against lavapipe on the same guest
//! (`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run`) for the contrast:
//! identical declared limits, real timestamps out of lavapipe.

use std::ffi::{c_char, CStr};

use ash::vk;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let entry = unsafe { ash::Entry::load()? };
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let instance_ci = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = unsafe { entry.create_instance(&instance_ci, None)? };

    // --- 1. Pick a graphics-capable device and report what it claims. ------------------------
    let phys = unsafe { instance.enumerate_physical_devices()? };
    let (phys, queue_family) = phys
        .into_iter()
        .find_map(|pd| {
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            families
                .iter()
                .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|i| (pd, i as u32))
        })
        .ok_or("no physical device with a graphics queue")?;

    let props = unsafe { instance.get_physical_device_properties(phys) };
    let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    let families = unsafe { instance.get_physical_device_queue_family_properties(phys) };
    let valid_bits = families[queue_family as usize].timestamp_valid_bits;

    println!("device: {name:?}");
    println!(
        "  timestampPeriod           = {}",
        props.limits.timestamp_period
    );
    println!(
        "  timestampComputeAndGraphics = {}",
        props.limits.timestamp_compute_and_graphics != 0
    );
    println!("  timestampValidBits (gfx q)  = {valid_bits}");

    if valid_bits == 0 {
        println!("\nthis queue does not claim timestamp support — nothing to reproduce");
        return Ok(());
    }

    // --- 2. Bring up a device with calibrated timestamps, so the clock can be cross-checked. --
    let has_calibrated = unsafe { instance.enumerate_device_extension_properties(phys)? }
        .iter()
        .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == c"VK_EXT_calibrated_timestamps");

    let mut wanted: Vec<*const c_char> = Vec::new();
    if has_calibrated {
        wanted.push(c"VK_EXT_calibrated_timestamps".as_ptr());
    }
    let priorities = [1.0f32];
    let queue_ci = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&priorities);
    let device_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(std::slice::from_ref(&queue_ci))
        .enabled_extension_names(&wanted);
    let device = unsafe { instance.create_device(phys, &device_ci, None)? };
    let queue = unsafe { device.get_device_queue(queue_family, 0) };

    // --- 3. A command buffer whose only work is two timestamp writes. ------------------------
    let pool = unsafe {
        device.create_query_pool(
            &vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2),
            None,
        )?
    };
    let cmd_pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(queue_family),
            None,
        )?
    };
    let cbuf = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?
    }[0];

    unsafe {
        device.begin_command_buffer(cbuf, &vk::CommandBufferBeginInfo::default())?;
        device.cmd_reset_query_pool(cbuf, pool, 0, 2);
        device.cmd_write_timestamp(cbuf, vk::PipelineStageFlags::TOP_OF_PIPE, pool, 0);
        device.cmd_write_timestamp(cbuf, vk::PipelineStageFlags::ALL_COMMANDS, pool, 1);
        device.end_command_buffer(cbuf)?;

        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cbuf));
        device.queue_submit(queue, std::slice::from_ref(&submit), fence)?;
        // Fully drained: whatever the queries were going to hold, they hold now.
        device.wait_for_fences(std::slice::from_ref(&fence), true, u64::MAX)?;
        device.destroy_fence(fence, None);
    }

    // --- 4. Read the results three ways. -----------------------------------------------------
    println!("\nafter submit + fence wait:");

    let mut with_wait = [0u64; 2];
    let r = unsafe {
        device.get_query_pool_results(
            pool,
            0,
            &mut with_wait,
            vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
        )
    };
    println!("  WAIT               -> {r:?}, values {with_wait:?}");

    let mut no_wait = [0u64; 2];
    let r = unsafe {
        device.get_query_pool_results(pool, 0, &mut no_wait, vk::QueryResultFlags::TYPE_64)
    };
    println!("  (no WAIT)          -> {r:?}, values {no_wait:?}");

    // The decisive one: is each query *available*? `WITH_AVAILABILITY` appends an
    // availability word per query, so the element type must be 16 bytes — ash derives
    // both `queryCount` and `stride` from the slice, and a `[u64; 4]` here would ask
    // for four queries at an 8-byte stride, which is invalid on both counts.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Availability {
        value: u64,
        available: u64,
    }
    let mut with_avail = [Availability::default(); 2];
    let r = unsafe {
        device.get_query_pool_results(
            pool,
            0,
            &mut with_avail,
            vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
        )
    };
    println!(
        "  WITH_AVAILABILITY  -> {r:?}, query0 value {} avail {}, query1 value {} avail {}",
        with_avail[0].value, with_avail[0].available, with_avail[1].value, with_avail[1].available,
    );

    // --- 5. Cross-check that a device clock exists at all. ------------------------------------
    if has_calibrated {
        let ct_instance = ash::ext::calibrated_timestamps::Instance::new(&entry, &instance);
        let domains =
            unsafe { ct_instance.get_physical_device_calibrateable_time_domains(phys) }?;
        println!("\ncalibrated time domains: {domains:?}");

        if domains.contains(&vk::TimeDomainEXT::DEVICE) {
            let ct_device = ash::ext::calibrated_timestamps::Device::new(&instance, &device);
            let infos = [
                vk::CalibratedTimestampInfoEXT::default().time_domain(vk::TimeDomainEXT::DEVICE),
                vk::CalibratedTimestampInfoEXT::default()
                    .time_domain(vk::TimeDomainEXT::CLOCK_MONOTONIC),
            ];
            let (first, deviation) = unsafe { ct_device.get_calibrated_timestamps(&infos)? };
            std::thread::sleep(std::time::Duration::from_millis(100));
            let (second, _) = unsafe { ct_device.get_calibrated_timestamps(&infos)? };

            println!(
                "  DEVICE   {} -> {} (advanced {} ticks over ~100ms)",
                first[0],
                second[0],
                second[0].wrapping_sub(first[0]),
            );
            println!("  MONOTONIC {} -> {}", first[1], second[1]);
            println!("  max deviation: {deviation} ns");
            println!(
                "\n  => the device clock is readable and advancing, so the gap is the\n     \
                 query-pool write/resolve path, not a missing GPU clock."
            );
        }
    }

    println!(
        "\nexpected on a conforming implementation: both queries available with two large,\n\
         advancing tick values (see the lavapipe run on this same guest)."
    );

    unsafe {
        device.destroy_command_pool(cmd_pool, None);
        device.destroy_query_pool(pool, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    Ok(())
}
