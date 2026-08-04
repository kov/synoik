# synoik

Rust, GNOME-behaviors Wayland desktop. A **hard fork** (no upstreaming, no niri rebasing),
bootstrapped from **niri**; the endgame is a modern base free of GObject/Cogl/Clutter/GJS.
License: GPL-3.0.

Design doc: `docs/fork/STRATEGY.md` — read it before any large change.

**Tenet — GNOME's way replaces niri's.** We are turning this niri fork into a faithful Rust
rewrite of gnome-shell. When niri merely does the same thing *differently* (its own config knob,
its own default, its own settings surface), throw niri's way away and implement GNOME's — e.g.
keyboard layouts come from `org.gnome.desktop.input-sources`, not niri's `input.keyboard.xkb`.
Do NOT discard *additional capabilities* we might still want (systemd-localed, an owned renderer,
extra protocols); those are kept, just re-homed behind GNOME's model. Removing niri-specific
config/features is expected and often desirable; when unsure whether something is "just niri's
way" vs. a capability worth keeping, ask.

## Conventions
- Each GNOME behavior we port is pinned by a headless test in `src/tests/gnome.rs` (the
  "conformance corpus"): drive the real `State::do_action` / synthetic input against the
  `Fixture` harness and assert observable state. GNOME's source is reference, never the spec.
- Dev loop: `cargo test --workspace` (headless, ~15s). **Not `cargo test` / `-p synoik`** — the root
  package is `synoik`, so those skip synoik-config/synoik-ipc/synoik-vk entirely and go green while another
  crate is red. The conformance corpus itself needs no renderer, but the suite also drives the
  Vulkan render tests, which want a real device (they skip themselves without one). Runtime control
  plane: synoik-ipc over `$SYNOIK_SOCKET`. New GNOME policy/UI state should flow through one
  inspectable model; keep the per-frame render path separate.
- The renderer is hand-rolled Vulkan, so **the spec is only checked when you ask**:
  `SYNOIK_VK_VALIDATION=1 cargo test --workspace` loads the Khronos validation layer (needs
  `vulkan-validation-layers`) and **fails the run** (non-zero exit, at process exit) if it reported
  anything. Run it after touching the renderer. Off by default: the layer costs real time per call.
  Note the layer never fails a test by itself, so libtest still prints `test result: ok` — trust the
  exit status, not that line. To find the culprit, re-run with `--test-threads=1`: parallel output
  interleaves, so the test name printed near a `VULKAN ERROR` means nothing.
- **Turn the layer on FIRST when the compositor misbehaves around the renderer** — before profiling,
  bisecting or theorising. `SYNOIK_VK_VALIDATION` is a plain env var read in `Gpu::with_selector`, so
  the **live session** honors it too: add a systemd drop-in with `Environment=SYNOIK_VK_VALIDATION=1`
  for the seat user, reproduce, and read the `VULKAN ERROR` lines out of the journal. Undefined
  behavior surfaces as whatever the driver felt like doing — a wedge that presented as
  `ERROR_OUT_OF_HOST_MEMORY` from every allocation was a destroyed `VkImage` inside a still-recording
  command buffer, and the allocation failures were the corpse, not the cause. Hours went into
  falsifying memory-pressure theories that the layer named in one run.
- Live validation needs the real binary: `cargo test` does NOT rebuild `target/debug/synoik`
  (only the test harness). After code changes, always `cargo build --bin synoik` and restart the
  session, or the running compositor keeps running stale code.
- Reference checkouts (read-only, not this repo): `~/Projects/gnome-shell` and
  `~/Projects/mutter` (both 50.3) — ground behavior there; never copy GObject. Ports and docs
  written before 2026-07-31 cite **50.1** line numbers; when one no longer matches, re-grep rather
  than assume drift is a divergence (`git show 50.1:<file>` gets the cited revision back).
- **Reference-first, not memory.** Before porting or changing any GNOME behavior/layout, read the
  actual 50.1 source in those checkouts and cite the file — do NOT rely on recollection of how GNOME
  looks/works. Memory drifts from the shipped version (e.g. the quick-settings system row is a
  full-width row at the *top*, not the bottom); grep the reference first, then implement.
  For *visual* specs (fonts, colors, radii, spacing, per-widget classes), `docs/fork/gnome-style-reference.md`
  is a cached, cited reading of the 50.1 theme — start there, but re-grep the reference if a row looks off.
  This includes **where** a widget sits: derive child order/placement from the JS construction sequence
  (`js/ui/*.js` `add_child`/`_addItems`), not from what looks right — the SCSS says how it looks, never
  where it goes. (The QS volume slider first landed at the menu bottom because the order was assumed.)
- **Toolkit-first — no faked chrome.** When a widget/CSS you're porting needs a draw op the toolkit
  lacks (border/outline, gradient, shadow, a new shape) or a control that shows up on more than one
  surface (button, slider, switch, entry), add it as a reusable `Painter` verb or `widget::` type —
  never a one-off in the caller. The `clear(border); fill_rect_px(inner)` fill-full+inner idiom is a
  fake-border smell: it can't round and duplicates behavior; use `Painter::stroke_rounded`. Shared
  controls become a `widget::` primitive so hover/focus/geometry stay consistent (see `widget::Button`,
  the inset-accent focus ring). Grow the toolkit as the port surfaces new interface, don't paint around it.
- **Share the frame's submit — never add a `run_commands` to the frame path.** On this stack a
  submit+fence-wait is a round trip whose cost is unrelated to its payload (a *barrier* on its own
  submit measured 2.5–3.5 ms; 11 shm re-uploads cost 19.38 ms to move 0.33 ms of bytes). Work whose
  only consumer is later GPU work gets **recorded** into a command buffer that is already being
  submitted — submits are totally ordered on one timeline semaphore, so recorded is as good as
  waited-for. Queue it for `VulkanFrame::begin`, or use `run_commands_deferred` when it truly can't
  fold. The catch is the other side: anything sampling a queued resource *outside* a frame must
  drain first (`flush_pending_texture_uploads`), so each such consumer is a stall you are adding.
  Read `docs/fork/frame-submit-discipline.md` before touching this.
- **A hazard you can describe, you can fix — do it now.** If you're about to write a comment saying
  something is a footgun, a flake generator, or "careful, this can X", that is the moment to fix it,
  not document it. Then check the neighbours for the same shape, because the pattern rarely has one
  instance. If it genuinely can't be fixed here, say *why* and what would fix it — a hazard note with
  no owner is a bug with better manners. (`dump_dir_from` was split out of the env precisely because
  "env mutation in a parallel test binary is a flake generator", and said so in a comment — while
  `dump_path`, one function above, kept reading the env on every call. That was a 1-in-6 suite flake
  until `453ffe7e`, blamed on the renderer twice on the way.)

## Git
**Hard fork (2026-07):** `main` is the only living branch (ours, to push later). We no longer
rebase or merge from niri. The `niri-main` branch and the `upstream` remote (niri) are frozen
reference/history only — do not rebase/merge from them. Owned-render-stack direction is in
`docs/fork/STRATEGY.md` §3.10.

**Pre-commit hook:** `.githooks/pre-commit` gates every commit on `cargo fmt --all -- --check`
and `cargo clippy --workspace --all-targets -- -D warnings`. It is not active until each clone
opts in: `git config core.hooksPath .githooks` (per-clone local config, so a fresh checkout must
re-run it). Always run `cargo fmt --all` before committing regardless.
