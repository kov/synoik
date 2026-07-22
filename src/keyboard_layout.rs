//! Deriving the short panel label for the active keyboard layout — the fork's stand-in for
//! gnome-shell's `InputSourceIndicator` short name (`js/ui/status/keyboard.js:986`,
//! `sessionMode.js:99` `keyboard` role).
//!
//! GNOME derives language-based short names ("en", "pt") from libgnome-desktop's `GnomeXkbInfo`
//! (evdev.xml `<shortDescription>`), which this fork does not link. We show the xkb **layout code**
//! lowercased ("us", "br") instead — a documented, deliberate divergence. When the effective keymap
//! isn't code-derived (an `xkb.file` keymap, or a count mismatch), we fall back to abbreviating
//! each layout's full xkb name. Either way, colliding labels are disambiguated (GNOME dedups too).

use niri_config::Xkb;

/// GNOME's fallback layout when `org.gnome.desktop.input-sources` is empty
/// (`js/ui/status/keyboard.js` `KeyboardManager.DEFAULT_LAYOUT`).
const DEFAULT_LAYOUT: &str = "us";

/// Build the effective niri `Xkb` from GNOME's `org.gnome.desktop.input-sources`
/// (its way replaces niri's `input.keyboard.xkb` — see the CLAUDE.md tenet).
///
/// - `sources`: the `sources` `a(ss)` array, `(type, id)` in order. Only `"xkb"`-type sources
///   contribute layouts; `"ibus"` is unsupported here and skipped (a documented divergence — this
///   fork has no IBus). An xkb `id` is `"layout"` or `"layout+variant"`.
/// - `options`: `xkb-options` (joined with `,`).
/// - `model`: `xkb-model` (empty → left unset, letting libxkbcommon default it).
///
/// An empty/all-ibus set yields the `"us"` default, matching gnome-shell's
/// `_inputSourcesChanged` fallback. `layout`/`variant` stay index-aligned so the
/// active group index maps straight through to a source.
pub fn xkb_from_input_sources(
    sources: &[(String, String)],
    options: &[String],
    model: &str,
) -> Xkb {
    let mut layouts = Vec::new();
    let mut variants = Vec::new();
    for (ty, id) in sources {
        if ty != "xkb" {
            continue;
        }
        let (layout, variant) = id.split_once('+').unwrap_or((id.as_str(), ""));
        layouts.push(layout.to_owned());
        variants.push(variant.to_owned());
    }
    if layouts.is_empty() {
        layouts.push(DEFAULT_LAYOUT.to_owned());
        variants.push(String::new());
    }
    Xkb {
        rules: String::new(),
        model: model.to_owned(),
        layout: layouts.join(","),
        variant: variants.join(","),
        options: (!options.is_empty()).then(|| options.join(",")),
        file: None,
    }
}

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

    fn srcs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(t, i)| (t.to_string(), i.to_string()))
            .collect()
    }

    #[test]
    fn input_sources_build_comma_aligned_layout_and_variant() {
        let xkb = xkb_from_input_sources(
            &srcs(&[("xkb", "us"), ("xkb", "de+nodeadkeys"), ("xkb", "br")]),
            &v(&["grp:alt_shift_toggle", "caps:escape"]),
            "pc105",
        );
        assert_eq!(xkb.layout, "us,de,br");
        // Index-aligned: only `de` has a variant, so its slot is filled and the
        // others are empty — libxkbcommon keeps the active group index mapping.
        assert_eq!(xkb.variant, ",nodeadkeys,");
        assert_eq!(
            xkb.options.as_deref(),
            Some("grp:alt_shift_toggle,caps:escape")
        );
        assert_eq!(xkb.model, "pc105");
        assert_eq!(xkb.rules, "");
        assert_eq!(xkb.file, None);
    }

    #[test]
    fn input_sources_empty_falls_back_to_us() {
        let xkb = xkb_from_input_sources(&[], &[], "");
        assert_eq!(xkb.layout, "us");
        assert_eq!(xkb.variant, "");
        assert_eq!(xkb.options, None);
    }

    #[test]
    fn input_sources_skip_ibus_but_keep_xkb() {
        // IBus sources are unsupported here (divergence) — skipped, leaving the
        // xkb layouts. An all-ibus set falls back to "us".
        let xkb = xkb_from_input_sources(&srcs(&[("ibus", "libpinyin"), ("xkb", "us")]), &[], "");
        assert_eq!(xkb.layout, "us");
        assert_eq!(
            xkb_from_input_sources(&srcs(&[("ibus", "anthy")]), &[], "").layout,
            "us"
        );
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
