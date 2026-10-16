#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# The tour of every switch the outer draws, on the harness VM (common.sh), for watching or
# recording from the host: boot to the greeter, log u2 in, Switch User, log claude in, log claude
# out, log u2 back in, power off.
#
#   scripts/harness/tour.sh prep STYLE   boot, make the outer draw switches as STYLE (fade, curtain
#                                        or depth), turn autologin off, power off
#   scripts/harness/tour.sh run          boot and drive the tour, until the VM powers off
#   scripts/harness/tour.sh restore      boot, autologin back on, the outer's default style
#
# `prep` leaves the VM off, so a recording can start before `run` boots it. Nothing is captured
# here: this is for eyes, the other drivers are for `report.py`.
set -euo pipefail
. "$(dirname "$0")/common.sh"

dropin=/etc/systemd/system/synoik-system.service.d/90-harness-transition.conf

# Keys for the VM, as `limina input` reads them. Piped on the host: our stdin does not reach it.
keys() { on_host "printf '$1' | '$LIMINA' input $(supervisor) -"; }

# Pick a user on the greeter's grid and give the password: 0 is u2 (the left tile), 1 claude.
# A Shift lifts the greeter's shield; two Lefts reach the first tile from wherever focus is.
pick() {
    local right=
    [ "$1" = 1 ] && right='key tap right\n'
    keys "key tap leftshift\nsleep 500\nkey tap left\nkey tap left\n${right}sleep 300\nkey tap enter\n"
    sleep 1.5
    keys 'type synoik-harness\nkey tap enter\n'
}

# Wait for user $1's session to be on screen, then $2 seconds more.
wait_user() {
    local s=
    for _ in $(seq 120); do
        s=$(seat_session "$1")
        [ -n "$s" ] && break
        sleep 0.5
    done
    [ -n "$s" ] || die "$1 did not log in"
    wait_fronted "$s" "$2"
}

greeter_up() { wait_fronted "$(seat_session greeter)" "$1"; }

power_off() {
    local pid
    pid=$(supervisor)
    on_guest 'systemctl poweroff' || true
    wait_gone "$pid" >/dev/null
}

autologin() {
    on_guest "sed -i 's/^AutomaticLoginEnable=.*/AutomaticLoginEnable=$1/' /etc/synoik/login.conf"
}

case ${1:-} in
prep)
    case ${2:-} in fade | curtain | depth) ;; *) die "prep takes fade, curtain or depth" ;; esac
    boot
    autologin false
    on_guest "mkdir -p $(dirname $dropin) &&
        printf '[Service]\nEnvironment=SYNOIK_SYSTEM_TRANSITION=$2\n' >$dropin &&
        systemctl daemon-reload"
    power_off
    say "ready for a $2 tour, the VM off"
    ;;
run)
    boot
    wait_session 1
    # The greeter takes keys only a few seconds after its first frame.
    greeter_up 6
    say "logging u2 in"
    pick 0
    wait_user u2 4
    say "switching user"
    as_user u2 /usr/local/bin/synoik msg action switch-user >&2
    greeter_up 2
    say "logging claude in"
    pick 1
    wait_user claude 4
    say "logging claude out"
    as_user claude /usr/local/bin/synoik msg action logout >&2
    sleep 1.5
    keys 'key tap enter\n'
    for _ in $(seq 60); do
        [ -z "$(seat_session claude)" ] && break
        sleep 0.5
    done
    greeter_up 2
    say "logging u2 in again"
    pick 0
    wait_user u2 3
    say "powering off"
    pid=$(supervisor)
    as_user u2 /usr/local/bin/synoik msg action power-off >&2
    sleep 1.5
    keys 'key tap enter\n'
    wait_gone "$pid" >/dev/null
    say "tour done, the VM off"
    ;;
restore)
    boot
    autologin true
    on_guest "rm -f $dropin && systemctl daemon-reload"
    say "restored; autologin applies from the next boot"
    ;;
*) die "usage: tour.sh prep STYLE | run | restore" ;;
esac
