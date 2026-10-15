#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

"""Read a harness run (`shutdown.sh`) back: what reached the screen, when, and what the outer meant.

    scripts/harness/report.py RUN [--strip N] [--all]

Everything is on the capture's clock (the host's), in ms from the driver's first step. The guest's
journal is put on it by its own wall clock (synoik's lines by the time they were written, not
received), which agrees with the host's to tens of ms: close enough to read the order of a fade
and a disconnect, not to attribute a single frame by time. Frames are attributed by order instead.
Each flip the flight recorder logs votes, with the first captured frame after it, on whether that
frame's virtio-gpu resource is one of the outer's own slots or a session's buffer; a split vote is
printed. The outer's frames, in capture order, are then paired with its logged flips in order,
which names each one (composite at alpha, splash, plate) — a count that differs is printed too.

Sections:
  timeline   frames merged with the driver's steps and the journal; a steady run of one label
             is one line (`--all` prints every frame)
  fades      per fade, each composite frame's alpha fitted from its pixels as A + a·(B − A),
             against the frames before and after the fade, beside the alpha the outer logged
  tail       the screen's last change before the power went, the longest still after the shutdown
             began, and how steadily the spinner turned
  filmstrip  RUN/filmstrip.png: up to N frames (default 40), every label change kept

Needs numpy and Pillow, and ImageMagick's `montage` for the filmstrip.
"""

import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np
from PIL import Image

FLIGHT = 'synoik::system_compositor::flight'
# A captured frame comes this long after the flip that put it there, at most: a vblank and the
# host's present, plus the clocks' disagreement.
FLIP_TO_FRAME_MS = 60
STEADY_GAP_MS = 25


def load_jsonl(path):
    if not path.exists():
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def strip_message(msg):
    """The synoik log line without its own timestamp and crate prefix."""
    m = re.match(r'\S+Z\s+(\w+)\s+(\S+): (.*)', msg)
    if not m:
        return msg
    level, target, text = m.groups()
    target = target.removeprefix('synoik::').removeprefix('system_compositor::')
    return f'{level} {target}: {text}'


ANSI = re.compile(r'\x1b\[[0-9;]*m')


def message(entry):
    """The entry's text: journald stores a line with control characters as a byte array."""
    msg = entry.get('MESSAGE')
    if isinstance(msg, list):
        msg = bytes(msg).decode(errors='replace')
    return ANSI.sub('', msg) if isinstance(msg, str) else None


def interesting(entry):
    msg = entry['MESSAGE'] = message(entry)
    if msg is None or entry.get('SYSLOG_IDENTIFIER') == 'audit':
        return False
    comm = entry.get('_COMM', '')
    if comm == 'synoik':
        return 'system_compositor' in msg and (' INFO ' in msg or ' WARN ' in msg or FLIGHT in msg)
    if entry.get('_PID') == '1':
        return any(w in msg for w in ('synoik', 'Reached target', 'session-', 'user@'))
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('run', type=Path)
    ap.add_argument('--strip', type=int, default=40, help='frames in the filmstrip')
    ap.add_argument('--all', action='store_true', help='print every frame')
    args = ap.parse_args()
    run = args.run

    frames = [r for r in load_jsonl(run / 'frames.jsonl') if 'seq' in r]
    steps = load_jsonl(run / 'driver.jsonl')
    journal = [e for e in load_jsonl(run / 'journal.jsonl') if interesting(e)]
    if not frames:
        sys.exit(f'{run}: no frames captured')

    t0 = steps[0]['t_realtime_ns'] / 1e6 if steps else frames[0]['t_realtime_ns'] / 1e6
    for f in frames:
        f['t'] = f['t_realtime_ns'] / 1e6 - t0
    events = [(s['t_realtime_ns'] / 1e6 - t0, 'driver', s['step']) for s in steps]
    flips = []
    first = min(frames[0]['t'], 0) - 2000
    for e in journal:
        t = int(e['__REALTIME_TIMESTAMP']) / 1e3 - t0
        written = re.match(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z', e['MESSAGE'])
        if written:
            t = datetime.fromisoformat(written.group(1)).replace(tzinfo=timezone.utc).timestamp() * 1e3 - t0
        if t < first:
            continue
        text = strip_message(e['MESSAGE']) if e.get('_COMM') == 'synoik' else e['MESSAGE']
        who = e.get('_COMM') or e.get('SYSLOG_IDENTIFIER') or '?'
        if FLIGHT in e['MESSAGE']:
            m = re.search(r'flip (\S+) fb=(\d+) (.*)', e['MESSAGE'])
            if m:
                flips.append((t, m.group(3)))
                continue
        events.append((t, who, text))
    events.sort()

    # Whose buffer each resource is, by vote.
    flips.sort()
    votes = defaultdict(Counter)
    for t, what in flips:
        owner = 'session' if what.startswith('session') else 'outer'
        frame = next((f for f in frames if t <= f['t'] <= t + FLIP_TO_FRAME_MS), None)
        if frame:
            votes[frame['guest_resource']][owner] += 1
    owner = {res: c.most_common(1)[0][0] for res, c in votes.items()}
    print(f'{run.name}: {len(frames)} frames from {frames[0]["t"]:.0f} to {frames[-1]["t"]:.0f} ms')
    print('resources:', ', '.join(
        f'{res}={owner[res]}' + (f' (split: {dict(votes[res])})' if len(votes[res]) > 1 else '')
        for res in sorted(votes)) or 'no flight recorder lines')

    # The outer's frames named by its flips, in order.
    outer_frames = [f for f in frames if owner.get(f['guest_resource']) == 'outer']
    outer_flips = [w for t, w in flips if not w.startswith('session') and t >= frames[0]['t'] - FLIP_TO_FRAME_MS]
    if len(outer_frames) != len(outer_flips):
        print(f'the outer logged {len(outer_flips)} flips of its own and the capture has {len(outer_frames)} '
              'frames of its slots: names past the first difference are off')
    for f, what in zip(outer_frames, outer_flips):
        f['label'] = what
    for f in frames:
        f.setdefault('label', f"{owner.get(f['guest_resource'], 'session')} res {f['guest_resource']}")
    print()

    print('== timeline (ms)')
    items = [(f['t'], 'frame', f) for f in frames] + [(t, 'event', (who, text)) for t, who, text in events]
    items.sort(key=lambda i: i[0])
    prev_t = None
    run_frames = []

    def flush():
        if not run_frames:
            return
        if len(run_frames) == 1 or args.all:
            for f in run_frames:
                gap = f['t'] - f['prev_t'] if f.get('prev_t') is not None else 0
                print(f"{f['t']:9.1f}  +{gap:5.1f}  #{f['seq']:<5} {f['label']}")
        else:
            gaps = sorted(f['t'] - f['prev_t'] for f in run_frames[1:])
            print(f"{run_frames[0]['t']:9.1f}  ..{run_frames[-1]['t']:7.1f}  #{run_frames[0]['seq']}-#{run_frames[-1]['seq']} "
                  f"{len(run_frames)} frames of {kind_of(run_frames[0])}, every {gaps[len(gaps) // 2]:.1f} ms")
        run_frames.clear()

    for t, kind, item in items:
        if kind == 'event':
            flush()
            who, text = item
            print(f'{t:9.1f}  {"":6}  [{who}] {text}')
            continue
        f = item
        f['prev_t'] = prev_t
        prev_t = f['t']
        gap = f['t'] - f['prev_t'] if f['prev_t'] is not None else 0
        if run_frames and (kind_of(f) != kind_of(run_frames[-1]) or gap > STEADY_GAP_MS):
            flush()
        if gap > STEADY_GAP_MS:
            flush()
            print(f"{f['t']:9.1f}  +{gap:5.1f}  #{f['seq']:<5} {f['label']}   <- still for {gap:.0f} ms")
            continue
        run_frames.append(f)
    flush()
    print()

    fades(run, frames, flips, events)
    tail(frames, events, steps, t0)
    filmstrip(run, frames, args.strip)


def kind_of(frame):
    """A frame's label without what changes from one frame of a steady run to the next."""
    return re.sub(r' (alpha|frame)=\S+', '', frame['label'])


def gray(run, f, cache={}):
    key = f['file']
    if key not in cache:
        img = Image.open(run / f['file']).convert('L')
        img = img.resize((img.width // 4, img.height // 4), Image.BILINEAR)
        cache[key] = np.asarray(img, dtype=np.float32)
    return cache[key]


def fades(run, frames, flips, events):
    """Each run of composite frames, split where the outer began a new fade."""
    print('== fades')
    starts = [t for t, who, text in events if 'flight: fade on' in text and (' to ' in text or 'from the plate' in text)]
    segments = []
    current = []
    for f in frames:
        composite = f['label'].startswith('composite')
        new_fade = current and any(current[-1]['t'] < s <= f['t'] for s in starts)
        if current and (not composite or new_fade):
            segments.append(current)
            current = []
        if composite:
            current.append(f)
    if current:
        segments.append(current)
    if not segments:
        print('no composite frames')
        print()
        return
    index = {f['seq']: i for i, f in enumerate(frames)}
    for seg in segments:
        i, j = index[seg[0]['seq']], index[seg[-1]['seq']]
        a = frames[i - 1] if i > 0 else seg[0]
        b = frames[j + 1] if j + 1 < len(frames) else seg[-1]
        A, B = gray(run, a), gray(run, b)
        d = B - A
        mask = np.abs(d) > 8
        span = seg[-1]['t'] - seg[0]['t']
        print(f"fade #{seg[0]['seq']}-#{seg[-1]['seq']}: {len(seg)} frames over {span:.0f} ms, "
              f"from #{a['seq']} ({a['label']}) to #{b['seq']} ({b['label']})")
        if mask.sum() < 100:
            print('  the two ends look the same: nothing to fit')
            continue
        denom = float((d[mask] ** 2).sum())
        prev = None
        for f in seg:
            X = gray(run, f)
            alpha = float(((X - A)[mask] * d[mask]).sum() / denom)
            resid = float(np.sqrt((((X - A) - alpha * d)[mask] ** 2).mean()))
            logged = re.search(r'alpha=(\S+)', f['label']).group(1)
            note = ''
            if prev is not None and alpha < prev - 0.02:
                note = '  <- goes back'
            print(f"  #{f['seq']:<5} {f['t']:8.1f}  fitted {alpha:5.3f}  logged {logged:>6}  "
                  f"residual {resid:5.1f}{note}")
            prev = alpha
    print()


def tail(frames, events, steps, t0):
    print('== tail')
    last = frames[-1]
    gone = next((s['t_realtime_ns'] / 1e6 - t0 for s in steps if s['step'] == 'vm-gone'), None)
    print(f"last frame #{last['seq']} at {last['t']:.0f} ms: {last['label']}")
    if gone is not None:
        print(f"the VM was gone at {gone:.0f} ms: the screen sat still for the last {gone - last['t']:.0f} ms")
    down = next((t for t, who, text in events if 'going down' in text), None)
    if down is None:
        print('no PrepareForShutdown in the journal')
        print()
        return
    after = [f for f in frames if f['t'] >= down]
    worst = max(((b['t'] - a['t'], a) for a, b in zip(after, after[1:])), default=(0, None), key=lambda x: x[0])
    print(f'shutdown began at {down:.0f} ms; {len(after)} frames after it')
    if worst[1] is not None:
        print(f"longest still after it: {worst[0]:.0f} ms from #{worst[1]['seq']} ({worst[1]['label']})")
    turns = [b['t'] - a['t'] for a, b in zip(after, after[1:]) if kind_of(a) == kind_of(b) and a['label'].startswith('splash')]
    if turns:
        turns.sort()
        print(f'the spinner turned a frame every {turns[len(turns) // 2]:.0f} ms (median), {turns[-1]:.0f} ms at most')
    shown = Counter(kind_of(f) for f in after)
    print('after it, frames by label:', ', '.join(f'{k} {v}' for k, v in shown.most_common()))
    print()


def filmstrip(run, frames, n):
    keep = set()
    for a, b in zip(frames, frames[1:]):
        if a['label'] != b['label']:
            keep.update((a['seq'], b['seq']))
    keep.update((frames[0]['seq'], frames[-1]['seq']))
    rest = [f['seq'] for f in frames if f['seq'] not in keep]
    room = max(0, n - len(keep))
    if rest and room:
        step = max(1, len(rest) // room)
        keep.update(rest[::step][:room])
    chosen = [f for f in frames if f['seq'] in keep]
    out = run / 'filmstrip.png'
    cmd = ['montage']
    for f in chosen:
        cmd += ['-label', f"#{f['seq']} {f['t']:.0f}ms {f['label']}", str(run / f['file'])]
    cmd += ['-tile', '6x', '-geometry', '320x200+3+3', '-pointsize', '11', str(out)]
    subprocess.run(cmd, check=True)
    print(f'== filmstrip: {out} ({len(chosen)} frames)')


if __name__ == '__main__':
    main()
