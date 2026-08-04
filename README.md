<h1 align="center">synoik</h1>
<p align="center">A GNOME-behaviors Wayland desktop, written in Rust.</p>

## About

synoik is a Wayland compositor and shell that reimplements GNOME's behavior in Rust,
on an owned Vulkan render stack.

### Built on niri

synoik is a **hard fork** of [niri](https://github.com/niri-wm/niri) by Ivan Molodetskikh
and the niri contributors, and would not exist without it. niri supplied the foundations —
the Smithay-based compositor core, the Wayland protocol implementations, the window
management and animation machinery — and years of work on making all of it correct. Every
release synoik makes stands on that.

Everything above those foundations is being rewritten to behave like gnome-shell rather
than like a scrollable tiler. The fork is hard by choice: there is no upstreaming
obligation in either direction, and no rebasing onto niri. That is a divergence in goals,
not a judgement about niri, which remains an excellent compositor and is what you want if
scrollable tiling is what you want.

The endgame is a modern base free of GObject, Cogl, Clutter and GJS. The working rule is
that GNOME's way replaces niri's: where niri merely did the same thing differently — its
own config knob, its own default, its own settings surface — niri's version is dropped and
GNOME's is implemented. Settings come from GSettings (`org.gnome.desktop.*`,
`org.gnome.shell`, `org.gnome.mutter`), not from a compositor config file. Genuinely
*additional* capabilities inherited from niri are kept, just re-homed behind GNOME's model.

The design document is [`docs/fork/STRATEGY.md`](docs/fork/STRATEGY.md). Read it before any
large change.

## Status

A personal project, daily-driven, under active development. It is not packaged anywhere
and has no release process yet. Ported subsystems are pinned by a headless conformance
corpus in `src/tests/gnome.rs` that drives the real compositor and asserts observable
state; gnome-shell's source is the reference, never the spec.

## Building

```sh
cargo build --workspace
cargo test --workspace
```

Always pass `--workspace`: the root package is `synoik`, so a bare `cargo test` skips the
other crates entirely. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop and
[`docs/fork/RUNNING.md`](docs/fork/RUNNING.md) for running a real session.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

synoik is derived from niri, © Ivan Molodetskikh and contributors, under the same license.
