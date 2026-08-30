# Emoji picker

**Status: the data, the insertion seam, colour glyphs and the caret anchor are built; no UI yet.** A shell-owned emoji picker on
`<Control><Alt>space`, macOS style: it opens over whatever has focus, and picking inserts the
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

## The grab: keys without focus

`text-input-v3` enter/leave rides `wl_keyboard` focus, and every shell-owned `KeyboardFocus` variant
returns `surface() == None` (`src/synoik.rs:1781`). So the picker **must not appear in
`update_keyboard_focus` at all**: the client keeps `wl_keyboard` focus, and the picker intercepts
keys in the input filter instead.

`FilterResult::Intercept` returns before `input_forward` (smithay `input/keyboard/mod.rs:971`), so an
intercepted key delivers neither the key *nor* the modifier update to the client. Three clauses keep
the client's view of the keyboard truthful:

1. **Presses are intercepted** — the picker owns them.
2. **Releases of keys the client already holds are forwarded.** A key held when the picker opens
   (autorepeating in a terminal) would otherwise repeat forever, since Wayland clients repeat
   themselves. `forwarded_pressed_keys` is `pub(crate)` in smithay, so snapshot
   `KeyboardHandle::pressed_keys()` minus `suppressed_keys` at open and forward releases for that
   set.
3. **Modifier keys are forwarded both directions**, so Ctrl/Alt never stick. The opening chord needs
   no special case: the client saw the Ctrl/Alt presses before the bind fired, and clause 3 delivers
   their releases.

Bindings that make sense over a picker (volume, media, brightness) still resolve, through the
existing `allowed_during_popup` filter shape (`src/input/mod.rs:1293`). A click on another window
closes the picker and then proceeds normally.

**The search entry does not use the IM.** Routing it through `im_offer_shell_key` would make it a
`ShellEntry`, and `sync_im_focus` (`mod.rs:578`) would move `ImFocus` off `Client` and reset the
engine — the very focus we are protecting. Plain `key_char` editing over `ui::text_edit::TextEdit`;
no dead keys in the search box, which costs nothing for ASCII emoji names.

**Where the bind is inert:** while locked (`is_locked()`), and while any modal `KeyboardFocus` owns
focus with no text target — switcher, screenshot UI, end-session. It stays live over the shell's own
entries (overview search, run dialog, folder/workspace rename), which work for free: `commit_text`
already routes `ImFocus::Shell` to `commit_into_shell_entry`.

## Colour emoji: routing, then rasterization

Emoji draw in colour through two pieces, both built.

**Routing.** cosmic-text's Unix fallback list puts `Noto Color Emoji` *after* `DejaVu Sans`, which
also covers the emoji block — so an emoji shaped with the UI face lands on DejaVu's monochrome
outline, silently. Text known to be emoji asks for the face by name instead:
`TextContext::shape_emoji` / `SpanFamily::Emoji` (`synoik-vk/src/text.rs`). Steering the *whole*
UI's fallback would change how `☺`, `❤` and the other dual-presentation characters render in every
existing label; getting that right needs per-character presentation itemization, and is not part
of this feature. Emoji inside ordinary labels therefore stay monochrome for now.

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
5. **The grab.** Open/close, the three key clauses above, no `KeyboardFocus` participation. Test that
   the client keeps focus, receives no printable keys, and is left with no stuck key or modifier.
6. **UI.** Grid, category rail, search entry, hover/keyboard selection, tone popover; chrome from
   `docs/fork/gnome-style-reference.md`.
7. **Recents**, per the round-trip rule above.
8. **Keybinding** + hotkey overlay.
