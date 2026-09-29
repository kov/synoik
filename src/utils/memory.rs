// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! How synoik's own memory behaves under pressure: what the allocator gives back to the kernel
//! ([`configure_allocator`]) and what stays out of swap ([`MemoryLocker`]).

use std::ops::Range;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

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

/// How much of synoik the locker may pin, counted the way the kernel counts it: the whole size of
/// every range locked, touched or not.
const LOCK_BUDGET: u64 = 768 << 20;

/// How often the locker looks for memory it has not pinned yet.
pub const LOCK_RESCAN: Duration = Duration::from_secs(5);

/// Keeps synoik's own memory out of swap: its heap, its anonymous mappings and its own code.
///
/// A compositor that has been swapped out pays for it on the next frame — every page it touches is
/// a disk read, and after a suspend or under pressure that was seconds of a frozen screen. So the
/// memory synoik owns is locked with `MLOCK_ONFAULT`, which pins every page once it is touched, and
/// then populated for reading, which brings back what is already in swap without giving an
/// untouched page any memory (it maps the zero page).
///
/// Targeted, not `mlockall`: GPU mappings, shared memory from clients and every library but synoik
/// itself stay swappable (libLLVM alone is more code than synoik has memory). New mappings appear
/// all the time — arenas grow, threads start — and the kernel says nothing when they do, so a
/// worker rescans on a timer and before a suspend, locking only what an earlier lock does not
/// cover.
///
/// The budget is `RLIMIT_MEMLOCK` itself: the soft limit is set to [`LOCK_BUDGET`] (or the hard
/// limit, if lower) and the kernel refuses a lock past it. Spawned clients inherit that soft limit;
/// it only lets them lock what they could already have asked for.
///
/// A locked page cannot be given back with `MADV_DONTNEED`, which is how glibc trims a thread
/// arena, so freed arena memory stays pinned up to its peak. That is what [`configure_allocator`]
/// is for: nothing big is ever in an arena.
pub struct MemoryLocker {
    scans: Sender<()>,
}

impl MemoryLocker {
    /// Start the worker. With the default hard limit of 8 MiB, synoik's own code alone spends the
    /// budget, so a session needs `LimitMEMLOCK=` on its unit for this to do much.
    pub fn start() -> Option<Self> {
        let budget = set_memlock_limit(LOCK_BUDGET);
        let (scans, requests) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("memory-locker".to_owned())
            .spawn(move || {
                let mut locker = Locker::new(budget);
                // Ends when the sender (held by `Synoik`) is dropped.
                for () in requests {
                    locker.scan();
                }
            });
        match spawned {
            Ok(_) => Some(Self { scans }),
            Err(err) => {
                warn!("could not spawn the memory locker: {err}; synoik stays swappable");
                None
            }
        }
    }

    /// Ask for a scan. It runs on the worker; the caller does not wait for it.
    pub fn rescan(&self) {
        let _ = self.scans.send(());
    }
}

/// Set the soft `RLIMIT_MEMLOCK` to `want`, or to the hard limit if that is lower, and return the
/// soft limit in force.
fn set_memlock_limit(want: u64) -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-pointer for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
        return 0;
    }
    let soft = want.min(limit.rlim_max);
    if soft != limit.rlim_cur {
        let set = libc::rlimit {
            rlim_cur: soft,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: `set` is a valid rlimit, its soft limit within its hard one.
        if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &set) } == 0 {
            limit.rlim_cur = soft;
        }
    }
    limit.rlim_cur
}

/// The worker's state between scans.
struct Locker {
    budget: u64,
    /// The mappings locked so far, as the last scan saw them.
    locked: Vec<Range<u64>>,
    over_budget_logged: bool,
}

impl Locker {
    fn new(budget: u64) -> Self {
        Self {
            budget,
            locked: Vec::new(),
            over_budget_logged: false,
        }
    }

    fn scan(&mut self) {
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return;
        };
        // Read every scan: once the installed binary is replaced, the kernel names it
        // "… (deleted)", here and in the maps alike.
        let exe = std::fs::read_link("/proc/self/exe").ok();
        let exe = exe.as_deref().and_then(Path::to_str);
        let plan = plan_locks(&maps, exe, &self.locked);
        self.locked = plan.covered;
        let mut refused = 0;
        for range in plan.lock {
            match lock_range(range.clone()) {
                Ok(()) => {
                    populate(range.clone());
                    self.locked.push(range);
                }
                Err(err) if err.raw_os_error() == Some(libc::ENOMEM) => {
                    // The budget, or a range unmapped since the maps were read; the next scan
                    // sees the truth either way.
                    refused += range.end - range.start;
                }
                Err(err) => debug!("could not lock {range:x?}: {err}"),
            }
        }
        if refused > 0 && !self.over_budget_logged {
            self.over_budget_logged = true;
            warn!(
                "the memory lock budget of {} MiB is spent; {} MiB of synoik stays swappable",
                self.budget >> 20,
                refused >> 20,
            );
        }
    }
}

/// What a scan will do.
#[derive(Debug, Default, PartialEq)]
struct LockPlan {
    /// Mappings to lock now.
    lock: Vec<Range<u64>>,
    /// Mappings an earlier lock already covers.
    covered: Vec<Range<u64>>,
}

/// Sort the lockable mappings of `maps` (the text of `/proc/self/maps`) into those inside a range
/// of `locked` and those to lock.
///
/// Inside, not equal to: a locked mapping split in two (an `munmap` or `mprotect` in its middle)
/// keeps its lock in both halves, while one that grew is a new, unlocked mapping until it is
/// locked — whereupon the kernel merges it with its locked neighbour, and the merged mapping is
/// wider than anything recorded. Locking that again is cheap and correct: the kernel does not count
/// the pages already locked twice.
fn plan_locks(maps: &str, exe: Option<&str>, locked: &[Range<u64>]) -> LockPlan {
    let mut plan = LockPlan::default();
    for range in maps.lines().filter_map(|line| lockable(line, exe)) {
        let covered = locked
            .iter()
            .any(|l| l.start <= range.start && range.end <= l.end);
        if covered {
            plan.covered.push(range);
        } else {
            plan.lock.push(range);
        }
    }
    plan
}

/// The range a `/proc/self/maps` line describes, if it is synoik's own: private, accessible, and
/// either anonymous and writable (the heap, a thread's stack or arena, an anonymous mapping) or
/// part of synoik's executable.
fn lockable(line: &str, exe: Option<&str>) -> Option<Range<u64>> {
    let mut fields = line.splitn(6, ' ');
    let (start, end) = fields.next()?.split_once('-')?;
    let perms = fields.next()?.as_bytes();
    let path = fields.nth(3).map_or("", str::trim_start);
    if perms.len() != 4 || perms[3] != b'p' || perms[..3] == *b"---" {
        return None;
    }
    let anonymous =
        path.is_empty() || path == "[heap]" || path == "[stack]" || path.starts_with("[anon:");
    let ours = anonymous && perms[..2] == *b"rw" || exe.is_some_and(|exe| path == exe);
    if !ours {
        return None;
    }
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    (start < end).then_some(start..end)
}

/// Lock `range` on fault: nothing is read in by the lock, and every page is pinned once touched.
fn lock_range(range: Range<u64>) -> std::io::Result<()> {
    let len = (range.end - range.start) as usize;
    // SAFETY: `mlock2` only changes how the kernel treats the pages of the range; it never reads
    // or writes them, and a range that is no longer mapped is an error, not undefined behavior.
    let ret = unsafe { libc::mlock2(range.start as *const libc::c_void, len, libc::MLOCK_ONFAULT) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Fault `range` in for reading: a page in swap comes back (and, locked, stays), a page of code
/// that was dropped is read in again, and a page never touched maps the shared zero page rather
/// than memory of its own — which a plain `mlock` would give every page of every thread's stack.
fn populate(range: Range<u64>) {
    let len = (range.end - range.start) as usize;
    // SAFETY: populating only faults pages in as a read would; it changes no contents, and a range
    // that is no longer mapped is an error.
    let ret = unsafe {
        libc::madvise(
            range.start as *mut libc::c_void,
            len,
            libc::MADV_POPULATE_READ,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        debug!("could not populate {range:x?}: {err}");
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

    const EXE: &str = "/usr/local/bin/synoik";

    /// A `/proc/self/maps` excerpt with one of everything the locker has to tell apart.
    const MAPS: &str = "\
55d0a0000000-55d0a1000000 r--p 00000000 fd:00 1234 /usr/local/bin/synoik
55d0a1000000-55d0a2000000 r-xp 01000000 fd:00 1234 /usr/local/bin/synoik
55d0a2000000-55d0a2100000 rw-p 02000000 fd:00 1234 /usr/local/bin/synoik
55d0b0000000-55d0b4000000 rw-p 00000000 00:00 0                          [heap]
7f0000000000-7f0000400000 rw-p 00000000 00:00 0 
7f0000400000-7f0004000000 ---p 00000000 00:00 0 
7f1000000000-7f1000800000 rw-s 00000000 00:06 42                         /dev/dri/renderD128
7f1100000000-7f1100100000 rw-s 00000000 00:01 77                         /memfd:wayland-shm (deleted)
7f1200000000-7f1200100000 r-xp 00000000 fd:00 99                         /usr/lib64/libLLVM.so.20
7f1200100000-7f1200200000 rw-p 00100000 fd:00 99                         /usr/lib64/libLLVM.so.20
7f1300000000-7f1300010000 rw-p 00000000 00:00 0                          [anon:glibc.malloc]
7ffd00000000-7ffd00100000 rw-p 00000000 00:00 0                          [stack]
7ffd00200000-7ffd00202000 r--p 00000000 00:00 0                          [vvar]
7ffd00202000-7ffd00204000 r-xp 00000000 00:00 0                          [vdso]
";

    fn plan(locked: &[Range<u64>]) -> LockPlan {
        plan_locks(MAPS, Some(EXE), locked)
    }

    /// synoik's own code and its private anonymous memory; not a GPU mapping, not a client's
    /// shared memory, not another library, not a reservation nothing can touch.
    #[test]
    fn the_locker_picks_only_synoiks_own_memory() {
        let starts: Vec<u64> = plan(&[]).lock.iter().map(|r| r.start).collect();
        assert_eq!(
            starts,
            [
                0x55d0a0000000,
                0x55d0a1000000,
                0x55d0a2000000,
                0x55d0b0000000,
                0x7f0000000000,
                0x7f1300000000,
                0x7ffd00000000,
            ]
        );
    }

    /// Once the installed binary is replaced the kernel names it "(deleted)" — in the maps and in
    /// `/proc/self/exe` alike, which is why the locker reads the link on every scan.
    #[test]
    fn a_replaced_binary_is_still_ours() {
        let maps = "55d0a1000000-55d0a2000000 r-xp 01000000 fd:00 1234 /usr/bin/synoik (deleted)\n";
        let plan = plan_locks(maps, Some("/usr/bin/synoik (deleted)"), &[]);
        assert_eq!(plan.lock, vec![0x55d0a1000000..0x55d0a2000000; 1]);
        assert!(plan_locks(maps, Some("/usr/bin/synoik"), &[])
            .lock
            .is_empty());
    }

    /// A mapping inside a locked range — the range itself, or half of one split since — is not
    /// locked again; one that grew past it is, and one that is gone is forgotten.
    #[test]
    fn a_rescan_locks_only_what_is_not_covered() {
        let heap = 0x55d0b0000000..0x55d0b4000000;
        let before_split = 0x7f0000000000..0x7f0000800000;
        let before_growth = 0x7f1300000000..0x7f1300008000;
        let gone = 0x7e0000000000..0x7e0000100000;
        let plan = plan(&[heap.clone(), before_split, before_growth, gone]);
        assert_eq!(plan.covered, [heap.clone(), 0x7f0000000000..0x7f0000400000]);
        assert!(plan.lock.contains(&(0x7f1300000000..0x7f1300010000)));
        assert!(!plan.lock.contains(&heap));
        assert_eq!(plan.lock.len(), 5);
    }

    /// A locked range shows the `lo` flag in `/proc/self/smaps`. Only the fresh mapping is locked,
    /// so the test binary's parallel neighbours are untouched, and it fits the default 8 MiB limit.
    #[test]
    fn a_locked_range_is_locked() {
        const LEN: usize = 64 << 10;
        // SAFETY: a fresh anonymous private mapping, unmapped below and never aliased.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        let start = addr as u64;
        lock_range(start..start + LEN as u64).expect("mlock2");
        let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
        // SAFETY: the mapping made above, unmapped exactly once.
        unsafe { libc::munmap(addr, LEN) };
        let header = format!("{start:x}-");
        let entry = smaps
            .split_inclusive("VmFlags:")
            .skip_while(|chunk| !chunk.lines().any(|l| l.starts_with(&header)))
            .nth(1)
            .expect("the mapping in smaps");
        let flags = entry.lines().next().unwrap_or("");
        assert!(
            flags.split_whitespace().any(|f| f == "lo"),
            "flags: {flags}"
        );
    }
}
