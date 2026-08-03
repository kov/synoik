#!/bin/bash
# Start a monitor screencast on the headless instance and capture frames with a *host* GStreamer
# consumer — no portal, no OBS, no Flatpak in the path.
#
# Prints the frame count and how many are DISTINCT. Distinct == 1 over a changing screen is the
# frozen-frame failure; that is the whole point of this script.
#
# Usage: cast.sh [frames] [action]     e.g. cast.sh 25 toggle-overview
set -u
R=${NH_DIR:-/tmp/nh}
FRAMES=${1:-25}
ACTION=${2:-toggle-overview}
export XDG_RUNTIME_DIR=$R PIPEWIRE_RUNTIME_DIR=$R
export DBUS_SESSION_BUS_ADDRESS=unix:path=$R/bus
# shellcheck disable=SC1090
. "$R/env"

SC=org.gnome.Mutter.ScreenCast
call() { gdbus call --session --dest $SC "$@"; }
path_of() { sed -E "s/.*'([^']+)'.*/\1/"; }

# CURSOR_MODE mirrors what a real consumer asks for: 0 hidden (default here), 1 embedded (drawn
# into the frame), 2 metadata (sent beside it, and a different queue path in pw_utils).
OPTS="{}"
[ -n "${CURSOR_MODE:-}" ] && OPTS="{'cursor-mode': <uint32 $CURSOR_MODE>}"

SP=$(call --object-path /org/gnome/Mutter/ScreenCast --method $SC.CreateSession "{}" | path_of)
STP=$(call --object-path "$SP" --method $SC.Session.RecordMonitor "headless-1" "$OPTS" | path_of)
gdbus monitor --session --dest $SC --object-path "$STP" > "$R/mon.log" 2>&1 & echo $! >> "$R/pids"
sleep 1
call --object-path "$SP" --method $SC.Session.Start > /dev/null
sleep 2

NODE=$(grep -oE "PipeWireStreamAdded \(uint32 [0-9]+" "$R/mon.log" | grep -oE "[0-9]+$" | head -1)
[ -n "$NODE" ] || { echo "no node id; see $R/niri.log"; exit 1; }
echo "node=$NODE session=$SP"
echo "$SP" > "$R/sess"

rm -f "$R"/f_*.png
# Keep the screen changing for the WHOLE capture, so identical frames mean a real freeze rather
# than a still desktop — and so the stream runs at a rate comparable to a real cast. A few frames
# seconds apart cannot show a bug that needs sustained throughput; CHURN_INTERVAL is the knob.
# No videorate in the pipeline: it pads with duplicates and would fake exactly that.
churn() {
    while :; do
        NIRI_SOCKET=$NIRI_SOCKET "$NIRI_BIN" msg action "$ACTION" >/dev/null 2>&1
        sleep "${CHURN_INTERVAL:-0.4}"
    done
}
churn & CHURN_PID=$!

START=$(date +%s.%N)
timeout "${CAPTURE_TIMEOUT:-20}" gst-launch-1.0 -q pipewiresrc path="$NODE" num-buffers="$FRAMES" \
    ! videoconvert ! pngenc ! multifilesink location="$R/f_%03d.png" 2>&1 | tail -3
ELAPSED=$(echo "$(date +%s.%N) - $START" | bc)

kill "$CHURN_PID" 2>/dev/null

N=$(ls "$R"/f_*.png 2>/dev/null | wc -l)
D=$(md5sum "$R"/f_*.png 2>/dev/null | awk '{print $1}' | sort -u | wc -l)
printf 'frames: %s  distinct: %s  in %.1fs (%.1f fps)\n' "$N" "$D" "$ELAPSED" \
    "$(echo "$N / $ELAPSED" | bc -l)"
[ "$N" -gt 1 ] && [ "$D" -le 1 ] && echo "FROZEN: every delivered frame is identical"
exit 0
