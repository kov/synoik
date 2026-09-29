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

## Keeping the compositor out of swap

A compositor that is swapped out stalls the screen: every page the next frame touches is a disk
read, and after a suspend or under memory pressure that is seconds of a frozen display. So synoik
locks its own memory (`src/utils/memory.rs`):

- **What:** its heap, its private anonymous mappings (thread stacks, allocator arenas) and its own
  executable. GPU mappings, shared memory from clients and other libraries stay swappable — this is
  deliberately not `mlockall`.
- **How:** `mlock2(MLOCK_ONFAULT)`, so a page is pinned once touched, then `MADV_POPULATE_READ`,
  which brings back whatever is already in swap without giving untouched pages memory of their own.
  The kernel does not announce new mappings, so a worker rescans every 5 s and before a suspend.
- **Budget:** the soft `RLIMIT_MEMLOCK`, which synoik raises to 768 MiB (or its hard limit, if
  lower). The kernel refuses locks past it; synoik logs that once and the rest stays swappable.

The default hard limit is 8 MiB, which synoik's code alone spends, so **a session has to grant the
limit**, at two levels — a user unit cannot be given more than its user manager has, and systemd
clamps it silently rather than failing:

```ini
# /etc/systemd/system/user@.service.d/memlock.conf  (the user manager's ceiling)
[Service]
LimitMEMLOCK=768M

# ~/.config/systemd/user/org.gnome.Shell@user.service.d/memory.conf  (the compositor)
[Service]
LimitMEMLOCK=768M
```

`resources/synoik.service` already carries the second. The first takes effect when the user
manager restarts — with lingering enabled that is a reboot (or `systemctl restart user@<uid>`), not
a relog. To check a running session, read `/proc/<pid>/limits` (`Max locked memory`) and
`VmLck`/`VmSwap` in `/proc/<pid>/status`.

Locked pages cannot be trimmed back to the kernel, so freed allocator memory stays resident up to
its peak; synoik also pins glibc's mmap threshold at 4 MiB so big buffers never sit in an arena.

## License

GPL-3.0. See [LICENSE](LICENSE).

synoik is derived from niri, © Ivan Molodetskikh and contributors, under the same license.
