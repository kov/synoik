pub mod calendar;
pub mod config_error_notification;
pub mod end_session_dialog;
pub mod exit_confirm_dialog;
pub mod hotkey_overlay;
pub mod input_source_menu;
pub mod mru;
pub mod notification_banner;
pub mod notification_card;
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
/// Value: GNOME's own theme conversion. The `fontsize` SCSS mixin (`_drawing.scss:69`) realizes a
/// point size as `pt * 1.091` px (documented there as "the assumption: 1pt = 1.091px"), NOT the
/// nominal 96-DPI `4/3`. Using `4/3` rendered every string ~22% larger than gnome-shell 50.1.
pub const PX_PER_PT: f64 = 1.091;

/// Convert a GNOME point size (the `%`-placeholder size in the style reference — e.g.
/// `%heading` is 11pt, `%title_3` is 15pt, `%caption` is 9pt) into our logical
/// pixels-per-em.
pub const fn pt_to_px(pt: f64) -> f64 {
    pt * PX_PER_PT
}
