pub mod a11y_menu;
pub mod app_grid;
pub mod app_menu;
pub mod calendar;
pub mod config_error_notification;
pub mod dash;
pub mod end_session_dialog;
pub mod exit_confirm_dialog;
pub mod folder_dialog;
pub mod hotkey_overlay;
pub mod input_source_menu;
pub mod media_card;
pub mod mru;
pub mod notification_banner;
pub mod notification_card;
pub mod osd;
pub mod overview_layout;
pub mod overview_search;
pub mod panel;
pub mod popover;
pub mod quick_settings;
pub mod run_dialog;
pub mod screen_transition;
pub mod screenshot_ui;
pub mod theme_node;
pub mod widget;
pub mod window_preview;

/// GNOME authors font sizes in points; we size glyphs in logical pixels-per-em, so route every
/// GNOME point size through [`pt_to_px`] instead of hardcoding a px (which drifted per-UI: 11pt
/// was realized as 13px in the panel but 12–14px elsewhere). See
/// `docs/fork/gnome-style-reference.md` §1.3. This is the *unit* conversion only; if the live
/// output reads uniformly too large or small, the knob is [`base_font_pt`], not this.
///
/// Value: the **realized** GNOME conversion, `4/3` (nominal 96 DPI). GNOME's `fontsize` SCSS mixin
/// (`_drawing.scss:69`) also mentions "1pt = 1.091px", but that `1.091` is only the mixin's
/// *internal em-ratio* against a 16px reference — the em unit it multiplies is the stage default
/// (`stage` = `fontsize($base_font_size=11pt)`, ≈19.56px), and `mixin_em(P) × 19.56` works out to
/// `P × 4/3` for every size (15pt→20px, 11pt→14.67px, 9pt→12px). St itself renders the base font by
/// converting pt→px at 96 DPI, i.e. `4/3`. Using `1.091` as the absolute factor undersized every
/// string ~18% and desynced it from px chrome sized against the 4/3 base em (e.g. panel `2.2em`).
pub const PX_PER_PT: f64 = 4. / 3.;

/// The theme's **nominal** base, `$base_font_size` (`_common.scss:30`) — the denominator
/// every `fontsize()` em is written against, and the size the style reference quotes.
///
/// It is not necessarily the size anything renders at: see [`base_font_pt`].
pub const BASE_FONT_PT: f64 = 11.;

/// The **realized** base font size, in points: `org.gnome.desktop.interface font-name`.
///
/// This is the piece that makes the theme's point sizes relative rather than absolute.
/// `fontsize($size)` (`_drawing.scss:69-75`) works out to `font-size: ($size/11pt)em` —
/// the `1.091` and the `16px` inside it cancel to exactly that — so nothing in the theme
/// states a size in points at all. It states a *ratio* against the stage, and the stage
/// is `fontsize($base_font_size)` = `1em`, which St resolves against the theme context's
/// font: built from `StSettings:font-name` (`st-theme-context.c:240-243`), which is
/// `org.gnome.desktop.interface font-name` (`st-settings.c:32,509`) and is re-derived
/// when it changes (`on_font_name_changed`, `st-theme-context.c:339-344`).
///
/// So on a desktop set to "Cantarell 12" every GNOME string is 12/11 larger than the
/// theme's nominal points — which is what made all of our text read ~9% small next to
/// real GNOME. Anything unsized in the theme (a `.popup-menu-item`, an app name under
/// its icon) is `1em`, i.e. exactly this.
///
/// Structural `em` multiples ride the same base, which is why [`em`] reads it too.
pub fn base_font_pt() -> f64 {
    f64::from_bits(BASE_FONT_PT_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Set the realized base — called from the `org.gnome.desktop.interface` watcher. A
/// global rather than plumbing: `pt_to_px` is called from inside every widget's bake,
/// none of which has a settings handle, and it is exactly the process-wide singleton
/// `StThemeContext` is.
pub fn set_base_font_pt(pt: f64) {
    if store_base_font_pt(&BASE_FONT_PT_BITS, pt) {
        STYLE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The store half of [`set_base_font_pt`], against an explicit cell: returns whether the
/// base actually changed, i.e. whether a generation bump is owed.
///
/// Split out so the tests can exercise it without touching the process-wide cell. They
/// cannot: `pt_to_px` is read from inside every widget's bake, so a test that moved the
/// real base would change what a *concurrently running* test measures — which it did,
/// as an occasional extra bake in `a_container_fade_reuses_one_pill_bake`.
fn store_base_font_pt(cell: &std::sync::atomic::AtomicU64, pt: f64) -> bool {
    if pt.to_bits() == cell.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    cell.store(pt.to_bits(), std::sync::atomic::Ordering::Relaxed);
    true
}

/// Bumped whenever something changes what every widget would draw — today only the
/// base font size. [`crate::ui::widget::bake`] folds it into each cache key, so one
/// counter re-bakes the whole UI without every widget needing an invalidate hook.
pub fn style_generation() -> u64 {
    STYLE_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

static STYLE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static BASE_FONT_PT_BITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(BASE_FONT_PT.to_bits());

/// Convert a GNOME point size (the `%`-placeholder size in the style reference — e.g.
/// `%heading` is 11pt, `%title_3` is 15pt, `%caption` is 9pt) into our logical
/// pixels-per-em, scaled by the user's font size the way the theme's ems are.
pub fn pt_to_px(pt: f64) -> f64 {
    pt_to_px_at(base_font_pt(), pt)
}

/// [`pt_to_px`] against an explicit base — the arithmetic, with no global read.
fn pt_to_px_at(base_pt: f64, pt: f64) -> f64 {
    pt * PX_PER_PT * base_pt / BASE_FONT_PT
}

/// GNOME's `$base_font_size` (`_common.scss:30`) is **11pt**, and the theme sizes structural
/// elements as multiples of that base em (`stage { fontsize($base_font_size) }`): the panel is
/// `2.2em`, a calendar day cell `3em`, the banner `34em` wide, and so on. [`em`] is the single
/// anchor those multiples ride, so font size and chrome size can never drift apart again. Fixed
/// design tokens that GNOME expresses in px (`$base_padding` 6px, `$base_margin` 4px,
/// `$base_border_radius` 8px, 1px borders) stay literal px — they are NOT em-relative.
pub fn em(mult: f64) -> f64 {
    mult * pt_to_px(BASE_FONT_PT)
}

/// The logical height of one line of text at `pt` — the box a single-line label
/// occupies, which is what a container's height is measured from.
///
/// St gets this from Pango's font metrics; we approximate with the `pt * 1.3` factor
/// the calendar has used since it was ported (its `line_h`), so a band sized here and
/// one sized there cannot drift. It is deliberately a *typographic* line box, not the
/// glyph ink extents — [`widget::ShapedText`] carries those for centering within it.
pub fn line_height_px(pt: f64) -> f64 {
    pt_to_px(pt) * 1.3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Every point size in the UI is a *ratio* against the user's font size, not an
    /// absolute — GNOME's `fontsize()` emits em, so a desktop set to "Cantarell 12"
    /// renders the theme's nominal 11pt at 12pt. Getting this wrong is invisible on a
    /// default desktop and makes every string ~9% small on this one.
    ///
    /// Nothing here touches the process-wide base: every widget bake reads it, so a
    /// test that moved it would change what a *concurrently running* test measures.
    /// It did — as an occasional extra bake in `a_container_fade_reuses_one_pill_bake`.
    #[test]
    fn point_sizes_scale_with_the_user_font_size() {
        assert!(close(pt_to_px_at(BASE_FONT_PT, 11.), 11. * 4. / 3.));
        assert!(
            close(pt_to_px_at(12., 11.), 11. * 4. / 3. * 12. / 11.),
            "the theme's nominal 11pt must render at the user's 12pt"
        );
        // `em` is the same conversion at the base size, so structure follows text.
        assert!(close(em(1.), pt_to_px(BASE_FONT_PT)), "1em is the base");
        assert!(close(pt_to_px(11.), pt_to_px_at(base_font_pt(), 11.)));
    }

    /// A base-size change has to re-bake every widget, including the ones whose own
    /// cache key (scale, size, revision) does not move — a fixed-size panel bar with
    /// text inside it would otherwise keep serving the old size. An unchanged base must
    /// bump nothing, or every settings notification would re-bake the whole UI.
    #[test]
    fn only_a_real_base_change_owes_a_style_generation() {
        let cell = std::sync::atomic::AtomicU64::new(BASE_FONT_PT.to_bits());
        assert!(!store_base_font_pt(&cell, BASE_FONT_PT));
        assert!(store_base_font_pt(&cell, 12.));
        assert!(!store_base_font_pt(&cell, 12.));
        assert!(close(f64::from_bits(cell.load(Relaxed)), 12.));
    }

    use std::sync::atomic::Ordering::Relaxed;
}
