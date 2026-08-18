# A workspace switch hands over the keyboard when it lands

**Divergence from GNOME, taken deliberately 2026-08-18.** Approved by kov on the reasoning that the
animation is short and nobody expects to be typing through a workspace switch.

## What GNOME does, and what we do

mutter focuses **synchronously at the start** of the switch. `meta_workspace_activate_with_focus`
(`src/core/workspace.c:557`) calls `meta_compositor_switch_workspace` to begin the animation and
then, in the same function, `meta_window_activate` / `meta_workspace_focus_default_window`. The
comment there about ordering is only to ensure the compositor knows a switch is in progress — it is
not a deferral.

We hold the focus change until the switch settles.

## Why

A focus change is not just a notification: clients repaint on it. A client whose toplevel is
`wl_shm` repaints its **whole window**, and we pay for that on the compositor thread as a staging
upload. Firefox is the case that forced this — its web content is dmabuf but its CSD chrome is a
full-window `wl_shm` surface, and it posts a fresh pool on every `wl_keyboard.enter`/`leave`.

Measured on kov's seat, differential within one run (switch frames carrying the upload vs not):

| | took | collect | upload |
|---|---|---|---|
| 22.2 MiB window | 8.37 / 14.16 ms | 5.28 / 8.42 | 4.99 ms |
| 74.3 MiB (two windows) | 9.48 / 20.59 ms | 5.90 / 14.46 | 5.75 ms |
| same run, no upload | ~3.4 ms | 0.07–0.09 | — |

So ~5–6 ms of a 16.67 ms budget, spent on a frame that is already animating. A settled frame absorbs
it invisibly. The upload itself cannot be made much cheaper — see
[`shm-upload-zero-copy.md`](./shm-upload-zero-copy.md), where all three routes are blocked outside
this repo.

## What it costs

**Keys pressed during the animation go to the window being switched away from**, and
`zwp_text_input` follows keyboard focus with them, so typing is delayed by the animation. Pinned by
`a_key_during_a_switch_goes_to_the_window_being_left` so it stays a decision.

**A focus change superseded during a single animation is dropped, not queued.** This is what makes
rapid switches coalesce — the workspace passed over is never focused, saving two repaints — but it
also means a window mapped during an unsettled switch gets no focus stamp. The app switcher's order
is a focus order, so a test that maps windows across switches must settle each one.

## Scope

Only **window-to-window** focus waits. A dialog, the lock screen or a layer surface taking focus is
not the switch's doing and is applied immediately. Switching away from an *empty* workspace holds
too: there is no surface to keep, but the destination must not be focused early either.

The hold is expressed as *keeping the focus we already have*, never as an early return from
`update_keyboard_focus` — the tail of that function runs whether or not focus moved, and skipping it
leaves an input method composing into an entry that has gone away.

## Tests

`src/tests/gnome.rs`, all three checked against a neutered hold so none is vacuous:

- `a_workspace_switch_holds_its_focus_change_until_it_settles` — nothing mid-animation, the change
  once settled. Uses `Fixture::run_until_settled`, so the settle is the real frame loop rather than
  a clock teleport.
- `switching_twice_never_focuses_the_workspace_passed_over` — without the hold the intermediate
  workspace sees `[Enter, Leave]`, two wasted repaints.
- `a_key_during_a_switch_goes_to_the_window_being_left` — the price, pinned.
