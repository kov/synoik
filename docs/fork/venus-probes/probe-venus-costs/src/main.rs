//! Guest-side probes for the three claims disputed in `docs/fork/venus-cost.md` §8.
//!
//! Each probe is a falsifiable experiment, not a benchmark for its own sake:
//!
//! * `image` — §8.1. The host side says our per-`vkCreateImage` cost is a **cache** effect: venus
//!   caches image memory requirements keyed by a BLAKE3 over `VkImageCreateInfo`
//!   `flags..sharingMode` (which includes `extent`); a hit takes `vn_async_vkCreateImage` and never
//!   talks to the host, a miss takes two synchronous round trips. If that is right, the same extent
//!   repeated is ~free, a novel extent every time costs ~2 ms, and rounding extents up to a grid
//!   buys back the difference. This probe measures all three, plus what `vkAllocateMemory` /
//!   `vkBindImageMemory` / `vkCreateImageView` cost on the side — the reply claims the allocate is
//!   asynchronous, but this device exposes exactly one memory type and it is `HOST_VISIBLE`, which
//!   sends every allocation down venus's guest-VRAM path.
//!
//! * `memory` — §8.2. The host side says the staging mapping is **cached**, not write-combined, so
//!   our 5.95 GB/s is a guest-kernel question rather than a host one. Write-combining has a
//!   signature that a write-bandwidth number alone cannot show: reads. A cached mapping reads at
//!   roughly its write speed; a WC / Normal-NC mapping reads an order of magnitude slower and has a
//!   much worse random-access latency. This probe measures both directions and random access
//!   against ordinary guest memory as the control.
//!
//! * `fence` — §8.4. The host side says nothing paces a guest fence to a refresh, and that a 13 ms
//!   fence wait therefore means the GPU genuinely took 13 ms. That is testable without timestamp
//!   queries: measure the wait for a submit with *no* work (the floor), then grade the GPU work
//!   linearly and see whether the wait follows. A linear fit gives both the fixed round-trip cost
//!   (intercept) and a GPU-time proxy (slope) — which is most of what §3.5 was wanted for. It also
//!   measures whether K submits cost K round trips or one.
//!
//! Run: `cargo run --release -- [image|memory|fence|all]`. Venus is the default ICD here; add
//! `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json` to contrast against lavapipe.
//! For the `image` probe, `VN_PERF=no_async_image_create` disables the cache entirely and
//! `VN_DEBUG=cache` dumps hit/miss counts at device teardown.

use std::ffi::{c_char, CStr};
use std::time::Instant;

use ash::vk;

const MIB: usize = 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// stats

/// Per-operation costs in milliseconds, summarised. `min` matters as much as the median here:
/// for a round-trip cost the fastest sample is the one with the least unrelated noise in it.
struct Summary {
    n: usize,
    min: f64,
    median: f64,
    mean: f64,
    p95: f64,
    max: f64,
}

impl Summary {
    fn of(mut samples: Vec<f64>) -> Self {
        assert!(!samples.is_empty());
        samples.sort_by(f64::total_cmp);
        let n = samples.len();
        Summary {
            n,
            min: samples[0],
            median: samples[n / 2],
            mean: samples.iter().sum::<f64>() / n as f64,
            p95: samples[(n * 95 / 100).min(n - 1)],
            max: samples[n - 1],
        }
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={:<4} min {:>8.4}  median {:>8.4}  mean {:>8.4}  p95 {:>8.4}  max {:>8.4}  (ms)",
            self.n, self.min, self.median, self.mean, self.p95, self.max
        )
    }
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------------------------
// setup

struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    /// True when `VK_EXT_image_drm_format_modifier` came up, so the dmabuf-shaped image phase
    /// (the shape every client window texture takes) can run.
    has_drm_modifier: bool,
    mem_type: u32,
}

impl Gpu {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let entry = unsafe { ash::Entry::load()? };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )?
        };

        let (phys, queue_family) = unsafe { instance.enumerate_physical_devices()? }
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

        let exts = unsafe { instance.enumerate_device_extension_properties(phys)? };
        let has = |want: &CStr| {
            exts.iter()
                .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == want)
        };
        let has_drm_modifier = has(c"VK_EXT_image_drm_format_modifier");

        let mut wanted: Vec<*const c_char> = vec![c"VK_KHR_external_memory_fd".as_ptr()];
        if has_drm_modifier {
            wanted.push(c"VK_EXT_image_drm_format_modifier".as_ptr());
        }

        let priorities = [1.0f32];
        let queue_ci = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let device = unsafe {
            instance.create_device(
                phys,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(std::slice::from_ref(&queue_ci))
                    .enabled_extension_names(&wanted),
                None,
            )?
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        // One memory type is the whole story on this device (see §8.2); take the first that is
        // host-visible, which on Venus/KosmicKrisp is index 0 and the only one there is.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phys) };
        let mem_type = (0..mem_props.memory_type_count)
            .find(|&i| {
                mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
            })
            .ok_or("no host-visible memory type")?;

        println!(
            "device: {name:?}  ({} memory types)",
            mem_props.memory_type_count
        );
        for i in 0..mem_props.memory_type_count {
            println!(
                "  memoryTypes[{i}] = {:?}",
                mem_props.memory_types[i as usize].property_flags
            );
        }
        println!("  VK_EXT_image_drm_format_modifier: {has_drm_modifier}");
        println!();

        Ok(Gpu {
            _entry: entry,
            instance,
            device,
            queue,
            queue_family,
            has_drm_modifier,
            mem_type,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// §8.1 — image creation: cache hit vs miss

fn image_ci(w: u32, h: u32) -> vk::ImageCreateInfo<'static> {
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
}

/// Time `vkCreateImage` + `vkGetImageMemoryRequirements2` for each extent in turn. The
/// requirements query is included deliberately: on a cache miss venus makes it a second
/// synchronous round trip, on a hit it is served from the cached entry.
fn time_image_creates(gpu: &Gpu, extents: &[(u32, u32)]) -> (Summary, Summary) {
    let mut create = Vec::with_capacity(extents.len());
    let mut reqs = Vec::with_capacity(extents.len());

    for &(w, h) in extents {
        let ci = image_ci(w, h);

        let t = Instant::now();
        let image = unsafe { gpu.device.create_image(&ci, None) }.expect("create_image");
        create.push(ms_since(t));

        let info = vk::ImageMemoryRequirementsInfo2::default().image(image);
        let mut out = vk::MemoryRequirements2::default();
        let t = Instant::now();
        unsafe { gpu.device.get_image_memory_requirements2(&info, &mut out) };
        reqs.push(ms_since(t));

        unsafe { gpu.device.destroy_image(image, None) };
    }

    (Summary::of(create), Summary::of(reqs))
}

/// The same, for an image declared as importable from a dmabuf — the shape every client window
/// texture takes here. §8.1 claims this path is cacheable too; the external and DRM-modifier
/// structs are hashed rather than skipped.
fn time_dmabuf_image_creates(gpu: &Gpu, extents: &[(u32, u32)]) -> Option<Summary> {
    if !gpu.has_drm_modifier {
        return None;
    }
    let modifiers = [0u64]; // DRM_FORMAT_MOD_LINEAR
    let mut create = Vec::with_capacity(extents.len());

    for &(w, h) in extents {
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut mod_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let ci = image_ci(w, h)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .push_next(&mut external)
            .push_next(&mut mod_list);

        let t = Instant::now();
        let image = match unsafe { gpu.device.create_image(&ci, None) } {
            Ok(i) => i,
            Err(e) => {
                println!("  (dmabuf-shaped create unsupported: {e:?})");
                return None;
            }
        };
        create.push(ms_since(t));
        unsafe { gpu.device.destroy_image(image, None) };
    }

    Some(Summary::of(create))
}

/// What the rest of "making an image usable" costs, at a repeated extent so the create itself is
/// a cache hit and the other three calls are what is left.
fn time_image_lifecycle(gpu: &Gpu, n: usize) {
    let (mut alloc, mut bind, mut view, mut destroy) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for _ in 0..n {
        let ci = image_ci(512, 512);
        let image = unsafe { gpu.device.create_image(&ci, None) }.expect("create_image");
        let req = unsafe { gpu.device.get_image_memory_requirements(image) };

        let t = Instant::now();
        let mem = unsafe {
            gpu.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(gpu.mem_type),
                None,
            )
        }
        .expect("allocate_memory");
        alloc.push(ms_since(t));

        let t = Instant::now();
        unsafe { gpu.device.bind_image_memory(image, mem, 0) }.expect("bind_image_memory");
        bind.push(ms_since(t));

        let t = Instant::now();
        let v = unsafe {
            gpu.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .expect("create_image_view");
        view.push(ms_since(t));

        let t = Instant::now();
        unsafe {
            gpu.device.destroy_image_view(v, None);
            gpu.device.destroy_image(image, None);
            gpu.device.free_memory(mem, None);
        }
        destroy.push(ms_since(t));
    }

    println!("  vkAllocateMemory            {}", Summary::of(alloc));
    println!("  vkBindImageMemory           {}", Summary::of(bind));
    println!("  vkCreateImageView           {}", Summary::of(view));
    println!("  destroy view+image+free mem {}", Summary::of(destroy));
}

fn probe_image(gpu: &Gpu) {
    const N: usize = 200;
    println!("=== §8.1  image creation: is the cost a cache miss? ===\n");

    // Warm-up: the very first create on a fresh device pays one-off ring/device setup that has
    // nothing to do with what is being measured.
    let _ = time_image_creates(gpu, &[(64, 64), (64, 64)]);

    let same: Vec<_> = (0..N).map(|_| (512u32, 512u32)).collect();
    let (c, r) = time_image_creates(gpu, &same);
    println!("same extent, 512x512 repeated (expect: 1 miss then all hits)");
    println!("  vkCreateImage               {c}");
    println!("  vkGetImageMemoryRequirements2 {r}\n");

    let novel: Vec<_> = (0..N).map(|i| (256 + i as u32, 256 + i as u32)).collect();
    let (c, r) = time_image_creates(gpu, &novel);
    println!("novel extent every time (expect: all misses)");
    println!("  vkCreateImage               {c}");
    println!("  vkGetImageMemoryRequirements2 {r}\n");

    // The proposed fix: the same novel sizes, rounded up to a 64 px grid. 200 sizes spread over
    // 256..2256 collapse to ~32 distinct buckets, so all but the first ~32 should hit.
    let bucketed: Vec<_> = novel
        .iter()
        .map(|&(w, h)| ((w + 63) & !63, (h + 63) & !63))
        .collect();
    let (c, _) = time_image_creates(gpu, &bucketed);
    println!("same sizes bucketed up to a 64px grid (the proposed guest-side fix)");
    println!("  vkCreateImage               {c}\n");

    println!("dmabuf-shaped (external + DRM modifier list), novel extent every time");
    match time_dmabuf_image_creates(gpu, &novel) {
        Some(s) => println!("  vkCreateImage               {s}\n"),
        None => println!("  skipped\n"),
    }
    println!("dmabuf-shaped, same extent repeated");
    match time_dmabuf_image_creates(gpu, &same) {
        Some(s) => println!("  vkCreateImage               {s}\n"),
        None => println!("  skipped\n"),
    }

    println!("the rest of the lifecycle, at a cache-hit extent");
    time_image_lifecycle(gpu, 64);
    println!();
}

// ---------------------------------------------------------------------------------------------
// §8.2 — host-visible mapping: cached or write-combined?

/// Sequential copy bandwidth in GB/s, best of `reps` (best, not mean: we are after the
/// mapping's ceiling, and a scheduler hiccup can only make a sample slower).
fn copy_gbps(dst: *mut u8, src: *const u8, len: usize, reps: usize) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..reps {
        let t = Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        best = best.min(t.elapsed().as_secs_f64());
    }
    len as f64 / best / 1e9
}

/// Average latency of a dependent, page-strided read, in nanoseconds. This is the measurement
/// write-combining cannot hide behind: an uncached mapping pays the full bus latency per access.
fn strided_read_ns(base: *const u8, len: usize, accesses: usize) -> (f64, u64) {
    const STRIDE: usize = 4096;
    let mut acc: u64 = 0;
    let t = Instant::now();
    for i in 0..accesses {
        // Offset depends on the previous read, so the accesses cannot be pipelined away.
        let off = ((i * STRIDE) + (acc as usize & 63)) % (len - 64);
        acc = acc.wrapping_add(unsafe { std::ptr::read_volatile(base.add(off)) } as u64);
    }
    (t.elapsed().as_secs_f64() * 1e9 / accesses as f64, acc)
}

fn probe_memory(gpu: &Gpu) -> Result<(), Box<dyn std::error::Error>> {
    const SIZE: usize = 64 * MIB;
    println!("=== §8.2  host-visible mapping: cached, or write-combined? ===\n");

    let buffer = unsafe {
        gpu.device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(SIZE as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )?
    };
    let req = unsafe { gpu.device.get_buffer_memory_requirements(buffer) };
    let mem = unsafe {
        gpu.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(gpu.mem_type),
            None,
        )?
    };
    unsafe { gpu.device.bind_buffer_memory(buffer, mem, 0)? };

    let t = Instant::now();
    let mapped = unsafe {
        gpu.device
            .map_memory(mem, 0, SIZE as u64, vk::MemoryMapFlags::empty())?
    } as *mut u8;
    println!("vkMapMemory of {} MiB: {:.3} ms", SIZE / MIB, ms_since(t));

    // Controls: ordinary guest heap, same size, same code path.
    let mut anon_a = vec![0u8; SIZE];
    let mut anon_b = vec![1u8; SIZE];
    // Fault both in before timing anything.
    anon_a.iter_mut().for_each(|b| *b = 2);
    anon_b.iter_mut().for_each(|b| *b = 3);
    unsafe { std::ptr::write_bytes(mapped, 4, SIZE) };

    println!();
    println!(
        "write  guest heap -> mapped blob : {:>6.2} GB/s",
        copy_gbps(mapped, anon_a.as_ptr(), SIZE, 5)
    );
    println!(
        "write  guest heap -> guest heap  : {:>6.2} GB/s   (control)",
        copy_gbps(anon_b.as_mut_ptr(), anon_a.as_ptr(), SIZE, 5)
    );
    println!(
        "read   mapped blob -> guest heap : {:>6.2} GB/s",
        copy_gbps(anon_a.as_mut_ptr(), mapped as *const u8, SIZE, 5)
    );
    println!(
        "read   guest heap -> guest heap  : {:>6.2} GB/s   (control)",
        copy_gbps(anon_b.as_mut_ptr(), anon_a.as_ptr(), SIZE, 5)
    );

    println!();
    let (blob_ns, s1) = strided_read_ns(mapped as *const u8, SIZE, 200_000);
    let (anon_ns, s2) = strided_read_ns(anon_a.as_ptr(), SIZE, 200_000);
    println!("strided read latency, mapped blob : {blob_ns:>7.1} ns/access");
    println!("strided read latency, guest heap  : {anon_ns:>7.1} ns/access   (control)");
    println!("  (checksums {s1} {s2} — printed only so the reads cannot be optimised out)");
    println!();
    println!(
        "read/write ratio on the blob is the discriminator: a cached mapping reads about as fast\n\
         as it writes; a write-combined / Normal-NC one reads far slower and its strided latency\n\
         is several times the cached control's."
    );
    println!();

    unsafe {
        gpu.device.unmap_memory(mem);
        gpu.device.destroy_buffer(buffer, None);
        gpu.device.free_memory(mem, None);
    }

    first_touch(gpu, SIZE)?;
    Ok(())
}

/// The measurement above writes into a mapping that has already been touched. A compositor
/// staging buffer has not: it is created, mapped, filled once and dropped, so **every page is a
/// first touch**. Time that separately — if the first pass over a fresh mapping is much slower
/// than the second, §3.4's 5.95 GB/s is page-fault cost, not mapping bandwidth, and no host
/// change would move it.
fn first_touch(gpu: &Gpu, size: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("first write into a freshly mapped blob vs. the second write into the same one");
    let source = vec![7u8; size];

    for round in 0..3 {
        let buffer = unsafe {
            gpu.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )?
        };
        let req = unsafe { gpu.device.get_buffer_memory_requirements(buffer) };
        let mem = unsafe {
            gpu.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(gpu.mem_type),
                None,
            )?
        };
        unsafe { gpu.device.bind_buffer_memory(buffer, mem, 0)? };

        let t = Instant::now();
        let ptr = unsafe {
            gpu.device
                .map_memory(mem, 0, size as u64, vk::MemoryMapFlags::empty())?
        } as *mut u8;
        let map_ms = ms_since(t);

        let t = Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), ptr, size) };
        let first_ms = ms_since(t);

        let t = Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), ptr, size) };
        let second_ms = ms_since(t);

        let gbps = |ms: f64| size as f64 / (ms / 1000.0) / 1e9;
        println!(
            "  round {round}: map {map_ms:>6.3} ms   first {first_ms:>7.3} ms ({:>6.2} GB/s)   \
             second {second_ms:>7.3} ms ({:>6.2} GB/s)",
            gbps(first_ms),
            gbps(second_ms)
        );

        unsafe {
            gpu.device.unmap_memory(mem);
            gpu.device.destroy_buffer(buffer, None);
            gpu.device.free_memory(mem, None);
        }
    }

    // The control: an ordinary guest allocation of the same size, also untouched. `vec![0u8; n]`
    // gets zero pages from the kernel, so this pays guest first-touch faults and nothing else.
    let t = Instant::now();
    let mut fresh: Vec<u8> = Vec::with_capacity(size);
    #[allow(clippy::uninit_vec)]
    unsafe {
        fresh.set_len(size)
    };
    let alloc_ms = ms_since(t);
    let t = Instant::now();
    unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), fresh.as_mut_ptr(), size) };
    let first_ms = ms_since(t);
    let t = Instant::now();
    unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), fresh.as_mut_ptr(), size) };
    let second_ms = ms_since(t);
    println!(
        "  control (fresh guest heap): alloc {alloc_ms:>6.3} ms   first {first_ms:>7.3} ms \
         ({:>6.2} GB/s)   second {second_ms:>7.3} ms ({:>6.2} GB/s)",
        size as f64 / (first_ms / 1000.0) / 1e9,
        size as f64 / (second_ms / 1000.0) / 1e9
    );
    // Read the destination back so neither copy can be optimised away.
    println!("  (control checksum {})", unsafe {
        std::ptr::read_volatile(fresh.as_ptr().add(size - 1))
    });
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// §8.4 — what is actually inside a fence wait?

struct FenceRig {
    pool: vk::CommandPool,
    cbuf: vk::CommandBuffer,
    fence: vk::Fence,
    src: vk::Image,
    dst: vk::Image,
    src_mem: vk::DeviceMemory,
    dst_mem: vk::DeviceMemory,
    width: u32,
    height: u32,
}

impl FenceRig {
    fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let pool = unsafe {
            gpu.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(gpu.queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        let cbuf = unsafe {
            gpu.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?
        }[0];
        let fence = unsafe {
            gpu.device
                .create_fence(&vk::FenceCreateInfo::default(), None)?
        };

        let make = || -> Result<(vk::Image, vk::DeviceMemory), Box<dyn std::error::Error>> {
            let ci = image_ci(width, height)
                .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST);
            let image = unsafe { gpu.device.create_image(&ci, None)? };
            let req = unsafe { gpu.device.get_image_memory_requirements(image) };
            let mem = unsafe {
                gpu.device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(gpu.mem_type),
                    None,
                )?
            };
            unsafe { gpu.device.bind_image_memory(image, mem, 0)? };
            Ok((image, mem))
        };
        let (src, src_mem) = make()?;
        let (dst, dst_mem) = make()?;

        // One-off transition of both to GENERAL so the copy loop needs no barriers of its own.
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        unsafe {
            gpu.device
                .begin_command_buffer(cbuf, &vk::CommandBufferBeginInfo::default())?;
            let barriers = [src, dst].map(|image| {
                vk::ImageMemoryBarrier::default()
                    .image(image)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            });
            gpu.device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            gpu.device.end_command_buffer(cbuf)?;
            let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cbuf));
            gpu.device
                .queue_submit(gpu.queue, std::slice::from_ref(&submit), fence)?;
            gpu.device
                .wait_for_fences(std::slice::from_ref(&fence), true, u64::MAX)?;
            gpu.device.reset_fences(std::slice::from_ref(&fence))?;
        }

        Ok(FenceRig {
            pool,
            cbuf,
            fence,
            src,
            dst,
            src_mem,
            dst_mem,
            width,
            height,
        })
    }

    /// One submit carrying `copies` full-surface image copies, then a fence wait. Returns
    /// (submit ms, wait ms).
    fn round(&self, gpu: &Gpu, copies: usize) -> (f64, f64) {
        let region = vk::ImageCopy::default()
            .src_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .dst_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            });

        unsafe {
            gpu.device
                .reset_command_buffer(self.cbuf, vk::CommandBufferResetFlags::empty())
                .unwrap();
            gpu.device
                .begin_command_buffer(self.cbuf, &vk::CommandBufferBeginInfo::default())
                .unwrap();
            for i in 0..copies {
                // Alternate direction so consecutive copies cannot be folded away.
                let (from, to) = if i % 2 == 0 {
                    (self.src, self.dst)
                } else {
                    (self.dst, self.src)
                };
                gpu.device.cmd_copy_image(
                    self.cbuf,
                    from,
                    vk::ImageLayout::GENERAL,
                    to,
                    vk::ImageLayout::GENERAL,
                    std::slice::from_ref(&region),
                );
                gpu.device.cmd_pipeline_barrier(
                    self.cbuf,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)],
                    &[],
                    &[],
                );
            }
            gpu.device.end_command_buffer(self.cbuf).unwrap();

            let submit =
                vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cbuf));
            let t = Instant::now();
            gpu.device
                .queue_submit(gpu.queue, std::slice::from_ref(&submit), self.fence)
                .unwrap();
            let submit_ms = ms_since(t);

            let t = Instant::now();
            gpu.device
                .wait_for_fences(std::slice::from_ref(&self.fence), true, u64::MAX)
                .unwrap();
            let wait_ms = ms_since(t);
            gpu.device
                .reset_fences(std::slice::from_ref(&self.fence))
                .unwrap();
            (submit_ms, wait_ms)
        }
    }

    fn destroy(self, gpu: &Gpu) {
        unsafe {
            gpu.device.destroy_fence(self.fence, None);
            gpu.device.destroy_command_pool(self.pool, None);
            gpu.device.destroy_image(self.src, None);
            gpu.device.destroy_image(self.dst, None);
            gpu.device.free_memory(self.src_mem, None);
            gpu.device.free_memory(self.dst_mem, None);
        }
    }
}

/// K independent submits, one fence wait on the last. Tests "you pay for the first submit and
/// ride the same cycle for the rest" (§3.2) against "every submit is its own round trip" (§3.1).
fn batched_submits(gpu: &Gpu, rig: &FenceRig, k: usize) -> f64 {
    unsafe {
        gpu.device
            .reset_command_buffer(rig.cbuf, vk::CommandBufferResetFlags::empty())
            .unwrap();
        gpu.device
            .begin_command_buffer(rig.cbuf, &vk::CommandBufferBeginInfo::default())
            .unwrap();
        gpu.device.end_command_buffer(rig.cbuf).unwrap();

        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&rig.cbuf));
        let t = Instant::now();
        for i in 0..k {
            let fence = if i == k - 1 {
                rig.fence
            } else {
                vk::Fence::null()
            };
            gpu.device
                .queue_submit(gpu.queue, std::slice::from_ref(&submit), fence)
                .unwrap();
        }
        gpu.device
            .wait_for_fences(std::slice::from_ref(&rig.fence), true, u64::MAX)
            .unwrap();
        let total = ms_since(t);
        gpu.device
            .reset_fences(std::slice::from_ref(&rig.fence))
            .unwrap();
        total
    }
}

/// Sweep the idle gap in front of an *empty* submit, across venus's 1 ms ring-idle timeout
/// (`VN_RING_IDLE_TIMEOUT_NS`, `vn_ring.c:18`).
///
/// The host ring thread backs off exponentially while the ring is empty (`vkr_ring_relax`,
/// 16 yields then 10 µs doubling) and, once `idleTimeout` has passed, parks on a condition
/// variable that only a guest notify can signal. If that park/wake is what our "fixed round
/// trip" really is, the cost of an identical zero-work submit must **step up at ~1 ms of
/// preceding idle** and stay flat on either side of it.
///
/// Each gap is run two ways. `sleep` parks the guest thread too, so a step could be guest
/// scheduling or DVFS; `spin` keeps the guest thread hot on its core and changes nothing but
/// how long the host ring sat empty. A step that survives `spin` is host-side.
fn idle_gap_sweep(gpu: &Gpu, rig: &FenceRig, reps: usize) {
    const GAPS_US: &[u64] = &[0, 200, 600, 1000, 1400, 2000, 5000, 16700];

    println!("idle-gap sweep before an empty submit (venus parks the host ring at 1000 us idle)");
    println!("  NOTE: this guest also runs a 60 Hz desktop of its own, so the host GPU and the");
    println!("  host renderer are never truly quiet. `min` is the least-contended estimate and");
    println!("  is the column to read; the medians carry that background in them.");
    println!(
        "        gap        spin gap: min / median / p95        sleep gap: min / median / p95"
    );

    for &gap_us in GAPS_US {
        let run = |spin: bool| {
            let mut waits = Vec::with_capacity(reps);
            for _ in 0..reps {
                if gap_us > 0 {
                    let d = std::time::Duration::from_micros(gap_us);
                    if spin {
                        let until = Instant::now() + d;
                        while Instant::now() < until {
                            std::hint::spin_loop();
                        }
                    } else {
                        std::thread::sleep(d);
                    }
                }
                waits.push(rig.round(gpu, 0).1);
            }
            Summary::of(waits)
        };
        let spin = run(true);
        let sleep = run(false);
        println!(
            "  {gap_us:>6} us   {:>7.4} {:>8.4} {:>8.4}        {:>7.4} {:>8.4} {:>8.4}  (ms)",
            spin.min, spin.median, spin.p95, sleep.min, sleep.median, sleep.p95
        );
    }
    println!();
}

fn probe_fence(gpu: &Gpu) -> Result<(), Box<dyn std::error::Error>> {
    const N: usize = 120;
    const W: u32 = 3840;
    const H: u32 = 2160;
    println!("=== §8.4  what is inside a fence wait? ===\n");

    let rig = FenceRig::new(gpu, W, H)?;
    for _ in 0..5 {
        rig.round(gpu, 1);
    }

    println!("back-to-back submits, graded GPU work ({W}x{H} image copies, ~33 MiB each way)");
    let mut points = Vec::new();
    for &copies in &[0usize, 1, 2, 4, 8] {
        let mut submits = Vec::with_capacity(N);
        let mut waits = Vec::with_capacity(N);
        for _ in 0..N {
            let (s, w) = rig.round(gpu, copies);
            submits.push(s);
            waits.push(w);
        }
        let s = Summary::of(submits);
        let w = Summary::of(waits);
        println!("  {copies} copies  vkQueueSubmit  {s}");
        println!("  {copies} copies  fence wait     {w}");
        // Fit on `min`: the background desktop on this guest inflates medians
        // unpredictably, and the least-contended sample is the closest thing to a clean read.
        points.push((copies as f64, w.min));
    }

    // Least-squares fit over the graded points: intercept = the fixed cost of a submit + wait
    // with no GPU work in it, slope = milliseconds of GPU time per full-surface copy. That pair
    // is exactly what §3.5's timestamps were wanted for, obtained by differential wall clock.
    let n = points.len() as f64;
    let sx: f64 = points.iter().map(|p| p.0).sum();
    let sy: f64 = points.iter().map(|p| p.1).sum();
    let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    let intercept = (sy - slope * sx) / n;
    println!(
        "\n  fit over the min of each row: wait ≈ {intercept:.3} ms + {slope:.3} ms per copy\n       \
         intercept = fixed round-trip + retire cost, slope = real GPU time per {W}x{H} copy\n       \
         ({:.0} GB/s of copy traffic, for a sanity check against the device)\n",
        (W as f64 * H as f64 * 4.0 * 2.0) / (slope / 1000.0) / 1e9
    );

    println!("the same, but idle for 20 ms between rounds (an uncontended pipe)");
    let mut waits = Vec::with_capacity(40);
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        waits.push(rig.round(gpu, 1).1);
    }
    println!("  1 copy, sparse  fence wait     {}\n", Summary::of(waits));

    idle_gap_sweep(gpu, &rig, 120);

    println!("K empty submits, one wait on the last (is the cost per submit or per wait?)");
    for &k in &[1usize, 2, 4, 8, 16] {
        let mut totals = Vec::with_capacity(60);
        for _ in 0..60 {
            totals.push(batched_submits(gpu, &rig, k));
        }
        let s = Summary::of(totals);
        println!("  K={k:<3} submit+submit+…+wait  {s}");
    }
    println!();

    rig.destroy(gpu);
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let gpu = Gpu::new()?;

    if matches!(which.as_str(), "image" | "all") {
        probe_image(&gpu);
    }
    if matches!(which.as_str(), "memory" | "all") {
        probe_memory(&gpu)?;
    }
    if matches!(which.as_str(), "fence" | "all") {
        probe_fence(&gpu)?;
    }
    if which == "idle" {
        // The sweep on its own, with enough reps that `min` means something.
        let rig = FenceRig::new(&gpu, 3840, 2160)?;
        for _ in 0..5 {
            rig.round(&gpu, 1);
        }
        idle_gap_sweep(&gpu, &rig, 400);
        rig.destroy(&gpu);
    }

    unsafe {
        gpu.device.destroy_device(None);
        gpu.instance.destroy_instance(None);
    }
    Ok(())
}
