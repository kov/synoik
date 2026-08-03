#!/bin/bash
# Bring up an isolated headless niri: private PipeWire, private session bus, private runtime dir.
#
# Nothing here touches your real session. The runtime dir must stay SHORT — a long one overflows
# the sockaddr_un path limit and the Wayland/PipeWire sockets silently fail to bind.
#
# NEVER pattern-kill anything from these scripts. They may run as the same user that owns your
# real desktop session, so `pgrep -u "$USER" -x pipewire` matches that session's daemon too —
# killing it is a live-session outage. Every process started here appends its PID to $R/pids and
# stop.sh kills only those.
set -u
R=${NH_DIR:-/tmp/nh}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
NIRI=${NIRI_BIN:-$ROOT/target/debug/niri}

[ -x "$NIRI" ] || { echo "no niri binary at $NIRI (cargo build --bin niri)"; exit 1; }

mkdir -p "$R/config" && chmod 700 "$R"
export XDG_RUNTIME_DIR=$R PIPEWIRE_RUNTIME_DIR=$R XDG_CONFIG_HOME=$R/config
export DBUS_SESSION_BUS_ADDRESS=unix:path=$R/bus
export RUST_LOG=${RUST_LOG:-niri=info,niri::screencasting=trace}

# The screencast D-Bus interfaces are session-instance only unless this is set.
export NIRI_DEBUG_DBUS_INTERFACES_IN_NON_SESSION_INSTANCES=1

: > "$R/pids"
rm -f "$R"/pipewire-0* "$R"/wayland-* "$R"/niri.*.sock "$R"/*.log

pipewire > "$R/pw.log" 2>&1 & echo $! >> "$R/pids"
sleep 1
# A session manager is REQUIRED, and its absence looks nothing like its cause: the cast starts, a
# node id is emitted, a consumer connects without error — and then blocks forever, because nobody
# ever links the two nodes. That cost an afternoon and a wrong diagnosis ("headless never
# renders"). `-p policy` loads only the linking policy: no ALSA, no bluez, no camera monitors, so
# this instance cannot touch the real session's hardware.
wireplumber -p policy > "$R/wp.log" 2>&1 & echo $! >> "$R/pids"
sleep 1
dbus-daemon --session --fork --address="unix:path=$R/bus" --print-pid=1 > "$R/dbus.pid" 2>/dev/null
cat "$R/dbus.pid" >> "$R/pids" 2>/dev/null
sleep 1
"$NIRI" --headless > "$R/niri.log" 2>&1 & echo $! >> "$R/pids"
sleep 5

NS=$(ls "$R"/niri.*.sock 2>/dev/null | head -1)
[ -n "$NS" ] || { echo "niri did not come up; see $R/niri.log"; exit 1; }
{ echo "NIRI_SOCKET=$NS"; echo "NIRI_BIN=$NIRI"; } > "$R/env"
echo "socket: $NS"
NIRI_SOCKET=$NS "$NIRI" msg outputs 2>&1 | head -3
