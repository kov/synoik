pub mod calendar;
pub mod config_error_notification;
pub mod dash;
pub mod end_session_dialog;
pub mod exit_confirm_dialog;
pub mod hotkey_overlay;
pub mod input_source_menu;
pub mod mru;
pub mod notification_banner;
pub mod notification_card;
pub mod overview_search;
pub mod panel;
pub mod popover;
pub mod quick_settings;
pub mod run_dialog;
pub mod screen_transition;
pub mod screenshot_ui;
pub mod widget;

/// GNOME authors font sizes in points; we size glyphs in logical pixels-per-em, so route every
/// GNOME point size through [`pt_to_px`] instead of hardcoding a px (which drifted per-UI: 11pt
/// was realized as 13px in the panel but 12–14px elsewhere). See
/// `docs/fork/gnome-style-reference.md` §1.3. One knob: if the live output reads uniformly too
/// large/small, adjust this and every UI tracks it.
///
/// Value: the **realized** GNOME conversion, `4/3` (nominal 96 DPI). GNOME's `fontsize` SCSS mixin
/// (`_drawing.scss:69`) also mentions "1pt = 1.091px", but that `1.091` is only the mixin's
/// *internal em-ratio* against a 16px reference — the em unit it multiplies is the stage default
/// (`stage` = `fontsize($base_font_size=11pt)`, ≈19.56px), and `mixin_em(P) × 19.56` works out to
/// `P × 4/3` for every size (15pt→20px, 11pt→14.67px, 9pt→12px). St itself renders the base font by
/// converting pt→px at 96 DPI, i.e. `4/3`. Using `1.091` as the absolute factor undersized every
/// string ~18% and desynced it from px chrome sized against the 4/3 base em (e.g. panel `2.2em`).
pub const PX_PER_PT: f64 = 4. / 3.;

/// Convert a GNOME point size (the `%`-placeholder size in the style reference — e.g.
/// `%heading` is 11pt, `%title_3` is 15pt, `%caption` is 9pt) into our logical
/// pixels-per-em.
pub const fn pt_to_px(pt: f64) -> f64 {
    pt * PX_PER_PT
}

/// GNOME's `$base_font_size` (`_common.scss:30`) is **11pt**, and the theme sizes structural
/// elements as multiples of that base em (`stage { fontsize($base_font_size) }`): the panel is
/// `2.2em`, a calendar day cell `3em`, the banner `34em` wide, and so on. [`em`] is the single
/// anchor those multiples ride, so font size and chrome size can never drift apart again. Fixed
/// design tokens that GNOME expresses in px (`$base_padding` 6px, `$base_margin` 4px,
/// `$base_border_radius` 8px, 1px borders) stay literal px — they are NOT em-relative.
pub const fn em(mult: f64) -> f64 {
    mult * pt_to_px(11.)
}
