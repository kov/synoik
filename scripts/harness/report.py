#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

"""Read a harness run (`shutdown.sh`, `reboot.sh`, `login.sh`, `switch.sh`, `logout.sh`) back: what reached the screen, when, and what the outer meant.

    scripts/harness/report.py RUN [--strip N] [--all]

Everything is on the capture's clock (the host's), in ms from the driver's first step. A guest
boot's journal is put on it through that boot's `clock` line (clock.py): its monotonic clock,
which never steps, tied to the host's to a few ms. (Its wall clock can be a second off early in
a boot; a boot with no `clock` line falls back to it.) synoik's lines are placed when they were
written, not when journald received them. Frames are named by identity: each captured frame carries its virtio-gpu resource, and the
outer's flight recorder says which framebuffer of which session or of its own that is
(`attribute`).

Sections:
  timeline   frames merged with the driver's steps and the journal; a steady run of one label
             is one line (`--all` prints every frame)
  fades      per crossfade, each composite frame's alpha fitted from its pixels as A + a·(B − A),
             against the frames before and after the fade (before the first frame, the still the
             run began with, `before.png`), beside the alpha the outer logged; per curtain or
             depth, which move their pictures and have no blend to fit, the progress it logged
  tail       the screen's last change before the power went, the longest still after the shutdown
             began, and how steadily the spinner turned
  filmstrip  RUN/filmstrip.png: up to N frames (default 40), every label change kept

Needs numpy and Pillow, and ImageMagick's `montage` for the filmstrip.
"""

import argparse
import bisect
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

    # A record without a file is a frame presented but not captured.
    frames = [r for r in load_jsonl(run / 'frames.jsonl') if 'seq' in r and 'file' in r]
    steps = load_jsonl(run / 'driver.jsonl')
    journal = [e for e in load_jsonl(run / 'journal.jsonl') if interesting(e)]
    if not frames:
        sys.exit(f'{run}: no frames captured')

    clocks = {s['boot_id']: s['t_realtime_ns'] - s['guest_monotonic_ns'] for s in steps if s['step'] == 'clock'}
    steps = [s for s in steps if s['step'] != 'clock']
    t0 = steps[0]['t_realtime_ns'] / 1e6 if steps else frames[0]['t_realtime_ns'] / 1e6
    for f in frames:
        f['t'] = f['t_realtime_ns'] / 1e6 - t0
    events = [[s['t_realtime_ns'] / 1e6 - t0, None, 'driver', s['step']] for s in steps]
    flips = []
    resources = []
    first = min(frames[0]['t'], 0) - 2000
    for e in journal:
        boot = e.get('_BOOT_ID')
        received = int(e['__REALTIME_TIMESTAMP']) / 1e3
        offset = clocks.get(boot)
        t = (received if offset is None else (int(e['__MONOTONIC_TIMESTAMP']) * 1000 + offset) / 1e6) - t0
        written = re.match(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z', e['MESSAGE'])
        if written:
            # Both wall-clock readings in the guest, a moment apart: the lag survives a step.
            at = datetime.fromisoformat(written.group(1)).replace(tzinfo=timezone.utc).timestamp() * 1e3
            t += at - received
        # Which buffer a resource is holds from when it was logged on, however long before.
        m = re.search(r'flight: fb (\d+) is virtio-gpu resource (\d+): (.*)', e['MESSAGE'])
        if m:
            resources.append((t, boot, int(m.group(1)), int(m.group(2)), m.group(3)))
            continue
        if t < first:
            continue
        if FLIGHT in e['MESSAGE']:
            m = re.search(r'flip (\S+) fb=(\d+) (.*)', e['MESSAGE'])
            if m:
                flips.append({'t': t, 'boot': boot, 'fb': int(m.group(2)), 'what': m.group(3)})
                continue
        text = strip_message(e['MESSAGE']) if e.get('_COMM') == 'synoik' else e['MESSAGE']
        who = e.get('_COMM') or e.get('SYSLOG_IDENTIFIER') or '?'
        events.append([t, boot, who, text])

    print(f'{run.name}: {len(frames)} frames from {frames[0]["t"]:.0f} to {frames[-1]["t"]:.0f} ms')
    attribute(frames, flips, resources, events)
    events = sorted((t, who, text) for t, _, who, text in events)
    flips = [(f['t'], f['what']) for f in flips]
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


def attribute(frames, flips, resources, events):
    """Name every frame, and put each boot's journal exactly on the capture's clock.

    A frame's virtio-gpu resource is a framebuffer of that boot (the flight recorder's `fb … is
    virtio-gpu resource …` lines); a session's framebuffer names the frame outright. The outer's
    two slots show composites and the splash in turn, so a frame of one is named by the slot's
    latest flip before it — on a clock fitted per boot: the shift that pairs the most of the
    outer's logged flips one to one with a captured frame of the same framebuffer shortly after.
    A boot's clock line gets within milliseconds when it has synced and is ~0.1-0.5 s off while
    chrony still slews it, early in a boot; the pairing gets within a frame. The fitted shift is
    applied to the boot's flips and journal lines alike.
    """
    starts = {}
    for t, boot, *_ in sorted(resources) + sorted((f['t'], f['boot']) for f in flips):
        starts.setdefault(boot, t)
    order = sorted(starts, key=starts.get)

    def boot_of(t):
        owner = order[0] if order else None
        for b in order:
            if starts[b] <= t + 1000:
                owner = b
        return owner

    names = defaultdict(dict)  # boot -> res -> (fb, owner)
    for t, boot, fb, res, owner in sorted(resources):
        names[boot][res] = (fb, owner)
    for f in frames:
        boot = boot_of(f['t'])
        f['boot'] = boot
        fb, owner = names.get(boot, {}).get(f['guest_resource'], (None, None))
        f['fb'] = fb
        f['label'] = owner if owner and owner.startswith('session') else f"res {f['guest_resource']}"
        f['outer'] = owner == "the outer's"
    window = 2 * FLIP_TO_FRAME_MS / 3
    for boot in order:
        logged = sorted((f for f in flips if f['boot'] == boot and not f['what'].startswith('session')),
                        key=lambda f: f['t'])
        seen = [f for f in frames if f['boot'] == boot and f['outer']]
        if not logged or not seen:
            continue
        by_fb = defaultdict(list)
        for f in seen:
            by_fb[f['fb']].append(f['t'])

        def score(shift):
            used = set()
            matched, delay = 0, 0.0
            for flip in logged:
                at = flip['t'] + shift
                times = by_fb.get(flip['fb'], [])
                i = bisect.bisect_left(times, at)
                while i < len(times) and times[i] <= at + window and (flip['fb'], i) in used:
                    i += 1
                if i < len(times) and times[i] <= at + window:
                    used.add((flip['fb'], i))
                    matched += 1
                    delay += times[i] - at
            return matched, -delay

        shift = max(range(-1000, 1001), key=score)
        matched, _ = score(shift)
        print(f'boot {boot[:8]}: journal shifted {shift:+d} ms onto the capture; '
              f'{matched} of {len(logged)} logged flips of the outer paired with a frame')
        for flip in flips:
            if flip['boot'] == boot:
                flip['t'] += shift
        for e in events:
            if e[1] == boot:
                e[0] += shift
        for fb, times in by_fb.items():
            mine = [f for f in logged if f['fb'] == fb]
            stamps = [f['t'] for f in mine]
            for frame in (f for f in seen if f['fb'] == fb):
                i = bisect.bisect_right(stamps, frame['t'] + 1) - 1
                if i >= 0 and frame['t'] - stamps[i] <= window * 2:
                    frame['label'] = mine[i]['what']
                else:
                    frame['label'] = f"the outer's, unlogged (res {frame['guest_resource']})"


def kind_of(frame):
    """A frame's label without what changes from one frame of a steady run to the next."""
    return re.sub(r' (alpha|progress|frame)=\S+', '', frame['label'])


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
    # What was on the screen when the recording began (capture_start), for a fade that starts the run.
    before = {'seq': 0, 'file': 'before.png', 'label': 'the screen before'} if (run / 'before.png').exists() else None
    for seg in segments:
        i, j = index[seg[0]['seq']], index[seg[-1]['seq']]
        a = frames[i - 1] if i > 0 else before or seg[0]
        b = frames[j + 1] if j + 1 < len(frames) else seg[-1]
        A, B = gray(run, a), gray(run, b)
        d = B - A
        mask = np.abs(d) > 8
        span = seg[-1]['t'] - seg[0]['t']
        print(f"fade #{seg[0]['seq']}-#{seg[-1]['seq']}: {len(seg)} frames over {span:.0f} ms, "
              f"from #{a['seq']} ({a['label']}) to #{b['seq']} ({b['label']})")
        if any('alpha=' not in f['label'] for f in seg):
            # A curtain or a depth moves its pictures: no blend of the two ends to fit.
            print(f"  {kind_of(seg[0]).removeprefix('composite ')}: progress per frame, as the outer logged it")
            for f in seg:
                logged = re.search(r'progress=(\S+)', f['label'])
                print(f"  #{f['seq']:<5} {f['t']:8.1f}  logged {logged.group(1) if logged else '?':>6}")
            continue
        if mask.sum() < 100:
            print('  the two ends look the same: nothing to fit')
            continue
        denom = float((d[mask] ** 2).sum())
        prev = None
        worst = 0.0
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
            worst = max(worst, resid)
        if worst > 3:
            print('  a residual this high means an end of the fade moved while it ran (a live session '
                  'frame, the turning spinner): the fit is against stills and does not apply')
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
