// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Live `VkDeviceMemory` accounting, per allocation site.
//!
//! **Why this exists at all:** on this stack the memory that matters is not in our process. Venus
//! allocations — device-local images, mappable blobs, imported dmabufs — live in the **VMM's**
//! address space on the host, and appear in *no* guest accounting: not `VmRSS`, not the cgroup's
//! `memory.peak`. The guest also has free-page reporting, so ordinary guest anonymous memory is
//! handed back and a guest-side high-water mark says nothing about the host's. A 33 MB wallpaper
//! staged through [`crate::staging::HostStaging`] moves `VmRSS` by about 8 MB, and a leak of them
//! would move it by about 8 MB each while costing the host 33 MB each.
//!
//! `VK_EXT_memory_budget` would have answered this from the driver, but venus does not expose it
//! (checked 2026-08-07), so the only guest-side instrument is to count what we allocate ourselves.
//! That is what this module is: every `vkAllocateMemory` this crate performs is recorded against
//! the source location that asked for it, and every `vkFreeMemory` clears it. What survives is
//! ours.
//!
//! Reading the result: **flat live bytes while the host grows means the leak is not ours** — it is
//! the VMM or venus retaining across a guest-side free, which is not fixable from here (see
//! `docs/fork/explicit-sync.md` for the precedent). **Growing live bytes are ours**, and the site
//! label names the subsystem.
//!
//! Imported dmabufs are counted like any other allocation even though the import allocates no new
//! storage: an import that is never freed pins the exporter's buffer just as effectively as a leak
//! of our own, and it is exactly as invisible from outside.

use std::collections::HashMap;
use std::fmt;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ash::vk;

/// Which allocation site a live block belongs to.
///
/// `Explicit` for the handful of sites that call `vkAllocateMemory` directly and have a name worth
/// reading ("scanout-export" beats "texture.rs:654"); `Caller` for everything routed through
/// [`Gpu::allocate`](crate::gpu::Gpu::allocate), whose `#[track_caller]` gives the real requester
/// for free rather than attributing every one of them to one line inside `gpu.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Site {
    Explicit(&'static str),
    Caller(&'static Location<'static>),
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Site::Explicit(name) => f.write_str(name),
            Site::Caller(loc) => write!(f, "{}:{}", loc.file(), loc.line()),
        }
    }
}

struct Live {
    site: Site,
    bytes: u64,
}

static LIVE: Mutex<Option<HashMap<u64, Live>>> = Mutex::new(None);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_COUNT: AtomicU64 = AtomicU64::new(0);
/// Highest [`LIVE_BYTES`] ever reached. A leak is a high-water mark that never comes down, so the
/// peak is worth keeping next to the current value — a sample that happens to land after a cleanup
/// would otherwise read as healthy.
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

/// Record `memory` as live. Call immediately after a successful `vkAllocateMemory`.
pub fn track(memory: vk::DeviceMemory, bytes: u64, site: Site) {
    if memory == vk::DeviceMemory::null() {
        return;
    }
    let mut guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    // A handle the driver has reused after a free we failed to record would otherwise double-count.
    if let Some(stale) = map.insert(as_key(memory), Live { site, bytes }) {
        LIVE_BYTES.fetch_sub(stale.bytes, Ordering::Relaxed);
        LIVE_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
    let total = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    LIVE_COUNT.fetch_add(1, Ordering::Relaxed);
    PEAK_BYTES.fetch_max(total, Ordering::Relaxed);
}

/// Clear `memory`. Call immediately before or after `vkFreeMemory`; a handle that was never tracked
/// (or is null, which `vkFreeMemory` accepts) is ignored.
pub fn untrack(memory: vk::DeviceMemory) {
    if memory == vk::DeviceMemory::null() {
        return;
    }
    let mut guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = guard.as_mut() else {
        return;
    };
    if let Some(live) = map.remove(&as_key(memory)) {
        LIVE_BYTES.fetch_sub(live.bytes, Ordering::Relaxed);
        LIVE_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

fn as_key(memory: vk::DeviceMemory) -> u64 {
    use ash::vk::Handle as _;
    memory.as_raw()
}

/// Total live bytes, live block count, and the peak live bytes since process start.
pub fn totals() -> (u64, u64, u64) {
    (
        LIVE_BYTES.load(Ordering::Relaxed),
        LIVE_COUNT.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed),
    )
}

/// Live bytes and block count per site, largest first.
pub fn by_site() -> Vec<(String, u64, u64)> {
    let guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = guard.as_ref() else {
        return Vec::new();
    };
    let mut per: HashMap<Site, (u64, u64)> = HashMap::new();
    for live in map.values() {
        let entry = per.entry(live.site).or_default();
        entry.0 += live.bytes;
        entry.1 += 1;
    }
    drop(guard);
    let mut out: Vec<_> = per
        .into_iter()
        .map(|(site, (bytes, count))| (site.to_string(), bytes, count))
        .collect();
    out.sort_by_key(|(_, bytes, _)| std::cmp::Reverse(*bytes));
    out
}

/// One-line summary plus the biggest sites, for a periodic log.
pub fn report(top: usize) -> String {
    let (bytes, count, peak) = totals();
    let mut out = format!(
        "device memory live {:.1} MiB in {count} blocks (peak {:.1} MiB)",
        mib(bytes),
        mib(peak),
    );
    for (site, site_bytes, site_count) in by_site().into_iter().take(top) {
        out.push_str(&format!(
            "\n    {:>9.1} MiB  {site_count:>4}  {site}",
            mib(site_bytes)
        ));
    }
    out
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024. * 1024.)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters are process-global, so two of these running at once would read each other's
    /// blocks into their own deltas — a parallel test binary makes that a flake, not a maybe.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn handle(raw: u64) -> vk::DeviceMemory {
        use ash::vk::Handle as _;
        vk::DeviceMemory::from_raw(raw)
    }

    /// The whole point of the instrument: what is freed must stop counting, and what is not must
    /// keep counting. A leak is the difference between the two.
    #[test]
    fn a_freed_block_stops_counting_and_a_leaked_one_does_not() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let base = totals().0;
        track(handle(0x1001), 1024, Site::Explicit("test-a"));
        track(handle(0x1002), 2048, Site::Explicit("test-b"));
        assert_eq!(totals().0, base + 3072);

        untrack(handle(0x1001));
        assert_eq!(
            totals().0,
            base + 2048,
            "the freed block must stop counting"
        );

        // The peak keeps the high-water mark a later free would otherwise hide.
        assert!(totals().2 >= base + 3072);

        untrack(handle(0x1002));
        assert_eq!(totals().0, base);
    }

    /// The driver reuses handles after a free. If a free ever goes unrecorded, the next allocation
    /// on that handle must not double-count — the counter has to stay usable across our own bugs.
    #[test]
    fn a_reused_handle_replaces_rather_than_doubles() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let base = totals().0;
        track(handle(0x2001), 4096, Site::Explicit("test-c"));
        // Same handle again with no intervening untrack, as a missed free would produce.
        track(handle(0x2001), 4096, Site::Explicit("test-c"));
        assert_eq!(
            totals().0,
            base + 4096,
            "a reused handle must replace its predecessor, not add to it"
        );
        untrack(handle(0x2001));
        assert_eq!(totals().0, base);
    }

    #[test]
    fn untracking_something_never_tracked_is_harmless() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = totals();
        untrack(handle(0xdead));
        untrack(vk::DeviceMemory::null());
        assert_eq!(totals().0, before.0);
        assert_eq!(totals().1, before.1);
    }
}
