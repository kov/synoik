#!/usr/bin/env bash
#
# Drive a deterministic, repeatable workload against a *live* compositor session,
# for frame-timing and latency A/Bs that need the two arms to be comparable.
#
# Why this exists: a human cannot reproduce a workload closely enough to measure
# anything subtle. The continuation-frame miss rate on this stack varies by 100x
# with what is on screen (docs/fork/present-misses.md §14), so an A/B driven by
# hand measures the workload, not the variable under study — which is exactly how
# §15 produced a 2x "effect" that §16 had to retract.
#
# Pointer nudges at ~300 Hz hold a continuous 60 fps flip stream (each nudge
# damages the cursor, so the compositor never goes idle); a scripted heavy action
# fires on each even-second boundary, so misses can be bucketed by time since the
# action. Actions are STATE TRANSITIONS, not repeated keystrokes: from the app
# grid a second double-Super does not close it, so a naive loop parks in the
# overview and measures nothing.
#
# State is pinned as well as actions. The first version of this reproduced the
# keystrokes but not the pixels — one arm ran with the grid left open from an
# earlier test, and the workspace phases visited different workspaces — and the
# comparison was meaningless. Hence the settle preamble, absolute workspace
# references, and the printed baseline.
#
# Usage:  scripts/drive-workload.sh [SEAT_UID] [WORKSPACE] [PROFILE]
#
#   PROFILE=mixed (default)  the five-phase workload: what a session feels like.
#   PROFILE=heavy            sustained animation, for the top of the cost range.
#
# Why `heavy` exists: `mixed` fires one action per 2 s while nudging the cursor
# continuously, so most frames in any 10 s window are cheap cursor repaints and
# the window's MEDIAN draw count stays under ~55 no matter how heavy the action
# was. That makes it blind to the region where the miss rate actually lives
# (§17: 4-26% above 60 draws, versus 0.6% below 40). Holding a UI open does not
# fix it either — a settled overview is static, and cursor damage repaints it at
# a flat 43 draws. Only *continuous transitions* keep a whole window expensive.
#
# Then score both arms with scripts/score-frame-log.py, which will REFUSE to
# compare phases whose element and bake counts disagree — that check is what
# caught both contaminations and is the reason any of this is trustworthy.
# For cost-vs-miss correlations across a whole session, use
# scripts/correlate-frame-log.py, which prints the covered range of every
# predictor: two runs only compare if they spanned the same workload.

set -u
UID_SEAT=${1:-1002}
WS=${2:-1}
PROFILE=${3:-mixed}
RUNTIME=/run/user/$UID_SEAT
SOCK=$(ls "$RUNTIME"/niri.wayland-*.sock 2>/dev/null | head -1)
NIRI=${NIRI_BIN:-$(dirname "$0")/../target/debug/niri}

if [ -z "${SOCK:-}" ]; then echo "no niri socket under $RUNTIME (run as the seat user, or via sudo -u)" >&2; exit 1; fi
export XDG_RUNTIME_DIR=$RUNTIME NIRI_SOCKET=$SOCK
msg() { "$NIRI" msg input "$@" >/dev/null 2>&1; }
act() { "$NIRI" msg action "$@" >/dev/null 2>&1; }
active_ws() { "$NIRI" msg --json workspaces | python3 -c 'import json,sys;print([w["idx"] for w in json.load(sys.stdin) if w["is_active"]][0])'; }
n_windows() { "$NIRI" msg --json windows | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))'; }

# --- settle to a known state; twice, because one Escape only unwinds one level
msg key Escape; sleep 1; msg key Escape; sleep 1
act focus-workspace "$WS"; sleep 2
echo "BASELINE workspace=$(active_ws) windows=$(n_windows)"

nudge_loop() {          # $1 = seconds, $2 = per-2s action | none
  local end=$(( $(date +%s) + $1 )) n=0 last=0 cycle=0 now
  while [ "$(date +%s)" -lt "$end" ]; do
    if [ $(( n % 2 )) -eq 0 ]; then msg pointer-motion -- 3 0; else msg pointer-motion -- -3 0; fi
    now=$(date +%s)
    if [ "$2" != none ] && [ "$now" -ne "$last" ] && [ $(( now % 2 )) -eq 0 ]; then
      case $2 in
        overview)  msg key Super_L ;;                                                    # self-toggling
        grid)      if [ $(( cycle % 2 )) -eq 0 ]; then msg key Super_L; msg key Super_L
                   else msg key Super_L; fi ;;
        workspace) if [ $(( cycle % 2 )) -eq 0 ]; then act focus-workspace $(( WS + 1 ))
                   else act focus-workspace "$WS"; fi ;;                                 # absolute, never relative
      esac
      last=$now; cycle=$((cycle+1))
    fi
    n=$((n+1))
  done
}

# As fast as the compositor will take it, so a transition is always in flight.
# The nudges still matter: they keep damage coming while an action settles.
hold_loop() {           # $1 = seconds, $2 = workspace | overview
  local end=$(( $(date +%s) + $1 )) n=0 i
  while [ "$(date +%s)" -lt "$end" ]; do
    case $2 in
      workspace) if [ $(( n % 2 )) -eq 0 ]; then act focus-workspace $(( WS + 1 ))
                 else act focus-workspace "$WS"; fi
                 msg pointer-motion -- 5 2; msg pointer-motion -- -5 -2 ;;
      overview)  msg key Super_L                                  # self-toggling: open, close, open
                 for i in 1 2 3 4 5 6; do msg pointer-motion -- 6 2; msg pointer-motion -- -6 -2; done ;;
    esac
    n=$((n+1))
  done
}

if [ "$PROFILE" = heavy ]; then
  echo "PHASE1-START $(date +%H:%M:%S)  workspace transitions (sustained)" ; hold_loop 70 workspace
  act focus-workspace "$WS"; sleep 2
  echo "PHASE2-START $(date +%H:%M:%S)  overview transitions (sustained)"  ; hold_loop 70 overview
  msg key Escape; sleep 1; msg key Escape
  echo "DONE $(date +%H:%M:%S) workspace=$(active_ws)"
  exit 0
fi

echo "PHASE1-START $(date +%H:%M:%S)  light (cursor only)"        ; nudge_loop 40 none
echo "PHASE2-START $(date +%H:%M:%S)  overview"                   ; nudge_loop 20 overview ; msg key Escape; sleep 1
echo "PHASE3-START $(date +%H:%M:%S)  app grid"                   ; nudge_loop 20 grid     ; msg key Escape; sleep 1
echo "PHASE4-START $(date +%H:%M:%S)  workspace switch"           ; nudge_loop 20 workspace; act focus-workspace "$WS"
echo "PHASE5-START $(date +%H:%M:%S)  idle"                       ; sleep 30
echo "DONE $(date +%H:%M:%S) workspace=$(active_ws)"
