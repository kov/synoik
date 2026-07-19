//! Deriving the short panel label for the active keyboard layout — the fork's stand-in for
//! gnome-shell's `InputSourceIndicator` short name (`js/ui/status/keyboard.js:986`,
//! `sessionMode.js:99` `keyboard` role).
//!
//! GNOME derives language-based short names ("en", "pt") from libgnome-desktop's `GnomeXkbInfo`
//! (evdev.xml `<shortDescription>`), which this fork does not link. We show the xkb **layout code**
//! lowercased ("us", "br") instead — a documented, deliberate divergence. When the effective keymap
//! isn't code-derived (an `xkb.file` keymap, or a count mismatch), we fall back to abbreviating
//! each layout's full xkb name. Either way, colliding labels are disambiguated (GNOME dedups too).

/// The short label for the active layout in the panel input-source indicator, or `None` when fewer
/// than two layouts are configured (GNOME hides the indicator with `<2` sources).
///
/// - `codes`: the effective xkb layout codes (e.g. from `input.keyboard.xkb.layout "us,br"`), or
///   empty when unavailable (a `file` keymap, or nothing configured).
/// - `names`: every compiled layout's full xkb name (e.g. "English (US)"), in xkb order; its length
///   is the layout count. `idx` is the active layout.
///
/// Note (accepted limitation): a `layout` string that failed to compile is indistinguishable here
/// from one that produced the live keymap, so a broken config with a coincidentally equal layout
/// count can mislabel — a harmless wrong label, never a panic.
pub fn short_label(codes: &[String], names: &[String], idx: usize) -> Option<String> {
    let count = names.len();
    if count < 2 {
        return None;
    }
    // Prefer the configured codes when they line up 1:1 with the compiled layouts (libxkbcommon
    // keeps the comma order, so the index maps straight through). A `file` keymap yields empty
    // `codes`, and a stale/mismatched `layout` string fails the length check — both fall through to
    // abbreviating the full xkb names.
    let labels = if codes.len() == count {
        dedup(codes.iter().map(|c| c.trim().to_lowercase()).collect())
    } else {
        dedup(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| abbreviate(name, i))
                .collect(),
        )
    };
    labels.into_iter().nth(idx)
}

/// The first two alphabetic chars of a full xkb layout name, lowercased ("English (US)" → "en"),
/// or `l{n}` for a nameless layout.
fn abbreviate(name: &str, idx: usize) -> String {
    let abbr: String = name
        .chars()
        .filter(|c| c.is_alphabetic())
        .take(2)
        .collect::<String>()
        .to_lowercase();
    if abbr.is_empty() {
        format!("l{}", idx + 1)
    } else {
        abbr
    }
}

/// Disambiguate duplicate labels by appending the 1-based occurrence ("us","us" → "us1","us2");
/// unique labels are left bare. Both a `layout "us,us" variant ",intl"` (US + US-International) and
/// two same-language names ("English (US)" + "English (UK)" → "en","en") collide, so the indicator
/// would be useless — showing an unchanging label across a switch — without this.
fn dedup(labels: Vec<String>) -> Vec<String> {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let total = labels.iter().filter(|l| *l == label).count();
            if total > 1 {
                let occurrence = labels[..=i].iter().filter(|l| *l == label).count();
                format!("{label}{occurrence}")
            } else {
                label.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hidden_with_fewer_than_two_layouts() {
        assert_eq!(short_label(&v(&["us"]), &v(&["English (US)"]), 0), None);
        assert_eq!(short_label(&[], &[], 0), None);
    }

    #[test]
    fn uses_lowercased_codes_when_they_line_up() {
        let codes = v(&["us", "br"]);
        let names = v(&["English (US)", "Portuguese (Brazil)"]);
        assert_eq!(short_label(&codes, &names, 0).as_deref(), Some("us"));
        assert_eq!(short_label(&codes, &names, 1).as_deref(), Some("br"));
        // Config codes are lowercased even if the user typed them uppercase.
        assert_eq!(
            short_label(&v(&["US", "BR"]), &names, 0).as_deref(),
            Some("us")
        );
    }

    #[test]
    fn disambiguates_duplicate_codes() {
        // `layout "us,us" variant ",intl"` → two "us" codes; each gets its occurrence suffix.
        let names = v(&["English (US)", "English (US, intl.)"]);
        let codes = v(&["us", "us"]);
        assert_eq!(short_label(&codes, &names, 0).as_deref(), Some("us1"));
        assert_eq!(short_label(&codes, &names, 1).as_deref(), Some("us2"));
        // A third, distinct code stays bare while the duplicates are numbered.
        let codes = v(&["us", "br", "us"]);
        let names = v(&["English (US)", "Portuguese (Brazil)", "English (US, intl.)"]);
        assert_eq!(short_label(&codes, &names, 1).as_deref(), Some("br"));
        assert_eq!(short_label(&codes, &names, 2).as_deref(), Some("us2"));
    }

    #[test]
    fn falls_back_to_name_abbrev_on_count_mismatch() {
        // A `file` keymap (empty codes) or a stale `layout` string: abbreviate the full names.
        let names = v(&["English (US)", "Portuguese (Brazil)"]);
        assert_eq!(short_label(&[], &names, 0).as_deref(), Some("en"));
        assert_eq!(short_label(&v(&["us"]), &names, 1).as_deref(), Some("po"));
    }

    #[test]
    fn disambiguates_colliding_name_abbreviations() {
        // The Finding-1 regression: two same-abbreviation names must NOT produce an identical,
        // never-changing label. Both abbreviate to "en", so they get occurrence suffixes.
        let names = v(&["English (US)", "English (UK)"]);
        assert_eq!(short_label(&[], &names, 0).as_deref(), Some("en1"));
        assert_eq!(short_label(&[], &names, 1).as_deref(), Some("en2"));
    }

    #[test]
    fn fallback_handles_nameless_layouts() {
        assert_eq!(
            short_label(&[], &v(&["123", "456"]), 0).as_deref(),
            Some("l1")
        );
        assert_eq!(short_label(&[], &v(&["", ""]), 1).as_deref(), Some("l2"));
    }
}
