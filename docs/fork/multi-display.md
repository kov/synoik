# Multi-display: workspace groups, the display they belong to, and what moves when they move

**Status: design agreed 2026-08-23; §1–§7 are implemented.**
Approved by Gustavo. This is the plan the
per-monitor-workspaces divergence (`dynamic-workspaces-divergence.md` §4) owes: what happens when a
display goes away, comes back, or is a different size than the one a workspace grew up on.

The model is macOS Mission Control's, with the differences recorded here. A workspace **belongs to
a display**, as a member of that display's group. The group survives the display being unplugged,
survives a logout, and returns as intact as the user's own edits allow. Where GNOME has no answer
because mutter has one global workspace list, this document says what ours is.

## The six things this settles

1. One notion of display identity, shared by the live layout and the session store.
2. A workspace's home display and its place in that display's strip, remembered across an unplug
   and a restart.
3. Which display a keybinding or a gesture acts on: the one under the pointer, with focus as the
   keyboard-only fallback.
4. Moving a workspace between displays, by drag and by menu.
5. What happens to a window whose work area just got smaller, and how it gets its size back.
6. A named workspace is always present — through an unplug, a logout and a reboot.

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

Putting a **window** on a workspace is neither, and no longer re-homes one. niri adopted any
unnamed workspace into the monitor a window was added on (`Monitor::add_column` and its two
neighbours), which took an evacuated workspace away from the display it was waiting for the moment
the user opened something on it — the display then came back to less than it left. What the rule
was actually good for is kept: a workspace that has no home at all (made while no display was
connected) still takes the monitor's, and a home carrying a bare connector name still fills in its
EDID. `Workspace::adopt_home`.

That is the semantics this design wants, and `Layout::add_output` already acts on it: a returning
display takes back every workspace on the primary whose tag matches (`src/layout/mod.rs:1038-1074`),
and `last_active_workspace_id` restores which of them was active. Three things are missing.

**A home ordinal.** Implemented. The reclaim preserved the group's relative order but nothing
pinned *where* in the strip a workspace sat. Each workspace now carries a `home_ordinal` alongside
its home tag, and the reclaim sorts by it (stably, so anything without one keeps evacuation order).

`Monitor::stamp_home_ordinals` keeps them true, from `clean_up_workspaces` — the one place every
reorder, insertion and removal funnels through — and from `into_workspaces`, the detach. It has two
rules, because a strip holds two kinds of workspace:

- **its own** get their rank among the own group: all of them are present, so rank *is* position;
- **visitors**, evacuated from a display that is away, keep their *set* of ordinals and only permute
  it into the order they are in now. Their group is incomplete — its empty members are parked, not
  here — so re-ranking them would close the holes in the arrangement their display is coming back
  to. Permuting honours a reorder the user makes while they are visiting without inventing
  positions for the ones that are not here.

Best-effort by construction, and deliberately not a snapshot of the whole arrangement. Keying the
*configuration* the way `monitors.xml` does would restore a stale arrangement and fight the edits
the user made in the meantime; per-workspace tags degrade one workspace at a time.

**Empty workspaces survive.** Implemented, reversing the "Accepted losses" clause in
`dynamic-workspaces-divergence.md`. `Monitor::into_workspaces` used to filter `has_windows_or_name()`
at unplug and `Layout::add_output` filtered again on reclaim, so a workspace emptied while homeless
could never come home. Both filters are gone; the split is `Layout::park_empties`, on
`has_windows()` alone. The reversal keeps the reason the loss existed — anonymous empties must not
pile up on the surviving display — by not putting them there in the first place:

> An absent display's **empty** workspaces are parked in `Layout`, keyed by identity, and never
> materialize on the primary. Only its **windowed** workspaces are appended to the primary's strip.

Nothing accumulates on the survivor, the strip stays honest, and an emptied homeless workspace still
has a group to return to. A **named** empty is the exception and travels with the windowed ones: a
name makes a workspace furniture, and furniture is always present (§7). The cost is deliberate —
unplugging a display grows the survivor's strip by that display's named empties, every dock cycle.

Two things follow from parking, both wanted. `last_active_workspace_id` can now resolve to a parked
empty, so a display that was showing an empty desktop comes back showing it — and it is keyed by
identity rather than by connector name, so a different panel in the same socket cannot consume the
entry. And a parked workspace is deliberately **invisible**: it is not in `Layout::workspaces()`, so
IPC, a11y and every `find_workspace_by_*` are blind to it. `Layout::verify_invariants` walks the
parked list explicitly, into the same id- and name-uniqueness sets.

**A lifetime.** Parked groups live for the session and die with it. An *unnamed* workspace is not
persisted on its own — across a restart, the saved application sessions are the only carriers (§3).
A named one is (§7).

## 3. Restore derives the stack; it does not persist one — *implemented*

A session record names a display and carries an output-local rect (`session-management-port.md`,
"A record is anchored to a display"). What it cannot say is *where in that display's stack* its
workspace was, because nothing remembers the stack.

The stack is **derived from the store, not persisted**. `SessionStore::load` reads every session at
once, so restore ranks the distinct `(connector, workspace index)` slots of every display that is
**not connected**, displays in connector order and each display's slots in its own, and consults
that ordering instead of counting workspaces. Two applications restoring at once therefore agree,
which the per-session offset it replaces could not: each of them counted only its own records, so
one app's homeless window landed on the workspace the other was about to be restored onto.

The ordering **reserves nothing**. A slot becomes a workspace only when a window actually restores
into it, so a session that never comes back costs the ones that do nothing — not a workspace, and
not a position. Distinct slots, so what it would cost is a rank per workspace it names rather than
the number it names.

Where the block goes is a **position, not an index**. A slot materializes above every materialized
slot that ranks after it, and at the bottom of the strip when none of them is there yet — which is
what makes the arrangement independent of who restores first. The bottom is the top of the strip's
trailing run of empty, unclaimed workspaces, and one of those is taken over rather than added to: a
strip holding nothing of its own is all trailing run, and a block inserted below it would leave the
first desktop empty and push everything down one. A record whose display *is* connected keeps
landing on its saved index literally, and the growth that reaches it inserts **above** the block
rather than past it — the block floats at the bottom, so the same records land the same way whether
it arrived first or last.

What restore materializes is tagged as the absent display's, with the saved index as its home
ordinal, so plugging that display in reclaims it into the arrangement it was saved in — the same
thing an unplug and a replug do for a workspace that never left (§2).

Gaps do not survive a restart, unlike a live unplug and replug: nothing persists an *unnamed* empty
workspace, so a desktop the user left empty and unnamed is a desktop no record names. For the same reason a
restored ordinal can collide with one the display's *own* strip is already using this run — the
saved index knows nothing about it. The anonymous empty gives way (`Layout::drop_displaced_empties`)
rather than sitting between two desktops that were side by side; a **named** empty is not filler and
stays.

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

## 5. A workspace whose work area changed — *implemented*

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

The fraction-of-size version this replaces (`Data::logical_to_size_frac_in_working_area`) was the
second branch without the centre term: a window at fraction 0.5 kept its left edge at half the width
and hung the rest off the right. Both branches now, in `move_rect_between_areas`, at the one seam
where the old and the new area both exist.

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
- **Cleared on a user-driven resize, and on a drop onto another display.** Both are the user saying
  this is the geometry now. A plain *move* does not clear it: where the window sits and what size a
  narrow display forced on it are separate answers, and only the size was ever overridden.
- **Not gated on the recorded display coming back**, unlike a remembered position. A size is not
  display-local, so restoring a session while the big monitor is unplugged must not be what makes a
  window small forever; the position half falls back to wherever placement put it.
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

**A workspace context menu** — rename, close, and **Send to \<display\>** — is **implemented**:
a right-click on a thumbnail, or `show-workspace-menu`, which opens the overview first because the
menu hangs off a thumbnail. It is the keyboard- and screen-reader-reachable way to express the
move, which a drag is not. Close and Send both go through the calls the existing gestures use, so
the closable rule and the home-tag rewrite are not restated anywhere.

Rules it keeps:

- **A right press on the strip is the menu, before the overview's pan grab.** That grab took every
  right press in the overview, the strip's included — the same ordering the peek's left press
  needed.
- **The menu survives the overview opening.** An overview coming up dismisses an open popover; this
  one is *of* the overview, and the keyboard route opens both in one action.
- **A display name that is not unique earns its connector.** Two of the same monitor otherwise
  produce two identical rows.

**Rename** is the UI the naming model was missing. A workspace has no default name, so an unnamed
thumbnail carries no label — inventing "Workspace 3" would put chrome on every thumbnail to repeat
what its position already says. A named one wears the caption pill the window picker uses for
window titles. The entry takes the label's place, opens with the old name selected, commits on
Enter, abandons on Escape, and **unsets the name when emptied** — a workspace is allowed to have
none. A name another workspace already answers to is refused and the entry stays up: `set_workspace_name`
refuses duplicates silently, and gnome-shell's theme has no error state for an entry to wear, so
the entry staying open is the signal — with the workspace holding the name visible on the strip.

**Close is offered on a named workspace**, unlike every other thing a name protects it from: with
§7 a name outlives the session, so closing is how a user is rid of one. Requiring the name to be
cleared first would be two steps for one intent, and closing is deliberate enough on its own — it
is a menu row, or a button that only appears on hover.

## 7. A named workspace is always present

A name is not a label on a container of windows; it is the user saying this desktop exists. So a
named workspace is there whether or not anything is living on it, whether or not its display is
connected, and whether or not this is the same boot.

Three rules, and everything else follows:

- **It never parks.** An unplug moves it to the surviving display, keeping its home tag and
  ordinal, so a replug takes it home into the arrangement it left (§2).
- **It is persisted on its own**, in `$XDG_DATA_HOME/synoik/workspaces.json`: name, home display
  identity, home ordinal. Read at startup *before* any session is restored, so a window whose
  record names its workspace finds the one the store made rather than minting a second under a name
  the first one holds.
- **Closing it is how it goes away.** Clearing the name leaves an ordinary empty, which the strip
  reaps as usual; closing removes it outright.

**Not `org.gnome.desktop.wm.preferences workspace-names`.** That is GNOME's surface, and the tenet
says GNOME's wins — but mutter keys the array by *global workspace index* (`prefs.c:1870-1924`), and
an index is not an identity here: workspaces are per-monitor, so the number shifts whenever anything
above it is added, closed or reordered, and it cannot say which display a workspace belongs to. A
key we could not write correctly is not a key we can adopt.

**The file is a snapshot, never an edit.** It is rewritten whole whenever the layout's list stops
matching it, compared every refresh cycle and debounced like the session store, so no mutation site
owes it a call and it cannot drift from the strip. Entries are canonically ordered by home display
and ordinal, so a named workspace visiting a survivor while its display is unplugged does not read
as a change. Two states must never be mistaken for "no named workspaces", or startup wipes the file
it just read: before the store's own entries are materialized, and while no display is connected.

**Startup materializes on whatever display exists**, tagged with the display each workspace belongs
to — the same state an unplug leaves them in, so a display connected a moment later reclaims its own
through the reclaim path a replug uses.

## Order of work

The two the seat wants first are also the two with no dependencies, so they lead. Neither is wasted
work: the drop rewrites the home tag through the same call site the identity change will rename, and
the drag does not widen the unplug/replug gaps below — it only makes them easier to reach.

1. **The pointer-decides chooser** (§4), and the removal of niri's `*UnderMouse` actions —
   LANDED.
2. **The cross-output workspace drag** (§6) — LANDED.
3. **Identity unification** (§1) — LANDED.
4. **Home ordinal and parked empties** (§2) — LANDED.
5. **Derived restore ordering with lazy materialization** (§3) — LANDED.
6. **Displaced geometry** (§5) — LANDED.
7. **The workspace context menu and naming UI** (§6) — LANDED.
8. **Named workspaces are always present** (§7) — LANDED.
