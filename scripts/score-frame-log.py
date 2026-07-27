#!/usr/bin/env python3
"""Score a frame-log journal window per phase, for A/B arms driven by drive-workload.sh.

The point of this script is the comparability gate. Two arms that ran the same
keystrokes are not necessarily comparable: state drifts (a grid left open, a
different workspace), and then the phases render different things. Frame-timing
differences between phases rendering different content are meaningless, and this
has already produced one retracted finding (docs/fork/present-misses.md §15/§16).

So a phase is scored only if the two arms agree on what was on screen, judged by
the distribution of element counts and the number of bakes. Disagreement is
reported as SKIPPED, loudly, rather than quietly averaged into the result.

Rates are computed on the `aim` tag, never the landing tag: a frame that misses
lands a cycle late by construction, so bucketing by landing makes the
back-to-back bucket definitionally miss-free (§14).

Usage:
  journalctl _COMM=niri --since ... -o short-precise > arm.log     (per arm)
  scripts/score-frame-log.py A.log B.log --labels ON OFF \
      --phases 12:58:21 12:59:01 12:59:22 12:59:43 13:00:03 13:00:33 \
      --phases 12:54:34 12:55:14 12:55:35 12:55:56 12:56:16 12:56:46
"""
import argparse, collections, re, statistics, sys
from datetime import datetime, timedelta

NAMES = ['light', 'overview', 'app grid', 'workspace', 'idle']


def stamp(line):
    m = re.match(r'(\w+ \d+ \d+:\d+:\d+\.\d+)', line)
    return datetime.strptime('2026 ' + m.group(1), '%Y %b %d %H:%M:%S.%f') if m else None


def read_phase(path, lo, hi):
    """Everything one phase of one arm did: what it drew, and what it missed."""
    out = {'frames': 0, 'bakes': 0, 'elements': collections.Counter(),
           'aim1': 0, 'miss1': 0, 'gpu': []}
    for line in open(path):
        t = stamp(line)
        if t is None or not (lo <= t < hi):
            continue
        if 'missed' in line and 'aimed at the next cycle' in line:
            out['miss1'] += 1
        a = re.search(r'aim ((?:\s*\S+×\d+)+)', line)
        if a:
            for tag, n in re.findall(r'(\S+?)×(\d+)', a.group(1)):
                if tag == '1':
                    out['aim1'] += int(n)
        if 'frame on' in line:
            out['frames'] += 1
            e = re.search(r'; (\d+) elements', line)
            if e:
                out['elements'][int(e.group(1))] += 1
            b = re.search(r'(\d+) bakes in', line)
            if b:
                out['bakes'] += int(b.group(1))
            g = re.search(r'\(gpu ([\d.]+)ms\)', line)
            if g:
                out['gpu'].append(float(g.group(1)))
    return out


def comparable(a, b):
    """Did the two arms render the same thing? Reason string, or None if they did."""
    ta = [k for k, _ in a['elements'].most_common(3)]
    tb = [k for k, _ in b['elements'].most_common(3)]
    # Element counts shift by a few between runs (a clock digit, a hover); the
    # scene *shape* is what has to match, so compare with a tolerance.
    if len(ta) != len(tb) or any(abs(x - y) > 8 for x, y in zip(sorted(ta), sorted(tb))):
        return f"element counts differ: {ta} vs {tb}"
    if max(a['bakes'], b['bakes']) > 2 * max(1, min(a['bakes'], b['bakes'])):
        return f"bake counts differ: {a['bakes']} vs {b['bakes']}"
    return None


def main():
    p = argparse.ArgumentParser()
    p.add_argument('logs', nargs=2)
    p.add_argument('--labels', nargs=2, default=['A', 'B'])
    p.add_argument('--phases', nargs=6, action='append', required=True,
                   help='six HH:MM:SS boundaries, once per log')
    p.add_argument('--date', default='2026-07-27')
    args = p.parse_args()
    if len(args.phases) != 2:
        sys.exit('--phases must be given once per log')

    def T(s):
        return datetime.strptime(f'{args.date} {s}', '%Y-%m-%d %H:%M:%S')

    bounds = [[T(x) for x in ph] for ph in args.phases]
    totals = [[0, 0], [0, 0]]
    print(f"{'phase':<12}{'arm':>5}{'gpu p50':>10}{'flips':>8}{'miss':>6}{'rate':>8}")
    for i, name in enumerate(NAMES):
        d = [read_phase(args.logs[k], bounds[k][i], bounds[k][i + 1]) for k in (0, 1)]
        why = comparable(d[0], d[1])
        if why:
            print(f"{name:<12}  SKIPPED — not comparable ({why})")
            continue
        for k in (0, 1):
            f1, m1 = d[k]['aim1'], d[k]['miss1']
            totals[k][0] += f1
            totals[k][1] += m1
            gpu = f"{statistics.median(d[k]['gpu']):.2f}ms" if d[k]['gpu'] else '-'
            rate = f"{100 * m1 / f1:.2f}%" if f1 else '-'
            print(f"{name if k == 0 else '':<12}{args.labels[k]:>5}{gpu:>10}{f1:>8}{m1:>6}{rate:>8}")
    print()
    for k in (0, 1):
        f1, m1 = totals[k]
        rate = f"{100 * m1 / f1:.2f}%" if f1 else '-'
        print(f"{'COMPARABLE':<12}{args.labels[k]:>5}{'':>10}{f1:>8}{m1:>6}{rate:>8}")


if __name__ == '__main__':
    main()
