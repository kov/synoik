# Multi-display: workspace groups, the display they belong to, and what moves when they move

**Status: design agreed 2026-08-23. §1, §4 and §6's drag are implemented; §2, §3 and §5 are not.**
Approved by Gustavo. This is the plan the
per-monitor-workspaces divergence (`dynamic-workspaces-divergence.md` §4) owes: what happens when a
display goes away, comes back, or is a different size than the one a workspace grew up on.

The model is macOS Mission Control's, with the differences recorded here. A workspace **belongs to
a display**, as a member of that display's group. The group survives the display being unplugged,
survives a logout, and returns as intact as the user's own edits allow. Where GNOME has no answer
because mutter has one global workspace list, this document says what ours is.

## The five things this settles

1. One notion of display identity, shared by the live layout and the session store.
2. A workspace's home display and its place in that display's strip, remembered across an unplug
   and a restart.
3. Which display a keybinding or a gesture acts on: the one under the pointer, with focus as the
   keyboard-only fallback.
4. Moving a workspace between displays, by drag and by menu.
5. What happens to a window whose work area just got smaller, and how it gets its size back.

## 1. One display identity

**Implemented.** `OutputIdentity` (`src/output_identity.rs`) is the single answer, shared by the
layout, the session store and the `monitors.xml` alignment it was shaped for. It replaced
`layout::workspace::OutputId`, a name string matched through `OutputName` by connector **or** the
make/model/serial triple. `OutputIdentity` is the one that can tell two displays apart when a
connector is reused, it is already the store's, and `monitors.xml` keys layouts by it.

What the layout gained and lost by the swap:

- **Gained a veto.** A *different* panel plugged into the connector a workspace's display used no
  longer inherits its workspaces — the old connector branch of the OR handed them over.
- **Lost the renamed-connector reach.** The same panel returning on a *different* connector used to
  be reclaimed through the make/model/serial branch, and now is not. Identity-only matching is the
  deferred half, and it stays deferred in both places at once: a session and its layout must never
  disagree about which display is which. A dock that renumbers its DisplayPort connectors is where
  this is felt, and is the reason to land it.

A home tag can still come from configuration rather than from hardware
(`OutputIdentity::from_connector` — a connector with no EDID behind it, which vetoes nothing). It
upgrades to the full identity the first time it meets its display, in `Workspace::set_output`. The
identity built with no display at all carries an empty connector, which names nothing: parking
empties by identity (§2) must not key on one.

## 2. A workspace remembers its home

`Workspace::original_output` is already the home tag, and the layout already distinguishes the two
ways a workspace changes display:

- an **explicit move** (the user drags it, or `move-workspace-to-monitor`) rewrites the tag —
  `src/layout/monitor.rs:991,1022,1051`, `src/layout/mod.rs:4238,4249`;
- an **evacuation** (the display was unplugged) leaves the tag alone, so the workspace is homeless
  rather than re-homed.

That is the semantics this design wants, and `Layout::add_output` already acts on it: a returning
display takes back every workspace on the primary whose tag matches (`src/layout/mod.rs:1038-1074`),
and `last_active_workspace_id` restores which of them was active. Three things are missing.

**A home ordinal.** The reclaim preserves the group's relative order but nothing pins *where* in the
strip a workspace sat, so a reorder performed while the group was homeless is unrecoverable. Each
workspace carries `(home: OutputIdentity, home_ordinal: usize)`, stamped at detach and re-stamped on
an explicit move. Reclaim sorts by ordinal. A workspace the user moved while homeless keeps its new
place because the move re-stamped it; everything else lands where it was.

Best-effort by construction, and deliberately not a snapshot of the whole arrangement. Keying the
*configuration* the way `monitors.xml` does would restore a stale arrangement and fight the edits
the user made in the meantime; per-workspace tags degrade one workspace at a time.

**Empty workspaces survive.** This reverses the "Accepted losses" clause in
`dynamic-workspaces-divergence.md`. `Monitor::into_workspaces` filters `has_windows_or_name()` at
unplug and `Layout::add_output` filters again on reclaim, so a workspace emptied while homeless can
never come home. The reversal keeps the reason the loss existed — anonymous empties must not pile up
on the surviving display — by not putting them there in the first place:

> An absent display's **empty** workspaces are parked in `Layout`, keyed by identity, and never
> materialize on the primary. Only its **windowed** workspaces are appended to the primary's strip.

Nothing accumulates on the survivor, the strip stays honest, and an emptied homeless workspace still
has a group to return to. Naming a homeless workspace does **not** re-home it: a name says "keep",
not "move".

**A lifetime.** Parked groups live for the session and die with it. Nothing about a workspace is
persisted on its own — across a restart, the saved application sessions are the only carriers (§3).

## 3. Restore derives the stack; it does not persist one

A session record already names a display and carries an output-local rect
(`session-management-port.md`, "A record is anchored to a display"). What it cannot do today is say
*where in that display's stack* its workspace was, because nothing remembers the stack — and the
absent-display offset it falls back on is computed per session, so two applications restoring at
once interleave their homeless windows.

The stack is **derived from the store, not persisted**:

- `SessionStore::load` reads every session at once (`src/session_state.rs:331`), so at startup the
  whole record set is visible, not just the application currently restoring. Sort the distinct
  `(display identity, workspace name or index)` slots per display into one **ordering**. Restore
  consults that ordering instead of counting, so the result no longer depends on who restores first
  and the interleave is gone.
- The ordering **reserves nothing**. A workspace materializes only when a window actually restores
  into its slot. That is the answer to "who counts": a stale session that never restores creates no
  workspaces, however many it names.

A workspace name still outranks the index when matching (`session-management-port.md`), which is
what makes naming worth a UI (§6).

## 4. The pointer decides; focus is the keyboard's answer

Every workspace-scoped action routes through `Layout::active_monitor` / `active_output`
(`src/layout/mod.rs:2078,2238`), and `focus_output` is called from about fifteen sites in
`src/input/mod.rs`. One chooser replaces that for workspace-scoped work:

> The monitor under the pointer, when a pointer exists and the cursor is on a monitor. Otherwise the
> focused monitor.

"A pointer exists" means the seat has a pointer device and the cursor is on a monitor — not merely
that libinput once saw one, so a machine with the touchpad disabled behaves keyboard-only. The
pointer half already exists as `Synoik::output_under_cursor`.

Focus stays as it is for accessibility and for keyboard-only navigation, which is the whole reason
it survives: with no pointer, `Super+2` has to mean something, and the focused display is what it
means.

The split:

- **Workspace-scoped — pointer decides.** `FocusWorkspace{,Up,Down,Previous}`, workspace switch
  gestures, the thumbnail strip and its close/reorder affordances. The overview is **not** in this
  list: `overview_open` is a `Layout`-wide flag stamped onto every monitor (`Layout::add_output`)
  and GNOME opens it everywhere, which this design does not change. What the chooser decides there
  is only which display's strip a keyboard-driven workspace action lands on while it is open.
- **Window-scoped — focus decides.** `MoveWindowToWorkspace*`, `MoveColumnToWorkspace*`, maximize,
  fullscreen, close. These follow the window, and the window is on the display it is on.
- **Monitor-scoped — unchanged.** `FocusMonitor*` names a display outright.

niri's `FocusWorkspaceDownUnderMouse` / `FocusWorkspaceUpUnderMouse` (`src/input/mod.rs:4089,4106`)
are niri's way of offering this as a *separate binding*. They go: the behavior becomes the default,
and a second set of actions for it is exactly the niri-shaped duplication the fork tenet drops.

## 5. A workspace whose work area changed

This applies to any change of the work area — a display swapped underneath a workspace, a workspace
dragged to another display, a mode or scale change, a strut appearing. Maximized, fullscreen and
edge-tiled windows need none of it: their geometry is a *function* of the work area and
`FloatingSpace::refit_to_working_area` (`src/layout/floating.rs:343`) already re-derives it. What
follows is for normal floating windows.

### The move is mutter's, and ours is currently the naive version of it

mutter's `move_rect_between_rects` (`~/Projects/mutter/src/core/window.c:4520-4562`, reached from
`meta_window_update_for_monitors_changed` → `meta_window_move_between_rects`, `:4137`) has two
branches:

- **the window fits in both areas** — preserve the *slack* fraction,
  `rel = (x - area.x) / (area.w - win.w)`, replayed as `new.x + rel * (new.w - win.w)`. A
  right-aligned window stays right-aligned, a centered one stays centered, and the result can never
  hang off the edge;
- **otherwise** — preserve the *center* fraction, clamped to `[ε, 1-ε]`.

Ours stores a plain `top-left / area.size` fraction
(`Data::logical_to_size_frac_in_working_area`, `src/layout/floating.rs:182`), which is the second
branch without the center term. That is the bug the user named: a window at fraction 0.5 keeps its
left edge at half the width and overflows the right. Adopt both branches.

### The shrink is ours, and it is remembered

mutter only *moves* here; `constrain_size_limits` applies the client's own hints and nothing shrinks
a normal window to fit a smaller monitor, so it overflows with the titlebar kept reachable. We do
better, and pay for it with one field:

- **Fit is size first, then move.** Clamp each axis **independently** — a too-tall window must not
  also narrow — bounded below by the client's minimum size (`ensure_min_max_size`). Then run the
  two-branch move above.
- **`displaced_rect: Option<Rect>` per tile**, holding the rect we overrode. This is mutter's
  `unconstrained_rect` in Wayland clothes: mutter can keep the desired rect and re-derive the
  visible one through the constraint system on every pass, but here the client owns the buffer, so
  the shrink has to be a real configure and the desired rect has to be kept on the side.
- **Stored verbatim, restored verbatim.** Never re-derived by inverting the shrink — a recovered
  value is not the original, and rounding makes that literally true.
- **Cleared on any user-driven resize.** Resizing is the user saying this is the size now.
- **Applied when a work area grows enough to hold it again**, which is what makes a workspace
  returning to its big display return the windows to their old geometry.
- **Unanimated**, for `refit_to_working_area`'s reason: the user did not ask for this, the area
  moved underneath them, and mutter re-constrains instantly.
- **Persisted into the session record**, so the "back on the big monitor tomorrow" case works
  across a logout.

### Configuring a window to fit is allowed

`xdg_toplevel.configure` is how a compositor sizes a window and clients must honour it; every tiling
compositor works this way. The narrower rule this fork keeps is about a *round trip*: never derive a
configure from a size the client itself just reported. The window that lost 35px per launch was
configured at its own `geometry().size`, sampled before its CSD toolkit had drawn decorations, which
froze a transient that would otherwise have healed (fix `0aae1a47`). Compositor-owned math — fit to
work area, maximize, tile — is the legitimate case, and this is one.

Two live footnotes: a configure's size means the window's *geometry*, never its buffer; and any
snapshot of client geometry must first ask whether a configure we sent is still unanswered
(`Tile::restore_in_flight`, fix `43b7622f`).

### When nothing fits, maximize

If the client's minimum size means no shrink can fit the new work area, maximize instead: it is the
honest "as large as we can give you" answer, and `save_restore_rect` / `restore_normal` already
carry the rect to come back to.

- It reuses `Workspace::auto_maximize_if_too_big` (`src/layout/workspace.rs:2172`) — its
  `has_maximize_func` guard, its `org.gnome.mutter auto-maximize` gsetting check, and its restore-size
  arithmetic. Honouring the gsetting matters: a user who turned auto-maximize off should not meet it
  here.
- It carries an **auto-maximized mark**, on the tile and in the session record (`state = Maximized`
  plus the mark, `floating-rect` holding the original). Only auto-maximized windows are un-maximized
  when the workspace comes home; a window the user maximized stays maximized. The mark persists
  across a restart.
- The map-time path has no such mark today, and gains one with this: an auto-maximize at map is
  currently indistinguishable from the user's own.

**The threshold is a knob we are not turning.** `auto_maximize_if_too_big` maximizes anything
covering more than `MAX_UNMAXIMIZED_WINDOW_AREA = 0.8` of the work area, and that is a *map-time*
policy. Displacement deliberately does **not** re-run it: doing so would maximize a deliberately
large window on every dock cycle. Making arrival on a smaller display maximize instead of shrink is
a one-line change to this predicate, kept known for later tuning.

## 6. Moving a workspace to another display

**Dragging a workspace from one display's strip to another's** is **implemented**. Most of the
machinery was already in the tree:

- `ThumbGrab` resolves `Synoik::output_under` on every motion and used to decline the case where
  that was not the output the drag started on (`src/input/thumb_grab.rs`). That branch is now the
  crossing.
- `Monitor::insert_workspace` / `remove_workspace_by_idx` maintain the workspace-list invariants on
  both sides, so a crossing cannot leave either row short.
- The overview opens on every monitor (`overview_open` is a `Layout`-wide flag), so both strips are
  on screen for the whole drag. Without that there would be nothing to aim at.

**A crossing moves the workspace for real.** The display under the pointer takes it and runs an
ordinary within-display drag on it; the display it left closes its row up. The alternative — the
source keeping the workspace while the target draws it — means rendering one display's workspace
inside another display's row at another display's scale, and a workspace's render state is
configured per monitor. Moving the model instead is also what the cross-output *window* drag does,
and what Mission Control shows: the space transfers as you cross.

Four rules it keeps, three of which are what the crossing costs:

**A crossing is not a drop.** The home tag is left alone while the workspace is in mid-air, and the
carried display does not become active. Both happen at the drop, which is an *explicit move*
(§2) — and the display it lands on becomes active for the same reason a keybinding aim makes its
display active (§4).

**A carry that never lands goes back.** `Layout::thumb_carry` remembers where the workspace was
picked up, so the overview closing under a carried thumbnail returns it there. Merely dragging over
another display must not move a desktop the user never let go of.

**A carry that wanders leaves no residue.** A display cannot be left short — removing a workspace
restores the trailing empty and `MIN_NUM_WORKSPACES` immediately — so without bookkeeping every
round trip across the seam would hand out one extra empty desktop. `ThumbCarry::counts` records
what each display had before the carry left it, and `Monitor::shed_carry_padding_to` gives back
exactly that much, never a desktop with windows or a name. The shed runs **before** the drag is
re-armed on the far side: it renumbers the row, and a drag armed on a stale index carries the wrong
workspace.

**The grab travels as a fraction, not an offset.** Two displays' rows are different widths, so
carrying the pixel offset would land the pointer somewhere else on the thumbnail, or clean off it.

Still deferred: the layout does not know monitor positions (`src/layout/mod.rs:4887`, the same
reason a cross-output *window* drag teleports instead of animating), so the carried thumbnail is
handed over at the seam rather than travelling across it. The pointer is the anchor, so the motion
is continuous; the visible artefact is the thumbnail resizing at the boundary when the two
displays' strips differ in scale. An animated carry lands with monitor positions, together with the
window drag.

**A workspace context menu** — rename, close, and **Send to \<display\>** — is the keyboard- and
screen-reader-reachable way to express the same move, which a drag is not. Rename is the UI the
naming model has been missing: `Workspace::name`, `Layout::set_workspace_name` /
`unset_workspace_name` (`src/layout/mod.rs:5744,5783`), `Action::SetWorkspaceName` and the
`synoik msg` verbs all exist, but nothing in the shell offers it and `ui/thumbnail_chrome.rs` never
draws a name. A finishing touch, and what makes "a name outranks the index" (§3) worth anything to
a user.

## Order of work

The two the seat wants first are also the two with no dependencies, so they lead. Neither is wasted
work: the drop rewrites the home tag through the same call site the identity change will rename, and
the drag does not widen the unplug/replug gaps below — it only makes them easier to reach.

1. **The pointer-decides chooser** (§4), and the removal of niri's `*UnderMouse` actions —
   LANDED.
2. **The cross-output workspace drag** (§6) — LANDED.
3. **Identity unification** (§1) — LANDED.
4. **Home ordinal and parked empties** (§2) — the unplug/replug fidelity a daily dock cycle needs.
5. **Derived restore ordering with lazy materialization** (§3) — kills the absent-display offset.
6. **Displaced geometry** (§5) — the two-branch move, the per-axis shrink, `displaced_rect`, the
   auto-maximize mark.
7. **The workspace context menu and naming UI** (§6) — any time after 2.
