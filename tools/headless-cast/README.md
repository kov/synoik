# headless-cast — a seat-free screencast reproduction

Drives the **real** screencast path — `org.gnome.Mutter.ScreenCast` → PipeWire → a consumer —
against a headless niri in a fully isolated session. No seat, no relog, no portal, no OBS.

```
tools/headless-cast/start.sh          # isolated niri + private pipewire + private bus
tools/headless-cast/cast.sh 25        # cast a monitor, capture 25 frames, report distinct count
tools/headless-cast/stop.sh           # tear down (only the PIDs we recorded)
```

`cast.sh` prints `frames: N distinct: D`. **`distinct: 1` over a changing screen is the failure**
— it means every delivered buffer carried identical content. It changes the screen mid-capture
(default `toggle-overview`) precisely so that a still desktop cannot fake a pass, and it uses no
`videorate` in the pipeline, because `videorate` pads with duplicate frames and would manufacture
the very symptom being measured.

Everything lands in `$NH_DIR` (default `/tmp/nh`): `niri.log`, `pw.log`, `f_*.png`, `env`, `pids`.

## Why this exists

The screencast memory-buffer path cost several rounds of "change something, ask for a relog, look
at OBS". Each round was minutes long and the feedback was a human describing pixels. This turns
that into seconds and a number.

It also produced the result that mattered most: pointing a **host** GStreamer at the stream showed
the same failure as Flatpak OBS, which is what proved the bug was ours rather than the sandbox's.
Keep a non-Flatpak consumer in the loop for exactly that reason.

## Rules

- **Never pattern-kill.** These scripts may run as the user who owns your real desktop session, so
  `pgrep -u "$USER" -x pipewire` matches *that* session's daemon. Killing it is a live outage; it
  has happened. Every process appends its PID to `$NH_DIR/pids` and `stop.sh` kills only those.
- **Keep `$NH_DIR` short.** It is the runtime dir; a long path overflows `sockaddr_un` and the
  sockets fail to bind with nothing obviously wrong in the logs.
- Rebuild first — `cargo build --bin niri` — since `cargo test` does not relink the binary.

## Requirements

`pipewire`, `dbus-daemon`, `gst-launch-1.0` with `pipewiresrc` (`gstreamer1-plugin-pipewire`), and
a render node. Override with `NH_DIR`, `NIRI_BIN`, `NIRI_HEADLESS_RENDER_NODE`.

## Known gap

`Headless::render` (`src/backend/headless.rs`) only emits presentation feedback — it does not
render. So a cast **starts** (a GBM device comes from the render node since `99c1f680`, the stream
reaches `Paused` and emits a node id) but no frames ever flow, and `cast.sh` reports `frames: 0`.
Wiring the live headless redraw loop to render through `with_vulkan_renderer` — the way the
`Fixture` tests already drive `Niri::render` — is the last step to a working reproduction.
