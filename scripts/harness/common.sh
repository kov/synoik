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

on_guest() {
    ssh -J "$HARNESS_HOST" -p "$HARNESS_SSH_PORT" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 root@127.0.0.1 "$@"
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

# Boot the VM unless it runs, and wait for its ssh.
boot() {
    [ -n "$(supervisor)" ] && return
    mount_limina
    say "booting $HARNESS_DISK"
    on_host "cd '$HARNESS_DIR' && K=\$(cat ~/.ssh/id_ed25519.pub) && \
        LIMINA_INPUT_INJECT=1 LIMINA_DEBUG_PORT=1 '$LIMINA' --disk '$HARNESS_DISK' --window --net \
        --ssh-port $HARNESS_SSH_PORT --display-resolution $HARNESS_SIZE --no-fingerprint --no-fido \
        --console '${HARNESS_DISK%.raw}.console' \
        --smbios-oem-string \"io.systemd.credential:ssh.authorized_keys.root=\$K\" --detach && \
        ~/Projects/limina/scripts/wait-guest-ssh.sh '$HARNESS_DISK.limina.log' 170 >/dev/null 2>&1" \
        || die "the VM did not come up"
}

# Wait until the outer has a session attached that shows frames: the user session after an
# autologin, or the greeter.
wait_session() {
    local i
    for i in $(seq 120); do
        if on_guest 'journalctl -b -u synoik-system -o cat | grep -q "the splash is over"'; then
            sleep "${1:-3}"
            return
        fi
        sleep 1
    done
    die "no session reached the screen"
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
