# Emoji picker

**Status: built.** A shell-owned emoji
picker on `<Control><Alt>space`, macOS style: it opens over whatever has focus, and picking inserts the
character into that client without the client ever losing focus.

**This is an addition, not a port.** GNOME has no shell-level emoji picker and no such binding —
GTK's `Ctrl+.` chooser is per-widget, inside the app. What we reuse from GNOME is the *delivery
mechanism*, which its on-screen keyboard already validates: `js/ui/keyboard.js` →
`Main.inputMethod.commit(str)`.

---

## Why it works: we are the input method

`src/input_method/` makes synoik the IM for every client, so `State::commit_text`
(`src/input_method/mod.rs:1103`) already sends arbitrary UTF-8 — ZWJ sequences and variation
selectors included — to the focused `zwp_text_input_v3`. The picker is a new caller of that
function plus a UI; no new protocol.

**Coverage, probed on this machine (2026-08-30), not recalled:**

| Client | text-input-v3 | Note |
| --- | --- | --- |
| GTK 3 / GTK 4 | yes | covers every GTK app and every VTE terminal |
| ghost | yes | `set_ime_allowed(true)` (`ghost-ui/src/lib.rs:2479`), handles `Ime::Commit` (`:7845`) |
| Firefox | yes | libxul logs "dispatching synthesized key events for Wayland text-input" |
| Chrome | yes | binds `zwp_text_input_manager_v3` (ozone) |
| kitty, alacritty | no | falls to the clipboard path below |
| Xwayland clients | no | text-input is a Wayland protocol; same fallback |

**There is no key-synthesis fallback, and GNOME does not have one either.** Mutter's virtual device
resolves a keyval only to a keycode already in the current group
(`pick_keycode_for_keyval_in_current_group_in_impl`, `meta-virtual-input-device-native.c:468`) and
warns `No keycode found for keyval` otherwise — so GNOME's own emoji→X11 path silently drops the
character. A wtype-style temporary keymap swap would work but makes every client reparse the keymap
mid-keystroke; rejected.

**Fallback: clipboard + OSD.** When nothing can receive text, picking sets the clipboard
(`clipboard.rs:202`) and shows an OSD saying so. Decided 2026-08-30.

## Placement

Anchor at the caret, fall back to the pointer: `State::text_anchor_rect`. The caret comes from
`zwp_text_input_v3.set_cursor_rectangle`, which IBus has no use for — the engine only needs it to
place a candidate popup, which is the unported Panel surface — so it is kept for us instead,
surface-local, **with the surface that sent it**. Nothing in the protocol says a rectangle is gone,
and only the recorded surface can tell that focus has moved off the field that described it.

Mapping is surface-local → window-local (`LayoutElement::buf_loc`) → output-local
(`Monitor::active_window_rectangle`) → global. That rectangle is the *unclamped* one on purpose:
`active_window_visual_rectangle` moves its own origin to the view edge when the window is partly
off-screen, which is exactly when a point mapped through it lands in the wrong place. Keeping the
picker on-screen is placement policy and belongs to the picker.

Declined, all falling back to the pointer: a client that never sent a rectangle, the degenerate
`0,0,0,0` GTK sends before its first layout, a rectangle whose surface no longer holds the keyboard,
and one from a client that does not own the engine (a shell entry of ours does, and its caret is not
that one). The picker opens on the display owning the anchor.

`zwp_text_input_v3` does not say *which* surface, so a text input on a popup or subsurface anchors
as if it were on the toplevel. Every client in the coverage table puts its entries on the toplevel.

The panel hangs below the anchor when it fits and above it when it does not, clamped to the
output either way — the caret's own line stays visible. Its size is fixed (`EmojiPicker::WIDTH` /
`HEIGHT`): a grid that resized as the search narrowed would move the cell under the pointer on
every keystroke.

## The grab: keys without focus

`text-input-v3` enter/leave rides `wl_keyboard` focus, and every shell-owned `KeyboardFocus` variant
returns `surface() == None` (`src/synoik.rs:1781`). So the picker **does not appear in
`update_keyboard_focus` at all**: the client keeps `wl_keyboard` focus, and the picker intercepts
keys from the input filter instead (`src/input/mod.rs`, the arm after the panel popover's).

`FilterResult::Intercept` returns before `input_forward` (smithay `input/keyboard/mod.rs:971`), so an
intercepted key delivers neither the key *nor* the modifier update to the client. Three clauses keep
the client's view of the keyboard truthful:

1. **Presses are intercepted** — the picker owns them.
2. **Releases of keys the client already holds are forwarded.** A key held when the picker opens
   (autorepeating in a terminal) would otherwise repeat forever, since Wayland clients repeat
   themselves. This needs no snapshot of the held keys: `suppressed_keys` is the ledger of what the
   compositor swallowed, so `pressed → insert` / `release found → intercept` / `release not found →
   forward` — the idiom every modal arm here already uses — *is* the rule.
3. **Modifier keys are forwarded both directions**, so Ctrl/Alt never stick. This is where the
   picker differs from every other grab in the filter, which forward modifier releases only: those
   move focus, and a keyboard leave releases every key client-side. Nothing moves focus here. The
   opening chord needs no special case: the client saw the Ctrl/Alt presses before the bind fired,
   and clause 3 delivers their releases.

Bindings that make sense over a picker (volume, media, brightness — all `Spawn` here) still resolve,
through `allowed_during_emoji_picker`, GNOME's `ActionMode.ALL` shape. Deliberately *not*
`allowed_during_popup`: opening the tray or quick settings over the picker would put two things on
screen fighting for the same keys. A click on another window closes the picker and then proceeds
normally.

**The search entry does not use the IM.** Routing it through `im_offer_shell_key` would make it a
`ShellEntry`, and `sync_im_focus` (`mod.rs:578`) would move `ImFocus` off `Client` and reset the
engine — the very focus we are protecting. Plain `key_char` editing over `ui::text_edit::TextEdit`;
no dead keys in the search box, which costs nothing for ASCII emoji names.

**Where the bind is inert:** every modal arm sits above the picker's in the filter, so the picker is
outranked by the lock shield, the run/end-session/polkit dialogs, a keyboard move-resize grab and a
panel popover without a single check of its own.

**It owns the cursor while it covers the pointer**, and it is the only overlay that has to force
that at the point of *write*. Every other one suppresses the window in `contents_under`, so the
client gets a pointer leave and stops being able to set the cursor at all; here the window keeps
pointer focus, so its I-beam would otherwise stand over the emoji grid. `SeatHandler::cursor_image`
remembers what the client asked for and substitutes the arrow while
`Synoik::emoji_picker_owns_cursor`; leaving the panel — or closing it under a parked pointer, which
is why every close goes through `State::close_emoji_picker` — puts the client's own back, since the
client never saw a leave and will not re-set it by itself.

Keys repeat through `RepeatKey::EmojiPicker`, the same route the shell's other own-drawn surfaces
take: a Wayland compositor is told about one press per physical key and leaves repeating to the
client, which does nothing for a surface we draw ourselves.

## Skin tone

One remembered tone plus a per-emoji popover, the shape `EmojiSelection` uses
(`js/ui/keyboard.js:862`). The popover lists the plain form **first**, then Unicode's tone
spellings; picking position 0 is what *forgets* a remembered tone, so there is always a way back to
the base. A remembered tone applies to any toned emoji picked plainly afterwards and leaves the
rest alone — GNOME's on-screen keyboard remembers nothing, but picking a tone once per emoji is the
alternative. It lives in memory for now: a recent already carries the tone it was
picked with, so the memory only affects the *next* new emoji, and persisting one more preference
buys little.

Opened by **Shift+Return** from the keyboard and by a **secondary click** from the pointer — "the
same thing, varied", on both devices. A long press, which is how GNOME's on-screen keyboard and
macOS reach it, is deferred. While it is up it is modal over the grid: it is a question about one
cell, so nothing behind it moves until it is answered or dismissed with Escape.

## Category rail

Ten tabs along the panel's bottom edge: the recents, then one per Unicode group, labelled with the
same nine emoji GNOME picks for its own section keys (`EmojiSelection._sections`,
`js/ui/keyboard.js:884-894`) — our table's groups are Unicode's order, which is the order that list
is in. The recents tab is GTK's chooser's, which opens on it; GNOME's keyboard has no history to
show. Tab and Shift+Tab walk them from the keyboard.

The nine group tabs are positions *within* the table's order, so the rail derives them from the
selection; the recents tab is a list of its own, so it is state. A search outranks either — it is
how you leave a tab — and no tab latches while one is up.

A tab lights on hover and the latched one carries the heavier `style::SELECTED_WASH`, the same
rule the grid cells follow: both wash the same direction, so the current one has to read heavier
than a hovered neighbour or the user cannot tell which one Enter takes.

The rail indexes the table's own order, so a click **clears the search**: a search result is not in
that order and an index into it would mean nothing. The latched tab follows the *selection*, not
the first visible row: a row straddles two groups whenever a group's length is not a multiple of
the column count, which is nearly always, and a tab lighting up for the group you are leaving reads
as a bug.

## Colour emoji: routing, then rasterization

Emoji draw in colour through two pieces, both built.

**Routing.** Text known to be emoji asks for the face **by name** rather than trusting the fallback
chain: `TextContext::shape_emoji` / `SpanFamily::Emoji` (`synoik-vk/src/text.rs`), reached through
`TextShaper::shape_emoji`. cosmic-text's Unix fallback list puts `Noto Color Emoji` *after*
`DejaVu Sans`, which also covers part of the emoji block — measured, the picker's own cells come
out in colour either way, because DejaVu has no U+1F600; asking by name is what keeps that from
being a property of a list order that is not a contract. Steering the *whole* UI's fallback is a
different matter: it would change how `☺`, `❤` and the other dual-presentation characters render
in every existing label, which needs per-character presentation itemization and is not part of this
feature. Emoji inside ordinary labels therefore stay monochrome for now.

**Rasterization.** swash reads the COLR **v0** table only (`scale/color.rs`), and Fedora ships Noto
Color Emoji as COLRv1 with no bitmap strikes — for U+1F600 it returns nothing from
`Source::ColorOutline`/`ColorBitmap` and a 0×0 mask from `Source::Outline`, because a COLRv1
glyph's base outline is empty. So `synoik-vk/src/colr.rs` is ours: skrifa walks the paint graph,
tiny-skia draws transforms, clip glyphs, clip boxes, layers with every COLR composite mode, and
solid/linear/radial/sweep brushes into a premultiplied RGBA pixmap.

Those land in a **second atlas** beside the R8 coverage one — its own residency index, its own
image, its own growth generation, created only once a colour glyph appears (RGBA is 4× the bytes).
`PlacedGlyph::color` picks which one a glyph samples, and `text_color.frag` samples it: the glyph
carries its own colours, so only the tint's *alpha* applies, which is what lets a fading label take
its emoji with it.

## Data

A generated table, **vendored in-tree and never fetched at build time**, from Unicode's
`emoji-test.txt` (order, groups, names) plus CLDR annotations (search keywords). The fetch is a
manual step run by `tools/emoji-table`, exactly as GNOME treats `data/update-osk-layouts.sh` — note
GNOME's `emoji.json` is generated and not in its tree, so there was nothing to copy.

Keywords are the point: "hello", "yay" and "lol" hit nothing in Unicode names. Skin tone is one
global preference plus a per-emoji variant popover, as in `EmojiSelection`
(`js/ui/keyboard.js:862`).

## Recents

Newest first, no repeats, capped at 21 — GTK's own `MAX_RECENT` (`gtkemojichooser.c`). Stored as the
strings the picker inserts, skin tone included, in `org.synoik.emoji recently-used-emoji`
(`resources/schemas/`). A recent is a *spelling*, not a table index: `tools/emoji-table` folds a
tone variant into its base's `tones` column rather than giving it an entry, so a cell carries a tone
beside its index (`Slot`), and a recent the vendored table cannot spell drops out of the grid
without leaving the history.

**We read GTK's history and write our own.** `org.gtk.gtk4.Settings.EmojiChooser
recently-used-emoji` seeds ours when ours is empty, so a fresh session starts with the emoji the
user already reaches for; from the first pick the two diverge. Its type `a((aussasasu)u)` is
authoritative for shape only, and `gtkemojichooser.c` for GTK 4 is not on this machine — the two
fields the import needs come from the key's own `gsettings describe`: the inner tuple opens with the
codepoints, and the trailing `u` is the Fitzpatrick modifier substituted for a placeholder (`0`, or
U+1F3FB) among them. A placeholder left unsubstituted drops out; anything that does not decode skips
its record, not the load.

Writing GTK's key stays deferred behind a read→write round-trip of their own data coming back
byte-identical, and **that cannot be demonstrated here**: the key is empty on this machine, and
producing a value with GTK's chooser only proves we can reproduce what we just watched it write.
It wants a machine with a real history in it.

## Keybinding

The `emoji-picker` key in `org.synoik.keybindings` (`resources/schemas/`, mirrored by
`adopted_synoik_keybindings` in `src/gnome.rs`), default `<Control><Alt>space`;
`synoik_accels_do_not_collide_with_gnome` proves it takes no GNOME chord and
`our_schema_matches_the_table` holds the mirror to the schema. It shadows emacs' `mark-sexp`
(`C-M-SPC`) — compositor binds win, and the key is rebindable. It is in the hotkey overlay
(`src/ui/hotkey_overlay.rs`).

`KeybindingAction::Synoik` maps straight through `action_for_keybinding`, so the overlay's
`hide_not_bound` sees it bound without a mapping arm of its own.

## Slices

1. ~~**Data + generator.**~~ `tools/emoji-table` and `resources/emoji-table.txt`, read by
   `src/emoji.rs`.
2. ~~**Insertion seam.**~~ `State::insert_text` → shell entry, else the client's text input, else
   clipboard + OSD; driven by `debug-insert-text` and pinned by three conformance tests.
3. ~~**Colour glyphs.**~~ `synoik-vk/src/colr.rs`, the RGBA atlas beside the mask one,
   `text_color.frag`, and `SpanFamily::Emoji` routing.
4. ~~**Caret anchor.**~~ `State::text_anchor_rect`, per the placement section above.
5. ~~**The grab.**~~ `src/ui/emoji_picker.rs`, `Action::ToggleEmojiPicker`, and the filter arm
   described above.
6. ~~**UI.**~~ `src/ui/emoji_picker.rs`.
7. ~~**Recents.**~~ `org.synoik.emoji`, seeded from GTK's key; writing GTK's stays deferred.
8. ~~**Keybinding** + hotkey overlay.~~ `emoji-picker`, `<Control><Alt>space`.
