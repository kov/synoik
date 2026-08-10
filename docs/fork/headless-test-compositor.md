# synoik as a headless test compositor

Plan, 2026-08-10. Prompted by `~/Projects/ghost/docs/synoik-as-a-test-compositor.md`, which lists
what ghost's test suite needs from a compositor in order to stop running its window-level tests
against *two* compositors (headless weston for the real-swapchain path, headless mutter for
fractional scale) because neither can do the whole job.

The asks are worth reading on their own terms: ghost is an outside consumer, so its list is an
unusually honest audit of what our `--headless` mode cannot do. Several of them are things our own
conformance corpus wants and does not have. This note records the verdict on each and the order to
build them in.

Ground rule throughout: **no test-only code paths.** `--headless` is production code the corpus
already runs (`src/backend/headless.rs`), not a fork of the compositor. Every item below either
lands on the shared backend or is declined for exactly that reason.

## 1. Already there

Six of the fourteen asks need no synoik change. Recorded here because the ghost doc asserts
otherwise, and because each one is a test that can be written today.

| Ask | Where it already is |
|---|---|
| 2 — no seat / logind / TTY | `--headless` uses no libseat; that is tty-only. The reported `Failed to open session: Function not implemented (os error 38)` is what you get running plain `synoik`, where `BackendMode::Auto` selects the tty backend (`src/main.rs:234`). |
| 6 — change output scale at runtime | `synoik msg output headless-1 scale 1.3333`, via `apply_transient_output_config` (`src/synoik.rs:3691`). The corpus already drives `headless-1` to 1.5 and back to automatic (`src/tests/gnome.rs:16107`). A transient IPC config outranks the monitors.xml store by design, so the config file's removal does not affect this. |
| 7 — maximize / tile / resize over IPC | `Maximize`, `Unmaximize`, `ToggleTiledLeft`, `ToggleTiledRight`, `SetWindowWidth`, `SetWindowHeight`. |
| 9 — key and text injection with held modifiers | `Request::InjectInput` → `input::synthetic::inject` (`src/ipc/server.rs:475`), backend-agnostic, so it works headless. |
| 10 — screenshot a window to a file | `ScreenshotWindow { id, path, write_to_disk }` already takes an absolute `--path`; the clipboard is not involved. |
| 13 — protocols | `BackgroundEffectState` is created unconditionally in `Synoik::new` (`src/synoik.rs:7018`), so headless advertises `ext-background-effect-v1`; `xdg_session_management_v1` is in `src/protocols/session_management.rs`; fractional-scale and viewporter are both up. This combination is the actual reason to prefer synoik over weston+mutter. |

**Caveat to pass on to any outside consumer:** `SetWindowWidth`/`SetWindowHeight` and the column
verbs are niri's model, and this fork replaces niri semantics with GNOME's (see `STRATEGY.md`). A
rig scripted against those will break. `Maximize`/`Unmaximize`/`ToggleTiledLeft`/`ToggleTiledRight`
are the GNOME-shaped verbs and are safe to build tests on.

## 2. The blocker: dmabuf in headless

Ask 1, and the only thing standing between ghost and dropping both compositors. It is also the
longest-standing blind spot in our own corpus: GPU-rendering clients composite empty under
`--headless`, so window *contents* can never be judged from a headless shot — see
`test-harness-realism.md` §1, which pins this as one of the two halves of the harness gap.

Today no dmabuf global is created outside the tty backend, and `Headless::import_dmabuf` is
`unimplemented!()` (`src/backend/headless.rs:249`).

Every piece needed already exists, and none of them want a seat, a DRM master or a VT:

- the owned Vulkan renderer imports client dmabufs — `ImportDma` at
  `src/render_helpers/vulkan/integration.rs:186` → `import_dmabuf_as_texture`;
- `Gpu::drm_render_node()` supplies the `(major, minor)` the feedback needs, via
  `VK_EXT_physical_device_drm` (`synoik-vk/src/gpu.rs:571`);
- `dmabuf_formats()` (`src/render_helpers/vulkan/renderer.rs:3411`) is the same single source of
  truth the tty path advertises from.

So the work is to build the feedback and create the global mirroring `src/backend/tty.rs:870-883`,
and to implement `Headless::import_dmabuf` mirroring `src/backend/tty.rs:1975` — delegating to the
renderer's `ImportDma`, which is what actually gates which client buffers are accepted.

Three constraints:

- **The global and the import land in one commit.** Advertising the global is what makes the
  `unimplemented!()` reachable; split across two commits, the intermediate state panics on the
  first dmabuf commit a client makes.
- **LINEAR 8888 only**, as on tty — a Venus constraint baked into `dmabuf_formats()`. Mesa's WSI
  allocates linear when the feedback says so, so this is sufficient, not a compromise to fix later.
- **Skip the global gracefully** where the driver has no `VK_EXT_physical_device_drm` (lavapipe).
  A compositor that advertises dmabuf it cannot import hands clients a blank window and per-frame
  error spam, which is precisely the failure the tty import comment warns about. No render node,
  no global: the client falls back to shm and everything still runs.

The headless present loop (frame callbacks and presentation feedback, `src/backend/headless.rs:212`
and `:264`) is machinery-present but has never been driven end to end by a real WSI client. Expect
that to be where the surprises are, not in the import itself.

## 3. Cheap additions worth making

- **Asks 4 and 8 — `--output WxH@scale`, repeatable.** Replaces the `SYNOIK_HEADLESS_MODE=WxH` env
  var, and adds a startup scale, which today can only be reached through the monitors.xml store or
  an IPC call after start. The backend is already N-output capable (`add_output(n, size)`,
  `src/backend/headless.rs:140`) and the Fixture uses several; only `src/main.rs:250-268` hardcodes
  one. A window crossing a scale boundary is untested in our corpus too, and per-window caches
  (glyph atlas, shadow, blur region) are exactly where we have been bitten before.
- **Ask 5 — gate xwayland-satellite off in headless.** `xwayland::satellite::setup` runs
  unconditionally (`src/main.rs:287`); it opens sockets in the shared `/tmp/.X11-unix` and can spawn
  a child (`src/utils/xwayland/satellite.rs:109`, `:186`). A Wayland-only test client never triggers
  the lazy spawn, but concurrent headless instances have no business touching a global namespace.
  The rest of ask 5 already holds: mutter-style immediate terminate on SIGTERM/SIGINT/SIGHUP
  (`src/utils/signals.rs:52`), session D-Bus interfaces off in non-session instances unless
  `SYNOIK_DEBUG_DBUS_INTERFACES_IN_NON_SESSION_INSTANCES`, stderr logging.
- **Ask 11 — window state in `msg windows`.** Additive fields on `synoik_ipc::Window`:
  maximized / tiled / activated, and the surface size alongside the geometry size (`window_size`,
  `synoik-ipc/src/lib.rs:1581`, is geometry only). Real compositor state, no test-only surface. It
  also lets a test assert the compositor's view against the client's own — "the shadow ring is
  surface minus geometry" is currently only ever checked from inside the client.
- **Ask 3 — `--wayland-display NAME`.** Least load-bearing of the set: the socket is auto-named and
  already logged (`src/main.rs:274`), and a private `XDG_RUNTIME_DIR` deterministically yields
  `wayland-0`. Worth having, worth doing last. (It also surfaced a real hazard: the bind was an
  `unwrap`, so a rig with a long `XDG_RUNTIME_DIR` — over the 108-byte `sockaddr_un` limit — got a
  backtrace naming neither the directory nor the reason. It now names both.)

## 4. Determinism: the GNOME-shaped version

Ask 14 wants animations off (or a fixed clock) so a test does not race a crossfade.

The internals exist — `Clock::set_complete_instantly` / `set_rate` (`src/animation/clock.rs`),
driven from `config.animations.off` (`src/synoik.rs:3314`, `:6918`). With the config file gone,
nothing external sets them; only tests reach in directly (`src/tests/gnome.rs:625`).

The right export is **not** a synoik-specific flag. We already read
`org.gnome.desktop.interface enable-animations` and explicitly do not gate our animations on it
(`src/gnome.rs:138`). Mutter honours it. Wiring that key to `set_complete_instantly` closes a real
conformance gap, is production behavior rather than test surface, and hands every consumer —
ghost's rig, our own live verification, the corpus — a deterministic mode through the same knob a
user has. It earns its own conformance test.

**Do not export a pinned or fixed clock over IPC.** Clock pinning is Fixture-internal machinery
(`src/tests/fixture.rs`); putting it in the live binary is the test-only path this whole note is
organised to avoid, and animations-off is enough for the stated need. See
`[[headless-animation-clock-trap]]` for why the clock is the part to leave alone.

## 5. Declined

**Ask 12 — a per-window frame/commit counter.** "Did anything actually reach the screen, and how
often" is a fair question; a bespoke second instrument answering it is not the way. We already have
one: `frame_log` is an always-on ring with autodump, and `synoik msg frame-perf` publishes
`FramePerf` (`synoik-ipc/src/lib.rs:256`) — per-output render tallies, animation causes, main-loop
stalls, GPU timings. Two instruments measuring adjacent things is how you get an instrument that
omits its own event and reads as perfect (`frame-log-instrument-confound`).

So: **`frame_log`/`FramePerf` is the instrument for this class of question.** If per-window
granularity turns out to be genuinely needed, it is added *to* that surface — smithay already keeps
per-buffer commit counters, so the data is reachable — and never as a parallel counter beside it.
Marginal value for synoik's own testing either way.

## 6. Order — all landed 2026-08-10

1. ✅ **Headless dmabuf global + import**, one commit, pinned by
   `a_client_dmabuf_reaches_the_composited_frame`: a real GBM client buffer, filled on the GPU,
   presented over `zwp_linux_dmabuf_v1` and counted in the composited readback (40000 green pixels
   for a 200×200 window). Breaking the import fails it at `create_immed`.
2. ✅ **`enable-animations` → animation clock**, pinned by
   `enable_animations_off_stops_the_shell_animating`. Both switches now meet in
   `State::apply_animation_clock`, so ours and GNOME's cannot drift apart.
3. ✅ **`--output WxH[@SCALE]`, repeatable**, replacing `SYNOIK_HEADLESS_MODE`; the scale goes
   through `apply_transient_output_config`, the same path `msg output … scale` takes.
   xwayland-satellite no longer starts headless.
4. ✅ **`msg windows` state fields** — maximized/fullscreen/activated/tiled edges from the last
   acked configure, plus `surface_size` beside `window_size`, and all of them in the event-stream
   change check so a consumer's copy cannot go stale.
5. ✅ **`--wayland-display NAME`**, and a bind failure that names the runtime dir and the reason
   (a too-long `XDG_RUNTIME_DIR` used to be a raw `BindError` unwrap backtrace).

That clears ghost's stated bar for replacing both mutter and weston.

**Confirmed from outside, same day.** ghost built `bcde724e` and drove its real binary against
`--headless --output 1600x1000@1.25 --wayland-display syn-test`: the swapchain came up with no
`ERROR_SURFACE_LOST_KHR`, and its own frame invariant (surface − geometry == inset) held at every
step of floating@1.25 → `maximize` → `output … scale 1.3333` → `unmaximize` — 65px, then 0 while
maximized, then 69/70 — with zero dropped frames. So the headless present loop *does* drive a real
WSI client end to end, which §2 called out as the likely place for surprises; there were none.
Their cross-check also validates `surface_size`: 780x527 against a 728x475 logical `window_size` is
the same 65px ghost measures for itself, the first time that arithmetic has been checked against
anything but ghost. Not exercised by them: input injection, screenshot-to-file,
`ext-background-effect-v1`.

## 7. One trap for any rig

`reload_output_config` loads the developer's real `~/.config/monitors.xml` (`src/synoik.rs:3568`),
which sits *above* the KDL-era output config in the precedence chain. A test rig that does not
isolate `XDG_CONFIG_HOME` can have its requested scale silently overridden by whatever the
developer's desktop last saved — and the symptom is a wrong scale with no error anywhere. Note that
`XDG_CONFIG_HOME` alone does not isolate gsettings; determinism via `enable-animations` (§4) needs
`dbus-run-session` as well.
