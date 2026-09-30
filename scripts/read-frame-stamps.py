#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

"""Read the frame stamps (`SYNOIK_FRAME_LOG=…,stamp`) out of a host-side screen recording.

Prints one line per recorded frame: its index, its timestamp, and the number each corner's stamp
shows (top-left, top-right, bottom-left, bottom-right), `-` where a corner does not decode. A
number is the frame log's `seq`, so `grep ', seq N$'` in a ring dump finds that frame's line.

Every frame of the recording is read, at its own timestamp — never resampled, because the recorder
runs at a variable rate below the display's and a blink lasts one display frame.

A corner that does not decode while the others do is itself a finding: the stamp is the frame's
last draw, so that corner lost what was drawn last there.

Mirrors `src/render_helpers/frame_stamp.rs`; keep the two in step.

    scripts/read-frame-stamps.py recording.mov [--from SECONDS] [--to SECONDS] [--scale S]

`--scale` is recorded pixels per output physical pixel, when the recording is not 1:1.
"""

import argparse
import json
import subprocess
import sys
import tempfile

import numpy as np

CELL_PX = 24
DATA_BITS = 22
ROW_CELLS = 2 + DATA_BITS + 2
BACKING = ((ROW_CELLS + 2) * CELL_PX, 3 * CELL_PX)


def decode(luma, origin, scale):
    """The stamp whose backing starts at `origin` (output pixels), or None."""
    h, w = luma.shape

    def white(cell):
        x = round((origin[0] + (1 + cell) * CELL_PX + CELL_PX / 2) * scale)
        y = round((origin[1] + CELL_PX + CELL_PX / 2) * scale)
        if not (0 <= x < w and 0 <= y < h):
            return None
        return luma[y, x] > 127.5

    cells = [white(c) for c in range(ROW_CELLS)]
    if None in cells:
        return None
    if not cells[0] or cells[1] or not cells[-1]:
        return None
    data = 0
    for bit in cells[2 : 2 + DATA_BITS]:
        data = (data << 1) | int(bit)
    parity = cells[2 + DATA_BITS]
    return data if parity == (bin(data).count("1") % 2 == 1) else None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("recording")
    ap.add_argument("--from", dest="start", type=float, default=0.0)
    ap.add_argument("--to", dest="end", type=float)
    ap.add_argument("--scale", type=float, default=1.0)
    args = ap.parse_args()

    probe = json.loads(
        subprocess.check_output(
            ["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
             "stream=width,height", "-of", "json", args.recording]
        )
    )["streams"][0]
    w, h = probe["width"], probe["height"]
    out_w, out_h = round(w / args.scale), round(h / args.scale)
    bw, bh = BACKING
    corners = [(0, 0), (out_w - bw, 0), (0, out_h - bh), (out_w - bw, out_h - bh)]

    # `info`, not `error`: showinfo reports at info level, and it is where the timestamps come from.
    cmd = ["ffmpeg", "-hide_banner", "-v", "info", "-ss", str(args.start)]
    if args.end is not None:
        cmd += ["-to", str(args.end)]
    cmd += ["-i", args.recording, "-fps_mode", "passthrough", "-vf", "showinfo",
            "-f", "rawvideo", "-pix_fmt", "gray", "-"]
    # showinfo reports each frame's pts on stderr. It goes to a file, not a pipe: a pipe nobody
    # reads until the frames are done fills up, and ffmpeg then blocks with the frames half out.
    log = tempfile.TemporaryFile()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=log)
    frames = []
    while True:
        buf = proc.stdout.read(w * h)
        if len(buf) < w * h:
            break
        luma = np.frombuffer(buf, np.uint8).reshape(h, w)
        frames.append([decode(luma, c, args.scale) for c in corners])
    proc.wait()
    log.seek(0)
    stderr = log.read().decode(errors="replace")
    times = [
        float(part.split(":", 1)[1])
        for line in stderr.splitlines()
        if "pts_time:" in line
        for part in line.split()
        if part.startswith("pts_time:")
    ]

    for i, seqs in enumerate(frames):
        t = args.start + times[i] if i < len(times) else float("nan")
        shown = " ".join("-" if s is None else str(s) for s in seqs)
        print(f"{i}\t{t:.3f}\t{shown}")
    if not any(s is not None for seqs in frames for s in seqs):
        print("no stamp decoded in any frame — is `stamp` in SYNOIK_FRAME_LOG, and is "
              "--scale right?", file=sys.stderr)


if __name__ == "__main__":
    main()
