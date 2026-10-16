#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Switch User from u2's session on the harness VM (common.sh), and record every frame on its screen
# until the greeter is up, for `report.py`.
#
#   scripts/harness/switch-user.sh [--transition STYLE] [--settle SECONDS] [--out DIR]
#
#   --settle   how long to keep recording once the outer fronts the greeter (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#   --transition  how the outer draws switches: fade, curtain or depth (default: as it is)
#
# u2 is logged in afresh and fronted first: a session left locked by an earlier run would refuse
# the action. The switch is the power menu's: lock, then the login screen. A run is laid out as
# shutdown.sh's.
set -euo pipefail
. "$(dirname "$0")/common.sh"

settle=3
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

run=switch-user${transition:+-$transition}-$(date +%Y%m%d-%H%M%S)
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
say "logging u2 in"
on_guest 'synoik-login open-session u2 --background' >&2
for _ in $(seq 60); do [ -n "$(seat_session u2)" ] && break; sleep 0.5; done
session=$(seat_session u2)
[ -n "$session" ] || die "u2 did not log in"
on_guest "loginctl activate $session"
wait_fronted "$session" 3

clock_line >"$out/driver.jsonl"
capture_start "$pid" "$run"
{
    mark_on_host switch-user-action true
    as_user u2 /usr/local/bin/synoik msg action switch-user >&2
} >>"$out/driver.jsonl"
say "waiting for the greeter"
greeter=$(seat_session greeter)
[ -n "$greeter" ] || die "no greeter"
wait_fronted "$greeter" "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
capture_stop "$pid" "$run" "$out"
boot_journal >"$out/journal.jsonl"
say "run in $out"
echo "$out"
