#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Install a synoik build on the harness VM (common.sh) and put it to work.
#
#   scripts/harness/deploy.sh [--reboot] [BINARY]
#
# BINARY defaults to this checkout's target/release/synoik. It is stripped, sent compressed and
# installed over /usr/local/bin/synoik (the previous one kept as synoik.prev), relabelled for
# SELinux. Then the outer restarts, handing the screen over through the fd store, and the
# sessions reattach to it — they keep running the binary they started with, which is enough for
# the outer's own behaviour; `--reboot` starts everything on the new one instead.
set -euo pipefail
. "$(dirname "$0")/common.sh"

reboot=
if [ "${1:-}" = --reboot ]; then
    reboot=1
    shift
fi
binary=${1:-$(dirname "$0")/../../target/release/synoik}
[ -x "$binary" ] || die "no binary at $binary"

boot
stripped=$(mktemp)
trap 'rm -f "$stripped"' EXIT
strip -o "$stripped" "$binary"
say "installing $("$binary" --version)"
zstd -q -c "$stripped" | on_guest 'mkdir -p /root/stage && zstd -q -d -f -o /root/stage/synoik.new &&
    cp -p /usr/local/bin/synoik /usr/local/bin/synoik.prev &&
    install -m 755 /root/stage/synoik.new /usr/local/bin/synoik && rm /root/stage/synoik.new &&
    restorecon /usr/local/bin/synoik /usr/local/bin/synoik.prev'
if [ -n "$reboot" ]; then
    boot=$(on_guest 'cat /proc/sys/kernel/random/boot_id')
    # The connection drops as the guest goes down, so the call's status says nothing; the boot id
    # afterwards does. An inhibitor (an unsaved document blocking the session's shutdown) refuses
    # the reboot outright, and the old binary would go on running.
    on_guest 'systemctl reboot' || true
    pid=$(supervisor)
    sleep 5
    i=0
    until on_guest true 2>/dev/null; do
        i=$((i + 1))
        [ $i -lt 90 ] || die "the guest did not come back"
        sleep 2
    done
    [ "$(supervisor)" = "$pid" ] || die "the VM restarted instead of rebooting"
    [ "$(on_guest 'cat /proc/sys/kernel/random/boot_id')" != "$boot" ] ||
        die "the guest did not reboot: $(on_guest 'systemd-inhibit --list --no-pager' | grep block)"
else
    on_guest 'systemctl restart synoik-system'
fi
wait_session 1
on_guest '/usr/local/bin/synoik --version'
