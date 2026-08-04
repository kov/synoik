// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Minimal reproducer: on this virtio-gpu/Venus VM, libgbm buffer allocation
//! accepts only a narrow set of flag/format combos. In particular the
//! GBM_BO_USE_WRITE usage flag makes allocation fail with EINVAL, while
//! create_buffer_object_with_modifiers2([LINEAR], empty_flags) succeeds.
//!
//! Build & run:  cargo run   (in this directory)
//! Device:       /dev/dri/renderD128 (render node, world-rw on this VM)

use std::fs::File;
use std::io;

use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};

const RENDER_NODE: &str = "/dev/dri/renderD128";

/// Print the outcome of one allocation attempt, and buffer facts when it succeeds.
fn report(label: &str, r: io::Result<BufferObject<()>>) {
    match r {
        Ok(bo) => println!(
            "OK    {label}  -> modifier={:?} stride={} planes={}",
            bo.modifier(),
            bo.stride(),
            bo.plane_count(),
        ),
        Err(e) => println!("ERR   {label}  -> {e:?} errno={:?}", e.raw_os_error()),
    }
}

fn main() -> io::Result<()> {
    let file = File::options().read(true).write(true).open(RENDER_NODE)?;
    let dev: Device<File> = Device::new(file)?;
    let (w, h, fmt) = (64u32, 64u32, Format::Argb8888);
    use BufferObjectFlags as F;

    report(
        "create_buffer_object ARGB8888 RENDERING|LINEAR",
        dev.create_buffer_object::<()>(w, h, fmt, F::RENDERING | F::LINEAR),
    );
    report(
        "create_buffer_object ARGB8888 LINEAR",
        dev.create_buffer_object::<()>(w, h, fmt, F::LINEAR),
    );
    report(
        "create_buffer_object ARGB8888 LINEAR|WRITE",
        dev.create_buffer_object::<()>(w, h, fmt, F::LINEAR | F::WRITE),
    );
    report(
        "modifiers2 ARGB8888 [LINEAR] empty()   (expected OK)",
        dev.create_buffer_object_with_modifiers2::<()>(
            w, h, fmt, [Modifier::Linear].into_iter(), F::empty(),
        ),
    );
    report(
        "modifiers2 ARGB8888 [LINEAR] WRITE     (expected FAIL)",
        dev.create_buffer_object_with_modifiers2::<()>(
            w, h, fmt, [Modifier::Linear].into_iter(), F::WRITE,
        ),
    );

    Ok(())
}
