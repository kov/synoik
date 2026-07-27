#!/usr/bin/env python3
"""Correlate the continuation-frame miss rate against per-frame scene cost.

This answers one question: does a frame miss because of how *long* the GPU runs,
or because of how *much* is recorded? Those point at different mechanisms — the
first at anything proportional to GPU time, the second at anything proportional
to command count, which is what a per-command host-side tax would be
(docs/fork/present-misses.md §17.3, §18).

Method: each 10 s summary line closes a window. A window contributes its median
per-frame gpu / draws / elements and its aim-1 miss rate. Spearman, because none
of these are linear, then partial correlations to separate draws from gpu.

Rates use the `aim` tag, never the landing tag: a missed frame lands a cycle late
by construction, so bucketing by landing makes the back-to-back bucket
definitionally miss-free (§14).

Two guards, both learned the hard way:
  * windows with a thin continuation stream are dropped (--min-aim1), since a
    handful of flips gives a rate of 0% or 50% and nothing in between;
  * the covered range of every predictor is printed, because a correlation from
    one run is only comparable to another's if both spanned the same workload.
    An "after" run that never got heavy cannot be compared to a "before" that did.

Usage:
  journalctl -b -1 --since ... -o short-iso > before.log
  scripts/correlate-frame-log.py before.log after.log --labels before after
"""
import argparse
import re
import statistics
import sys

SUMMARY = re.compile(r'(\S+): [\d.]+ fps over .*aim ((?:\s*\S+×\d+)+)')
FRAME = re.compile(r'frame on \S+ took ')
GPU = re.compile(r'\(gpu ([\d.]+)ms\)')
DRAWS = re.compile(r'(\d+) draws covering')
ELEMENTS = re.compile(r'; (\d+) elements')
MISS1 = 'aimed at the next cycle'


def windows(path):
    """Split a log into 10 s windows, each closed by its summary line."""
    cur = {'gpu': [], 'draws': [], 'elements': [], 'miss1': 0}
    out = []
    for line in open(path, errors='replace'):
        if FRAME.search(line):
            for key, rx in (('gpu', GPU), ('draws', DRAWS), ('elements', ELEMENTS)):
                m = rx.search(line)
                if m:
                    cur[key].append(float(m.group(1)))
        elif 'missed' in line and MISS1 in line:
            cur['miss1'] += 1
        else:
            m = SUMMARY.search(line)
            if not m:
                continue
            aim1 = 0
            for tag, n in re.findall(r'(\S+?)×(\d+)', m.group(2)):
                if tag == '1':
                    aim1 += int(n)
            cur['aim1'] = aim1
            cur['frames'] = len(cur['gpu'])
            out.append(cur)
            cur = {'gpu': [], 'draws': [], 'elements': [], 'miss1': 0}
    return out


def reduce_windows(ws, min_aim1, min_frames):
    rows = []
    for w in ws:
        if w['aim1'] < min_aim1 or w['frames'] < min_frames:
            continue
        rows.append({
            'gpu': statistics.median(w['gpu']),
            'draws': statistics.median(w['draws']),
            'elements': statistics.median(w['elements']),
            'rate': w['miss1'] / w['aim1'],
            'aim1': w['aim1'],
            'miss1': w['miss1'],
        })
    return rows


def rank(xs):
    """Fractional ranks, ties averaged — required for Spearman to be exact."""
    order = sorted(range(len(xs)), key=lambda i: xs[i])
    ranks = [0.0] * len(xs)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and xs[order[j + 1]] == xs[order[i]]:
            j += 1
        shared = (i + j) / 2 + 1
        for k in range(i, j + 1):
            ranks[order[k]] = shared
        i = j + 1
    return ranks


def pearson(xs, ys):
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    num = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    dx = sum((x - mx) ** 2 for x in xs) ** 0.5
    dy = sum((y - my) ** 2 for y in ys) ** 0.5
    return num / (dx * dy) if dx and dy else float('nan')


def spearman(xs, ys):
    return pearson(rank(xs), rank(ys))


def partial(xs, ys, zs):
    """rho(x,y) with z held fixed, on ranks."""
    rxy, rxz, ryz = spearman(xs, ys), spearman(xs, zs), spearman(ys, zs)
    den = ((1 - rxz ** 2) * (1 - ryz ** 2)) ** 0.5
    return (rxy - rxz * ryz) / den if den else float('nan')


def bands(rows, key, edges):
    """Pooled miss rate by band of `key` — the interpretable before/after view."""
    out = []
    for lo, hi in zip(edges, edges[1:]):
        sel = [r for r in rows if lo <= r[key] < hi]
        flips = sum(r['aim1'] for r in sel)
        miss = sum(r['miss1'] for r in sel)
        out.append((lo, hi, len(sel), flips, miss, miss / flips if flips else None))
    return out


def report(label, rows):
    print(f"\n{'=' * 62}\n{label}: {len(rows)} qualifying windows\n{'=' * 62}")
    if len(rows) < 8:
        print("  too few windows to correlate — widen the capture")
        return
    rate = [r['rate'] for r in rows]
    preds = {k: [r[k] for r in rows] for k in ('draws', 'gpu', 'elements')}

    print("\n  coverage (the comparability guard — two runs only compare if these overlap)")
    for k, v in preds.items():
        print(f"    {k:<9} min {min(v):8.2f}  p50 {statistics.median(v):8.2f}  max {max(v):8.2f}")
    flips = sum(r['aim1'] for r in rows)
    miss = sum(r['miss1'] for r in rows)
    print(f"    {'overall':<9} {flips} aim-1 flips, {miss} misses, {100 * miss / flips:.2f}%")

    print("\n  rho with the aim-1 miss rate")
    for k, v in preds.items():
        print(f"    {k:<9} {spearman(v, rate):+.3f}")

    print("\n  collinearity")
    print(f"    draws~gpu      {spearman(preds['draws'], preds['gpu']):+.3f}")
    print(f"    elements~gpu   {spearman(preds['elements'], preds['gpu']):+.3f}")

    print("\n  partial correlations")
    print(f"    draws | gpu       {partial(preds['draws'], rate, preds['gpu']):+.3f}")
    print(f"    elements | gpu    {partial(preds['elements'], rate, preds['gpu']):+.3f}")
    print(f"    gpu | draws       {partial(preds['gpu'], rate, preds['draws']):+.3f}")
    print(f"    gpu | elements    {partial(preds['gpu'], rate, preds['elements']):+.3f}")

    print("\n  pooled miss rate by draw count")
    print(f"    {'band':<16}{'windows':>9}{'flips':>9}{'miss':>7}{'rate':>9}")
    for lo, hi, n, f, m, r in bands(rows, 'draws', [0, 40, 60, 90, 130, 200, 10 ** 6]):
        if n:
            print(f"    {f'{lo}-{hi}':<16}{n:>9}{f:>9}{m:>7}{(f'{100 * r:.2f}%') if r is not None else '-':>9}")

    print("\n  pooled miss rate by gpu p50")
    print(f"    {'band':<16}{'windows':>9}{'flips':>9}{'miss':>7}{'rate':>9}")
    for lo, hi, n, f, m, r in bands(rows, 'gpu', [0, 1, 2, 4, 6, 12, 10 ** 6]):
        if n:
            print(f"    {f'{lo}-{hi}ms':<16}{n:>9}{f:>9}{m:>7}{(f'{100 * r:.2f}%') if r is not None else '-':>9}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument('logs', nargs='+')
    p.add_argument('--labels', nargs='+')
    p.add_argument('--min-aim1', type=int, default=200)
    p.add_argument('--min-frames', type=int, default=100)
    args = p.parse_args()
    labels = args.labels or args.logs
    if len(labels) != len(args.logs):
        sys.exit('--labels must be given once per log')
    for path, label in zip(args.logs, labels):
        report(label, reduce_windows(windows(path), args.min_aim1, args.min_frames))


if __name__ == '__main__':
    main()
