# Minimize

GNOME's minimize (`meta_window_minimize`, `window.c:2734`; the shell's `Hide` row,
`windowMenu.js:40-45`). A minimized window is **alive, still ours, and out of the layout**: it
keeps its surface, its place in the switcher and the app system, and comes back to the workspace it
left.

## The model

`Workspace::minimized: Vec<RemovedTile<W>>` — the same record `remove_tile` produces for an
interactive move, parked on the workspace instead of carried by the pointer. That is what makes the
round trip exact: width, full-width and floating-vs-scrolling all ride the record, so unminimize is
a `put back`, not a re-place.

Minimizing goes through a **real** `remove_tile`, never a flag flip, so the focus fixup, the column
collapse and the workspace bookkeeping all happen the way they do for any other take-out.

The bucket lives on the `Workspace`, not on the `Layout`, because everything that walks workspaces —
`windows()`, `session_snapshot`, `has_windows_or_name` — must still see the window. `Workspace::tiles`
and `tiles_with_ipc_layouts` chain it for exactly that reason.

**`has_window` means *laid out here*; `holds_window` means *on this workspace at all*.** The ~60
`Layout` dispatchers that ask "which workspace owns this?" before doing something positional want
the first: a parked window has no column to collapse and no tile to animate. Removal and lookup want
the second.

`LayoutElement::is_minimized` is a **cache** the layout writes on the window; the bucket is the
truth. Nothing may set it directly.

### `Home`

Three places a window can be on a workspace, so `Workspace` dispatches on

```rust
enum Home { Scrolling, Floating, Minimized }
```

with `home_of` / `target_home` and **exhaustive matches, no catch-all** — the boolean this replaced
sent a minimized window down the scrolling path and panicked on an unwrap. A fourth home must not be
addable without the compiler naming every site.

## The four ways in

| Trigger | Reference |
|---|---|
| The window menu’s **Hide** row | `windowMenu.js:40-45` |
| `minimize` (`<Super>h`) | `handle_minimize`, `keybindings.c:2183` |
| A client's own minimize button, `xdg_toplevel.set_minimized` | `meta-wayland-xdg-shell.c` |
| wlr-foreign-toplevel `set_minimized` / `unset_minimized` | — |

`set_minimized` was the second silent drop of the same shape as `show_window_menu`: smithay's
`XdgShellHandler::minimize_request` defaults to a no-op, so a GTK window's own minimize button did
nothing at all.

**Activating unminimizes.** `activate_window` restores before it focuses (`window.c:3908`), so
the switcher, the dash and an activation token all bring a hidden window back without knowing about
minimize.

## State surfaces

Every consumer that answers "what windows are there, and can you see them" carries the bit:

- **IPC** `Window.is_minimized`, read off `Mapped` (not off an acked configure) and in the
  change-detection chain.
- **wlr-foreign-toplevel** — the `Minimized` state in the array, alongside focus, in `ShellState`.
- **The switcher** keeps minimized windows in the list, sorted last: key `(minimized,
  Reverse(focus_timestamp))`.
- **The app system** — `RunningWindow.minimized`, and `RunningApp::is_minimized()` is *all* windows
  minimized (`shell_app_is_minimized`).
- **Session save/restore** — the record carries it, and a restored window is re-minimized after it
  maps.
- **`is_hidden`** now means minimized *or* on an inactive workspace.

## Divergences, for now

- **The overview does not show minimized windows.** GNOME's window picker includes them (the
  thumbnail strip does not). Ours drops them, because the picker reads the laid-out tiles.
- **No minimize animation.** GNOME shrinks the window toward its icon; ours vanishes.
