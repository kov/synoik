#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Log u2 in through the greeter on the harness VM (common.sh) and record every frame on its
# screen, from the greeter through the new session's startup, for `report.py`.
#
#   scripts/harness/login.sh [--transition STYLE] [--settle SECONDS] [--out DIR]
#
#   --settle   how long to keep recording once the outer fronts the new session (default 5)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#   --transition  how the outer draws switches: fade, curtain or depth (default: as it is)
#
# u2 is logged out first if it has a session, and the greeter fronted. A Shift lifts the greeter's
# shield if it is up, which takes the first key and does nothing else with it (a Shift on the grid
# does nothing either); then u2 is picked with two Lefts from wherever the grid's focus is (its
# tile is the first of the row, Left does not wrap) and Enter, and its password typed. A run is
# laid out as shutdown.sh's.
set -euo pipefail
. "$(dirname "$0")/common.sh"

settle=5
out=
transition=
while [ $# -gt 0 ]; do
    case $1 in
    --transition) transition=$2; shift 2 ;;
    --settle) settle=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument $1" ;;
    esac
done

run=login${transition:+-$transition}-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"

boot
wait_session 1
[ -z "$transition" ] || set_transition "$transition"
pid=$(supervisor)
if [ -n "$(seat_session u2)" ]; then
    say "logging u2 out first"
    on_guest 'loginctl terminate-user u2'
    for _ in $(seq 60); do [ -z "$(seat_session u2)" ] && break; sleep 0.5; done
fi
greeter=$(seat_session greeter)
[ -n "$greeter" ] || die "no greeter session"
on_guest "loginctl activate $greeter"
wait_fronted "$greeter" 3

clock_line >"$out/driver.jsonl"
capture_start "$pid" "$run"
{
    mark_on_host pick-u2 "printf 'key tap leftshift\nsleep 500\nkey tap left\nkey tap left\nsleep 300\nkey tap enter\n' |
        '$LIMINA' input $pid -"
    sleep 1.5
    mark_on_host password "printf 'type synoik-harness\nkey tap enter\n' | '$LIMINA' input $pid -"
} >>"$out/driver.jsonl"
say "waiting for u2's session"
for _ in $(seq 60); do
    session=$(seat_session u2)
    [ -n "$session" ] && break
    sleep 0.5
done
[ -n "$session" ] || die "u2 did not log in"
wait_fronted "$session" "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
capture_stop "$pid" "$run" "$out"
boot_journal >"$out/journal.jsonl"
say "run in $out"
echo "$out"
