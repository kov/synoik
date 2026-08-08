# session-probe

A minimal `xdg_session_management_v1` client, for driving toplevel session restore against a
**live** compositor.

The conformance corpus in `src/tests/gnome.rs` drives this protocol through the headless fixture,
which is where the rules belong. What it cannot tell you is whether the seat in front of you
behaves the same way — a rebuilt binary does not reach a running session until that session is
restarted, so "the tests pass" and "the desktop is fixed" are different claims. This closes that
gap.

```sh
cargo build --manifest-path tools/session-probe/Cargo.toml
```

## Two runs make a test

```sh
# First run: prints the session id, holds the windows open.
session-probe --windows 2 --hold 30
# ... meanwhile, move one somewhere:
synoik msg action move-window-to-workspace-down

# Second run: ask for the same windows back.
session-probe --restore --session-id <ID> --windows 2
```

Where they landed comes from the compositor, not the probe:

```sh
synoik msg windows | grep -A6 session-probe
```

## Driving it on the gsrs seat

```sh
sudo -u gsrs env XDG_RUNTIME_DIR=/run/user/1002 WAYLAND_DISPLAY=wayland-1 \
  tools/session-probe/target/debug/session-probe --windows 2 --hold 30
```

and read the store with
`sudo cat /home/gsrs/.local/share/synoik/session.json`.

## Exit matters

Exiting destroys the toplevels, which is what makes the compositor save. `--hold N` and
`--quit-on-configure` both take that path. **A `SIGKILL` does not**: the session id survives but
every toplevel record is lost, so the next run restores nothing and the windows open wherever new
windows go. If you are chasing "my windows all came back on the wrong desktop", check that the app
is quitting cleanly before you suspect placement.
