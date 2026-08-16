// LD_PRELOAD shim that times the DRM syncobj wait ioctl, to answer the one question
// `foundation.md` §5 leaves open.
//
// The host side timed every hop it can see and found the complete host round trip is ~0.076 ms,
// against 0.43-1.48 ms measured in the guest for the same waits — so ~90% of it is guest-side, on
// one side or the other of the host window. Their probes cannot say which side, because both are
// outside the host. Ours cannot either: `vkWaitForFences` is mesa's, and the ioctl inside it is
// not a boundary anything in our stack can see.
//
// This shim makes it visible without patching mesa or the kernel. Interposing libc's `ioctl` puts
// a stamp exactly at the guest->kernel boundary, so a fence wait splits into three:
//
//   [ wait start ......... ioctl entry ... HOST WINDOW ... ioctl return ......... wait end ]
//     \___ mesa, before ___/               \_ ~0.076ms _/   \___ mesa + scheduler, after __/
//     \_________________________ what the guest measures _________________________________/
//
// The probe reports the outer span, this reports the middle, and the difference is mesa's own
// work on either side. Inside the ioctl, subtracting the host window leaves guest driver and
// wakeup time. That is the split §10.1 asks for.
//
// Build + use:
//   cc -O2 -fPIC -shared -o shim.so shim.c
//   IOCTL_SPLIT_OUT=/tmp/ioctl.csv LD_PRELOAD=$PWD/shim.so <program>
//
// Only DRM_IOCTL_SYNCOBJ_*WAIT is recorded — every other ioctl is forwarded untouched and
// unstamped, so the shim costs nothing on the paths it is not measuring.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <sys/ioctl.h>
#include <stdint.h>
#include <sys/types.h>

// Matched on the ioctl's *type* and *nr* fields rather than the whole encoded request, because
// the size field varies with the kernel's struct layout and getting it wrong silently records
// nothing (which is exactly what a first pass with hardcoded values did). DRM's type byte is 'd'.
#define IOC_TYPE(req) (((req) >> 8) & 0xff)
#define IOC_NR(req)   ((req) & 0xff)
#define DRM_IOCTL_TYPE            0x64 /* 'd' */
#define DRM_NR_SYNCOBJ_WAIT          0x58
#define DRM_NR_SYNCOBJ_TIMELINE_WAIT 0x62

static int (*real_ioctl)(int, unsigned long, void *);
static FILE *out;
static int initialized;

static void init(void) {
    if (initialized) return;
    initialized = 1;
    real_ioctl = dlsym(RTLD_NEXT, "ioctl");
    const char *path = getenv("IOCTL_SPLIT_OUT");
    if (path) {
        out = fopen(path, "w");
        if (out) fprintf(out, "start_ns,dur_ns,request\n");
    }
}

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

// The fence wait turned out never to enter the kernel to *wait* — no syncobj and no
// DRM_VIRTGPU_WAIT ioctl is issued at all. What it does instead is spin in mesa's `vn_relax`
// (`vn_common.c:245`): a few `sched_yield`s, then `os_time_sleep`. So the interesting boundary is
// not the ioctl after all, it is the sleep — record what mesa *asks* for and what it actually
// gets, since a sleep quantum coarser than the host's answer is pure added latency.
int clock_nanosleep(clockid_t clk, int flags, const struct timespec *req,
                    struct timespec *rem) {
    static int (*real)(clockid_t, int, const struct timespec *, struct timespec *);
    if (!real) real = dlsym(RTLD_NEXT, "clock_nanosleep");
    if (!initialized) init();
    if (!out) return real(clk, flags, req, rem);

    uint64_t want = (uint64_t)req->tv_sec * 1000000000ull + (uint64_t)req->tv_nsec;
    uint64_t t0 = now_ns();
    int ret = real(clk, flags, req, rem);
    uint64_t t1 = now_ns();
    fprintf(out, "%llu,%llu,sleep_want_%llu\n", (unsigned long long)t0,
            (unsigned long long)(t1 - t0), (unsigned long long)want);
    return ret;
}

int ioctl(int fd, unsigned long request, ...) {
    // Linux passes a single pointer-sized argument; read it without varargs machinery so the
    // forwarded call is byte-identical to what the caller made.
    void *arg;
    __builtin_va_list ap;
    __builtin_va_start(ap, request);
    arg = __builtin_va_arg(ap, void *);
    __builtin_va_end(ap);

    if (!initialized) init();

    // `IOCTL_SPLIT_ALL=1` records every ioctl, for finding out what a path actually calls rather
    // than assuming — which is how the syncobj request codes above were pinned down.
    static int all = -1;
    if (all < 0) all = getenv("IOCTL_SPLIT_ALL") != NULL;
    int watched = all || IOC_TYPE(request) == DRM_IOCTL_TYPE &&
                  (IOC_NR(request) == DRM_NR_SYNCOBJ_WAIT ||
                   IOC_NR(request) == DRM_NR_SYNCOBJ_TIMELINE_WAIT);
    if (!watched || !out) return real_ioctl(fd, request, arg);

    uint64_t t0 = now_ns();
    int ret = real_ioctl(fd, request, arg);
    uint64_t t1 = now_ns();
    fprintf(out, "%llu,%llu,0x%lx\n", (unsigned long long)t0,
            (unsigned long long)(t1 - t0), request);
    return ret;
}
