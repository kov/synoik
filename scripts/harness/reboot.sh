#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Reboot the harness VM (common.sh) and record every frame on its screen through both ends: the
# shutdown fade and splash, the firmware, the initramfs splash, its handover to the outer, and
# the fade from the splash to the greeter or the autologin session — for `report.py`.
#
#   scripts/harness/reboot.sh [--settle SECONDS] [--out DIR]
#
#   --settle   how long to keep recording after the session's first frame (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#
# A run is laid out as shutdown.sh's, with both boots' journals in journal.jsonl and a clock line
# for each in driver.jsonl.
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

run=reboot-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"
capture="$HARNESS_DIR/captures/$run"

boot
wait_session 3
pid=$(supervisor)
went_down=$(on_guest 'cat /proc/sys/kernel/random/boot_id' | tr -d -)
clock_line >"$out/driver.jsonl"
on_host "mkdir -p '$HARNESS_DIR/captures' && '$LIMINA' debug $pid capture start '$capture' >/dev/null"
sleep 1
mark_on_host reboot true >>"$out/driver.jsonl"
on_guest 'systemctl --no-block reboot' || true

say "waiting for the guest to come back"
sleep 5
i=0
until on_guest true 2>/dev/null; do
    i=$((i + 1))
    [ $i -lt 90 ] || die "the guest did not come back"
    sleep 2
done
[ "$(supervisor)" = "$pid" ] || die "the VM restarted instead of rebooting"
wait_session "$settle"
mark_on_host settled true >>"$out/driver.jsonl"
clock_line >>"$out/driver.jsonl"
on_host "'$LIMINA' debug $pid capture stop >/dev/null"

scp -q "$HARNESS_HOST:$capture/*" "$out/"
on_host "rm -r '$capture'"
came_up=$(on_guest 'cat /proc/sys/kernel/random/boot_id' | tr -d -)
for b in "$went_down" "$came_up"; do
    on_guest "journalctl -b $b -o json _COMM=synoik + _COMM=synoik-splash + \
        _SYSTEMD_UNIT=systemd-logind.service + _PID=1"
done >"$out/journal.jsonl"
say "run in $out"
echo "$out"
