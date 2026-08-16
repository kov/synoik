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
//! `VK_EXT_memory_budget` answers a *different* question, and it is available after all. The note
//! here used to say venus does not expose it (checked 2026-08-07); it is **gated on
//! `VN_DEBUG=mem_budget`**, which the enhanced-tier environment now sets, and with that set
//! `vulkaninfo` lists it (verified 2026-08-16 against a control extension, so the absence was the
//! gate and not the driver). What it reports is the **host's** view: `heapBudget` carries the VMM's
//! per-context GPU-memory cap, which is the one backpressure channel the venus transport does not
//! throw away — over the cap the host kills the context rather than returning an error, because an
//! async `vkAllocateMemory` has already returned `VK_SUCCESS`.
//!
//! So the two are complements, not substitutes, and this module is still the only thing that can
//! attribute: every `vkAllocateMemory` this crate performs is recorded against the source location
//! that asked for it, and every `vkFreeMemory` clears it. What survives is ours. Reading
//! `heapBudget` alongside it — so we can see the cap approaching instead of discovering it as a
//! dead context — is open work, tracked in `docs/fork/foundation.md` §6.
//!
//! Two different questions, and the census answers both because **live bytes alone cannot tell them
//! apart**:
//!
//! - *Is something holding memory?* Growing live bytes. The site label names the subsystem.
//! - *Is something cycling memory?* Flat live bytes, large per-census churn. This is the one that
//!   actually took the host down on 2026-08-06: a per-frame `reset_buffer_ages()` reallocated the
//!   whole 4K swapchain every frame, ~3.9 GB/s into the VMM, while the live set never exceeded four
//!   buffers. Every guest-side instrument pointed at *retention* read as perfectly healthy
//!   throughout. Churn is cumulative allocation, reported as a delta per census.
//!
//! And if neither moves while the host still grows, the retention is **not ours** — it is the VMM
//! or venus holding on across a guest-side free, which is not fixable from here (see
//! `docs/fork/explicit-sync.md` for the precedent).
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

/// What a site has allocated over the whole run, and what it had allocated when the census last
/// looked. The difference between the two is the only thing that distinguishes a subsystem sitting
/// on memory from one *cycling* it.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Churn {
    allocs: u64,
    bytes: u64,
}

impl std::ops::Sub for Churn {
    type Output = Churn;

    fn sub(self, rhs: Churn) -> Churn {
        Churn {
            allocs: self.allocs.saturating_sub(rhs.allocs),
            bytes: self.bytes.saturating_sub(rhs.bytes),
        }
    }
}

#[derive(Default)]
struct State {
    /// Live blocks by `VkDeviceMemory` handle.
    live: HashMap<u64, Live>,
    /// Cumulative allocation per site, never decremented.
    churn: HashMap<Site, Churn>,
    /// [`Self::churn`] as of the previous [`census`], so each census can report a delta.
    reported: HashMap<Site, Churn>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
/// Highest [`LIVE_BYTES`] ever reached. A leak is a high-water mark that never comes down, so the
/// peak is worth keeping next to the current value — a sample that happens to land after a cleanup
/// would otherwise read as healthy.
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

/// Record `memory` as live. Call immediately after a successful `vkAllocateMemory`.
pub fn track(memory: vk::DeviceMemory, bytes: u64, site: Site) {
    if memory == vk::DeviceMemory::null() {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);
    // A handle the driver has reused after a free we failed to record would otherwise double-count.
    if let Some(stale) = state.live.insert(as_key(memory), Live { site, bytes }) {
        LIVE_BYTES.fetch_sub(stale.bytes, Ordering::Relaxed);
        LIVE_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
    let churn = state.churn.entry(site).or_default();
    churn.allocs += 1;
    churn.bytes += bytes;
    drop(guard);

    let total = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    LIVE_COUNT.fetch_add(1, Ordering::Relaxed);
    PEAK_BYTES.fetch_max(total, Ordering::Relaxed);
    TOTAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
    TOTAL_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Clear `memory`. Call immediately before or after `vkFreeMemory`; a handle that was never tracked
/// (or is null, which `vkFreeMemory` accepts) is ignored.
pub fn untrack(memory: vk::DeviceMemory) {
    if memory == vk::DeviceMemory::null() {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    if let Some(live) = state.live.remove(&as_key(memory)) {
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

/// Every allocation this process has made: count and bytes, never decremented.
///
/// Live bytes answer "is something holding memory". These answer "is something *cycling* it", which
/// is a different failure and the one that actually took the host down — a path that allocates and
/// frees a 4K image every frame reads as perfectly flat on the live counters while handing the VMM
/// gigabytes a second.
pub fn churn_totals() -> (u64, u64) {
    (
        TOTAL_ALLOCS.load(Ordering::Relaxed),
        TOTAL_BYTES.load(Ordering::Relaxed),
    )
}

/// One row of the census: a site, what it holds now, and what it has allocated since the last one.
pub struct SiteRow {
    pub site: String,
    pub live_bytes: u64,
    pub live_count: u64,
    /// Allocations by this site since the previous [`census`] — the churn rate, given a census on
    /// a fixed timer.
    pub new_allocs: u64,
    pub new_bytes: u64,
}

/// The periodic census: live totals, churn since the last call, and the busiest sites.
///
/// **Advances the delta window**, so it must have exactly one caller (the timer in `synoik`). A
/// second caller would silently halve everyone's deltas — which is why there is no non-mutating
/// variant to reach for by mistake; use [`totals`] and [`churn_totals`] for a one-off look.
///
/// Rows are ranked by churn first and size second: a site cycling 4 GB/s while holding 32 MB is the
/// interesting one, and ranking by live bytes alone would bury it.
pub fn census(top: usize) -> String {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);

    let mut rows: HashMap<Site, SiteRow> = HashMap::new();
    for live in state.live.values() {
        let row = rows.entry(live.site).or_insert_with(|| SiteRow {
            site: live.site.to_string(),
            live_bytes: 0,
            live_count: 0,
            new_allocs: 0,
            new_bytes: 0,
        });
        row.live_bytes += live.bytes;
        row.live_count += 1;
    }

    let mut delta = Churn::default();
    for (site, churn) in &state.churn {
        let since = *churn - state.reported.get(site).copied().unwrap_or_default();
        delta.allocs += since.allocs;
        delta.bytes += since.bytes;
        if since == Churn::default() {
            continue;
        }
        // A site can churn while holding nothing live, which is exactly the case worth seeing.
        let row = rows.entry(*site).or_insert_with(|| SiteRow {
            site: site.to_string(),
            live_bytes: 0,
            live_count: 0,
            new_allocs: 0,
            new_bytes: 0,
        });
        row.new_allocs = since.allocs;
        row.new_bytes = since.bytes;
    }
    state.reported = state.churn.clone();
    drop(guard);

    let (bytes, count, peak) = totals();
    let (all_allocs, all_bytes) = churn_totals();
    let mut out = format!(
        "device memory live {:.1} MiB in {count} blocks (peak {:.1} MiB); \
         since last census +{} allocs / {:.1} MiB; total {all_allocs} allocs / {:.1} MiB",
        mib(bytes),
        mib(peak),
        delta.allocs,
        mib(delta.bytes),
        mib(all_bytes),
    );

    let mut rows: Vec<SiteRow> = rows.into_values().collect();
    rows.sort_by_key(|row| {
        (
            std::cmp::Reverse(row.new_bytes),
            std::cmp::Reverse(row.live_bytes),
        )
    });
    for row in rows.into_iter().take(top) {
        out.push_str(&format!(
            "\n    live {:>9.1} MiB {:>4}   new {:>9.1} MiB {:>5}   {}",
            mib(row.live_bytes),
            row.live_count,
            mib(row.new_bytes),
            row.new_allocs,
            row.site,
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

    /// The failure this module exists to catch, and the reason live bytes are not enough: a site
    /// that allocates and frees at the same rate holds nothing and costs the host everything.
    #[test]
    fn churn_is_visible_even_though_live_bytes_never_move() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let site = Site::Explicit("test-churn");
        census(0); // open a clean delta window

        let (base_allocs, base_bytes) = churn_totals();
        let live_before = totals().0;

        // Ten frames of allocate-then-free, exactly like the swapchain bug.
        for i in 0..10 {
            let handle = handle(0x3000 + i);
            track(handle, 1 << 20, site);
            untrack(handle);
        }

        assert_eq!(
            totals().0,
            live_before,
            "live bytes must be unchanged — that is what makes this invisible without churn",
        );
        let (allocs, bytes) = churn_totals();
        assert_eq!(allocs - base_allocs, 10);
        assert_eq!(bytes - base_bytes, 10 << 20);

        let line = census(8);
        assert!(
            line.contains("+10 allocs"),
            "the census must report the churn it just saw, got: {line}",
        );
        assert!(
            line.contains("test-churn"),
            "and must name the site that did it, got: {line}",
        );
    }

    /// The delta is per-census, not cumulative, or a rate cannot be read off it.
    #[test]
    fn the_delta_window_closes_at_each_census() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let site = Site::Explicit("test-window");
        census(0);

        let handle = handle(0x4001);
        track(handle, 4096, site);
        untrack(handle);
        assert!(census(8).contains("+1 allocs"));

        // Nothing happened since; the same allocation must not be counted twice.
        assert!(
            census(8).contains("+0 allocs"),
            "a quiet interval must report zero, not repeat the last one",
        );
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
