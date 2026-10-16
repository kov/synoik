#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Log u2 out on the harness VM (common.sh) through the session's own path, and record every frame
# on its screen until the greeter is back, for `report.py`.
#
#   scripts/harness/logout.sh [--settle SECONDS] [--out DIR]
#
#   --settle   how long to keep recording once the outer fronts the greeter (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#
# u2 is logged in and fronted first if it is not. The logout is the power menu's: gnome-session's
# confirmation dialog, confirmed with Enter. A run is laid out as shutdown.sh's.
set -euo pipefail
. "$(dirname "$0")/common.sh"

settle=3
out=
while [ $# -gt 0 ]; do
    case $1 in
    --settle) settle=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument $1" ;;
    esac
done

run=logout-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"

boot
wait_session 1
pid=$(supervisor)
if [ -z "$(seat_session u2)" ]; then
    say "logging u2 in"
    on_guest 'synoik-login open-session u2 --background' >&2
    for _ in $(seq 60); do [ -n "$(seat_session u2)" ] && break; sleep 0.5; done
fi
session=$(seat_session u2)
[ -n "$session" ] || die "u2 did not log in"
on_guest "loginctl activate $session"
wait_fronted "$session" 3

clock_line >"$out/driver.jsonl"
capture_start "$pid" "$run"
{
    mark_on_host logout-action true
    as_user u2 /usr/local/bin/synoik msg action logout >&2
    # The dialog opens and its countdown starts; confirm well before it runs out.
    sleep 1.5
    mark_on_host confirm "echo 'key tap enter' | '$LIMINA' input $pid -"
} >>"$out/driver.jsonl"
say "waiting for the greeter"
for _ in $(seq 60); do [ -z "$(seat_session u2)" ] && break; sleep 0.5; done
greeter=$(seat_session greeter)
[ -n "$greeter" ] || die "no greeter after the logout"
wait_fronted "$greeter" "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
capture_stop "$pid" "$run" "$out"
boot_journal >"$out/journal.jsonl"
say "run in $out"
echo "$out"
