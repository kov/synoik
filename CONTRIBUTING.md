# Contributing to synoik

synoik is a personal project with no public remote yet, so there is no PR process to
describe. This file records the conventions the code is held to; `CLAUDE.md` is the
authoritative short version, and `docs/fork/STRATEGY.md` is the design document.

## Dev loop

```sh
cargo test --workspace
```

Always `--workspace`. The root package is `synoik`, so a bare `cargo test` skips
`synoik-config`, `synoik-ipc` and `synoik-vk` and goes green while one of them is red.

The renderer is hand-rolled Vulkan, so the spec is only checked when you ask for it:

```sh
SYNOIK_VK_VALIDATION=1 cargo test --workspace
```

It loads the Khronos validation layer and fails the run at process exit if it reported
anything — trust the exit status, not libtest's `test result: ok`. Run it after touching
the renderer, and turn it on *first* when the compositor misbehaves around the renderer,
before profiling or theorising.

`cargo test` does not rebuild `target/debug/synoik`. After code changes, run
`cargo build --bin synoik` and restart the session, or the running compositor keeps
running stale code. See `docs/fork/RUNNING.md` for the test-session setup.

## Conventions

- **GNOME's way replaces niri's.** Where niri merely did the same thing differently, drop
  niri's version and implement GNOME's. Genuinely additional capabilities are kept, just
  re-homed behind GNOME's model.
- **Reference-first, not memory.** Read the actual gnome-shell/mutter source in the local
  reference checkouts and cite the file before porting or changing a behavior.
- **Pin every ported behavior** with a headless test in `src/tests/gnome.rs` — drive the
  real `State::do_action` or synthetic input against the `Fixture` harness and assert
  observable state.
- **Toolkit-first.** A draw op or control the toolkit lacks becomes a reusable `Painter`
  verb or `widget::` type, never a one-off in the caller.
- Format with `cargo fmt --all` and keep `cargo clippy --workspace --all-targets -D
  warnings` clean; `.githooks/pre-commit` gates commits on both once you opt in with
  `git config core.hooksPath .githooks`.
- Keep commits small and self-contained; every commit should build and pass tests.
