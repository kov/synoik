# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Shared by the system compositor's harness drivers: where the harness VM lives and how to reach
# it. Sourced, not run. The VM is a Limina guest on a macOS host reached over ssh, running synoik
# as its display manager (`scripts/install-display-manager.sh`) with the outer's flight recorder
# on (`RUST_LOG=synoik=debug` on `synoik-system.service`).
#
# Every setting is an environment variable with the harness's defaults:
#   HARNESS_HOST      the macOS host                       (alface)
#   HARNESS_DIR       the VM's directory on the host        (/Users/kov/Projects/synoik-harness)
#   HARNESS_DISK      the boot disk, relative to it         (m0.raw)
#   HARNESS_SSH_PORT  the guest's forwarded ssh port        (2300)
#   HARNESS_DMG       the Limina build, mounted at $HARNESS_DIR/mnt when nothing is there yet
#                     (the newest ~/Projects/LiminaParkingLot/Limina-*.dmg)
#   HARNESS_SIZE      the display mode                      (1280x800)
#   HARNESS_LIMINA_ARGS  more launch flags, e.g. `--machine`
#                     (none; the ACPI machine, whose firmware publishes its logo as BGRT, is the default)

HARNESS_HOST=${HARNESS_HOST:-alface}
HARNESS_DIR=${HARNESS_DIR:-/Users/kov/Projects/synoik-harness}
HARNESS_DISK=${HARNESS_DISK:-m0.raw}
HARNESS_SSH_PORT=${HARNESS_SSH_PORT:-2300}
HARNESS_SIZE=${HARNESS_SIZE:-1280x800}
LIMINA="$HARNESS_DIR/mnt/Limina.app/Contents/MacOS/limina"

die() {
    echo "harness: $*" >&2
    exit 1
}

say() {
    echo "harness: $*" >&2
}

on_host() {
    ssh "$HARNESS_HOST" "$@"
}

GUEST_SSH=(ssh -J "$HARNESS_HOST" -p "$HARNESS_SSH_PORT" -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 root@127.0.0.1)

on_guest() {
    "${GUEST_SSH[@]}" "$@"
}

# A `driver.jsonl` line tying the running guest boot's monotonic clock to the host's wall clock
# (clock.py): how report.py puts that boot's journal on the capture's clock.
clock_line() {
    python3 "$(dirname "${BASH_SOURCE[0]}")/clock.py" ssh "$HARNESS_HOST" -- "${GUEST_SSH[@]}"
}

# The running VM's supervisor pid, which `limina debug` and `limina input` take; empty if none.
supervisor() {
    on_host "pgrep -f -- 'limina --disk $HARNESS_DISK ' | head -1"
}

# Mount the Limina build if nothing is mounted yet.
mount_limina() {
    on_host "cd '$HARNESS_DIR' && { [ -x '$LIMINA' ] || hdiutil attach -nobrowse -readonly \
        -mountpoint mnt \"\${HARNESS_DMG:-\$(ls -t ~/Projects/LiminaParkingLot/Limina-*.dmg | head -1)}\" \
        >/dev/null; }" || die "cannot mount a Limina build"
}

# Boot the VM unless it runs, and wait for its ssh. `HARNESS_LAUNCH_ENV` goes in front of the
# launch, e.g. Limina's whole-lifetime video capture.
boot() {
    [ -n "$(supervisor)" ] && return
    mount_limina
    say "booting $HARNESS_DISK"
    on_host "cd '$HARNESS_DIR' && K=\$(cat ~/.ssh/id_ed25519.pub) && \
        LIMINA_INPUT_INJECT=1 LIMINA_DEBUG_PORT=1 ${HARNESS_LAUNCH_ENV:-} '$LIMINA' --disk '$HARNESS_DISK' --window --net \
        --ssh-port $HARNESS_SSH_PORT --display-resolution $HARNESS_SIZE --no-fingerprint --no-fido \
        --console '${HARNESS_DISK%.raw}.console' ${HARNESS_LIMINA_ARGS:-} \
        --smbios-oem-string \"io.systemd.credential:ssh.authorized_keys.root=\$K\" --detach && \
        ~/Projects/limina/scripts/wait-guest-ssh.sh '$HARNESS_DISK.limina.log' 170 >/dev/null 2>&1" \
        || die "the VM did not come up"
}

# Wait until the running outer — this invocation of its unit, not an earlier one this boot — has
# a session attached, then let it settle for $1 seconds (default 3) so its frames are on screen.
wait_session() {
    local i
    for i in $(seq 120); do
        if on_guest 'inv=$(systemctl show -p InvocationID --value synoik-system) && [ -n "$inv" ] &&
            journalctl -b _SYSTEMD_INVOCATION_ID=$inv -o cat | grep -q "attached (pid"' 2>/dev/null; then
            sleep "${1:-3}"
            return
        fi
        sleep 1
    done
    die "no session attached to the outer"
}

# Wait for the VM to power off, and print a `driver.jsonl` line for the moment it did (its
# process gone, polled every 20 ms on the host).
wait_gone() {
    local pid=$1
    on_host "for i in \$(seq $((${2:-120} * 50))); do kill -0 $pid 2>/dev/null || break; sleep 0.02; done
        kill -0 $pid 2>/dev/null && exit 1
        python3 -c 'import json,time; print(json.dumps({\"step\": \"vm-gone\", \"t_realtime_ns\": time.time_ns()}))'" \
        || die "the VM is still running after ${2:-120} s"
}

# A host clock reading taken right before `cmd` runs on the host, as one `driver.jsonl` line
# (the capture's `t_realtime_ns` is on the same clock).
mark_on_host() {
    local step=$1
    shift
    on_host "python3 -c 'import json,sys,time; print(json.dumps({\"step\": sys.argv[1], \"t_realtime_ns\": time.time_ns()}))' '$step' && { $*; } >&2"
}

# The seat0 session of user $1 (`greeter` for the greeter's), from logind; empty if none.
seat_session() {
    on_guest "loginctl list-sessions --json=short" | python3 -c '
import json, sys
want = sys.argv[1]
for s in json.load(sys.stdin):
    if s["seat"] == "seat0" and (s["class"] == "greeter" if want == "greeter" else s["user"] == want):
        print(s["session"])
        break' "$1"
}

# Run a command in user $1's seat0 session (`synoik msg …` reaches that session's compositor).
as_user() {
    local user=$1
    shift
    on_guest "u=\$(id -u $user) && S=\$(ls /run/user/\$u/synoik.*.sock | head -1) &&
        setpriv --reuid $user --regid $user --init-groups env SYNOIK_SOCKET=\$S $*"
}

# The session the running outer fronts now, from its journal; empty for none.
fronted_session() {
    on_guest 'inv=$(systemctl show -p InvocationID --value synoik-system) &&
        journalctl -b _SYSTEMD_INVOCATION_ID=$inv -o cat | grep "seat0 fronts session" | tail -1' |
        sed -n 's/.*fronts session Some("\([^"]*\)").*/\1/p'
}

# Wait until the outer fronts session $1, then let it settle for $2 seconds (default 3).
wait_fronted() {
    local i
    for i in $(seq 120); do
        if [ "$(fronted_session)" = "$1" ]; then
            sleep "${2:-3}"
            return
        fi
        sleep 0.5
    done
    die "the outer does not front session $1"
}

# Start recording every frame on the screen of supervisor $1 into run $2's capture directory,
# with a still of what is on it now as `before.png`: a still screen presents no frame, so a
# transition's first end is otherwise not in the run.
capture_start() {
    on_host "mkdir -p '$HARNESS_DIR/captures' &&
        '$LIMINA' debug $1 capture start '$HARNESS_DIR/captures/$2' >/dev/null &&
        '$LIMINA' debug $1 capture still '$HARNESS_DIR/captures/$2/before.png' >/dev/null"
    sleep 1
}

# Stop the recording of run $2 on supervisor $1 and bring its frames into directory $3.
capture_stop() {
    on_host "'$LIMINA' debug $1 capture stop >/dev/null"
    scp -q "$HARNESS_HOST:$HARNESS_DIR/captures/$2/*" "$3/"
    on_host "rm -r '$HARNESS_DIR/captures/$2'"
}

# This boot's log lines that report.py reads: the outer's, the sessions', logind's and pid 1's,
# as `journalctl -o json`. The whole boot, since the outer names each framebuffer once.
boot_journal() {
    on_guest "journalctl -b ${1:-} -o json _COMM=synoik + _COMM=synoik-login + \
        _SYSTEMD_UNIT=systemd-logind.service + _COMM=gnome-session-b + _PID=1"
}

# Make the outer draw switches as $1 (fade, curtain or depth; `SYNOIK_SYSTEM_TRANSITION`) through
# a drop-in, restarting it only if that changes what it runs with; the sessions reattach.
set_transition() {
    local style=$1 dropin=/etc/systemd/system/synoik-system.service.d/90-harness-transition.conf
    case $style in fade | curtain | depth) ;; *) die "a transition is fade, curtain or depth" ;; esac
    local want="[Service]
Environment=SYNOIK_SYSTEM_TRANSITION=$style"
    if [ "$(on_guest "cat $dropin 2>/dev/null")" != "$want" ]; then
        say "the outer draws switches as $style from now"
        on_guest "mkdir -p $(dirname $dropin) && printf '%s\n' '$want' >$dropin &&
            systemctl daemon-reload && systemctl restart synoik-system"
        wait_session 3
    fi
}
