// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! System-compositor spike, sender half: allocate a scanout image on our Vulkan device exactly as
//! the tty backend does (`Texture::allocate_scanout`), clear it to a colour, and hand its dmabuf to
//! another process over a unix socket (SCM_RIGHTS). The receiver (`xproc_scanout_recv.c`) holds
//! DRM master and flips it. Re-clears to a new colour every second for `secs`, then exits while
//! the receiver keeps scanning the buffer out.
//!
//! Usage: xproc_scanout_send <socket> <width> <height> <secs>

use std::io::IoSlice;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result};
use ash::vk;
use synoik_vk::gpu::Gpu;
use synoik_vk::stats::SubmitSite;
use synoik_vk::texture::Texture;

fn clear(
    gpu: &Gpu,
    pool: vk::CommandPool,
    image: vk::Image,
    first: bool,
    rgb: [f32; 3],
) -> Result<()> {
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    gpu.run_commands(pool, SubmitSite::Transition, |cb| unsafe {
        // Acquire from the foreign queue (KMS) on every frame but the first, release back after.
        let to_dst = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(if first {
                vk::QUEUE_FAMILY_IGNORED
            } else {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            })
            .dst_queue_family_index(if first {
                vk::QUEUE_FAMILY_IGNORED
            } else {
                gpu.queue_family
            })
            .image(image)
            .subresource_range(range);
        gpu.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        );
        let color = vk::ClearColorValue {
            float32: [rgb[0], rgb[1], rgb[2], 1.0],
        };
        gpu.device.cmd_clear_color_image(
            cb,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &color,
            &[range],
        );
        let release = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(gpu.queue_family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(image)
            .subresource_range(range);
        gpu.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[release],
        );
    })
}

fn send_fd(sock: &UnixStream, fd: i32, header: &[u8]) -> Result<()> {
    let iov = [IoSlice::new(header)];
    let mut cmsg = vec![0u8; unsafe { libc::CMSG_SPACE(4) } as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len();
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd);
        anyhow::ensure!(
            libc::sendmsg(sock.as_raw_fd(), &msg, 0) >= 0,
            "sendmsg: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "usage: {} <socket> <width> <height> <secs>",
        args[0]
    );
    let (w, h, secs): (u32, u32, u32) = (args[2].parse()?, args[3].parse()?, args[4].parse()?);

    let gpu = Gpu::new()?;
    eprintln!("sender: device {}", gpu.device_name);
    let (tex, export) = Texture::allocate_scanout(
        &gpu,
        w,
        h,
        vk::Format::B8G8R8A8_UNORM,
        &[0], // DRM_FORMAT_MOD_LINEAR
        vk::FormatFeatureFlags::TRANSFER_DST,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        vk::Filter::LINEAR,
    )?;
    eprintln!(
        "sender: allocated {w}x{h} modifier={:#x} offset={} stride={}",
        export.modifier, export.offset, export.stride
    );
    let pool = unsafe {
        gpu.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(gpu.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }?;
    clear(&gpu, pool, tex.image, true, [1.0, 0.0, 0.0])?;

    let sock = UnixStream::connect(&args[1]).context("connecting to the receiver")?;
    // Header: width, height, stride, offset (u32 LE) + modifier (u64 LE).
    let mut header = Vec::new();
    for v in [w, h, export.stride, export.offset] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    header.extend_from_slice(&export.modifier.to_le_bytes());
    send_fd(&sock, export.fd.as_raw_fd(), &header)?;
    eprintln!("sender: dmabuf sent");

    let colours = [
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [1.0, 0.0, 0.0],
    ];
    for i in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        clear(
            &gpu,
            pool,
            tex.image,
            false,
            colours[i as usize % colours.len()],
        )?;
        eprintln!("sender: cleared to colour {}", i % colours.len() as u32);
    }
    eprintln!("sender: exiting (receiver keeps the buffer)");
    Ok(())
}
