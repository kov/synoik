#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Run an offline update on the harness VM (common.sh) and record every frame on its screen
# through all three boots' worth of it: the shutdown into the update, the update boot's splash,
# handover and update screen, its reboot, and the boot after — for `report.py`.
#
#   scripts/harness/update.sh [--fake] [--transition STYLE] [--settle SECONDS] [--out DIR]
#
#   --fake     no package manager: a unit drives the splash as dnf5 would, but stops the bar at
#              40 % before changing to the reboot screen, as a real update may (boot-splash.md
#              §4.3); the package managers' offline units are masked meanwhile
#   --transition  how the outer draws switches and the splash's mode changes: fade, curtain or
#              depth (default: as it is)
#   --settle   how long to keep recording after the session's first frame (default 3)
#   --out      where the run lands (default: target/harness/<run> in this checkout)
#
# Without `--fake`, stages `dnf5 reinstall --offline which` unless a transaction is staged
# already. The harness is SELinux enforcing, where every plymouth client is denied the outer's
# socket (boot-splash.md §6), so the default entry boots with `enforcing=0` for the run; the
# argument is removed after.
set -euo pipefail
. "$(dirname "$0")/common.sh"

settle=3
out=
fake=
transition=
while [ $# -gt 0 ]; do
    case $1 in
    --fake) fake=1; shift ;;
    --transition) transition=$2; shift 2 ;;
    --settle) settle=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument $1" ;;
    esac
done

run=update${transition:+-$transition}-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"
capture="$HARNESS_DIR/captures/$run"

boot
wait_session 3
[ -z "$transition" ] || set_transition "$transition"
pid=$(supervisor)
offline_units="dnf5-offline-transaction.service packagekit-offline-update.service dnf-system-upgrade.service"
fake_unit=/etc/systemd/system/harness-fake-update.service
if [ -n "$fake" ]; then
    on_guest "systemctl mask --quiet $offline_units && cat >$fake_unit && systemctl daemon-reload &&
        systemctl enable --quiet harness-fake-update.service" <<'UNIT'
[Unit]
Description=harness: a fake offline update
DefaultDependencies=no
Requires=sysinit.target
After=sysinit.target system-update-pre.target
Before=shutdown.target system-update.target
ConditionPathExists=/system-update

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'plymouth display-message --text="Starting offline transaction. This will take a while."; sleep 1; \
    plymouth system-update --progress=0; plymouth change-mode --updates; sleep 1; \
    for p in 15 40; do plymouth system-update --progress=$$p; sleep 1; done; \
    plymouth display-message --text="Transaction complete! Cleaning up and rebooting..."; \
    plymouth change-mode --reboot; rm -f /system-update; systemctl reboot'

[Install]
WantedBy=system-update.target
UNIT
else
    on_guest 'dnf5 offline status | grep -q "dnf5 offline reboot" || dnf5 reinstall --offline -y which >/dev/null' ||
        die "cannot stage an offline transaction"
fi
on_guest 'grubby --update-kernel=DEFAULT --args=enforcing=0'
cleanup() {
    on_guest 'grubby --update-kernel=DEFAULT --remove-args=enforcing=0' || true
    if [ -n "$fake" ]; then
        on_guest "systemctl disable --quiet harness-fake-update.service; rm -f $fake_unit /system-update;
            systemctl unmask --quiet $offline_units; systemctl daemon-reload" || true
    fi
}
trap cleanup EXIT
went_down=$(on_guest 'cat /proc/sys/kernel/random/boot_id' | tr -d -)
clock_line >"$out/driver.jsonl"
on_host "mkdir -p '$HARNESS_DIR/captures' && '$LIMINA' debug $pid capture start '$capture' >/dev/null"
sleep 1
mark_on_host reboot true >>"$out/driver.jsonl"
if [ -n "$fake" ]; then
    on_guest 'ln -sfn /var/empty /system-update && systemctl --no-block reboot' || true
else
    on_guest 'systemd-run --no-block dnf5 offline reboot -y' >/dev/null || true
fi

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
