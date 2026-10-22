// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The keyboard's emoji page — `EmojiSelection` and `EmojiPager` (`keyboard.js:575-1045`).
//!
//! The emoji come from the table the picker uses ([`crate::emoji`]), in Unicode's order, cut into
//! gnome-shell's nine sections where its `first` names fall — which is not always where Unicode's
//! groups begin: the smileys run on into the hands, up to "selfie", and the objects start at
//! "muted speaker" (`_populateSections`). A section is cut into pages of the grid's size, and the
//! pages of every section form one ring a swipe goes round (`_initPagingInfo`, `_prevPage`,
//! `_nextPage`).
//!
//! Divergences: a key's long-press variants are its skin tones, where gnome-shell takes every
//! following emoji whose name starts with its own (so "kiss" also offers the couples); and a key
//! commits the table's fully-qualified spelling, without gnome-shell's extra U+FE0F.

use std::ops::Range;
use std::sync::OnceLock;

use super::layout::{Action, Face, Key, Level, Mode, Pager};

/// `this._sections`: the emoji each section starts at, and the label of its key.
const SECTIONS: [(&str, &str); 9] = [
    ("grinning face", "🙂️"),
    ("selfie", "👍️"),
    ("monkey face", "🌷️"),
    ("grapes", "🍴️"),
    ("globe showing Europe-Africa", "✈️"),
    ("jack-o-lantern", "🏃️"),
    ("muted speaker", "🔔️"),
    ("ATM sign", "❤️"),
    ("chequered flag", "🚩️"),
];

/// The bottom row: ABC at one and a half keys, the sections, and hide.
const BOTTOM_UNITS: f64 = 1.5 + SECTIONS.len() as f64 + 1.;

/// One emoji key: what it commits, and what its long press offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiKey {
    pub ch: &'static str,
    /// The key's own spelling first, then its tones — `variants`, which always holds the emoji
    /// itself.
    pub variants: Vec<String>,
}

#[derive(Debug)]
pub struct Section {
    pub label: &'static str,
    pub keys: Vec<EmojiKey>,
}

/// The sections, built from the table once.
pub fn sections() -> &'static [Section] {
    static SECTIONS_CELL: OnceLock<Vec<Section>> = OnceLock::new();
    SECTIONS_CELL.get_or_init(|| {
        let mut sections: Vec<Section> = Vec::new();
        for emoji in crate::emoji::table().entries() {
            if let Some(&(_, label)) = SECTIONS.iter().find(|(first, _)| *first == emoji.name) {
                sections.push(Section {
                    label,
                    keys: Vec::new(),
                });
            }
            // Anything before the first section's first emoji has nowhere to go.
            let Some(section) = sections.last_mut() else {
                continue;
            };
            section.keys.push(EmojiKey {
                ch: emoji.ch,
                variants: std::iter::once(emoji.ch)
                    .chain(emoji.tones.iter().copied())
                    .map(str::to_owned)
                    .collect(),
            });
        }
        sections
    })
}

/// One page of the ring: a run of one section's keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub section: usize,
    /// Which of its section's pages it is, and how many the section has.
    pub page: usize,
    pub n_pages: usize,
    pub keys: Range<usize>,
}

/// `_initPagingInfo`: every section cut into pages of `per_page`.
pub fn pages(per_page: usize) -> Vec<Page> {
    let per_page = per_page.max(1);
    let mut pages = Vec::new();
    for (index, section) in sections().iter().enumerate() {
        let n_pages = section.keys.len().div_ceil(per_page);
        for page in 0..n_pages {
            let start = page * per_page;
            pages.push(Page {
                section: index,
                page,
                n_pages,
                keys: start..(start + per_page).min(section.keys.len()),
            });
        }
    }
    pages
}

/// The page shown, as a level the keyboard's key handling runs on: the page's emoji in a
/// `cols` × `rows` grid, then the bottom row (`_generatePanel`, `_createBottomRow`).
///
/// `base` is the letter level whose ratio the page takes (`setRatio(...getRatio())`).
pub fn page_level(base: &Level, cols: u32, rows: u32, page: &Page) -> Level {
    let section = &sections()[page.section];
    let mut keys: Vec<Key> = section.keys[page.keys.clone()]
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let i = i as u32;
            Key {
                commit: Some(key.ch.to_owned()),
                extended: key.variants.clone(),
                face: Face::Emoji(key.ch.to_owned()),
                keyval: None,
                action: None,
                default_key: false,
                col: f64::from(i % cols.max(1)),
                row: i / cols.max(1),
                width: 1.,
                height: 1.,
                flat: true,
            }
        })
        .collect();

    let bottom = |col: f64, width: f64, face: Face, action: Action, default_key: bool| Key {
        commit: None,
        extended: Vec::new(),
        face,
        keyval: None,
        action: Some(action),
        default_key,
        col,
        row: rows,
        width,
        height: 1.,
        flat: false,
    };
    keys.push(bottom(
        0.,
        1.5,
        Face::Label("ABC".to_owned()),
        Action::Emoji,
        true,
    ));
    for (i, (_, label)) in SECTIONS.iter().enumerate() {
        keys.push(bottom(
            1.5 + i as f64,
            1.,
            Face::Emoji((*label).to_owned()),
            Action::EmojiSection(i),
            false,
        ));
    }
    keys.push(bottom(
        BOTTOM_UNITS - 1.,
        1.,
        Face::Icon("osk-hide-symbolic".to_owned()),
        Action::Hide,
        true,
    ));

    Level {
        name: "emoji".to_owned(),
        mode: Mode::Default,
        keys,
        cols: base.cols,
        rows: base.rows,
        pager: Some(Pager {
            cols,
            rows,
            indicators: page.n_pages > 1,
            bottom_units: BOTTOM_UNITS,
        }),
    }
}

/// The section key on the bottom row of [`page_level`], by section.
pub fn section_key(level: &Level, section: usize) -> Option<usize> {
    level
        .keys
        .iter()
        .position(|k| k.action == Some(Action::EmojiSection(section)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_start_where_gnome_cuts_them() {
        let sections = sections();
        assert_eq!(sections.len(), 9);
        assert_eq!(sections[0].keys[0].ch, "😀");
        // The hands are still smileys; "selfie" opens the people.
        assert!(sections[0].keys.iter().any(|k| k.ch == "👋"));
        assert_eq!(sections[1].keys[0].ch, "🤳");
        assert_eq!(sections[6].keys[0].ch, "🔇");
        assert_eq!(sections[8].keys[0].ch, "🏁");
        let wave = sections[0].keys.iter().find(|k| k.ch == "👋").unwrap();
        assert_eq!(wave.variants.len(), 6, "itself and the five tones");
    }

    #[test]
    fn pages_cut_each_section_and_ring() {
        let pages = pages(33);
        let smileys = sections()[0].keys.len();
        assert_eq!(pages[0].n_pages, smileys.div_ceil(33));
        assert_eq!(pages[0].keys, 0..33);
        let last_smiley = &pages[pages[0].n_pages - 1];
        assert_eq!(last_smiley.keys.end, smileys);
        assert_eq!(pages[pages[0].n_pages].section, 1);
        assert_eq!(pages[pages[0].n_pages].page, 0);
        assert_eq!(pages.last().unwrap().section, 8);
    }
}
