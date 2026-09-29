// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! How synoik's own memory behaves under pressure: what the allocator hands back, and (see
//! [`configure_allocator`]) what it keeps.

/// Allocations at least this big get a mapping of their own, returned to the kernel on free.
const MMAP_THRESHOLD: usize = 4 << 20;

/// Pin glibc's mmap threshold, so a big buffer never lands in — and stays in — a thread arena.
///
/// glibc starts at 128 KiB and **raises the threshold to the size of any mmapped block that is
/// freed**, up to 32 MiB — so a buffer of a given size is mapped the first time and served from
/// the thread's arena every time after. The wallpaper worker allocates 15 MiB buffers for the
/// slideshow cross-fade's resize (`image`'s `resize_exact`) on every fade. Freed, they stay
/// committed in its arena, which only gives memory back from its top: it sat at 48 MiB committed,
/// all of it free, all of it swapped out — a third of synoik's swap on the seat (measured
/// 2026-09-29, the allocation stacks traced with bpftrace).
///
/// Setting the threshold explicitly turns the dynamic adjustment off (and with it the dynamic trim
/// threshold, which stays at its 128 KiB default), so from here on a buffer this big is mapped on
/// allocation and unmapped on free, whichever thread asks. Nothing on the frame path allocates this
/// much — a frame that did would be allocating per frame, which the renderer already forbids — so
/// the extra `mmap`/`munmap` per big buffer lands on workers doing tens of milliseconds of work
/// each.
///
/// Call it first thing in `main`, before any thread exists.
pub fn configure_allocator() {
    // SAFETY: `mallopt` only sets allocator parameters; it is safe to call at any time, and is
    // called here before any other thread could be allocating.
    let ok = unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD as libc::c_int) };
    if ok != 1 {
        warn!("could not pin the malloc mmap threshold");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether glibc served `ptr` with a mapping of its own: the `IS_MMAPPED` bit of the chunk's
    /// size field, the word just below the pointer. Asking the chunk, not `/proc/self/maps`,
    /// because in a parallel test binary another thread can map the freed range again before the
    /// check runs.
    fn is_mmapped(ptr: *const u8) -> bool {
        // SAFETY: `ptr` is a live allocation from glibc's malloc, whose chunk header — the size
        // word, with its flag bits — sits immediately before the pointer it returns.
        let size_word = unsafe { ptr.cast::<usize>().sub(1).read() };
        size_word & 0x2 != 0
    }

    /// The shape of the wallpaper worker's leak: a thread allocates the same big buffer twice.
    /// Unpinned, freeing the first raised glibc's threshold to its size, so the second is served
    /// from the thread's arena — and stays committed there after it is freed. Pinned, both are
    /// mappings of their own, unmapped the moment they are freed.
    ///
    /// It runs in a child process of its own. An arena serves a request from any free chunk it
    /// already holds before it looks at the threshold, and arenas are shared between threads, so
    /// in the test binary another test's leftovers can hand back 12 MiB whatever the threshold
    /// says. A fresh process has no leftovers.
    #[test]
    fn a_big_buffer_is_mapped_on_its_own_every_time() {
        const CHILD: &str = "SYNOIK_MALLOC_THRESHOLD_PROBE";
        if std::env::var_os(CHILD).is_some() {
            configure_allocator();
            std::thread::spawn(|| {
                drop(std::hint::black_box(vec![1u8; 12 << 20]));
                let buffer = std::hint::black_box(vec![1u8; 12 << 20]);
                assert!(
                    is_mmapped(buffer.as_ptr()),
                    "a 12 MiB buffer came from a thread arena, where it stays committed after free"
                );
            })
            .join()
            .expect("the probe thread");
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                "utils::memory::tests::a_big_buffer_is_mapped_on_its_own_every_time",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run the probe in a child process");
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
