# emoji-table

Generates `resources/emoji-table.txt`, the table `src/emoji.rs` embeds — the emoji the picker
shows, their names, their CLDR search keywords and their skin-tone spellings.

The table is **vendored, never fetched at build time**. Run this by hand when Unicode or CLDR ship
a release, then commit the regenerated file with the version bump in its header.

```sh
cd /tmp
curl -O https://unicode.org/Public/emoji/latest/emoji-test.txt
# Pick the current CLDR release tag from https://github.com/unicode-org/cldr/releases
CLDR=release-48-2
curl -O https://raw.githubusercontent.com/unicode-org/cldr/$CLDR/common/annotations/en.xml
curl -o derived-en.xml \
  https://raw.githubusercontent.com/unicode-org/cldr/$CLDR/common/annotationsDerived/en.xml

cd ~/Projects/gnome-shell-rs
cargo run --manifest-path tools/emoji-table/Cargo.toml -- \
  /tmp/emoji-test.txt /tmp/en.xml /tmp/derived-en.xml 48.2 resources/emoji-table.txt
cargo test --workspace emoji::
```

`the_table_parses_whole` pins the entry and group counts, so a Unicode bump fails it by design:
update the numbers in `src/emoji.rs` in the same commit as the regenerated file.

The CLDR version is an argument because CLDR's own files carry `<version number="$Revision$"/>`.

The generated file is a derivative of Unicode and CLDR data and carries their notice in its
header: © Unicode, Inc., Unicode License v3 (SPDX `Unicode-3.0`).

## What it checks

Every skin-tone spelling must resolve to the entry it varies, or the run fails — the alternative is
a picker that shows tone variants as separate emoji or drops them. Two foldings are legitimate and
reported rather than fatal:

- A tone part can sit anywhere in the comma list ("couple with heart: woman, man, dark skin tone,
  light skin tone"), so the base is the name minus the tone parts, not the text before the colon.
- Some sequences exist *only* toned: there is no "kiss: person, person", just "kiss" and its 25
  toned couples. Those fold into the head, which is the emoji the picker shows.

CLDR keys its annotations by the sequence with every `U+FE0F` stripped, so the lookup retries
stripped — without that, 365 of 1914 entries silently have no keywords. The run prints how many
entries ended up without any; it should be zero.
