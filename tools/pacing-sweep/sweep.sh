#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
#
# A/B the frame clock's dispatch dials against a live session and report each
# condition's miss rate.
#
# What it is for: `SYNOIK_DEADLINE_DISPATCH` and the render-time margin trade dropped
# frames against how fresh a frame's contents are at the moment it is scanned out. Only
# the drop half is measurable from inside the compositor, and only against a *continuous*
# client, because deadline dispatch short-circuits to immediate dispatch after any idle
# period. So: put a continuous load on one output, cycle the conditions underneath it,
# and difference `msg frame-perf` across each block.
#
# Usage:
#   sweep.sh [-o OUTPUT] [-b SECS] [-c CYCLES] [-s SOCKET] [-u USER] [-j FILE] ARM [ARM...]
#
#   ARM        `off`, or a margin in milliseconds (`0.5`, `2`, `4`)
#   -o OUTPUT  connector the load must be on; verified before the run (default: the
#              output with the most frames when the sweep starts)
#   -b SECS    seconds per block (default 60)
#   -c CYCLES  times to cycle through the arms (default 8)
#   -s SOCKET  synoik IPC socket (default: autodetected from $SYNOIK_SOCKET, else the
#              single socket in the session's runtime dir)
#   -u USER    run `synoik msg` as this user, via sudo -- for driving another seat
#   -j FILE    write one JSON object per block here (default: sweep.jsonl in $PWD)
#
# Example, against your own session:
#   vkcube &
#   tools/pacing-sweep/sweep.sh -c 8 off 1 2 4
#
# Read the caveats in README.md before believing a number this prints. The two that bite
# hardest: rates from different runs are not comparable, and a condition you did not
# verify is a condition you did not control.
set -u

BLOCK=60; CYCLES=8; OUTPUT=; SOCKET=${SYNOIK_SOCKET:-}; ASUSER=; JSONL=$PWD/sweep.jsonl
while getopts "o:b:c:s:u:j:h" opt; do
  case $opt in
    o) OUTPUT=$OPTARG ;;
    b) BLOCK=$OPTARG ;;
    c) CYCLES=$OPTARG ;;
    s) SOCKET=$OPTARG ;;
    u) ASUSER=$OPTARG ;;
    j) JSONL=$OPTARG ;;
    h) sed -n '6,40p' "$0"; exit 0 ;;
    *) exit 2 ;;
  esac
done
shift $((OPTIND - 1))
ARMS=("$@")
[ ${#ARMS[@]} -ge 2 ] || { echo "need at least two arms to compare; see -h" >&2; exit 2; }

HERE=$(cd "$(dirname "$0")" && pwd)
for tool in jq synoik; do
  command -v $tool >/dev/null || { echo "need $tool on PATH" >&2; exit 2; }
done

# `synoik msg`, however this run has to reach the session.
if [ -n "$ASUSER" ]; then
  MSG=(sudo -u "$ASUSER" env "SYNOIK_SOCKET=$SOCKET" synoik msg)
  RUNDIR=$(sudo -u "$ASUSER" sh -c 'echo $XDG_RUNTIME_DIR' 2>/dev/null)
  [ -n "$SOCKET" ] || {
    SOCKET=$(sudo ls "${RUNDIR:-/run/user/$(id -u "$ASUSER")}" 2>/dev/null | grep '^synoik\.' | head -1)
    SOCKET="${RUNDIR:-/run/user/$(id -u "$ASUSER")}/$SOCKET"
    MSG=(sudo -u "$ASUSER" env "SYNOIK_SOCKET=$SOCKET" synoik msg)
  }
else
  [ -n "$SOCKET" ] || {
    SOCKET=$(ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"/synoik.* 2>/dev/null | head -1)
  }
  MSG=(env "SYNOIK_SOCKET=$SOCKET" synoik msg)
fi
[ -n "$SOCKET" ] || { echo "no synoik socket found; pass -s" >&2; exit 2; }

perf() { "${MSG[@]}" -j frame-perf; }

snap=$(perf) || { echo "cannot reach the session at $SOCKET" >&2; exit 2; }
if [ "$(jq -r .enabled <<<"$snap")" != true ]; then
  # Every tally below would be zero, which reads exactly like a flawless session.
  echo "frame logging is OFF in that session -- start it with SYNOIK_FRAME_LOG=ring,gpu" >&2
  exit 2
fi

# Which output carries the load. Left unset, take whichever is presenting most; either
# way it is CHECKED, because a sweep that silently measures an idle output produces a
# full set of plausible numbers and answers nothing.
sleep 2
snap2=$(perf)
busiest=$(jq -n --argjson a "$snap2" --argjson b "$snap" '
  [ $a.outputs[] as $ao | ($b.outputs[]|select(.output==$ao.output)) as $bo
    | {output: $ao.output, d: ($ao.frames - $bo.frames)} ]
  | max_by(.d) | .output')
busiest_d=$(jq -n --argjson a "$snap2" --argjson b "$snap" --arg o "$busiest" '
  [ $a.outputs[] as $ao | ($b.outputs[]|select(.output==$ao.output)) as $bo
    | select($ao.output==$o) | ($ao.frames - $bo.frames) ] | first')
if [ -z "$OUTPUT" ]; then
  OUTPUT=$busiest
  echo "load output (autodetected): $OUTPUT"
elif [ "$OUTPUT" != "$busiest" ]; then
  echo "WARNING: -o $OUTPUT, but $busiest is the busy one ($busiest_d frames in 2s)" >&2
fi
if [ "${busiest_d:-0}" -lt 30 ]; then
  echo "no output is presenting continuously (busiest: ${busiest_d:-0} frames in 2s)." >&2
  echo "Deadline dispatch only engages on continuous frames -- start a client like vkcube." >&2
  exit 2
fi

# Put the session in one condition. `debug-toggle-deadline-dispatch` is a TOGGLE, so
# converge on the compositor's own readback rather than assuming a flip landed.
set_arm() {
  local want=$1 want_on=1 is_on cur
  [ "$want" = off ] && want_on=0
  for _ in 1 2 3; do
    is_on=$(perf | jq -r .deadline_dispatch)
    [ "$is_on" = true ] && cur=1 || cur=0
    [ "$cur" = "$want_on" ] && break
    "${MSG[@]}" action debug-toggle-deadline-dispatch >/dev/null || return 1
    sleep 1
  done
  if [ "$want_on" = 1 ]; then
    "${MSG[@]}" action debug-set-render-time-margin "$want" >/dev/null || return 1
    sleep 1
  fi
}

: > "$JSONL"
echo "sweeping ${ARMS[*]} on $OUTPUT: $CYCLES cycles x ${#ARMS[@]} arms x ${BLOCK}s"
echo "  = $((CYCLES * ${#ARMS[@]} * BLOCK / 60)) minutes, into $JSONL"

c=0
while [ $c -lt "$CYCLES" ]; do
  c=$((c + 1))
  # Reverse the within-cycle order on alternate cycles, so no condition permanently
  # follows the same neighbour and inherits its warm or cold state.
  if [ $((c % 2)) -eq 1 ]; then
    order=("${ARMS[@]}")
  else
    order=(); for ((i = ${#ARMS[@]} - 1; i >= 0; i--)); do order+=("${ARMS[$i]}"); done
  fi
  for a in "${order[@]}"; do
    set_arm "$a" || { echo "could not set arm $a" >&2; exit 1; }
    before=$(perf)
    sleep "$BLOCK"
    after=$(perf)
    jq -n --argjson b "$before" --argjson a "$after" \
          --arg secs "$BLOCK" --arg out "$OUTPUT" --argjson cycle "$c" \
          -f "$HERE/block.jq" | jq -c . >> "$JSONL"
    printf '.'
  done
done
echo
"$HERE/report.sh" "$JSONL"
