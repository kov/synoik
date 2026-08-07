// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! What [`Swapchain::reset_buffer_ages`] is allowed to do to a slot.
//!
//! This is the one thing the tty backend asks smithay for every frame when full-damage rendering is
//! on (`backend::tty`, `force_full_damage`), and the version of it we inherited answered "the slot
//! is shared, I cannot take `&mut`" by *replacing* the slot with a default one — dropping its
//! buffer and its userdata, which is where `Slot::export` caches the exported `Dmabuf`. A slot in
//! flight is always shared, so that was every slot, every frame: a fresh 4K scanout allocation and
//! a fresh dmabuf identity per frame, ~3.9 GB/s of allocation handed to a virtualized GPU. It took
//! the host VM's memory from 10 GB to 50 GB in a minute and got the VM OOM-killed. Fixed in our
//! smithay fork by storing through the `Arc` (`age` is an `AtomicU8` for exactly this).
//!
//! It lives here, and not near the code that calls it, because the caller is the **tty** backend:
//! there is no `DrmCompositor` and no KMS in the headless harness, so the call site itself cannot
//! be reached from a test. `Swapchain` is public and generic over `Allocator`, though, so the
//! invariant can be pinned directly against the dependency version this workspace actually builds —
//! no GPU, no device, no KMS. The `EXPORTS` counter in `backend::vulkan_scanout` observes the same
//! invariant on a live seat (a re-export line in the journal means a slot died), but it needs a
//! real device, so it stays a diagnostic and this is the pin.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use smithay::backend::allocator::{Allocator, Buffer, Format, Fourcc, Modifier, Swapchain};
use smithay::utils::{Buffer as BufferCoords, Size};

/// A buffer that is nothing but a size and a format — enough for `Swapchain`, which never looks
/// inside one.
#[derive(Debug)]
struct CountedBuffer {
    size: Size<i32, BufferCoords>,
    format: Format,
}

impl Buffer for CountedBuffer {
    fn size(&self) -> Size<i32, BufferCoords> {
        self.size
    }

    fn format(&self) -> Format {
        self.format
    }
}

/// `Allocator::Error` has to be an `std::error::Error`; this one is never constructed.
#[derive(Debug)]
struct Never;

impl std::fmt::Display for Never {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the counting allocator cannot fail")
    }
}

impl std::error::Error for Never {}

/// Counts allocations, which is the whole measurement: a swapchain that recycles its slots
/// allocates once per slot and never again.
#[derive(Default)]
struct CountingAllocator {
    created: Arc<AtomicUsize>,
}

impl Allocator for CountingAllocator {
    type Buffer = CountedBuffer;
    type Error = Never;

    fn create_buffer(
        &mut self,
        width: u32,
        height: u32,
        fourcc: Fourcc,
        _modifiers: &[Modifier],
    ) -> Result<Self::Buffer, Self::Error> {
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(CountedBuffer {
            size: Size::from((width as i32, height as i32)),
            format: Format {
                code: fourcc,
                modifier: Modifier::Linear,
            },
        })
    }
}

/// smithay's `SLOT_CAP`, which its `swapchain` module does not re-export. A swapchain never holds
/// more than this many buffers, so it is the ceiling any number of frames must stay under.
const SLOT_CAP: usize = 4;

/// Marker stashed in slot userdata, standing in for the cached `Dmabuf` that the real scanout path
/// keeps there. If it survives, the slot survived.
#[derive(Debug, PartialEq)]
struct Marker(u32);

fn swapchain() -> (Swapchain<CountingAllocator>, Arc<AtomicUsize>) {
    let allocator = CountingAllocator::default();
    let created = allocator.created.clone();
    let swapchain = Swapchain::new(
        allocator,
        3840,
        2160,
        Fourcc::Argb8888,
        vec![Modifier::Linear],
    );
    (swapchain, created)
}

/// The headline invariant: **N full-damage frames must not mean N buffer allocations.**
///
/// This is the tty backend's per-frame loop with `force_full_damage` on, in miniature — acquire,
/// reset ages, submit, release — and the whole bug is that it used to allocate once per frame
/// forever. A swapchain recycles a fixed set of slots (`SLOT_CAP`), so the count must stop there no
/// matter how long the session runs.
///
/// Note that the reset happens while the slot is *held*, which is what makes the internal `Arc`
/// shared and is the case the old implementation answered by replacing the slot. The damage does
/// not show on the handle you are holding — that still points at the old `Arc` — it shows on the
/// *next acquire*, which finds a slot with no buffer and allocates again. Asserting on the held
/// handle would pass against the broken code.
#[test]
fn repeated_full_damage_frames_do_not_keep_allocating() {
    let (mut swapchain, created) = swapchain();

    for _ in 0..10 {
        let slot = swapchain.acquire().unwrap().expect("a free slot");
        swapchain.reset_buffer_ages();
        swapchain.submitted(&slot);
    }
    // Deliberately nothing about age in the loop: on the broken implementation the reset has
    // already removed this slot from the swapchain, so `submitted` no-ops and an age assertion
    // fires first — failing for a true but much less legible reason than the count below.

    let allocated = created.load(Ordering::SeqCst);
    assert!(
        allocated <= SLOT_CAP,
        "10 full-damage frames allocated {allocated} buffers; a swapchain recycles at most \
         {SLOT_CAP}, so this is one allocation per frame for as long as the session lasts",
    );
}

/// And the slot has to still be *the same slot* on the next round trip, or the caller re-exports
/// and every downstream cache keyed on buffer identity misses.
#[test]
fn a_slot_survives_a_reset_across_submit_and_reacquire() {
    let (mut swapchain, created) = swapchain();

    let slot = swapchain.acquire().unwrap().expect("a free slot");
    slot.userdata().insert_if_missing(|| Marker(11));
    swapchain.reset_buffer_ages();
    swapchain.submitted(&slot);
    drop(slot);

    let slot = swapchain.acquire().unwrap().expect("the same free slot");
    assert_eq!(
        slot.userdata().get::<Marker>(),
        Some(&Marker(11)),
        "re-acquiring must hand back the slot that was reset, not a fresh one",
    );
    assert_eq!(
        created.load(Ordering::SeqCst),
        1,
        "the round trip must not have allocated a second buffer",
    );
}

/// The reset must not stop ageing from working afterwards — a full-damage frame is still a frame,
/// and the next submit has to advance the ages the damage tracker reads.
#[test]
fn ages_still_advance_after_a_reset() {
    let (mut swapchain, _created) = swapchain();

    // Both held at once, or `acquire` just hands the same slot back each time and there is only
    // ever one age to look at.
    let presented = swapchain.acquire().unwrap().expect("a free slot");
    let idle = swapchain.acquire().unwrap().expect("a second free slot");

    swapchain.submitted(&presented);
    assert_eq!(
        presented.age(),
        1,
        "the just-submitted slot is one frame old"
    );

    swapchain.reset_buffer_ages();
    assert_eq!(presented.age(), 0);
    assert_eq!(idle.age(), 0);

    // Ageing still works afterwards, which is what makes "reset, then render" a full redraw rather
    // than a permanently confused swapchain: the submitted slot becomes one frame old, and the one
    // that was reset stays unknown because `submitted` only advances ages that are already
    // non-zero.
    swapchain.submitted(&presented);
    assert_eq!(
        presented.age(),
        1,
        "ageing must still advance after a reset"
    );
    assert_eq!(
        idle.age(),
        0,
        "a slot whose age was reset must still read as unknown-contents",
    );
}
