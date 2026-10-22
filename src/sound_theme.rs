// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Finding an event sound in an XDG sound theme — libcanberra's `sound-theme-spec.c`, which is
//! what mutter's `MetaSoundPlayer` resolves `meta_sound_player_play_from_theme` through.
//!
//! The search order is canberra's, not the spec's prose, because the order is what decides which
//! file plays:
//!
//! - **Themes:** the named theme, then each theme it `Inherits` (recursively), then `freedesktop` —
//!   loaded even when the named theme exists (`load_theme_data`). A theme that is not installed
//!   falls back to `freedesktop` alone (`find_sound_for_theme`), and after all themes come the
//!   unthemed files directly under `sounds/`.
//! - **Within a theme:** each `Directories` entry whose `OutputProfile` is `stereo` or unset, then
//!   the theme's own top directory (`find_sound_in_theme`).
//! - **Within a directory:** every data dir (`$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`), then the
//!   name with its `-`-separated tail dropped one component at a time (`bell-window-system`,
//!   `bell-window`, `bell`), then the locale chain, then the suffixes `.disabled`, `.oga`, `.ogg`,
//!   `.wav` (`find_sound_in_subdir` .. `find_sound_in_locale`). So a theme's generic `bell` wins
//!   over an inherited theme's exact `bell-window-system`.
//!
//! A `.disabled` file is a result, not a miss: the search stops and nothing plays. That is how
//! GNOME Settings' "no alert sound" works.

use std::path::{Path, PathBuf};

/// The theme every lookup falls back to (`FALLBACK_THEME`).
pub const FALLBACK_THEME: &str = "freedesktop";

/// The output profile mutter never overrides (`DEFAULT_OUTPUT_PROFILE`).
const OUTPUT_PROFILE: &str = "stereo";

/// `N_THEME_DIR_MAX`: how many `index.theme` files one lookup reads, which is also what stops an
/// `Inherits` cycle.
const MAX_THEME_DIRS: usize = 8;

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(PathBuf),
    /// The theme ships `<name>.disabled`: this event is deliberately silent.
    Disabled,
    NotFound,
}

/// Look `name` up in `theme`, searching `data_dirs` (each the parent of a `sounds/` directory,
/// user directory first) for the `locale` chain.
pub fn lookup(data_dirs: &[PathBuf], theme: &str, name: &str, locale: &str) -> Lookup {
    if name.is_empty() {
        return Lookup::NotFound;
    }
    let search = Search {
        data_dirs,
        name,
        locales: locale_chain(locale),
    };
    let loaded = Theme::load(data_dirs, theme).or_else(|| {
        (theme != FALLBACK_THEME)
            .then(|| Theme::load(data_dirs, FALLBACK_THEME))
            .flatten()
    });
    if let Some(theme) = loaded {
        let found = search.in_theme(Some(&theme));
        if found != Lookup::NotFound {
            return found;
        }
    }
    search.in_theme(None)
}

/// The locale from the environment the way `setlocale(LC_MESSAGES, NULL)` resolves it.
pub fn messages_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".to_owned())
}

/// `find_sound_for_locale`: the locale, then without its `@modifier`, then without its
/// `_territory` (and everything after), then `C`, then no locale directory at all.
fn locale_chain(locale: &str) -> Vec<Option<String>> {
    let mut chain = vec![Some(locale.to_owned())];
    if let Some((base, _)) = locale.split_once('@') {
        chain.push(Some(base.to_owned()));
    }
    if let Some((base, _)) = locale.split_once('_') {
        chain.push(Some(base.to_owned()));
    }
    if locale != "C" {
        chain.push(Some("C".to_owned()));
    }
    chain.push(None);
    chain
}

/// One `Directories` entry of a loaded theme (`ca_data_dir`).
#[derive(Debug)]
struct DataDir {
    theme: String,
    dir: String,
    profile: Option<String>,
}

/// A theme with its inherited themes flattened into one ordered directory list
/// (`ca_theme_data`).
#[derive(Debug)]
struct Theme {
    name: String,
    dirs: Vec<DataDir>,
    n_theme_dirs: usize,
    loaded_fallback: bool,
}

impl Theme {
    /// `load_theme_data`: `None` when `name` is installed nowhere.
    fn load(data_dirs: &[PathBuf], name: &str) -> Option<Theme> {
        let mut theme = Theme {
            name: name.to_owned(),
            dirs: Vec::new(),
            n_theme_dirs: 0,
            loaded_fallback: false,
        };
        if !theme.load_dir(data_dirs, name) {
            return None;
        }
        if !theme.loaded_fallback {
            // The fallback theme may intentionally not exist.
            theme.load_dir(data_dirs, FALLBACK_THEME);
        }
        Some(theme)
    }

    /// `load_theme_dir`: read the first `index.theme` for `name` along the data dirs.
    fn load_dir(&mut self, data_dirs: &[PathBuf], name: &str) -> bool {
        if self.n_theme_dirs >= MAX_THEME_DIRS {
            return false;
        }
        if name == FALLBACK_THEME {
            self.loaded_fallback = true;
        }
        data_dirs.iter().any(|root| {
            let index = root.join("sounds").join(name).join("index.theme");
            match std::fs::read_to_string(index) {
                Ok(index) => {
                    self.parse_index(data_dirs, name, &index);
                    true
                }
                Err(_) => false,
            }
        })
    }

    /// `load_theme_path`'s parser: `[Sound Theme]`'s `Directories` and `Inherits`, and each
    /// directory section's `OutputProfile`. Inherited themes load after this one's directories.
    fn parse_index(&mut self, data_dirs: &[PathBuf], name: &str, index: &str) {
        let mut in_theme_section = false;
        let mut section_dir: Option<usize> = None;
        let mut inherits: Option<&str> = None;
        for line in index.lines() {
            let line = line.split('#').next().unwrap_or_default().trim_end();
            if line.is_empty() {
                continue;
            }
            if line == "[Sound Theme]" {
                in_theme_section = true;
                section_dir = None;
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_theme_section = false;
                section_dir = self
                    .dirs
                    .iter()
                    .position(|d| d.theme == name && d.dir == section);
                continue;
            }
            if in_theme_section {
                if let Some(value) = line.strip_prefix("Inherits=") {
                    inherits.get_or_insert(value);
                } else if let Some(value) = line.strip_prefix("Directories=") {
                    for dir in list(value) {
                        if !self.dirs.iter().any(|d| d.theme == name && d.dir == dir) {
                            self.dirs.push(DataDir {
                                theme: name.to_owned(),
                                dir: dir.to_owned(),
                                profile: None,
                            });
                        }
                    }
                }
            } else if let Some(i) = section_dir {
                if let Some(value) = line.strip_prefix("OutputProfile=") {
                    self.dirs[i].profile.get_or_insert_with(|| value.to_owned());
                }
            }
        }
        self.n_theme_dirs += 1;
        for parent in inherits.into_iter().flat_map(list) {
            self.load_dir(data_dirs, parent);
        }
    }
}

/// A `Directories=` / `Inherits=` value: separated by commas or spaces.
fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split([',', ' ']).filter(|s| !s.is_empty())
}

struct Search<'a> {
    data_dirs: &'a [PathBuf],
    name: &'a str,
    locales: Vec<Option<String>>,
}

impl Search<'_> {
    /// `find_sound_in_theme`, with `None` for the unthemed files.
    fn in_theme(&self, theme: Option<&Theme>) -> Lookup {
        if let Some(theme) = theme {
            for dir in &theme.dirs {
                if dir.profile.as_deref().is_some_and(|p| p != OUTPUT_PROFILE) {
                    continue;
                }
                let found = self.in_subdir(Some(&dir.theme), Some(&dir.dir));
                if found != Lookup::NotFound {
                    return found;
                }
            }
        }
        self.in_subdir(theme.map(|t| t.name.as_str()), None)
    }

    /// `find_sound_in_subdir` → `find_sound_for_name` → `find_sound_for_locale` →
    /// `find_sound_in_locale`.
    fn in_subdir(&self, theme: Option<&str>, subdir: Option<&str>) -> Lookup {
        for root in self.data_dirs {
            let mut dir = root.join("sounds");
            dir.extend(theme);
            dir.extend(subdir);
            for name in name_chain(self.name) {
                for locale in &self.locales {
                    let found = in_locale(&dir, locale.as_deref(), name);
                    if found != Lookup::NotFound {
                        return found;
                    }
                }
            }
        }
        Lookup::NotFound
    }
}

/// `bell-window-system`, `bell-window`, `bell`.
fn name_chain(name: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(name), |n| {
        n.rfind('-').filter(|&i| i > 0).map(|i| &n[..i])
    })
}

fn in_locale(dir: &Path, locale: Option<&str>, name: &str) -> Lookup {
    let mut dir = dir.to_path_buf();
    dir.extend(locale);
    if dir.join(format!("{name}.disabled")).exists() {
        return Lookup::Disabled;
    }
    [".oga", ".ogg", ".wav"]
        .into_iter()
        .map(|suffix| dir.join(format!("{name}{suffix}")))
        .find(|path| path.is_file())
        .map_or(Lookup::NotFound, Lookup::Found)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(test: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("synoik-sound-theme-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        /// `rel` under this data dir's `sounds/`.
        fn file(&self, root: &str, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(root).join("sounds").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        fn roots(&self, roots: &[&str]) -> Vec<PathBuf> {
            roots.iter().map(|r| self.0.join(r)).collect()
        }

        fn freedesktop(&self, root: &str) -> PathBuf {
            self.file(
                root,
                "freedesktop/index.theme",
                "[Sound Theme]\nName=Default\nDirectories=stereo\n\n[stereo]\nOutputProfile=stereo\n",
            );
            self.file(root, "freedesktop/stereo/bell.oga", "")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// GNOME Settings' alert-sound chooser writes a `__custom` theme in the user's data dir:
    /// `Directories=.`, `Inherits=freedesktop`, and `.ogg` files (symlinks to the chosen sound).
    /// Its exact name wins over freedesktop's generic `bell.oga`, reached through `.oga` missing.
    #[test]
    fn the_custom_alert_sound_theme_wins() {
        let tree = Tree::new("custom");
        tree.freedesktop("usr");
        tree.file(
            "home",
            "__custom/index.theme",
            "[Sound Theme]\nName=Custom\nInherits=freedesktop\nDirectories=.\n",
        );
        let chosen = tree.file("home", "__custom/bell-window-system.ogg", "");
        let roots = tree.roots(&["home", "usr"]);
        assert_eq!(
            lookup(&roots, "__custom", "bell-window-system", "C"),
            Lookup::Found(chosen)
        );
    }

    /// With no exact match anywhere, the name loses components from the right until freedesktop's
    /// `bell` matches; a theme that is not installed falls back to freedesktop.
    #[test]
    fn a_name_falls_back_to_its_generic_prefix() {
        let tree = Tree::new("prefix");
        let bell = tree.freedesktop("usr");
        let roots = tree.roots(&["home", "usr"]);
        assert_eq!(
            lookup(&roots, "freedesktop", "bell-window-system", "pt_BR.UTF-8"),
            Lookup::Found(bell.clone())
        );
        assert_eq!(
            lookup(&roots, "no-such-theme", "bell-window-system", "C"),
            Lookup::Found(bell)
        );
        assert_eq!(
            lookup(&roots, "freedesktop", "camera-shutter", "C"),
            Lookup::NotFound
        );
    }

    /// A `.disabled` file silences the event: the search stops there instead of falling through to
    /// the inherited theme's sound.
    #[test]
    fn a_disabled_sound_stops_the_search() {
        let tree = Tree::new("disabled");
        tree.freedesktop("usr");
        tree.file(
            "home",
            "quiet/index.theme",
            "[Sound Theme]\nInherits=freedesktop\nDirectories=stereo\n",
        );
        tree.file("home", "quiet/stereo/bell-window-system.disabled", "");
        let roots = tree.roots(&["home", "usr"]);
        assert_eq!(
            lookup(&roots, "quiet", "bell-window-system", "C"),
            Lookup::Disabled
        );
    }

    /// A theme's directories come before the themes it inherits, so its generic `bell` beats an
    /// inherited theme's exact `bell-window-system`; and a directory for another output profile is
    /// skipped.
    #[test]
    fn a_theme_directory_beats_an_inherited_exact_name() {
        let tree = Tree::new("order");
        tree.freedesktop("usr");
        tree.file("usr", "freedesktop/stereo/bell-window-system.oga", "");
        tree.file(
            "usr",
            "mine/index.theme",
            "[Sound Theme]\nInherits=freedesktop\nDirectories=surround,stereo\n\n\
             [surround]\nOutputProfile=5.1\n\n[stereo]\nOutputProfile=stereo\n",
        );
        tree.file("usr", "mine/surround/bell-window-system.oga", "");
        let mine = tree.file("usr", "mine/stereo/bell.wav", "");
        let roots = tree.roots(&["usr"]);
        assert_eq!(
            lookup(&roots, "mine", "bell-window-system", "C"),
            Lookup::Found(mine)
        );
    }

    /// A localized sound is found under the locale's language when the full locale has no
    /// directory.
    #[test]
    fn a_localized_sound_follows_the_locale_chain() {
        let tree = Tree::new("locale");
        tree.freedesktop("usr");
        let pt = tree.file("usr", "freedesktop/stereo/pt/bell.oga", "");
        let roots = tree.roots(&["usr"]);
        assert_eq!(
            lookup(&roots, "freedesktop", "bell", "pt_BR.UTF-8"),
            Lookup::Found(pt)
        );
        assert_eq!(
            locale_chain("de_DE@euro"),
            [
                Some("de_DE@euro"),
                Some("de_DE"),
                Some("de"),
                Some("C"),
                None
            ]
            .map(|l| l.map(str::to_owned))
        );
    }
}
