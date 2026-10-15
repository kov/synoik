#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Power the harness VM (common.sh) off from a running session and record every frame on its
# screen until the power goes, for `report.py`.
#
#   scripts/harness/shutdown.sh [--via ui|systemctl] [--out DIR]
#
#   --via ui         the session's power menu path: the end-session dialog, confirmed with Enter
#                    (gnome-session ends the session, then asks logind to power off). Default.
#   --via systemctl  `systemctl poweroff` as root: logind's PrepareForShutdown with the sessions
#                    still up, the order an update or an admin's command takes.
#   --out DIR        where the run lands (default: target/harness/<run> in this checkout)
#
# The VM is booted first if it is not running, and booted again at the end to read the
# shutdown's journal (it needs the persistent journal of the boot that went down). A run is:
#   frames.jsonl, s0-*.png   Limina's capture: one record and PNG per frame that reached the
#                            screen, on the host's clock
#   driver.jsonl             what the driver did, and when, on the same clock, and the boot's
#                            clock line (clock.py)
#   journal.jsonl            the outer's and the sessions' log lines and logind's, from the boot
#                            that went down (`journalctl -o json`)
set -euo pipefail
. "$(dirname "$0")/common.sh"

via=ui
out=
while [ $# -gt 0 ]; do
    case $1 in
    --via) via=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument $1" ;;
    esac
done
case $via in ui | systemctl) ;; *) die "--via is ui or systemctl" ;; esac

run=shutdown-$via-$(date +%Y%m%d-%H%M%S)
out=${out:-$(dirname "$0")/../../target/harness/$run}
mkdir -p "$out"
capture="$HARNESS_DIR/captures/$run"

boot
wait_session 3
pid=$(supervisor)
on_guest 'cat /proc/sys/kernel/random/boot_id' | tr -d - >"$out/boot_id"

clock_line >"$out/driver.jsonl"
on_host "mkdir -p '$HARNESS_DIR/captures' && '$LIMINA' debug $pid capture start '$capture' >/dev/null"
sleep 1
{
    case $via in
    ui)
        mark_on_host power-off-action true
        on_guest 'S=$(ls /run/user/1000/synoik.*.sock | head -1) &&
            setpriv --reuid claude --regid claude --init-groups env SYNOIK_SOCKET=$S \
            /usr/local/bin/synoik msg action power-off'
        # The dialog opens and its countdown starts; confirm well before it runs out.
        sleep 1.5
        mark_on_host confirm "echo 'key tap enter' | '$LIMINA' input $pid -"
        ;;
    systemctl)
        mark_on_host systemctl-poweroff true
        on_guest 'systemctl --no-block poweroff' || true
        ;;
    esac
} >>"$out/driver.jsonl"
say "waiting for the power to go"
wait_gone "$pid" >>"$out/driver.jsonl"

scp -q "$HARNESS_HOST:$capture/*" "$out/"
on_host "rm -r '$capture'"

boot
went_down=$(cat "$out/boot_id")
on_guest "journalctl -b $went_down -o json _COMM=synoik + _SYSTEMD_UNIT=systemd-logind.service \
    + _COMM=gnome-session-b + _PID=1" >"$out/journal.jsonl"
say "run in $out"
echo "$out"
