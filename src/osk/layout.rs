// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The on-screen keyboard's layouts: gnome-shell's own JSON models (`data/osk-layouts/`),
//! embedded, and the grid `Keyboard._updateLayout` builds from one (`keyboard.js:1388-1549`).
//!
//! A model is a list of levels (`default`, `shift`, `opt`, `opt+shift`), each a list of rows of
//! keys. A key carries the strings it commits — the first one on a tap, the rest behind a long
//! press — or a keyval, or an action, and its width in key units.

use serde::Deserialize;

use crate::dbus::ibus::purpose;

/// What the focused text field says it holds, as far as the keyboard cares
/// (`Clutter.InputContentPurpose`, mapped from the IBus purpose we track).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Purpose {
    #[default]
    Normal,
    Alpha,
    Digits,
    Number,
    Phone,
    Url,
    Email,
    Name,
    Password,
    Pin,
    Terminal,
}

impl Purpose {
    pub fn from_ibus(value: u32) -> Self {
        match value {
            purpose::ALPHA => Self::Alpha,
            purpose::DIGITS => Self::Digits,
            purpose::NUMBER => Self::Number,
            purpose::PHONE => Self::Phone,
            purpose::URL => Self::Url,
            purpose::EMAIL => Self::Email,
            purpose::NAME => Self::Name,
            purpose::PASSWORD => Self::Password,
            purpose::PIN => Self::Pin,
            purpose::TERMINAL => Self::Terminal,
            _ => Self::Normal,
        }
    }

    /// Whether the emoji key stays (`keyboard.js:1429-1432`); elsewhere its width goes to the key
    /// after it.
    fn emoji_visible(self) -> bool {
        matches!(
            self,
            Self::Normal | Self::Alpha | Self::Password | Self::Terminal
        )
    }
}

/// A key's action (`key.action`, `keyboard.js:1482-1505`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Hide,
    LanguageMenu,
    Emoji,
    /// A latching modifier — Ctrl or Alt on the extended layouts — by its keyval.
    Modifier,
    Delete,
    /// Switch to the named level.
    LevelSwitch(String),
    /// The emoji page's section key: its first page.
    EmojiSection(usize),
}

/// What a key shows: an icon wins over a label, a label over the string it commits
/// (`Key._makeKey`, `keyboard.js:378-386`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Face {
    Icon(String),
    Label(String),
    /// An emoji, drawn in colour.
    Emoji(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    /// The string a tap commits (`strings[0]`).
    pub commit: Option<String>,
    /// The rest of `strings`: the long-press popup's keys.
    pub extended: Vec<String>,
    pub face: Face,
    pub keyval: Option<u32>,
    pub action: Option<Action>,
    /// `.default-key`: the darker, non-character keys (`keyboard.js:1544-1545`).
    pub default_key: bool,
    /// Grid placement, in key units from the level's top-left: `col` and `width` are in key
    /// widths, `row` and `height` in rows.
    pub col: f64,
    pub row: u32,
    pub width: f64,
    pub height: f64,
    /// `.emoji-page .keyboard-key`: no cap, only the glyph.
    pub flat: bool,
}

impl Key {
    /// Whether this is a shift key, the ones whose icon turns into caps lock while the level is
    /// latched (`keyboard.js:1521-1531`).
    pub fn is_shift(&self) -> bool {
        matches!(self.action, Some(Action::LevelSwitch(_)))
            && self.face == Face::Icon("osk-shift-symbolic".into())
    }
}

/// How a level behaves once switched to (`currentLevel.mode`). Carried for completeness:
/// gnome-shell sets it on the level and never reads it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    Latched,
    Locked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub name: String,
    pub mode: Mode,
    pub keys: Vec<Key>,
    /// `KeyContainer.getRatio()`: the widest row in key units, and the row count.
    pub cols: f64,
    pub rows: u32,
    /// Laid out as the emoji page rather than one key grid.
    pub pager: Option<Pager>,
}

/// The emoji page's shape: a grid of `cols` by `rows` emoji over the page dots, the keys in rows
/// `0..rows`, then one bottom row of `bottom_units` key widths at the level's own ratio
/// (`EmojiSelection.setRatio`, `keyboard.js:1030-1043`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pager {
    pub cols: u32,
    pub rows: u32,
    /// Whether the page dots show: the section has more than one page.
    pub indicators: bool,
    pub bottom_units: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The model it was built from, for telling layouts apart.
    pub name: String,
    pub levels: Vec<Level>,
}

impl Layout {
    pub fn level(&self, name: &str) -> Option<&Level> {
        self.levels.iter().find(|l| l.name == name)
    }

    /// The layout for a group and purpose, with `_updateLayout`'s fallbacks (`keyboard.js:1397-
    /// 1427`): the purpose layouts first; otherwise the group, the group without its variant, then
    /// `us`, each `-extended` for a terminal. `None` only if not even `us` loads, which is what
    /// makes gnome-shell keep the layout it had.
    pub fn load(group: &str, purpose: Purpose) -> Option<Self> {
        let fixed = match purpose {
            Purpose::Digits => Some("digits"),
            Purpose::Number => Some("number"),
            Purpose::Phone => Some("phone"),
            Purpose::Email => Some("email"),
            Purpose::Url => Some("url"),
            _ => None,
        };
        let mut groups = match fixed {
            Some(name) => vec![name.to_owned()],
            None => {
                let mut groups = vec![group.to_owned()];
                if let Some((base, _)) = group.split_once('+') {
                    groups.push(base.to_owned());
                }
                groups.push("us".to_owned());
                groups
            }
        };
        if fixed.is_none() && purpose == Purpose::Terminal {
            for group in &mut groups {
                group.push_str("-extended");
            }
        }
        groups
            .iter()
            .find_map(|name| Self::build(name, purpose.emoji_visible()))
    }

    fn build(name: &str, emoji_visible: bool) -> Option<Self> {
        let model: Model = match serde_json::from_str(source(name)?) {
            Ok(model) => model,
            Err(err) => {
                // `ge.json` ships malformed; gnome-shell's `JSON.parse` throws on it too, and
                // falls back to the next group.
                debug!("OSK layout {name} does not parse: {err}");
                return None;
            }
        };
        let levels = model
            .levels
            .into_iter()
            .map(|level| build_level(level, emoji_visible))
            .collect();
        Some(Self {
            name: name.to_owned(),
            levels,
        })
    }
}

/// `_addRowKeys` and `KeyContainer.appendKey` over one level (`keyboard.js:105-114,1456-1549`).
fn build_level(level: ModelLevel, emoji_visible: bool) -> Level {
    let mut keys = Vec::new();
    let mut rows = 0;
    let mut cols: f64 = 0.;
    for row in level.rows {
        let mut col = 0.;
        let mut accumulated = 0.;
        for key in row {
            let action = key.action.as_deref().map(|action| match action {
                "hide" => Action::Hide,
                "languageMenu" => Action::LanguageMenu,
                "emoji" => Action::Emoji,
                "modifier" => Action::Modifier,
                "delete" => Action::Delete,
                _ => Action::LevelSwitch(key.level.clone().unwrap_or_default()),
            });
            let mut width = key.width.unwrap_or(1.);
            if action == Some(Action::Emoji) && !emoji_visible {
                // Its width passes on to the next key.
                accumulated = width;
                continue;
            }
            if accumulated > 0. {
                width += accumulated;
                accumulated = 0.;
            }

            let mut strings = key.strings.into_iter();
            let commit = strings.next();
            let extended: Vec<String> = strings.collect();
            let keyval = key
                .keyval
                .as_deref()
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
            let face = match (key.icon_name, key.label, &commit) {
                (Some(icon), _, _) => Face::Icon(icon),
                (None, Some(label), _) => Face::Label(label),
                (None, None, Some(commit)) => Face::Label(commit.clone()),
                (None, None, None) => Face::Label(String::new()),
            };
            let left_offset = key.left_offset.unwrap_or(0.);
            keys.push(Key {
                default_key: action.is_some() || keyval.is_some(),
                commit,
                extended,
                face,
                keyval,
                action,
                col: col + left_offset,
                row: rows,
                width,
                height: key.height.unwrap_or(1.),
                flat: false,
            });
            col += left_offset + width;
            cols = cols.max(col);
        }
        rows += 1;
    }
    Level {
        name: level.level,
        mode: match level.mode.as_deref() {
            Some("latched") => Mode::Latched,
            Some("locked") => Mode::Locked,
            _ => Mode::Default,
        },
        keys,
        cols,
        rows,
        pager: None,
    }
}

#[derive(Deserialize)]
struct Model {
    levels: Vec<ModelLevel>,
}

#[derive(Deserialize)]
struct ModelLevel {
    level: String,
    mode: Option<String>,
    rows: Vec<Vec<ModelKey>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelKey {
    #[serde(default)]
    strings: Vec<String>,
    label: Option<String>,
    icon_name: Option<String>,
    keyval: Option<String>,
    action: Option<String>,
    level: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    left_offset: Option<f64>,
}

/// Every layout gnome-shell ships (`data/osk-layouts/`, its `osk-layouts` gresource), by the
/// name `KeyboardModel` loads it under.
fn source(name: &str) -> Option<&'static str> {
    Some(match name {
        "am" => include_str!("../../resources/osk-layouts/am.json"),
        "ara" => include_str!("../../resources/osk-layouts/ara.json"),
        "at" => include_str!("../../resources/osk-layouts/at.json"),
        "at-extended" => include_str!("../../resources/osk-layouts/at-extended.json"),
        "be" => include_str!("../../resources/osk-layouts/be.json"),
        "bg" => include_str!("../../resources/osk-layouts/bg.json"),
        "by" => include_str!("../../resources/osk-layouts/by.json"),
        "ca" => include_str!("../../resources/osk-layouts/ca.json"),
        "ch" => include_str!("../../resources/osk-layouts/ch.json"),
        "ch+fr" => include_str!("../../resources/osk-layouts/ch+fr.json"),
        "cz" => include_str!("../../resources/osk-layouts/cz.json"),
        "de" => include_str!("../../resources/osk-layouts/de.json"),
        "de-extended" => include_str!("../../resources/osk-layouts/de-extended.json"),
        "digits" => include_str!("../../resources/osk-layouts/digits.json"),
        "dk" => include_str!("../../resources/osk-layouts/dk.json"),
        "ee" => include_str!("../../resources/osk-layouts/ee.json"),
        "email" => include_str!("../../resources/osk-layouts/email.json"),
        "epo" => include_str!("../../resources/osk-layouts/epo.json"),
        "es" => include_str!("../../resources/osk-layouts/es.json"),
        "es+cat" => include_str!("../../resources/osk-layouts/es+cat.json"),
        "fi" => include_str!("../../resources/osk-layouts/fi.json"),
        "fr" => include_str!("../../resources/osk-layouts/fr.json"),
        "ge" => include_str!("../../resources/osk-layouts/ge.json"),
        "gr" => include_str!("../../resources/osk-layouts/gr.json"),
        "hr" => include_str!("../../resources/osk-layouts/hr.json"),
        "hu" => include_str!("../../resources/osk-layouts/hu.json"),
        "id" => include_str!("../../resources/osk-layouts/id.json"),
        "il" => include_str!("../../resources/osk-layouts/il.json"),
        "in+bolnagri" => include_str!("../../resources/osk-layouts/in+bolnagri.json"),
        "in+mal" => include_str!("../../resources/osk-layouts/in+mal.json"),
        "ir" => include_str!("../../resources/osk-layouts/ir.json"),
        "is" => include_str!("../../resources/osk-layouts/is.json"),
        "it" => include_str!("../../resources/osk-layouts/it.json"),
        "ke" => include_str!("../../resources/osk-layouts/ke.json"),
        "kg" => include_str!("../../resources/osk-layouts/kg.json"),
        "kh" => include_str!("../../resources/osk-layouts/kh.json"),
        "kr" => include_str!("../../resources/osk-layouts/kr.json"),
        "la" => include_str!("../../resources/osk-layouts/la.json"),
        "latam" => include_str!("../../resources/osk-layouts/latam.json"),
        "lt" => include_str!("../../resources/osk-layouts/lt.json"),
        "lv" => include_str!("../../resources/osk-layouts/lv.json"),
        "mk" => include_str!("../../resources/osk-layouts/mk.json"),
        "mn" => include_str!("../../resources/osk-layouts/mn.json"),
        "my" => include_str!("../../resources/osk-layouts/my.json"),
        "nl" => include_str!("../../resources/osk-layouts/nl.json"),
        "no" => include_str!("../../resources/osk-layouts/no.json"),
        "number" => include_str!("../../resources/osk-layouts/number.json"),
        "ph" => include_str!("../../resources/osk-layouts/ph.json"),
        "phone" => include_str!("../../resources/osk-layouts/phone.json"),
        "pl" => include_str!("../../resources/osk-layouts/pl.json"),
        "pt" => include_str!("../../resources/osk-layouts/pt.json"),
        "ro" => include_str!("../../resources/osk-layouts/ro.json"),
        "rs" => include_str!("../../resources/osk-layouts/rs.json"),
        "ru" => include_str!("../../resources/osk-layouts/ru.json"),
        "se" => include_str!("../../resources/osk-layouts/se.json"),
        "si" => include_str!("../../resources/osk-layouts/si.json"),
        "sk" => include_str!("../../resources/osk-layouts/sk.json"),
        "th" => include_str!("../../resources/osk-layouts/th.json"),
        "tr" => include_str!("../../resources/osk-layouts/tr.json"),
        "ua" => include_str!("../../resources/osk-layouts/ua.json"),
        "uk" => include_str!("../../resources/osk-layouts/uk.json"),
        "url" => include_str!("../../resources/osk-layouts/url.json"),
        "us" => include_str!("../../resources/osk-layouts/us.json"),
        "us-extended" => include_str!("../../resources/osk-layouts/us-extended.json"),
        "vn" => include_str!("../../resources/osk-layouts/vn.json"),
        "za" => include_str!("../../resources/osk-layouts/za.json"),
        "za-extended" => include_str!("../../resources/osk-layouts/za-extended.json"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_layout_but_ge_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/osk-layouts");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_suffix(".json").unwrap().to_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names.len(), 67);
        for name in names {
            let built = Layout::build(&name, true);
            assert_eq!(built.is_some(), name != "ge", "{name}");
        }
    }

    #[test]
    fn us_grid_matches_append_key() {
        let layout = Layout::load("us", Purpose::Normal).unwrap();
        assert_eq!(layout.name, "us");
        let default = layout.level("default").unwrap();
        // q..p plus a 1.5-wide delete: 11.5 key units wide, four rows.
        assert_eq!((default.cols, default.rows), (11.5, 4));
        let q = &default.keys[0];
        assert_eq!(q.commit.as_deref(), Some("q"));
        assert_eq!((q.col, q.row, q.width), (0., 0, 1.));
        assert!(!q.default_key);
        let e = &default.keys[2];
        assert_eq!(e.extended, ["è", "é", "ê", "ë", "ē"]);
        let enter = default
            .keys
            .iter()
            .find(|k| k.keyval == Some(0xff0d))
            .unwrap();
        assert_eq!(enter.face, Face::Icon("osk-enter-symbolic".into()));
        assert!(enter.default_key);
        assert!(default.keys.iter().any(Key::is_shift));
    }

    #[test]
    fn hidden_emoji_key_gives_its_width_away() {
        let normal = Layout::load("us", Purpose::Normal).unwrap();
        let number = Layout::load("us", Purpose::Name).unwrap();
        let bottom = |l: &Layout| -> Vec<(Option<Action>, f64)> {
            let level = l.level("default").unwrap();
            level
                .keys
                .iter()
                .filter(|k| k.row == 3)
                .map(|k| (k.action.clone(), k.width))
                .collect()
        };
        assert!(bottom(&normal).contains(&(Some(Action::Emoji), 1.5)));
        let name = bottom(&number);
        assert!(!name.iter().any(|(a, _)| *a == Some(Action::Emoji)));
        // `?123` (1.5) keeps its width; the space bar after the emoji key takes its 1.5.
        assert_eq!(name[1], (None, 6.5));
    }

    #[test]
    fn group_fallbacks() {
        assert_eq!(
            Layout::load("de+nodeadkeys", Purpose::Normal).unwrap().name,
            "de"
        );
        assert_eq!(Layout::load("xx", Purpose::Normal).unwrap().name, "us");
        assert_eq!(Layout::load("ge", Purpose::Normal).unwrap().name, "us");
        assert_eq!(
            Layout::load("de", Purpose::Terminal).unwrap().name,
            "de-extended"
        );
        assert_eq!(
            Layout::load("fr", Purpose::Terminal).unwrap().name,
            "us-extended"
        );
        assert_eq!(Layout::load("fr", Purpose::Digits).unwrap().name, "digits");
        assert_eq!(Layout::load("fr", Purpose::Url).unwrap().name, "url");
    }
}
