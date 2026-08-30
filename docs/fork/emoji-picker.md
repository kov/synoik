## Skin tone

One remembered tone plus a per-emoji popover, the shape `EmojiSelection` uses
(`js/ui/keyboard.js:862`). The popover lists the plain form **first**, then Unicode's tone
spellings; picking position 0 is what *forgets* a remembered tone, so there is always a way back to
the base. A remembered tone applies to any toned emoji picked plainly afterwards and leaves the
rest alone — GNOME's on-screen keyboard remembers nothing, but picking a tone once per emoji is the
alternative. It lives in memory for now; persisting it belongs with the recents, whose GTK records
carry a modifier field of their own.

Opened by **Shift+Return** from the keyboard and by a **secondary click** from the pointer — "the
same thing, varied", on both devices. A long press, which is how GNOME's on-screen keyboard and
macOS reach it, is deferred. While it is up it is modal over the grid: it is a question about one
cell, so nothing behind it moves until it is answered or dismissed with Escape.

## Category rail

Nine tabs along the panel's bottom edge, one per Unicode group, labelled with the same nine emoji
GNOME picks for its own section keys (`EmojiSelection._sections`, `js/ui/keyboard.js:884-894`) —
our table's groups are Unicode's order, which is the order that list is in. Tab and Shift+Tab walk
them from the keyboard.

The rail indexes the table's own order, so a click **clears the search**: a search result is not in
that order and an index into it would mean nothing. The latched tab follows the *selection*, not
the first visible row: a row straddles two groups whenever a group's length is not a multiple of
the column count, which is nearly always, and a tab lighting up for the group you are leaving reads
as a bug.

## Data

A generated table, **vendored in-tree and never fetched at build time**, from Unicode's
`emoji-test.txt` (order, groups, names) plus CLDR annotations (search keywords). The fetch is a
manual step run by `tools/emoji-table`, exactly as GNOME treats `data/update-osk-layouts.sh` — note
GNOME's `emoji.json` is generated and not in its tree, so there was nothing to copy.

Keywords are the point: "hello", "yay" and "lol" hit nothing in Unicode names. Skin tone is one
global preference plus a per-emoji variant popover, as in `EmojiSelection`
(`js/ui/keyboard.js:862`).

## Recents

Share GTK's history: `org.gtk.gtk4.Settings.EmojiChooser recently-used-emoji`, so our picker and
every GTK app's `Ctrl+.` show the same emoji. The schema type `a((aussasasu)u)` is authoritative for
shape only — field semantics are in `gtkemojichooser.c`, which is not on this machine. **Parse
defensively, and only write back once a read→write round-trip of GTK's own data is byte-identical.**
Until that is verified: import their recents, write ours to our own key.

## Keybinding

A new `emoji-picker` key in `org.synoik.keybindings` (`resources/schemas/`, mirrored by
`adopted_synoik_keybindings`, `src/gnome.rs:2128`), default `<Control><Alt>space`;
`synoik_accels_do_not_collide_with_gnome` proves it takes no GNOME chord. It shadows emacs'
`mark-sexp` (`C-M-SPC`) — compositor binds win, and the key is rebindable. Add it to the hotkey
overlay (`src/ui/hotkey_overlay.rs`).

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
7. **Recents**, per the round-trip rule above.
8. **Keybinding** + hotkey overlay.
