#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
#
# Pool a sweep's blocks by condition.  report.sh sweep.jsonl
#
# Reports the median block alongside the pooled rate on purpose: misses arrive in bursts,
# so a pooled total can be carried by one or two bad blocks, and the two statistics
# disagreeing is the signal that a difference is not yet real.
set -u
IN=${1:-sweep.jsonl}
[ -s "$IN" ] || { echo "no blocks in $IN" >&2; exit 2; }

unstable=$(jq -r 'select(.arm_stable | not) | .arm' "$IN" | wc -l)
[ "$unstable" -eq 0 ] || echo "WARNING: $unstable block(s) changed arm mid-measurement" >&2

jq -s -r '
  group_by(.arm)
  | map({
      arm: .[0].arm,
      blocks: length,
      frames: (map(.frames) | add),
      misses: (map(.misses) | add),
      median_block: (map(.misses) | sort | .[length / 2 | floor]),
      worst_block: (map(.misses) | max),
      over_budget: (map(.over_budget) | add),
      held: (map(.held_frames) | add),
      late: (map(select(.lateness_mean_ms != null) | .lateness_mean_ms)
             | if length > 0 then (add / length) else null end),
      over_margin: (map(select(.over_margin != null) | .over_margin)
                    | if length > 0 then (add / length) else null end),
    })
  | (map(select(.arm == "off") | .misses / .frames) | first) as $base
  | sort_by(.arm)
  | (["arm","blocks","frames","misses","rate","vs_off","med_blk","worst_blk","over_budget","late_mean","P(late>margin)"] | @tsv),
    (.[] | [ .arm, .blocks, .frames, .misses,
             ((.misses / .frames * 1e6 | round) / 1e6 | tostring),
             (if $base and $base > 0 then ((.misses / .frames / $base * 100 | round) / 100 | tostring + "x") else "-" end),
             .median_block, .worst_block, .over_budget,
             (if .late then ((.late * 1000 | round) / 1000 | tostring + "ms") else "-" end),
             (if .over_margin then ((.over_margin * 1e5 | round) / 1e5 | tostring) else "-" end) ]
           | @tsv)
' "$IN" | column -t

echo
echo "over_budget is the host-noise proxy, not an outcome: if it is lopsided across arms,"
echo "the arms did not see the same machine and the rates are not comparable."
