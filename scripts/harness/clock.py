#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

"""Tie the guest's boot to the host's wall clock, as one `driver.jsonl` line.

    scripts/harness/clock.py HOST_SSH... -- GUEST_SSH...

Each ssh command line runs a small loop on its machine that answers every line it reads with
that machine's clock: the host's wall clock (the capture's), the guest's CLOCK_MONOTONIC (the
journal's `__MONOTONIC_TIMESTAMP`) and its boot id. Both are pinged from here over one connection
each, and the reply with the shortest round trip is taken, at its midpoint: the error is half of
that round trip, about a millisecond, where a wall clock can be a second off early in a boot that
has not synced yet. Within a boot the monotonic clock never steps, so one line serves the whole
boot.

Prints {"step": "clock", "boot_id", "guest_monotonic_ns", "t_realtime_ns", "error_ns"}.
"""

import json
import subprocess
import sys
import time

HOST_LOOP = r'''import sys, time
for _ in sys.stdin:
    print(time.time_ns(), flush=True)'''
GUEST_LOOP = r'''import sys, time
boot = open("/proc/sys/kernel/random/boot_id").read().strip().replace("-", "")
for _ in sys.stdin:
    print(time.clock_gettime_ns(time.CLOCK_MONOTONIC), boot, flush=True)'''
PINGS = 20


def best(cmd, loop):
    """(local midpoint, remote reply, round trip) of the fastest of PINGS pings."""
    p = subprocess.Popen(cmd + ['python3', '-u', '-c', f"'{loop}'"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, text=True)
    samples = []
    for _ in range(PINGS):
        t1 = time.time_ns()
        p.stdin.write('\n')
        p.stdin.flush()
        reply = p.stdout.readline().split()
        t2 = time.time_ns()
        samples.append(((t1 + t2) // 2, reply, t2 - t1))
    p.stdin.close()
    p.wait()
    return min(samples, key=lambda s: s[2])


def main():
    args = sys.argv[1:]
    split = args.index('--')
    host, guest = args[:split], args[split + 1:]
    h_mid, (h_now,), h_rtt = best(host, HOST_LOOP)
    g_mid, (g_mono, boot), g_rtt = best(guest, GUEST_LOOP)
    # Here's clock to the host's at h_mid; the guest's reading taken at g_mid, here.
    host_at_g = int(h_now) + (g_mid - h_mid)
    print(json.dumps({'step': 'clock', 'boot_id': boot, 'guest_monotonic_ns': int(g_mono),
                      't_realtime_ns': host_at_g, 'error_ns': (h_rtt + g_rtt) // 2}))


if __name__ == '__main__':
    main()
