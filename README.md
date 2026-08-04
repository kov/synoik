<h1 align="center">synoik</h1>
<p align="center">A GNOME-behaviors Wayland desktop, written in Rust.</p>

## About

synoik is a Wayland compositor and shell that reimplements GNOME's behavior in Rust,
on an owned Vulkan render stack. It is a **hard fork** of
[niri](https://github.com/niri-wm/niri): niri supplied the compositor foundations, and
everything above them is being rewritten to behave like gnome-shell rather than like a
scrollable tiler.

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
