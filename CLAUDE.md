# gnome-shell-rs

Rust, GNOME-behaviors Wayland desktop. A **hard fork** (no upstreaming, no niri rebasing),
bootstrapped from **niri**; the endgame is a modern base free of GObject/Cogl/Clutter/GJS.
License: GPL-3.0.

Design doc: `docs/fork/STRATEGY.md` — read it before any large change.

## Conventions
- Each GNOME behavior we port is pinned by a headless test in `src/tests/gnome.rs` (the
  "conformance corpus"): drive the real `State::do_action` / synthetic input against the
  `Fixture` harness and assert observable state. GNOME's source is reference, never the spec.
- Dev loop: `cargo test --workspace` (headless, ~15s). **Not `cargo test` / `-p niri`** — the root
  package is `niri`, so those skip niri-config/niri-ipc/niri-vk entirely and go green while another
  crate is red. The conformance corpus itself needs no renderer, but the suite also drives the
  Vulkan render tests, which want a real device (they skip themselves without one). Runtime control
  plane: niri-ipc over `$NIRI_SOCKET`. New GNOME policy/UI state should flow through one
  inspectable model; keep the per-frame render path separate.
- The renderer is hand-rolled Vulkan, so **the spec is only checked when you ask**:
  `NIRI_VK_VALIDATION=1 cargo test --workspace` loads the Khronos validation layer (needs
  `vulkan-validation-layers`) and **fails the run** (non-zero exit, at process exit) if it reported
  anything. Run it after touching the renderer. Off by default: the layer costs real time per call.
  Note the layer never fails a test by itself, so libtest still prints `test result: ok` — trust the
  exit status, not that line. To find the culprit, re-run with `--test-threads=1`: parallel output
  interleaves, so the test name printed near a `VULKAN ERROR` means nothing.
- Live validation needs the real binary: `cargo test` does NOT rebuild `target/debug/niri`
  (only the test harness). After code changes, always `cargo build --bin niri` and restart the
  session, or the running compositor keeps running stale code.
- Reference checkouts (read-only, not this repo): `~/Projects/gnome-shell` and
  `~/Projects/mutter` (both 50.1) — ground behavior there; never copy GObject.

## Git
**Hard fork (2026-07):** `main` is the only living branch (ours, to push later). We no longer
rebase or merge from niri. The `niri-main` branch and the `upstream` remote (niri) are frozen
reference/history only — do not rebase/merge from them. Owned-render-stack direction is in
`docs/fork/STRATEGY.md` §3.10.
