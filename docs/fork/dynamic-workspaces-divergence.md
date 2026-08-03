# Dynamic workspaces: manual close, always-on strip

**Status: implemented 2026-08-03.** Approved by Gustavo in the session that wrote it, with three
sub-decisions taken up front (named workspaces stay un-closable; closing animates the strip
closed; niri's `empty-workspace-above-first` goes).

Three changes, only two of which are divergences.

## 1. Two workspaces at startup — *not* a divergence, a missed behavior

gnome-shell's `WorkspaceTracker._checkWorkspaces` enforces `MIN_NUM_WORKSPACES = 2`
(`js/ui/windowManager.js:42`) in two places: it appends until the count reaches 2
(`:273-276`), and the reap loop breaks the moment the count is back down to 2 (`:286`). A fresh
GNOME session therefore has **two** workspaces, not one.

Ours had one: `Monitor::new` pushed exactly one trailing empty workspace and nothing set a floor.
`MIN_NUM_WORKSPACES` now lives in `src/layout/monitor.rs` and is enforced in `Monitor::new` and
in `clean_up_workspaces`, in gnome-shell's order: ensure a trailing empty, then pad to the
minimum.

This is invisible on its own — GNOME hides the strip at 2 workspaces — which is why it went
unnoticed until (2).

## 2. DIVERGENCE — the thumbnail strip is always shown

gnome-shell's `ThumbnailsBox._updateShouldShow` (`js/ui/workspaceThumbnail.js:697-706`) shows the
strip only when `nWorkspaces > NUM_WORKSPACES_THRESHOLD` (2, `:16`), and eases `expandFraction`
0↔1 as the count crosses that line. Since the trailing empty always counts, GNOME's strip appears
only once a *second* desktop is populated.

We always show it. The strip is the desktop switcher; one that appears and disappears as a
side effect of what you happen to have open is not one you can aim at, and with (3) the count now
reflects a deliberate user choice rather than a transient. `Monitor::thumbnails_expand_fraction`
is a constant 1, and the count-crossing ease it used to drive is gone.

`ui::overview_layout` keeps its `expand_fraction` parameter — that is the ported
`ControlsManagerLayout` signature and it is unit-tested on its own — and the monitor hands it the
constant.

## 3. DIVERGENCE — empty workspaces are closed by hand, macOS-style

gnome-shell reaps: `_checkWorkspaces` removes every empty workspace that is not the active one
and not the last (`windowManager.js:278-291`), so closing your last window on a desktop makes
that desktop vanish and renumbers everything after it. That is the behavior we are dropping.

Instead, an emptied workspace **stays**, and grows a close button on hover in the overview —
Mission Control's model. The consequences:

- **Workspace indices are stable.** `Super+3` keeps meaning the same desktop across a day of
  opening and closing windows, which is the actual point of the change.
- `clean_up_workspaces` no longer reaps. It keeps only the invariants the reaper also
  maintained: a trailing empty workspace, and at least `MIN_NUM_WORKSPACES` of them. Every call
  site is unchanged — the policy moved, not the plumbing.
- The layout invariant "no non-last non-active empty workspace" is **deleted**
  (`Monitor::verify_invariants`). Its violation is now the feature.

### What is closable

`Monitor::workspace_is_closable`: windowless, **unnamed**, not the last workspace, and not if
closing it would drop below `MIN_NUM_WORKSPACES`.

- *Unnamed*: naming a workspace is how you say you want it kept — it is already what made a
  workspace un-reapable (`has_windows_or_name`). A named empty workspace shows no close button.
- *Not the last*: the trailing empty is re-appended the instant it is removed, so a close button
  there would be a no-op that flickers.
- *Not below the minimum*: same reason GNOME's reap loop breaks at 2.

A fresh session therefore shows two thumbnails, neither closable. That is intended: the second
desktop is scratch space, not clutter to tidy away.

## Accepted losses

**Empty workspaces do not survive their output going away.** `Monitor::into_workspaces` and
`Layout::add_output` both filter to `has_windows_or_name()` when migrating workspaces between
monitors, so unplugging a display drops its empty desktops rather than piling them onto the
primary. Keeping them would mean a monitor that is plugged and unplugged repeatedly accumulates
anonymous empties on whichever output is left. The persistent-desktop promise is a within-session,
within-output one.

**niri's `empty-workspace-above-first` is gone** (config field, ~50 special-case sites, its
tests, and its wiki section). It is niri's way of doing workspaces, GNOME has no equivalent, and
it complicated every invariant this change touches. Two of its tests — `add_and_remove_output`
and `move_window_to_different_output` — were generic invariant checks that merely happened to set
the flag; they were kept, flagless.
