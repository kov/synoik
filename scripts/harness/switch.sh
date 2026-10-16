#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Switch the harness VM (common.sh) between two running user sessions and record every frame on
# its screen through the outer's transition, for `report.py`.
#
#   scripts/harness/switch.sh [--to USER] [--settle SECONDS] [--out DIR]
#
#   --to       the session to switch to: u2 (default) or claude; the other one is fronted first
#   --settle   how long to keep recording once the outer fronts it (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#
# u2 is logged in in the background first if it has no session (`synoik-login open-session`).
# The switch is logind's `ActivateSession`, as the greeter's unlock of a running session and
# GNOME's own switch both are. A run is laid out as shutdown.sh's.
set -euo pipefail
. "$(dirname "$0")/common.sh"

to=u2
settle=3
out=
while [ $# -gt 0 ]; do
    case $1 in
    --to) to=$2; shift 2 ;;
    --settle) settle=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument $1" ;;
    esac
done
case $to in u2) from=claude ;; claude) from=u2 ;; *) die "--to is u2 or claude" ;; esac

run=switch-$to-$(date +%Y%m%d-%H%M%S)
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
source_session=$(seat_session $from)
target=$(seat_session $to)
[ -n "$source_session" ] && [ -n "$target" ] || die "no sessions for $from and $to"
on_guest "loginctl activate $source_session"
wait_fronted "$source_session" 3

clock_line >"$out/driver.jsonl"
capture_start "$pid" "$run"
mark_on_host activate true >>"$out/driver.jsonl"
on_guest "loginctl activate $target"
wait_fronted "$target" "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
capture_stop "$pid" "$run" "$out"
boot_journal >"$out/journal.jsonl"
say "run in $out"
echo "$out"
