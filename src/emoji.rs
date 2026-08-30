// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The emoji table the picker shows and searches.
//!
//! Vendored, never fetched: `resources/emoji-table.txt` is generated from Unicode's
//! `emoji-test.txt` and CLDR's English annotations by `tools/emoji-table`, which runs by hand when
//! either ships a release (see its `README.md`). Group order, entry order and names are Unicode's;
//! the search keywords are CLDR's, and they are the point — "party", "thumbs" and "joy" all have
//! to find something, and Unicode names alone answer none of them.
//!
//! English only for now. CLDR ships every locale, but one language is 148 KB and all of them are
//! not; picking the session locale's file is a later slice, not a format change.

use std::ops::Range;
use std::sync::OnceLock;

static TABLE_SOURCE: &str = include_str!("../resources/emoji-table.txt");

/// One emoji, with everything the picker needs to show, search and vary it.
#[derive(Debug)]
pub struct Emoji {
    /// The fully-qualified sequence to insert.
    pub ch: &'static str,
    /// Unicode's name, e.g. "waving hand".
    pub name: &'static str,
    /// CLDR search keywords.
    pub keywords: Vec<&'static str>,
    /// Skin-tone spellings in Unicode's order, empty when the emoji takes none. A one-person
    /// emoji has the five tones, light → dark; a two-person one has every combination of them,
    /// up to 25.
    pub tones: Vec<&'static str>,
    /// `name` and `keywords` lowercased once, so a search never lowercases the table.
    name_lc: String,
    keywords_lc: Vec<String>,
}

impl Emoji {
    pub fn has_tones(&self) -> bool {
        !self.tones.is_empty()
    }
}

/// A Unicode group ("Smileys & Emotion"), naming a contiguous run of [`Table::entries`].
#[derive(Debug, Clone)]
pub struct Group {
    pub name: &'static str,
    pub entries: Range<usize>,
}

#[derive(Debug)]
pub struct Table {
    entries: Vec<Emoji>,
    groups: Vec<Group>,
}

/// The parsed table, built once on first use.
pub fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| Table::parse(TABLE_SOURCE))
}

impl Table {
    pub fn entries(&self) -> &[Emoji] {
        &self.entries
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn group_entries(&self, group: &Group) -> &[Emoji] {
        &self.entries[group.entries.clone()]
    }

    /// Emoji matching every word of `query`, best first.
    ///
    /// Words are ANDed: "red heart" must not return every heart. Within a word a whole match
    /// outranks a prefix and a prefix outranks a substring, so "cat" leads with 🐈, whose name it
    /// is, not with the first of the fifty entries that merely contain those letters. Ties keep
    /// Unicode's order, which groups related emoji together and is the order the grid shows.
    pub fn search(&self, query: &str) -> Vec<&Emoji> {
        let query = query.trim().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }

        let mut hits: Vec<(u32, usize)> = Vec::new();
        for (at, emoji) in self.entries.iter().enumerate() {
            let mut total = 0;
            for word in &words {
                match emoji.score(word) {
                    0 => {
                        total = 0;
                        break;
                    }
                    score => total += score,
                }
            }
            if total > 0 {
                hits.push((total, at));
            }
        }

        // Descending score, then table order — `sort_by_key` is stable, so the second is free.
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        hits.into_iter().map(|(_, at)| &self.entries[at]).collect()
    }

    fn parse(source: &'static str) -> Table {
        let mut entries = Vec::new();
        let mut groups: Vec<Group> = Vec::new();

        for line in source.lines() {
            // "# " and not '#': the keycap emoji "#\u{fe0f}\u{20e3}" opens a data line with a hash.
            if line.starts_with("# ") || line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('<').and_then(|l| l.strip_suffix('>')) {
                if let Some(previous) = groups.last_mut() {
                    previous.entries.end = entries.len();
                }
                groups.push(Group {
                    name,
                    entries: entries.len()..entries.len(),
                });
                continue;
            }

            let mut fields = line.split('\t');
            let (Some(ch), Some(name), Some(keywords), Some(tones)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                debug_assert!(false, "malformed emoji table line {line:?}");
                continue;
            };
            let keywords: Vec<&str> = keywords.split('|').filter(|k| !k.is_empty()).collect();
            entries.push(Emoji {
                ch,
                name,
                name_lc: name.to_lowercase(),
                keywords_lc: keywords.iter().map(|k| k.to_lowercase()).collect(),
                keywords,
                tones: tones.split(' ').filter(|t| !t.is_empty()).collect(),
            });
        }

        if let Some(last) = groups.last_mut() {
            last.entries.end = entries.len();
        }
        Table { entries, groups }
    }
}

impl Emoji {
    /// How well one lowercased search word fits this emoji; 0 is no match at all.
    fn score(&self, word: &str) -> u32 {
        let name = self.name_lc.as_str();
        let words = || name.split(|c: char| !c.is_alphanumeric());
        if name == word {
            return 100;
        }
        if words().any(|w| w == word) {
            return 50;
        }
        // A keyword the user typed whole beats a name merely *starting* with those letters:
        // "lol" means the emoji whose keyword it is, not the lollipop whose name begins with it.
        if self.keywords_lc.iter().any(|k| k == word) {
            return 45;
        }
        if name.starts_with(word) {
            return 40;
        }
        if words().any(|w| w.starts_with(word)) {
            return 30;
        }
        if self.keywords_lc.iter().any(|k| k.starts_with(word)) {
            return 25;
        }
        if name.contains(word) || self.keywords_lc.iter().any(|k| k.contains(word)) {
            return 10;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(ch: &str) -> &'static Emoji {
        table()
            .entries()
            .iter()
            .find(|e| e.ch == ch)
            .unwrap_or_else(|| panic!("{ch} is not in the table"))
    }

    #[test]
    fn the_table_parses_whole() {
        let table = table();
        // Emoji 17.0 has 1914 base entries in nine displayable groups; a table that lost a chunk
        // still parses, so the count is the check.
        assert_eq!(table.entries().len(), 1914);
        assert_eq!(table.groups().len(), 9);
        assert_eq!(table.groups()[0].name, "Smileys & Emotion");
        assert_eq!(table.groups().last().unwrap().name, "Flags");

        // Every entry is complete, and the groups tile the table with no gap or overlap.
        for emoji in table.entries() {
            assert!(!emoji.ch.is_empty(), "{:?} has no character", emoji.name);
            assert!(!emoji.name.is_empty(), "{} has no name", emoji.ch);
            assert!(!emoji.keywords.is_empty(), "{} has no keywords", emoji.ch);
        }
        let mut at = 0;
        for group in table.groups() {
            assert_eq!(group.entries.start, at, "gap before {}", group.name);
            at = group.entries.end;
        }
        assert_eq!(at, table.entries().len());
    }

    #[test]
    fn tone_variants_ride_their_base() {
        let wave = find("\u{1f44b}");
        assert_eq!(wave.name, "waving hand");
        assert_eq!(wave.tones.len(), 5);
        assert_eq!(wave.tones[0], "\u{1f44b}\u{1f3fb}");
        assert_eq!(wave.tones[4], "\u{1f44b}\u{1f3ff}");
        // A toned spelling is never an entry of its own.
        assert!(!table().entries().iter().any(|e| e.ch == wave.tones[0]));
        // Most emoji take no tone at all.
        assert!(!find("\u{1f600}").has_tones());
    }

    #[test]
    fn keywords_carry_the_searches_names_cannot() {
        // None of these words appear in the emoji's Unicode name; without CLDR they find nothing.
        assert_eq!(table().search("hello")[0].ch, "\u{1f44b}");
        assert_eq!(table().search("yay")[0].ch, "\u{1f603}");
        // Six emoji list "lol"; they tie, so all we can pin is that they are the whole answer
        // and 😂 is among them. Which of a tie leads is Unicode's order until recents reorder it.
        let lol = table().search("lol");
        assert!(lol[..6].iter().any(|e| e.ch == "\u{1f602}"));
    }

    #[test]
    fn a_whole_match_outranks_a_prefix() {
        // 🐈 *is* "cat"; 🐱 is "cat face" and 😹 only lists "cat" as a keyword. And "lol" must not
        // lead with the lollipop, whose name merely starts with those three letters.
        let hits = table().search("cat");
        assert_eq!(hits[0].ch, "\u{1f408}");
        assert!(hits.iter().any(|e| e.ch == "\u{1f431}"));
        assert!(hits.iter().any(|e| e.ch == "\u{1f639}"));
        assert!(table().search("lol")[0].ch != "\u{1f36d}");
    }

    #[test]
    fn words_are_anded_not_ored() {
        let hits = table().search("red heart");
        assert_eq!(hits[0].ch, "\u{2764}\u{fe0f}");
        for hit in &hits {
            assert!(
                hit.score("red") > 0 && hit.score("heart") > 0,
                "{} matched only half the query",
                hit.ch
            );
        }
        assert!(table().search("no such emoji at all").is_empty());
    }

    #[test]
    fn flags_are_searchable_by_country() {
        let hits = table().search("brazil");
        assert_eq!(hits[0].ch, "\u{1f1e7}\u{1f1f7}");
    }
}
