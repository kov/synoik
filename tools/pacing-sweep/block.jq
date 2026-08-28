# One block of a pacing sweep, as a delta between two session-cumulative frame-perf
# snapshots. `frame-perf` reports session lifetime tallies, so every figure here is a
# difference; the lateness mean has to be un-meaned and re-meaned to difference at all.
def armof($s): (if $s.deadline_dispatch then "on@\($s.deadline_margin_ms)ms" else "off" end);
($a.held_frames - $b.held_frames) as $held
| ($secs | tonumber) as $t
| ($a.outputs[] | select(.output == $out)) as $ao
| ($b.outputs[] | select(.output == $out)) as $bo
| {
    cycle: $cycle,
    seconds: $t,
    # The arm is read out of the compositor at both ends, never taken from the label:
    # a mislabelled arm inverts the result silently and every number still looks sane.
    arm: armof($a),
    arm_stable: (armof($b) == armof($a)),
    output: $out,
    frames: ($ao.frames - $bo.frames),
    misses: ($ao.misses - $bo.misses),
    missed_cycles: ($ao.missed_cycles - $bo.missed_cycles),
    # Frames that overran the budget. Not an outcome of the dials -- it is the proxy for
    # how loud the HOST was during this block, which is what makes cross-block comparison
    # checkable rather than assumed.
    over_budget: ($ao.over_budget - $bo.over_budget),
    worst_ms: $ao.worst_ms,
    stalls: ($a.stalls - $b.stalls),
    held_frames: $held,
    lateness_mean_ms: (if $held > 0
      then (($a.lateness_mean_ms * $a.held_frames) - ($b.lateness_mean_ms * $b.held_frames)) / $held
      else null end),
    lateness_buckets: [ range(0; ($a.lateness_buckets | length))
                        | ($a.lateness_buckets[.] - $b.lateness_buckets[.]) ],
    lateness_edges_us: $a.lateness_edges_us,
    # Share of held frames released later than this block's own margin: the mechanism the
    # margin is buying, reported next to the outcome it is supposed to explain.
    over_margin: (if $held > 0 and $a.deadline_dispatch
      then ([ range(0; ($a.lateness_edges_us | length))
              | select($a.lateness_edges_us[.] >= ($a.deadline_margin_ms * 1000))
              | ($a.lateness_buckets[.] - $b.lateness_buckets[.]) ] | add) / $held
      else null end),
  }
| . + { miss_rate: (if .frames > 0 then (.misses / .frames) else null end),
        fps: (.frames / $t) }
