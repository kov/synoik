#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Run an offline update on the harness VM (common.sh) and record every frame on its screen
# through all three boots' worth of it: the shutdown into the update, the update boot's splash,
# handover and update screen, its reboot, and the boot after — for `report.py`.
#
#   scripts/harness/update.sh [--settle SECONDS] [--out DIR]
#
#   --settle   how long to keep recording after the session's first frame (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#
# Stages `dnf5 reinstall --offline which` unless a transaction is staged already. The harness is
# SELinux enforcing, where every plymouth client is denied the outer's socket (boot-splash.md §6),
# so the default entry boots with `enforcing=0` for the run; the argument is removed after.
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

run=update-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"
capture="$HARNESS_DIR/captures/$run"

boot
wait_session 3
pid=$(supervisor)
on_guest 'dnf5 offline status | grep -q "dnf5 offline reboot" || dnf5 reinstall --offline -y which >/dev/null' ||
    die "cannot stage an offline transaction"
on_guest 'grubby --update-kernel=DEFAULT --args=enforcing=0'
trap "on_guest 'grubby --update-kernel=DEFAULT --remove-args=enforcing=0' || true" EXIT
went_down=$(on_guest 'cat /proc/sys/kernel/random/boot_id' | tr -d -)
clock_line >"$out/driver.jsonl"
on_host "mkdir -p '$HARNESS_DIR/captures' && '$LIMINA' debug $pid capture start '$capture' >/dev/null"
sleep 1
mark_on_host reboot true >>"$out/driver.jsonl"
on_guest 'systemd-run --no-block dnf5 offline reboot -y' >/dev/null || true

# The update boot has no sshd (system-update.target pulls in no multi-user units): the first
# boot that answers is the one after it.
say "waiting for the update to run and the guest to come back"
sleep 10
i=0
until on_guest true 2>/dev/null; do
    i=$((i + 1))
    [ $i -lt 300 ] || die "the guest did not come back"
    sleep 2
done
[ "$(supervisor)" = "$pid" ] || die "the VM restarted instead of rebooting"
wait_session "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
clock_line >>"$out/driver.jsonl"
on_host "'$LIMINA' debug $pid capture stop >/dev/null"

scp -q "$HARNESS_HOST:$capture/*" "$out/"
on_host "rm -r '$capture'"
on_guest "journalctl --list-boots -o json" >"$out/boots.json"
for b in $(python3 -c '
import json, sys
boots = json.load(open(sys.argv[1]))
ids = [b["boot_id"] for b in boots]
print(" ".join(ids[ids.index(sys.argv[2]):]))' "$out/boots.json" "$went_down"); do
    on_guest "journalctl -b $b -o json _COMM=synoik + _COMM=synoik-splash + \
        _SYSTEMD_UNIT=systemd-logind.service + _PID=1 + _COMM=dnf5"
done >"$out/journal.jsonl"
say "run in $out"
echo "$out"
