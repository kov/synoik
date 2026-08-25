// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Reusable widget-construction helpers shared by the popover/panel UIs.
//!
//! Every baking UI component (`input_source_menu`, `calendar`, `quick_settings`,
//! the dialogs, the panel bar, …) used to hand-roll the same offscreen-bake dance
//! — `create_buffer` → `bind` → `render` → `clear`/draw → `finish` →
//! `make_offscreen_sampleable` — behind its own subtly-different texture cache.
//! [`bake`] absorbs that dance once, keyed by `(scale, physical_size, revision)`
//! so a size change auto-invalidates (the calendar-height-freeze class of bug,
//! commit `128d112e`, cannot recur). See `docs/fork/widget-layer-design.md`.
//!
//! The `paint` closure draws through a [`Painter`] over the bound [`VulkanFrame`]:
//! logical/pt verbs ([`clear`](Painter::clear), [`fill_rounded`](Painter::fill_rounded),
//! [`text`](Painter::text)/[`text_clipped`](Painter::text_clipped)) fold the single
//! `× scale` conversion inside the toolkit (H2 in the design doc, the structural fix
//! for the HiDPI-glyph bug class); content-sized text blocks laid out in physical px
//! use the physical-coordinate verbs ([`paragraph`](Painter::paragraph),
//! [`paragraph_spans`](Painter::paragraph_spans), [`fill_rect_px`](Painter::fill_rect_px)).

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use anyhow::Context as _;
use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::image_source::ImageSource;
use crate::render_helpers::icon::{AppIconCache, IconCache, ImageCache, ImageFit};
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{
    premultiply, GlyphRun, VkTexture, VulkanFrame, VulkanRenderer, NATIVE_FOURCC,
};
use crate::ui::text_edit::TextEdit;
use crate::utils::to_physical_precise_round;

/// The upload cache for full-color app icons, keyed by
/// `(scale, descriptor, logical size)`. Lives on a baking widget (the dash, later
/// the grid/search) so a hover re-bake doesn't re-upload the icon textures.
pub type AppIconUploads = HashMap<(NotNan<f64>, AppIconRef, u16), TextureBuffer<VkTexture>>;

/// One [`AppIconUploads`] shared by every surface that draws app icons.
///
/// gnome-shell keeps **one** Cogl texture per gicon+size for the whole shell
/// (`st-texture-cache.c:998`, `POLICY_FOREVER`), so an app that appears in the dash, the
/// grid and a search result is uploaded once. Ours were per surface, which meant typing a
/// query re-uploaded, at the same size, icons the grid already had on the GPU.
///
/// Each surface still keeps its own renderer-context check, but it must clear this map only on a
/// real *change* — see [`sync_icon_upload_context`], which is the only correct way to do it.
pub type SharedAppIconUploads = std::rc::Rc<std::cell::RefCell<AppIconUploads>>;

/// Invalidate the shared icon uploads when *this* surface's renderer context has changed.
///
/// The subtlety is that the map is **shared** while the context field is **per surface**. A
/// surface that has never drawn holds `None`, and treating that as "changed" makes its first
/// draw clear textures the *other* icon surfaces already uploaded — so their next frame
/// re-uploads and hands out fresh element `Id`s for icons that did not change. A churned `Id`
/// throws away the output's backdrop blur, and this fired once per surface, on the frame after
/// each one first drew.
///
/// So: only a `Some(old) != new` invalidates. `None` means this surface holds no textures of its
/// own yet, so it has nothing to invalidate; whichever surface *did* draw under the old context
/// still clears the map when it next runs.
pub fn sync_icon_upload_context(
    seen: &mut Option<ContextId<VkTexture>>,
    uploads: &SharedAppIconUploads,
    context: ContextId<VkTexture>,
) {
    if seen.as_ref().is_some_and(|seen| *seen != context) {
        uploads.borrow_mut().clear();
    }
    *seen = Some(context);
}

/// Drop the uploads of one icon at one logical size, across every output scale.
///
/// The upload key carries no notion of *which* decode produced the pixels, so a
/// texture uploaded from a stale buffer would otherwise be served forever once the
/// fresh decode landed. Surfaces call this as each decode arrives, which is also why
/// nothing needs to clear the whole map on a theme or catalog change.
pub fn drop_app_icon_upload(uploads: &mut AppIconUploads, icon: &AppIconRef, logical_px: u16) {
    uploads.retain(|(_, cached, px), _| cached != icon || *px != logical_px);
}

/// Straight-alpha RGBA, the color type every draw verb takes (glyph coverage /
/// SDF alpha modulates it). Matches the `[f32; 4]` the frame primitives want.
pub type Rgba = [f32; 4];

/// Shared visual tokens, so the same color/icon-set is not re-declared per widget
/// (they drifted before: `HOVER_WASH` in 3 files, `TEXT`/`CHECK_ICONS` in more).
/// Only genuinely-shared, identically-valued tokens live here; widget-specific or
/// divergent values (a menu bg vs a tile bg, the separator alphas) stay local until
/// a port reconciles them against `docs/fork/gnome-style-reference.md`.
pub mod menu;
pub use menu::{Menu, MenuEntry, MenuHit, MenuItem, Ornament};

/// A keyboard navigation step, as GNOME's `StDirectionType` reaches a widget through
/// `StFocusManager` (`src/st/st-focus-manager.c:82-106`): the four arrows navigate within the
/// focus group, and Tab / Shift-Tab step the chain with wrap-around.
///
/// Every popover content maps these onto its own rows or cells — there is no actor tree here for
/// a stage-level focus manager to walk, so the focus state lives in each content, next to the
/// rects it already computes for hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
    /// Tab — a forward step through the focus chain.
    TabForward,
    /// Shift-Tab (`ISO_Left_Tab`) — a backward step.
    TabBackward,
}

/// Step a focus index over `count` linear rows by `delta`, wrapping at both ends and entering
/// from the top on a forward step / the bottom on a backward one — the wrap GNOME's menus do
/// (`popupMenu.js:171-177`, `navigate_focus(..., wrap_around = true)`).
///
/// For a list whose rows are all focusable; a list with unfocusable rows in it wants
/// [`Menu::focus_step`], which skips them.
pub fn step_rows(focused: Option<usize>, count: usize, delta: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let n = count as isize;
    Some(match focused {
        Some(cur) => (((cur as isize + delta) % n + n) % n) as usize,
        None if delta >= 0 => 0,
        None => count - 1,
    })
}

/// Move the focus one step over a set of rects, reproducing `st_widget_navigate_focus`
/// (`st-widget.c:2086-2169`) for a flat, untransformed focus chain — which is what every popover
/// content is.
///
/// `group` is the box the chain lives in, needed for the entering-from-nowhere case: with no
/// current focus, `StWidget` collapses the *group's* box onto the edge the direction comes from
/// and sorts against that (`st-widget.c:2127-2150`), so Down enters at the top and Right at the
/// left rather than at some arbitrary first child.
///
/// The arrows do **not** wrap: `StFocusManager` passes `wrap_around = FALSE` for them
/// (`st-focus-manager.c:82-96`), so Down at the bottom of a grid stays put. Tab does wrap. A menu
/// is the deliberate exception — its items re-navigate their own group with wrap on
/// (`popupMenu.js:171-177`) — which is why [`Menu::focus_step`] wraps and this does not.
pub fn step_rects(
    rects: &[Rectangle<f64, Logical>],
    group: Rectangle<f64, Logical>,
    from: Option<usize>,
    dir: Dir,
) -> Option<usize> {
    if rects.is_empty() {
        return None;
    }
    if let Dir::TabForward | Dir::TabBackward = dir {
        let delta = if dir == Dir::TabForward { 1 } else { -1 };
        return step_rows(from, rects.len(), delta);
    }

    // The box to measure from: the focused rect, or the group collapsed onto the edge this
    // direction enters from.
    let src = match from {
        Some(i) => *rects.get(i)?,
        None => {
            let (loc, size) = (group.loc, group.size);
            let (loc, size) = match dir {
                Dir::Down => (loc, Size::from((size.w, 0.))),
                Dir::Up => (
                    Point::from((loc.x, loc.y + size.h)),
                    Size::from((size.w, 0.)),
                ),
                Dir::Right => (loc, Size::from((0., size.h))),
                Dir::Left => (
                    Point::from((loc.x + size.w, loc.y)),
                    Size::from((0., size.h)),
                ),
                Dir::TabForward | Dir::TabBackward => unreachable!("handled above"),
            };
            Rectangle::new(loc, size)
        }
    };

    // "an actor is down (etc.) from another actor even if it overlaps it by up to 0.1 pixels"
    // (`filter_by_position`, `st-widget.c:1950-1975`).
    const SLACK: f64 = 0.1;
    let past = |r: &Rectangle<f64, Logical>| match dir {
        Dir::Up => r.loc.y + r.size.h <= src.loc.y + SLACK,
        Dir::Down => r.loc.y >= src.loc.y + src.size.h - SLACK,
        Dir::Left => r.loc.x + r.size.w <= src.loc.x + SLACK,
        Dir::Right => r.loc.x >= src.loc.x + src.size.w - SLACK,
        Dir::TabForward | Dir::TabBackward => unreachable!("handled above"),
    };
    let mid = |r: &Rectangle<f64, Logical>| (r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.);
    let (sx, sy) = mid(&src);
    // Squared midpoint distance — `get_distance` (`st-widget.c:1999-2017`), "not the exact
    // distance, but good enough to sort by".
    rects
        .iter()
        .enumerate()
        .filter(|&(i, r)| Some(i) != from && past(r))
        .min_by(|&(_, a), &(_, b)| {
            let d = |r: &Rectangle<f64, Logical>| {
                let (x, y) = mid(r);
                (x - sx) * (x - sx) + (y - sy) * (y - sy)
            };
            d(a).total_cmp(&d(b))
        })
        .map(|(i, _)| i)
}

impl Dir {
    /// The row delta this direction means to a **linear** list of rows: a menu treats Tab exactly
    /// as Down, which is what `StFocusManager` produces for a single-column focus chain.
    /// `None` for the horizontal directions, which a linear list does not consume.
    pub fn row_delta(self) -> Option<isize> {
        match self {
            Dir::Down | Dir::TabForward => Some(1),
            Dir::Up | Dir::TabBackward => Some(-1),
            Dir::Left | Dir::Right => None,
        }
    }
}

pub mod style {
    use super::Rgba;

    /// Fully transparent — the clear color for a rounded (transparent-corner) surface.
    pub const TRANSPARENT: Rgba = [0., 0., 0., 0.];
    /// Primary foreground (opaque white); glyph coverage modulates the alpha.
    pub const TEXT: Rgba = [1., 1., 1., 1.];

    /// An opaque straight-alpha colour from 8-bit sRGB components, for palette values quoted as
    /// hex in the GNOME sass.
    pub const fn rgb8(r: u8, g: u8, b: u8) -> Rgba {
        [r as f32 / 255., g as f32 / 255., b as f32 / 255., 1.]
    }
    /// Dimmed foreground (secondary labels, e.g. a row's short code).
    pub const MUTED: Rgba = [0.6, 0.6, 0.6, 1.];
    /// The hover highlight wash over a row/tile (a subtle lighten). NOTE: the
    /// lighten-vs-darken *direction* is per-widget (read from the SCSS cascade); this
    /// is the standard lighten used by menu rows / QS tiles / calendar days.
    pub const HOVER_WASH: Rgba = [1., 1., 1., 0.1];
    /// `$borders_color` = `transparentize($fg_color, 0.9)` (white @ 10%, dark theme) — the 1px
    /// rule color for the calendar column separator (`.message-list` border-right,
    /// `_message-list.scss:8`) and the QS group separators
    /// (`.popup-separator-menu-item-separator`, `_popovers.scss:117`). The single source of truth
    /// so the two can't drift; draw both via [`Painter::hairline`](super::Painter::hairline).
    pub const BORDERS: Rgba = [1., 1., 1., 0.1];

    /// Source-over composite of `fg` (possibly translucent) onto `bg` — the color a translucent
    /// overlay *would* produce over `bg`. Use it to lay a translucent [`BORDERS`] hairline onto a
    /// surface via the crisp (replacing) clear that
    /// [`Painter::hairline`](super::Painter::hairline) uses, so the line reads the same as it would
    /// blended (a raw translucent clear would instead punch a hole to whatever is behind).
    ///
    /// `bg` need not be opaque: the result carries the composited alpha, so a hairline laid on a
    /// *translucent* plate ([`OVERVIEW_PLATE`]) stays as see-through as the plate around it. Taking
    /// the alpha as 1 — which this did while every surface it served was opaque — draws the line as
    /// a solid bar across a plate the backdrop is supposed to show through.
    pub fn over(bg: Rgba, fg: Rgba) -> Rgba {
        let a = fg[3];
        let out_a = a + bg[3] * (1. - a);
        if out_a <= f32::EPSILON {
            return TRANSPARENT;
        }
        // Premultiplied source-over, un-premultiplied back out.
        let ch = |i: usize| (fg[i] * a + bg[i] * bg[3] * (1. - a)) / out_a;
        [ch(0), ch(1), ch(2), out_a]
    }
    /// Icon-name fallback chain for an "active/selected" check mark.
    pub const CHECK_ICONS: &[&str] = &["object-select-symbolic", "emblem-ok-symbolic"];

    /// SCSS `lighten($c, n%)` / `darken($c, n%)`: shift HSL **lightness** by `delta` (in 0..=1,
    /// signed), clamped, leaving hue and saturation alone. Alpha is untouched.
    ///
    /// Most of this module resolves such expressions at authoring time into a literal constant —
    /// which is only possible when the input is literal too. The *accent* is chosen at runtime, so
    /// every accent-derived hover/press state has to do the arithmetic here instead. Multiplying
    /// the channels is not the same operation: it darkens saturated colours toward black and
    /// cannot lighten a pure channel at all.
    pub fn shift_lightness(c: Rgba, delta: f32) -> Rgba {
        let (r, g, b) = (c[0], c[1], c[2]);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.;
        let target = (l + delta).clamp(0., 1.);

        // Scale toward white or black about the current lightness, which preserves hue and
        // saturation the way HSL's own round trip does for the achromatic-safe cases.
        let scale = |v: f32| {
            if target >= l {
                if l >= 1. {
                    return 1.;
                }
                v + (1. - v) * ((target - l) / (1. - l))
            } else {
                if l <= 0. {
                    return 0.;
                }
                v * (target / l)
            }
        };
        [scale(r), scale(g), scale(b), c[3]]
    }

    /// SCSS `lighten($c, n%)`, `amount` in 0..=1.
    pub fn lighten(c: Rgba, amount: f32) -> Rgba {
        shift_lightness(c, amount)
    }

    /// `$accent_borders_color` — the accent lightened 30% on a dark theme
    /// (`_colors.scss:70`). It is the focus-ring color for a button whose fill is *already* the
    /// accent (`@include focus_ring($fc: $accent_borders_color)` on the `default` style,
    /// `_drawing.scss:313-316`): the plain accent ring would vanish into an accent-filled button.
    pub fn accent_borders(accent: Rgba) -> Rgba {
        lighten(accent, 0.30)
    }

    /// SCSS `darken($c, n%)`, `amount` in 0..=1.
    pub fn darken(c: Rgba, amount: f32) -> Rgba {
        shift_lightness(c, -amount)
    }

    /// `%osd_panel` background — `$osd_bg_color` = `lighten(#222226, 5%)` ≈ `#2e2e33`
    /// (`_colors.scss:17`). Shared: the OSD pill and the Alt-Tab `.switcher-list` both extend
    /// `%osd_panel` (`_common.scss:294`, `_switcher-popup.scss:11`), so they are one value.
    pub const OSD_BG: Rgba = [0.180, 0.180, 0.200, 1.];
    /// `%osd_panel` hairline — `$osd_outer_borders_color` =
    /// `transparentize($osd_fg_color, 0.98)` (`_colors.scss:44`).
    pub const OSD_BORDER: Rgba = [1., 1., 1., 0.02];
    /// `$osd_fg_color` = `$light_1` (`_colors.scss:16`) — the foreground on an OSD panel.
    pub const OSD_FG: Rgba = TEXT;

    /// `%osd_button_flat` state fills (`_common.scss:324-332`).
    ///
    /// The *flat* style is not transparent: `button()` overrides the mix with the background input
    /// itself (`_drawing.scss:176`), so a flat OSD button's base **is** [`OSD_BG`] — it reads as
    /// flat because it sits on a panel of the same colour. The states lighten it, and
    /// `$always_dark: true` means they lighten in both themes (`:204-213`). Flat raises the hover
    /// step to 7% (`:188-190`); active is 9% and checked 8% (`:182-184`).
    ///
    /// Derived by taking `OSD_BG` to HSL (H 240°, S 0.053, L 0.190) and adding the step to L.
    pub const OSD_FLAT_HOVER: Rgba = [0.246, 0.246, 0.274, 1.];
    /// See [`OSD_FLAT_HOVER`] — checked, +8% lightness.
    pub const OSD_FLAT_CHECKED: Rgba = [0.256, 0.256, 0.284, 1.];
    /// See [`OSD_FLAT_HOVER`] — active/pressed, +9% lightness.
    pub const OSD_FLAT_ACTIVE: Rgba = [0.265, 0.265, 0.295, 1.];

    /// Modal-dialog card background — GNOME `$bg_color` `#36363a` (`_dialogs.scss:4`,
    /// `_colors.scss:12`). Flat, borderless; corners rounded to `$alert_radius` (18px).
    pub const DIALOG_BG: Rgba = [0.212, 0.212, 0.227, 1.];
    /// `.popup-menu-content` background — GNOME `$bg_color` `#36363a` (`_popovers.scss:31`,
    /// `_colors.scss:12`). The single home for the panel-popover box fill (QS / date /
    /// input-source), drawn once by the shared popover chrome so the three surfaces can't drift
    /// (they each hand-rolled a *different, too-dark* value before). Same value as [`DIALOG_BG`]
    /// today — both are `$bg_color` — but cited to the menu surface so they may diverge.
    pub const MENU_BG: Rgba = [0.212, 0.212, 0.227, 1.];
    /// `%card` / `.message` base surface — GNOME `$card_bg_color` = `lighten($bg_color, 7%)` ≈
    /// `#47474c` (`_colors.scss:29`). The "one step lighter than the menu" fill used by the
    /// date popover's today card and the QS detail card.
    pub const CARD_BG: Rgba = [0.278, 0.278, 0.298, 1.];
    /// `.button` normal fill — the subtle raised gray `mix($fg, $bg, 9%)` over the
    /// dialog card (`_drawing.scss:171`, `$background_mix_factor` 9%).
    pub const BUTTON_BG: Rgba = [0.283, 0.283, 0.297, 1.];
    /// `.modal-dialog-button` fill — translucent white `rgba(255,255,255,0.1)`
    /// (`%dialog_button`, `_drawing.scss:218`). Neutral dialog action button.
    pub const DIALOG_BUTTON_BG: Rgba = [1., 1., 1., 0.1];
    /// `.destructive-action` fill — `$red_4 #c01c28` (`_default-colors.scss:11`).
    pub const DESTRUCTIVE_BG: Rgba = [0.753, 0.110, 0.157, 1.];
    /// `%system_entry` normal background — `mix($system_fg_color, $system_bg_color, 9%)`
    /// (`_drawing.scss` `entry()` mixin, `$background_mix_factor` 9%), with
    /// `$system_fg_color`=#fafafb and `$system_bg_color`=lighten(#222226, 5%). The
    /// overview `search-entry` pill fill; always-dark. (`_colors.scss:20-21,47`.)
    pub const ENTRY_BG: Rgba = [0.252, 0.252, 0.267, 1.];
    /// `$system_overlay_bg_color` = `mix($system_base_color, $system_fg_color, 90%)`
    /// ≈ `#38383b` (`_colors.scss:50`) — the non-transparent overlay surface used by
    /// the dash and the `.search-section-content` results card. Same value the dash
    /// bakes as its pill (kept in sync via this one constant).
    pub const OVERLAY_BG: Rgba = [0.218, 0.218, 0.233, 1.];
    /// The overview chrome's **plate** fill — every opaque surface the overview lays over its
    /// backdrop: the dash pill, the search entry, the search-results card and the app-grid's folder
    /// tiles. One constant so the four cannot drift, which is the whole point: they read as one
    /// material only if they are one colour.
    ///
    /// **Divergence (chosen 2026-08-04).** GNOME gives each of these an opaque dark fill of its own
    /// (`$system_overlay_bg_color`, `%system_entry`, `.app-folder`) because `#overviewGroup` behind
    /// them is a flat `$system_base_color` slab. Ours is a blurred wallpaper
    /// (`overview-port.md` §13), and opaque plates over it read as the chrome forgot the backdrop
    /// changed — the same reason the Blur my Shell extension ships this restyle alongside its blur
    /// (`overview/style-components`; `docs/fork/blur-my-shell-inventory.md` §1).
    ///
    /// The plate is per-[`Appearance`] as of 2026-08-05, which is why it is a function of one
    /// rather than a constant. This is the **dark** value: [`crate::ui::panel::BAR_BG`], the same
    /// black wash the panel lays over its own blurred capture.
    ///
    /// Deliberately **not** adopted from that stylesheet: its re-specification of every
    /// hover/focus/active state at `rgba(230,230,230,.08–.3)`. Ours are relative washes over
    /// whatever they sit on ([`HOVER_WASH`], and an accent-derived focus fill), so they already
    /// compose over a translucent plate — and keeping them keeps GNOME's accent in the focus state,
    /// which that stylesheet drops.
    pub const OVERVIEW_PLATE: Rgba = crate::ui::panel::BAR_BG;

    /// The plate in the **light** appearance — that extension's "light" variant,
    /// `rgba(200,200,200,.2)`, which is where ours started and where it belongs.
    pub const OVERVIEW_PLATE_LIGHT: Rgba = [0.784, 0.784, 0.784, 0.2];

    /// Which way `org.gnome.desktop.interface color-scheme` is pointing, for the shell surfaces
    /// that follow it.
    ///
    /// **A divergence, and a narrow one.** GNOME's own chrome is always dark: every
    /// `$system_*` colour in the 50.1 theme is a fixed dark value and nothing under
    /// `js/ui` reads `color-scheme` for anything but the wallpaper variant
    /// (`background.js` `_loadBackground`) and the quick-settings tile. Ours follows it for
    /// exactly one thing — [`Appearance::plate`], the fill the overview lays over its backdrop —
    /// because that plate is *already* a divergence (a translucent wash where GNOME has an
    /// opaque slab), and a translucent wash is the one kind of surface whose right value
    /// genuinely depends on whether the desktop is meant to read light or dark. Everything else,
    /// the panel included, stays always-dark as GNOME has it.
    ///
    /// `default` counts as light, matching how GNOME resolves the tri-state everywhere else:
    /// only `prefer-dark` is dark.
    ///
    /// `Default` is `Light`, matching `GnomeSettings::default()` (whose `dark_style` is false,
    /// because the schema default is `default`). Nothing should be *reading* the default — it
    /// exists for the geometry-only paths that carry an [`EntryStyle`](super::EntryStyle) around
    /// without ever asking it for a fill — but if one ever does, agreeing with the pristine
    /// settings is the failure mode that looks like nothing happened rather than like a bug.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Appearance {
        Dark,
        #[default]
        Light,
    }

    impl Appearance {
        /// From `GnomeSettings::quick_toggles.dark_style`, i.e. `color-scheme == "prefer-dark"`.
        pub fn from_dark_style(dark: bool) -> Self {
            if dark {
                Self::Dark
            } else {
                Self::Light
            }
        }

        /// The plate fill — the shared surface the overview lays over its backdrop: the dash
        /// pill, the search entry, the search-results card and the app grid's folder tiles. One
        /// function so the four cannot drift, which is the whole point: they read as one
        /// material only if they are one colour.
        pub fn plate(self) -> Rgba {
            match self {
                Self::Dark => OVERVIEW_PLATE,
                Self::Light => OVERVIEW_PLATE_LIGHT,
            }
        }

        /// One bit for a bake revision key.
        ///
        /// Every plate is drawn into a cached texture, and a cache keyed on content alone would
        /// keep serving the old appearance's fill until something *else* changed — the toggle
        /// would look like it did nothing, then apply itself later at random. Callers fold this
        /// into their revision; see [`Appearance::plate`] for who they are.
        pub fn rev(self) -> u64 {
            match self {
                Self::Dark => 0,
                Self::Light => 1,
            }
        }
    }

    /// The system accent as an [`Rgba`] — `org.gnome.desktop.interface accent-color` arrives
    /// resolved to 8-bit RGB, and every widget that draws with it needs the float form.
    pub fn accent_rgba(accent: [u8; 3]) -> Rgba {
        [
            f32::from(accent[0]) / 255.,
            f32::from(accent[1]) / 255.,
            f32::from(accent[2]) / 255.,
            1.,
        ]
    }

    /// An entry's selection wash: `selection-background-color:
    /// st-transparentize(-st-accent-color, 0.7)` (`%entry_common`, `_common.scss:178`) — the
    /// live accent at 30%. Its companion `selected-color` is `$fg_color`, i.e. exactly what
    /// unselected text already draws in, so selected glyphs need no second pass.
    pub fn selection_bg(accent: Rgba) -> Rgba {
        [accent[0], accent[1], accent[2], 0.3]
    }
    /// A `.button` / `.icon-button` sitting **on** [`OVERLAY_BG`] — `button(normal)`'s
    /// `st-mix($system_fg_color, $system_overlay_bg_color, 9%)` (`_drawing.scss:171`,
    /// `$background_mix_factor` 9%), i.e. 9% of #fafafb over the overlay surface ≈ `#49494d`.
    /// NOT the surface itself: `$c` is the background the button sits on, not its fill, and
    /// using it directly makes the button invisible against its own container.
    pub const OVERLAY_BUTTON_BG: Rgba = [0.287, 0.287, 0.301, 1.];

    /// `.app-folder` fill — the one tile in the grid that is **raised** rather than flat:
    /// `tile_button($bg:$system_base_color, $raised:true)` (`_app-grid.scss:41`) resolves to
    /// `button(normal)`'s `st-mix($system_fg_color, $system_base_color, 9%)` = `mix(#fafafb,
    /// #222226, 9%)` ≈ `#353539` (`_drawing.scss:353-354`, `$background_mix_factor`
    /// `_default-colors.scss:33`). An app tile in the same grid is `$style: flat` and forced
    /// transparent at rest, which is why only folders show a resting background.
    ///
    /// Being the grid's only resting plate is exactly why it is [`Appearance::plate`] here rather
    /// than that opaque value: it is the one thing in the app grid the backdrop would stop at.
    pub fn folder_bg(appearance: Appearance) -> Rgba {
        appearance.plate()
    }

    /// `%system_button`'s normal fill — `button(normal, $tc: $system_fg_color, $c:
    /// $system_bg_color)`, i.e. `st-mix(#fafafb, lighten(#222226, 5%), 9%)` (`_common.scss:348`,
    /// `_drawing.scss:171`, `_colors.scss:20-21,47`).
    ///
    /// Numerically the same as [`ENTRY_BG`] — the `entry()` and `button()` mixins compute the
    /// resting fill the same way from the same pair — but a separate constant because they are
    /// separate rules: an entry and a button on a system surface are free to diverge, and one of
    /// them changing must not silently move the other.
    pub const SYSTEM_BUTTON_BG: Rgba = [0.252, 0.252, 0.267, 1.];

    /// Modal-dialog corner radius, logical px — GNOME `$alert_radius` (`_common.scss:43`,
    /// applied at `_dialogs.scss:6`). Note this is 18px, not `$modal_radius` (16px).
    pub const DIALOG_RADIUS: f64 = 18.;
}

/// Composite a symbolic icon — the first of `candidates` that resolves — centered
/// at `center` (relative to the element `origin`), sized `logical_px`, tinted
/// `color`. The single home for the icon-compositing helper that was copy-pasted
/// across the popover/panel UIs (`icon_element` ×2 + 3 inline sequences). Returns
/// `None` (logging on a GPU upload error) if no candidate resolves or the upload
/// fails, so callers can `if let Some(el) = …` and simply skip a missing glyph.
#[allow(clippy::too_many_arguments)]
pub fn icon_element<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Option<TextureRenderElement<VkTexture>> {
    icon_element_alpha(
        renderer, icons, candidates, logical_px, scale, color, origin, center, 1.,
    )
}

/// [`icon_element`] with an explicit element `alpha` — for a surface that fades as a
/// whole (the OSD), where the icon rides on top of the fading bake rather than
/// inside it and so must fade with it.
#[allow(clippy::too_many_arguments)]
pub fn icon_element_alpha<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let tb = candidates
        .iter()
        .find_map(|name| icons.texture(renderer, name.as_ref(), logical_px, scale, color))?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        alpha,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// [`icon_element_alpha`], but drawn at `page_scale` **without changing the size asked of the
/// cache**.
///
/// For an icon on a surface that animates its own scale. Asking for a different `logical_px` each
/// frame is a different cache key each frame, and a cold key draws *nothing* — so an icon whose
/// size rides an animation blinks its way through the first run of that animation, once per
/// distinct size. Bucketing the size bounds how many keys there are but does not stop the cold
/// ones being empty; it just turns one cold miss into a dozen.
///
/// So the size is fixed and the scaling happens on the GPU, via the buffer scale — the same trick
/// the bakes already use, and what `window_preview` does for the overview's app icon.
#[allow(clippy::too_many_arguments)]
pub fn icon_element_scaled<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    page_scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let tb = candidates
        .iter()
        .find_map(|name| icons.texture(renderer, name.as_ref(), logical_px, scale, color))?;
    // Dividing the buffer scale is what magnifies: a buffer tagged at half the output scale
    // covers twice the logical area.
    let tb = TextureBuffer::from_texture(
        renderer,
        tb.texture().clone(),
        scale / page_scale.max(f64::EPSILON),
        Transform::Normal,
        Vec::new(),
    );
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        alpha,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// Composite a **full-color application icon** ([`AppIconRef`]), resolved +
/// decoded by the [`AppIconCache`], centered at `center` (relative to the element
/// `origin`), sized `logical_px`. The full-color sibling of [`icon_element`]: no
/// tint (the icon keeps its own colors), and uploads are cached in the caller's
/// [`AppIconUploads`] map (keyed by scale + descriptor + size) so a hover re-bake
/// reuses them. `alpha` multiplies the element (the overview fade). `None` if even
/// the fallback icon fails to resolve/upload.
#[allow(clippy::too_many_arguments)]
pub fn app_icon_element(
    renderer: &mut VulkanRenderer,
    uploads: &mut AppIconUploads,
    icons: &AppIconCache,
    icon: &AppIconRef,
    logical_px: f64,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let scale_key = NotNan::new(scale).ok()?;
    let key = (scale_key, icon.clone(), (logical_px.round() as u16).max(1));
    #[allow(clippy::map_entry)]
    if !uploads.contains_key(&key) {
        let buffer = icons.buffer(icon, logical_px, scale)?;
        match TextureBuffer::from_memory_buffer(renderer, &buffer) {
            Ok(tb) => {
                uploads.insert(key.clone(), tb);
            }
            Err(err) => {
                tracing::error!("error uploading app icon: {err:#}");
                return None;
            }
        }
    }
    let tb = uploads.get(&key)?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb.clone(),
        loc,
        alpha,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// Uploaded textures for runtime-chosen images (album art, the account picture), keyed by output
/// scale, an owner-chosen slot id, and the source. See [`image_element`]; owners prune on the slot
/// id with [`retain_slots`](ImageUploads::retain_slots).
///
/// **The scale is in the key and must stay there.** The decode is per physical size, so one source
/// is a different texture per output scale — without it, whichever output drew first wins and every
/// other one reuses its texture at the wrong resolution, silently and permanently. Two outputs at
/// different scales is the obvious way in; the quieter one is a *scale change*, where the re-warmed
/// decode is never uploaded because the stale entry still hits. Matches [`AppIconUploads`], which
/// has been keyed this way all along.
///
/// It is a type rather than a `HashMap` alias so the **renderer-context check cannot be
/// forgotten**: [`image_element`] and [`Avatar::element`] run it themselves on every lookup, so a
/// texture from a dead context can never be served. As an alias it was one line each owner had to
/// remember, and the lock screen did not.
#[derive(Default)]
pub struct ImageUploads {
    map: HashMap<(NotNan<f64>, u64, ImageSource), TextureBuffer<VkTexture>>,
    context: Option<ContextId<VkTexture>>,
}

impl ImageUploads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop everything if the renderer context changed. Called for the owner by the element
    /// builders; owners that mint their own textures into this map must call it too.
    pub fn ensure_context(&mut self, renderer: &VulkanRenderer) {
        let context = renderer.context_id();
        if self.context.as_ref() != Some(&context) {
            self.map.clear();
            self.context = Some(context);
        }
    }

    /// Drop every upload whose slot id `keep` rejects, at every scale.
    pub fn retain_slots(&mut self, keep: impl Fn(u64) -> bool) {
        self.map.retain(|(_, slot, _), _| keep(*slot));
    }

    /// Drop every upload whose source `keep` rejects, at every scale and slot.
    pub fn retain_sources(&mut self, keep: impl Fn(&ImageSource) -> bool) {
        self.map.retain(|(_, _, source), _| keep(source));
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// Composite an image an app pointed us at (album art today), loaded by the [`ImageCache`],
/// centered at `center` (relative to the element `origin`), fitted into a `logical_px` square.
///
/// The full-color, app-content sibling of [`app_icon_element`]. Two differences, both because the
/// source is content some *app* chose rather than an installed asset:
///
/// - a source that will not load yields `None` rather than GNOME's executable glyph — the caller
///   draws its own fallback (a media card's `audio-x-generic-symbolic`);
/// - the upload slot is keyed by source as well as id, so an owner that reuses slots cannot serve
///   the previous image for a new one.
///
/// The load itself is aspect-fit and centered on a transparent square (`decode_image_bytes`), which
/// is what makes placing it like any other centered icon reproduce St's `RESIZE_ASPECT` gravity.
/// `None` while an async load is still in flight — the caller draws its fallback until it lands, so
/// the arrival has to invalidate whatever cached it (see `media_card`).
#[allow(clippy::too_many_arguments)]
pub fn image_element(
    renderer: &mut VulkanRenderer,
    uploads: &mut ImageUploads,
    images: &ImageCache,
    source: &ImageSource,
    slot: u64,
    logical_px: f64,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    uploads.ensure_context(renderer);
    let key = (NotNan::new(scale).ok()?, slot, source.clone());
    #[allow(clippy::map_entry)]
    if !uploads.map.contains_key(&key) {
        let buffer = images.buffer(source, ImageFit::Contain, logical_px, scale)?;
        match TextureBuffer::from_memory_buffer(renderer, &buffer) {
            Ok(tb) => {
                uploads.map.insert(key.clone(), tb);
            }
            Err(err) => {
                tracing::error!("error uploading image {source:?}: {err:#}");
                return None;
            }
        }
    }
    let tb = uploads.map.get(&key)?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb.clone(),
        loc,
        alpha,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// GNOME's `.user-icon` (`_misc.scss:8-27`) — the circular user picture.
///
/// It is a widget rather than a lock-screen detail because `.user-icon` is one style class with
/// several homes: the unlock dialog's 160px one, the login screen's user list, and the 64px
/// `AVATAR_ICON_SIZE` default (`userWidget.js:11`) the system menu will want. The *size* is a
/// per-home `icon-size` and so stays a parameter.
///
/// Three layers, and each caller owns its own placement of them:
///
/// 1. the plate — `background-color: transparentize($_gdm_fg, .87)` under the lock screen
///    (`_login-lock.scss:358-360`), which shows through only when there is no picture;
/// 2. the picture — [`Avatar::element`], `background-size: cover`, clipped to the circle;
/// 3. the ring — `box-shadow: inset 0 0 0 1px transparentize($fg_color, 0.9)`, and **only when
///    there is a picture**: the shadow is on `&.user-avatar` (`_misc.scss:20-21`), a class
///    `Avatar.update()` adds in the same branch that sets the `background-image`
///    (`userWidget.js:81-85`). Bake it with [`bake_card_border`] at a circular radius.
///
/// The fallback when there is no picture is not here: it is a plain themed
/// `avatar-default-symbolic` [`icon_element_scaled`], inset by the caller's `StIcon` padding.
#[derive(Debug, Clone, Copy)]
pub struct Avatar;

impl Avatar {
    /// The inset ring's colour — `$fg_color` at 10%, i.e. [`style::BORDERS`]. Note it is **not**
    /// re-tinted to `$_gdm_fg` under the lock screen: `_login-lock.scss:358` overrides the
    /// background and colour only, so the shadow keeps `_misc.scss`'s value. Its width is the 1px
    /// [`bake_card_border`] already strokes.
    pub const RING_COLOR: Rgba = style::BORDERS;

    /// The picture, clipped to a circle, centred at `center` (relative to `origin`) in a
    /// `logical_px` square. `None` when there is no picture, the load has not landed yet, or it
    /// failed — in every one of those the caller draws the themed fallback.
    ///
    /// `page_scale` is the surface's own animated scale, applied through the *buffer* scale exactly
    /// as [`icon_element_scaled`] does, so a page that scales during a crossfade never asks the
    /// cache for a new size (a cold key draws nothing — see that function).
    #[allow(clippy::too_many_arguments)]
    pub fn element(
        renderer: &mut VulkanRenderer,
        uploads: &mut ImageUploads,
        images: &ImageCache,
        source: &ImageSource,
        slot: u64,
        logical_px: f64,
        scale: f64,
        page_scale: f64,
        origin: Point<f64, Logical>,
        center: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<RoundedTextureRenderElement<VkTexture>> {
        uploads.ensure_context(renderer);
        let key = (NotNan::new(scale).ok()?, slot, source.clone());
        #[allow(clippy::map_entry)]
        if !uploads.map.contains_key(&key) {
            let buffer = images.buffer(source, ImageFit::Cover, logical_px, scale)?;
            match TextureBuffer::from_memory_buffer(renderer, &buffer) {
                Ok(tb) => {
                    uploads.map.insert(key.clone(), tb);
                }
                Err(err) => {
                    tracing::error!("error uploading the avatar {source:?}: {err:#}");
                    return None;
                }
            }
        }
        let tb = uploads.map.get(&key)?;
        // Re-tag at the page's scale: dividing the buffer scale magnifies.
        let tb = TextureBuffer::from_texture(
            renderer,
            tb.texture().clone(),
            scale / page_scale.max(f64::EPSILON),
            Transform::Normal,
            Vec::new(),
        );
        let logical = tb.logical_size();
        let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
        let inner = TextureRenderElement::from_texture_buffer(
            tb,
            loc,
            alpha,
            None,
            None,
            Kind::Unspecified,
        );
        // `border-radius: $forced_circular_radius` on a square box. The radius is in the element's
        // *logical* units, which already carry `page_scale` (the buffer scale above), so half the
        // logical width stays a circle at every point of the crossfade.
        Some(RoundedTextureRenderElement::new(
            inner,
            logical.w / 2.,
            smithay::utils::Scale::from(scale),
        ))
    }
}

/// GNOME's `wiggle()` (`misc/animationUtils.js:41-73`) — the shake that says "no".
///
/// **6 px, 65 ms a leg, 3 wiggles** at every call site so far (`authPrompt.js:489`,
/// `polkitAgent.js:268`), so those are the defaults rather than parameters.
///
/// The shape is three eases, not one: accelerate out to `-offset`, then a *linear* triangle wave
/// between the extremes, then decelerate back to rest. Clutter's `repeat_count` is a count of
/// **repeats**, so `wiggleCount: 3` runs the middle leg four times, and `autoReverse` flips each
/// run — which is what makes it a wave rather than a saw, and why it ends back at `-offset` for the
/// deceleration to unwind. Six legs of 65 ms: 390 ms in total.
///
/// Whatever it shakes must be its **own** render element. A translation baked into a texture
/// re-rasterises that texture every frame for the whole 390 ms; see the lock screen's message,
/// which was moved out of its column bake for exactly this.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wiggle {
    /// When the current shake started. `None` is at rest.
    since: Option<Duration>,
}

impl Wiggle {
    pub const OFFSET: f64 = 6.;
    pub const LEG: Duration = Duration::from_millis(65);
    pub const COUNT: u32 = 3;
    /// Legs in the middle phase — see the type docs on why it is `count + 1`.
    pub const LEGS: u32 = Self::COUNT + 1;
    /// The whole animation: one leg out, [`Self::LEGS`] across, one leg back.
    pub const TIME: Duration =
        Duration::from_millis(Self::LEG.as_millis() as u64 * (Self::LEGS as u64 + 2));

    pub fn start(&mut self, now: Duration) {
        self.since = Some(now);
    }

    /// Put whatever is shaking back where it belongs, immediately.
    pub fn settle(&mut self) {
        self.since = None;
    }

    /// Whether it still owes frames.
    pub fn is_animating(&self, now: Duration) -> bool {
        self.since
            .is_some_and(|since| now.saturating_sub(since) < Self::TIME)
    }

    /// How far the element is displaced, in logical pixels. 0 at rest.
    pub fn offset(&self, now: Duration) -> f64 {
        let Some(since) = self.since else {
            return 0.;
        };
        let leg = Self::LEG.as_secs_f64();
        let elapsed = now.saturating_sub(since).as_secs_f64();
        // Which leg we are on, and how far through it.
        let index = (elapsed / leg).floor();
        let t = (elapsed / leg - index).clamp(0., 1.);
        let index = index as u32;

        match index {
            // Accelerate out to `-offset`.
            0 => -Self::OFFSET * crate::animation::Curve::EaseOutQuad.y(t),
            // The wave. Even legs run `-offset` → `+offset`, odd ones back, which is what
            // `autoReverse` does to a repeating transition.
            i if i <= Self::LEGS => {
                let up = (i - 1) % 2 == 0;
                let t = if up { t } else { 1. - t };
                -Self::OFFSET + 2. * Self::OFFSET * t
            }
            // Decelerate from `-offset` back to rest. The wave ends on an odd (returning) leg, so
            // this always starts from the same side.
            i if i == Self::LEGS + 1 => {
                -Self::OFFSET * (1. - crate::animation::Curve::EaseInQuad.y(t))
            }
            _ => 0.,
        }
    }
}

/// GNOME's `%tooltip` (`_common.scss:225-238`) — the black pill behind a short
/// label. Shared by `.window-caption` (the overview preview title,
/// `_window-picker.scss:24-26`), `.dash-label` (`_dash.scss:103-106`) and the
/// screenshot UI's tip (`_screenshot.scss:200`), so it lands here rather than in
/// the first caller.
///
/// The widget owns its box model and its paint; the caller owns placement and the
/// bake cache (captions differ per instance, so one cache per *text* is the
/// natural key).
#[derive(Debug, Clone, Copy)]
pub struct Tooltip;

impl Tooltip {
    /// `padding: $base_padding $base_padding * 2` (`_common.scss:231`).
    pub const PAD_V: f64 = 6.;
    pub const PAD_H: f64 = 12.;
    /// `border: 1px solid transparentize($light_1, 0.9)` (`:227`).
    pub const BORDER: f64 = 1.;
    /// `background-color: transparentize(black, 0.1)` (`:226`).
    pub const BG: Rgba = [0., 0., 0., 0.9];
    /// The 1px border — white at 10%.
    pub const BORDER_COLOR: Rgba = style::BORDERS;
    /// The label inherits the stage's 11pt body size; `%tooltip` sets no font.
    pub const TEXT_PT: f64 = 11.;

    /// The pill's box for `text`, at the shared body size. Height is fixed by the
    /// box model (it is a single line), width is the label plus padding.
    pub fn size(text: &str) -> Size<f64, Logical> {
        let px = crate::ui::pt_to_px(Self::TEXT_PT);
        let w = synoik_vk::text::measure_line_width_weighted(text, px as f32, false);
        Size::from((w + 2. * Self::PAD_H, px.ceil() + 2. * Self::PAD_V))
    }

    /// `border-radius: $forced_circular_radius` — a pill, so half the height.
    pub fn radius(size: Size<f64, Logical>) -> f64 {
        size.h / 2.
    }
}

impl Painter<'_, '_, '_> {
    /// Paint a [`Tooltip`] filling the whole buffer, with `label` centred in it.
    /// `text-align: center` (`_common.scss:232`).
    pub fn tooltip(&mut self, size: Size<f64, Logical>, label: &ShapedText) -> anyhow::Result<()> {
        let radius = Tooltip::radius(size);
        self.clear(style::TRANSPARENT)?;
        self.fill_rounded_full(radius, Tooltip::BG)?;
        self.stroke_rounded_full(radius, Tooltip::BORDER, Tooltip::BORDER_COLOR)?;
        self.text(
            label,
            Point::from((size.w / 2., size.h / 2.)),
            Align::CENTER,
            style::TEXT,
        )
    }
}

/// GNOME's `.overview-icon` tile (`%tile`, `_common.scss:84-90`): a rounded square
/// holding an icon, with a hover state. The reusable geometry + state shared by the
/// dash (S3) and, later, the app grid and grid search results — GNOME shares it the
/// same way (`DashIcon`/`GridSearchResult` extend `AppIcon`). The icon pixels ride
/// on top as an [`app_icon_element`]; this type owns the tile box, the hover fill,
/// and hit-testing.
#[derive(Debug, Clone, Copy)]
pub struct AppIcon {
    /// The tile box (logical), laid out by the owner.
    pub rect: Rectangle<f64, Logical>,
    pub hovered: bool,
    /// Corner radius of the hover/selection fill. Which one depends on where the
    /// tile lives — see [`AppIcon::RADIUS`] vs [`AppIcon::OVERVIEW_TILE_RADIUS`].
    pub radius: f64,
}

impl AppIcon {
    /// `%tile` padding around the icon (`_common.scss:86`).
    pub const PADDING: f64 = 6.;
    /// `%tile` corner radius (`_common.scss:85`).
    pub const RADIUS: f64 = 16.;

    /// `.overview-tile` padding (`$base_padding * 2`, `_app-grid.scss:26`).
    ///
    /// The app grid and the search results put the button — and so the
    /// hover/selection fill — on the *outer* `.overview-tile`, which overrides
    /// `%tile`'s padding and radius (`_app-grid.scss:24-26`) and wraps the label
    /// as well as the icon. The dash is the other case: it resets
    /// `.overview-tile` and styles the inner `.overview-icon` as a plain `%tile`
    /// instead (`_dash.scss:49-63`), so it keeps [`PADDING`]/[`RADIUS`].
    ///
    /// [`PADDING`]: Self::PADDING
    /// [`RADIUS`]: Self::RADIUS
    pub const OVERVIEW_TILE_PADDING: f64 = 12.;
    /// `.overview-tile` corner radius (`$base_border_radius * 3`, `_app-grid.scss:25`).
    pub const OVERVIEW_TILE_RADIUS: f64 = 24.;

    /// The tile side for a given icon size (icon + padding both sides).
    pub fn size(icon_px: f64) -> f64 {
        icon_px + 2. * Self::PADDING
    }

    pub fn icon_center(&self) -> Point<f64, Logical> {
        Point::from((
            self.rect.loc.x + self.rect.size.w / 2.,
            self.rect.loc.y + self.rect.size.h / 2.,
        ))
    }
}

/// Fixed geometry of a labelled `.overview-tile` — a full-color icon with a caption
/// line beneath it. Shared by the search results grid and the app grid, which GNOME
/// builds from the same `IconGrid.BaseIcon` (`search.js:144-146`, `appDisplay.js`),
/// so the two stay pixel-identical. The hover/selection wash and the label are baked
/// by [`Painter::labelled_tile`]; the icon pixels ride on top as an
/// [`app_icon_element`].
#[derive(Debug, Clone, Copy)]
pub struct TileMetrics {
    /// Full-color icon side (logical).
    pub icon_px: f64,
    /// `.overview-tile` padding around the icon+label (`_app-grid.scss:26`).
    pub pad: f64,
    /// Gap from the icon to the label (`.overview-icon-with-label` spacing,
    /// `$base_padding`, `_app-grid.scss:31-35`).
    pub label_gap: f64,
    /// Height of the single label line.
    pub label_h: f64,
    /// `.overview-tile` corner radius of the hover/selection fill.
    pub radius: f64,
}

impl TileMetrics {
    /// The 96 px labelled tile the app grid and search results share (`ICON_SIZE`=96,
    /// `iconGrid.js:11,83`; `.overview-tile` metrics `_app-grid.scss:24-35`).
    ///
    /// A function rather than a const because `label_h` is a *line box*, which depends on the
    /// realized font — it was pinned at 18 while the caption draws at `LABEL_PT` (the base 11pt),
    /// whose box is 19. That one pixel is load-bearing: a `BaseIcon` is a `SquareBin`, so the
    /// tile's width follows its content height and the whole tile was a pixel narrow with it.
    pub fn overview() -> Self {
        Self {
            icon_px: 96.,
            pad: AppIcon::OVERVIEW_TILE_PADDING,
            label_gap: 6.,
            label_h: crate::ui::line_height_px(crate::ui::BASE_FONT_PT),
            radius: AppIcon::OVERVIEW_TILE_RADIUS,
        }
    }

    /// The tile's outer size — **square**: [`Self::label_w`] plus padding on each side.
    ///
    /// A tile is `.overview-tile` padding around a `BaseIcon`, and a `BaseIcon` is a
    /// `Shell.SquareBin` (`iconGrid.js:62`) whose preferred *width* is its preferred
    /// *height* (`shell-square-bin.c:14-30`). So the width follows the content height —
    /// icon + spacing + one caption line — and not, as this used to say, the icon alone.
    /// At GNOME's metrics that is 144, not 120: the tile was 24 px too narrow, which
    /// showed as a hover wash narrower than GNOME's.
    pub fn size(&self) -> Size<f64, Logical> {
        let side = self.pad + self.label_w() + self.pad;
        Size::from((side, side))
    }

    /// How far a caption of `lines` hangs below the tile box.
    ///
    /// The tile is sized for **one** caption line — that is the `Shell.SquareBin` rule
    /// and it is what keeps the cell square, so reserving our resting
    /// [`TILE_LABEL_LINES`] in the box instead would cost a rung of the icon ladder on
    /// exactly the small canvases where the second line was supposed to help. So the
    /// extra line hangs into the row gap (6px of it, the rest is the tile's own bottom
    /// padding) and whatever is *drawn* around the caption — a hover wash, a focus ring,
    /// a folder tile's bubble — grows by this much instead.
    pub fn caption_overhang(&self, lines: usize) -> f64 {
        self.label_h * (lines.max(1) as f64 - 1.)
    }

    /// The width the caption is laid out in: the `Shell.SquareBin` box, i.e. the tile
    /// minus its padding (see [`Self::size`] for why that box is square).
    pub fn label_w(&self) -> f64 {
        self.icon_px + self.label_gap + self.label_h
    }

    /// Top of the caption band within a tile box `rect` (logical).
    pub fn label_top(&self, rect: Rectangle<f64, Logical>) -> f64 {
        rect.loc.y + self.pad + self.icon_px + self.label_gap
    }

    /// The icon's center within a tile box `rect` (logical) — the icon sits at the
    /// top of the tile, the label below it.
    pub fn icon_center(&self, rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + self.pad + self.icon_px / 2.,
        ))
    }

    /// One folder sub-icon's side, in logical px — [`FOLDER_SUBICON_FRACTION`] of the
    /// tile's icon box (`createFolderIcon`, `appDisplay.js:2149`).
    pub fn folder_subicon_px(&self) -> f64 {
        (FOLDER_SUBICON_FRACTION * self.icon_px).floor()
    }

    /// The center of folder sub-icon `i` (`0..4`, filled left-to-right then top-to-
    /// bottom) within a tile box `rect`.
    ///
    /// `createFolderIcon` (`appDisplay.js:2138-2162`) composes the folder's icon as a
    /// **homogeneous** 2×2 `Clutter.GridLayout` over an icon-box-sized widget, one
    /// member per cell at `(i % 2, i / 2)`. Homogeneous means each cell is half the
    /// box; the member icon is smaller than its cell and paints centered in it
    /// (`st-icon.c:478-479` center-aligns the texture inside the actor), which is what
    /// leaves the gap down the middle of a folder tile.
    pub fn folder_subicon_center(
        &self,
        rect: Rectangle<f64, Logical>,
        i: usize,
    ) -> Point<f64, Logical> {
        let center = self.icon_center(rect);
        let quarter = self.icon_px / 4.;
        let dx = if i.is_multiple_of(2) {
            -quarter
        } else {
            quarter
        };
        let dy = if i / 2 == 0 { -quarter } else { quarter };
        Point::from((center.x + dx, center.y + dy))
    }
}

/// The share of a folder tile's icon box that one member sub-icon takes
/// (`FOLDER_SUBICON_FRACTION`, `appDisplay.js:31`).
pub const FOLDER_SUBICON_FRACTION: f64 = 0.4;

/// How many lines an *expanded* tile caption may use.
///
/// GNOME does not cap it — `line_wrap: true` with `ellipsize: NONE` lets the label
/// grow and the tile's allocation follow it. A bake needs a size up front, so we cap;
/// three lines at the caption's wrap width clears every name in a default install,
/// and [`tile_label_lines`] ellipsizes past it rather than dropping text silently.
pub const TILE_LABEL_EXPAND_LINES: usize = 3;

/// How many lines a *resting* app-grid caption may use.
///
/// **Divergence (chosen).** GNOME's is one: `StLabel` puts `PANGO_ELLIPSIZE_END` on its
/// `ClutterText` (`st-label.c:331`) and the collapsed state turns wrapping off outright
/// (`_updateMultiline`, `appDisplay.js:1891-1924`), so a two-word name is cut at rest and
/// only readable on hover. Two lines read most names without hovering, and the tile has
/// the room: a second line takes the tile's bottom padding and 6 px of the row gap, which
/// is at minimum spacing still 18 px clear of the icon below.
///
/// Search results are **not** this: they keep GNOME's single line (see the call site).
pub const TILE_LABEL_LINES: usize = 2;

/// The caption lines of a `.overview-tile`, top to bottom, for a label box `wrap_w`
/// wide (see [`TileMetrics::label_w`]).
///
/// GNOME's two states (`AppViewItem._updateMultiline`, `appDisplay.js:1891-1924`):
///
/// * **collapsed** — ellipsized at the end. That is not something the app grid opts into: `StLabel`
///   sets `PANGO_ELLIPSIZE_END` on its `ClutterText` (`st-label.c:331`), so it is what *every*
///   caption does, search results included (they pass `expandTitleOnHover: false`,
///   `appDisplay.js:1837-1841`, and so never leave this state). GNOME's collapsed label is one
///   line; the grid passes [`TILE_LABEL_LINES`], which is the divergence recorded there.
/// * **expanded** — hover, key focus, or the forced highlight of an open context menu
///   (`appDisplay.js:1901`): wrapping on (`Pango.WrapMode.WORD_CHAR`), ellipsis off, so the whole
///   name is readable.
///
/// Break points are computed in **logical** px, so they do not move with the output
/// scale; the result is memoized in [`synoik_vk::text::wrap_lines_weighted`].
/// `max_lines` is which of the two states this is: [`TILE_LABEL_LINES`] collapsed,
/// [`TILE_LABEL_EXPAND_LINES`] expanded — or 1 for a caption that is neither, like a
/// search result's.
///
/// `break_words` is Pango's `WORD_CHAR` vs `WORD`, and it goes with the state: **expanded**
/// breaks words, because the point of expanding is to show the whole name and a word wider
/// than the tile has nowhere else to go. **At rest** it must not — a resting caption that
/// splits "Graphics" into "Graphi/cs" reads as broken, where "Graphic…" reads as a name
/// that did not fit. (GNOME never faces this: its resting label is one line, so there is
/// no wrap to choose a mode for.)
pub fn tile_label_lines(
    name: &str,
    pt: f64,
    wrap_w: f64,
    max_lines: usize,
    break_words: bool,
) -> Vec<String> {
    let px = crate::ui::pt_to_px(pt) as f32;
    synoik_vk::text::wrap_lines_weighted(name, px, false, wrap_w, max_lines.max(1), break_words)
}

/// One line of `text`, ellipsized at the end if it does not fit `max_w` logical px.
///
/// This is what a plain `StLabel` does with no styling at all: St puts `PANGO_ELLIPSIZE_END` on
/// its `ClutterText` (`st-label.c:331`), so any label given less width than it wants is cut with
/// an ellipsis rather than overflowing its allocation. Reach for this for a *single-line* label
/// whose text is content (a window title, a device name) rather than something you control —
/// [`tile_label_lines`] is the multi-line, app-grid-specific counterpart.
///
/// Break points are computed in logical px, so they do not move with the output scale.
pub fn ellipsized_line(text: &str, pt: f64, max_w: f64) -> String {
    let px = crate::ui::pt_to_px(pt) as f32;
    synoik_vk::text::wrap_lines_weighted(text, px, false, max_w, 1, false)
        .pop()
        .unwrap_or_default()
}

/// A rounded single-line text-entry chrome — the GNOME `St.Entry` used for the
/// overview `search-entry` (`_search-entry.scss`, `overviewControls.js:325`). A
/// **view + geometry** primitive: the caller owns the editable string (like
/// [`crate::ui::run_dialog`] does) and feeds it in. [`Entry::bake`] draws the pill
/// background plus the placeholder/typed text and a trailing caret; the primary
/// (find) and optional secondary (clear) symbolic glyphs ride on top as
/// [`icon_element`]s the caller composites (so they fade with the overview, like the
/// dash's show-apps glyph). run_dialog keeps its own bespoke centered mono field for
/// now — adopting this here is a follow-up.
pub struct Entry;

/// Where an [`Entry`]'s parts sit (all logical, absolute output coords).
#[derive(Debug, Clone, Copy)]
pub struct EntryLayout {
    /// The rounded pill box.
    pub pill: Rectangle<f64, Logical>,
    /// Center of the primary (`edit-find-symbolic`) glyph.
    pub primary_icon: Point<f64, Logical>,
    /// Center of the trailing (`edit-clear-symbolic`) glyph.
    pub secondary_icon: Point<f64, Logical>,
    /// Left x where the text/placeholder begins.
    pub text_x: f64,
}

/// What an [`Entry`] shows — the string plus where the caret and selection are.
///
/// Bundled rather than passed as five more arguments to [`Entry::bake`], and built from a
/// [`TextEdit`](crate::ui::text_edit::TextEdit) by [`EntryContent::of`] so the offsets a
/// caller draws can't drift from the ones it edits.
#[derive(Debug, Clone, Default)]
pub struct EntryContent<'a> {
    /// The text as typed — **unmasked**, so `cursor`/`selection` index it directly.
    pub text: &'a str,
    /// Shown instead, muted and caret-less, while `text` is empty.
    pub placeholder: &'a str,
    /// Caret byte offset into `text`. `None` draws no caret — an unfocused field, or one
    /// whose caller has no caret model yet.
    pub cursor: Option<usize>,
    /// Selected byte range of `text`, drawn behind the glyphs in
    /// `selection-background-color` (`%entry_common`, `_common.scss:178`).
    pub selection: Option<Range<usize>>,
    /// Draw every character as this glyph instead — a password field. Offsets still index
    /// `text`; [`Entry::bake`] remaps them onto the mask, so a caller never has to.
    pub mask: Option<char>,
    /// The input method's in-progress composition, drawn at the caret but not part of `text`.
    ///
    /// Shown **unmasked even in a password field**, which is what GNOME does: `ClutterText`
    /// masks its buffer and then splices the raw preedit into the result
    /// (`clutter-text.c:848-877`). A composition nobody can see is one nobody can steer.
    pub preedit: Option<&'a str>,
}

impl<'a> EntryContent<'a> {
    /// Plain read-only text with no caret — a field whose caller has no editing model.
    pub fn plain(text: &'a str, placeholder: &'a str) -> Self {
        Self {
            text,
            placeholder,
            ..Self::default()
        }
    }

    /// The live view of an editing model. `focused` gates the caret only: an unfocused
    /// entry still shows its text, it just doesn't blink at you.
    pub fn of(edit: &'a TextEdit, placeholder: &'a str, focused: bool) -> Self {
        Self {
            text: edit.text(),
            placeholder,
            cursor: focused.then(|| edit.cursor()),
            selection: focused.then(|| edit.selection()).flatten(),
            mask: None,
            preedit: edit.preedit(),
        }
    }

    /// [`Self::of`], masked — a password entry.
    pub fn masked(edit: &'a TextEdit, placeholder: &'a str, focused: bool, mask: char) -> Self {
        Self {
            mask: Some(mask),
            ..Self::of(edit, placeholder, focused)
        }
    }

    /// The string actually drawn, and a byte-offset mapper onto it.
    ///
    /// Masking is per **character**, so an offset maps by counting characters before it and
    /// multiplying by the mask's own encoded length — not by reusing the byte offset, which
    /// would land mid-glyph for any non-ASCII password.
    fn display(&self) -> (String, impl Fn(usize) -> usize + '_) {
        let mask = self.mask;
        let text = self.text;
        let preedit = self.preedit.unwrap_or("");
        let masked_at = move |at: usize| match mask {
            Some(m) => text[..at.min(text.len())].chars().count() * m.len_utf8(),
            None => at.min(text.len()),
        };

        let base = match mask {
            Some(m) => m.to_string().repeat(text.chars().count()),
            None => text.to_owned(),
        };
        // The composition is spliced in at the caret, so every offset from there on shifts by
        // its length — including the caret itself, which belongs *after* what is being composed.
        let split = masked_at(self.cursor.unwrap_or(text.len()));
        let shown = if preedit.is_empty() {
            base
        } else {
            let mut out = String::with_capacity(base.len() + preedit.len());
            out.push_str(&base[..split]);
            out.push_str(preedit);
            out.push_str(&base[split..]);
            out
        };
        let map = move |at: usize| {
            let at = masked_at(at);
            if at >= split {
                at + preedit.len()
            } else {
                at
            }
        };
        (shown, map)
    }
}

/// What a point over an [`Entry`] hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryHit {
    /// The trailing glyph — the search entry's `edit-clear-symbolic`, or the password entry's
    /// `view-reveal-symbolic` peek toggle.
    Trailing,
    /// Anywhere else in the pill.
    Field,
}

/// Which entry family to draw — the two `%…_entry` placeholders the port has reached.
///
/// The families differ in more than colour, which is why this is not a colour argument: the search
/// entry reserves gutters for its find/clear glyphs and starts its text after them, while the
/// lock screen's has no icons at all and centres what you type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStyle {
    /// `%system_entry` — the overview search field.
    ///
    /// The one entry family whose fill follows the appearance, because it is the one that sits
    /// on the overview's backdrop and takes the shared plate; it carries its
    /// [`Appearance`](style::Appearance) rather than every entry taking one it has no use for.
    Search(style::Appearance),
    /// `%lockscreen_entry` (`_common.scss:370-379`): `entry(…, $style: lockscreen)` fills with
    /// `transparentize($system_fg_color, .9)` over the wallpaper and takes its focus ring from
    /// `transparentize($fg, 0.6)` (`_drawing.scss:99-105,138-142`). No icons.
    ///
    /// It `@extend`s `%entry_common` (`:174-180`), so it is a `$base_border_radius` box with
    /// `$base_padding * 1.5` padding and ordinary left-aligned text — **not** a pill, and not
    /// centred. Its placeholder is `transparentize($system_fg_color, 0.3)` (`:378`).
    Lockscreen,
    /// An ordinary `%entry` on a *dialog* rather than over the wallpaper, so it takes
    /// `_entries.scss`'s normal fill (`mix($fg_color, $bg_color, 9%)`, i.e. [`style::ENTRY_BG`])
    /// rather than the lock screen's translucent one, and its focus ring is the accent.
    ///
    /// Both dialogs that have an entry land here: `.prompt-dialog-password-entry`
    /// (`_dialogs.scss:119-122`) and `.run-dialog-entry` (`:90-92`). Neither restyles the box —
    /// the run dialog only overrides its padding, which is a height the caller passes in.
    Dialog,
}

/// `%entry_common` `padding: $base_padding * 1.5` (`_common.scss:177`).
const ENTRY_PAD: f64 = 9.;

impl EntryStyle {
    fn bg(self) -> Rgba {
        match self {
            // The one entry that sits on the overview backdrop, so it takes the shared plate
            // rather than `%system_entry`'s opaque fill — see [`style::Appearance::plate`]. The
            // other two are on a wallpaper and on a dialog respectively, and keep GNOME's values
            // (both always-dark, so neither follows the appearance).
            EntryStyle::Search(appearance) => appearance.plate(),
            // `transparentize(white, .9)`.
            EntryStyle::Lockscreen => [1., 1., 1., 0.1],
            EntryStyle::Dialog => style::ENTRY_BG,
        }
    }

    /// The focus ring's colour, when the caller says the entry has focus.
    fn focus_ring(self, accent: Rgba) -> Option<Rgba> {
        match self {
            // The search entry's focus is drawn by its caller's inset-accent ring.
            EntryStyle::Search(_) => None,
            // `transparentize(white, 0.6)`.
            EntryStyle::Lockscreen => Some([1., 1., 1., 0.4]),
            // An ordinary entry on a dialog takes `focus_ring()`'s accent (`_drawing.scss:99-105`),
            // not the lock screen's white — that one exists because there is a wallpaper behind it.
            EntryStyle::Dialog => Some(accent),
        }
    }

    /// Whether this family reserves a **leading** icon gutter. Only the search entry has a
    /// primary icon (`searchController.js:72-75`); the password entries put their one glyph
    /// on the trailing side.
    fn has_primary_icon(self) -> bool {
        matches!(self, EntryStyle::Search(_))
    }

    /// Where the text starts, and how it is aligned in the box.
    fn text(self, _width: f64) -> (f64, HAlign) {
        let x = if self.has_primary_icon() {
            Entry::ICON_GUTTER
        } else {
            ENTRY_PAD
        };
        (x, HAlign::Left)
    }

    /// `$forced_circular_radius` for the search pill; `$base_border_radius` for the plain box.
    ///
    /// Circular takes the drawn `height`, not [`Entry::HEIGHT`]: the search entry rests as a
    /// puck taller than the open pill, and a fixed radius would square off its corners.
    fn radius(self, height: f64) -> f64 {
        match self {
            EntryStyle::Search(_) => height / 2.,
            EntryStyle::Lockscreen | EntryStyle::Dialog => 8.,
        }
    }

    /// The placeholder's colour.
    fn placeholder(self) -> Rgba {
        match self {
            EntryStyle::Search(_) => style::MUTED,
            // `transparentize($system_fg_color, 0.3)`.
            EntryStyle::Lockscreen => [1., 1., 1., 0.7],
            // `$fg_color` at 70% — `%entry`'s placeholder (`_entries.scss:3`).
            EntryStyle::Dialog => [1., 1., 1., 0.7],
        }
    }
}

impl Entry {
    /// Pill height, logical: `%entry_common` 9px padding around a ~15px line, rounded
    /// up to a comfortable pill (`.search-entry` is `$forced_circular_radius`, so the
    /// radius is `height/2`). S5-tunable.
    pub const HEIGHT: f64 = 40.;
    /// The find/clear symbolic glyph side (`.search-entry-icon` `$scalable_icon_size`
    /// = 16px, `_search-entry.scss:10`).
    pub const ICON_PX: f64 = 16.;
    /// An entry icon's own horizontal padding: `.search-entry-icon { padding: 0 $base_margin }`
    /// (`_search-entry.scss:13`), and identically `StIcon.peek-password`/`.capslock-warning`
    /// (`_entries.scss:9,14`). So every entry glyph, in every family, has the same 4px each side.
    const ICON_PAD: f64 = 4.;
    /// The gap between an icon box and the text — `priv->spacing`, **hardcoded** `6.0f` in
    /// `st_entry_init` (`st-entry.c:1025`). It is not a CSS property and has no setter, so it
    /// is a fixed 6 logical px whatever the theme does.
    const ICON_SPACING: f64 = 6.;
    /// Icon centre inset from the pill's near edge: `st_entry_allocate` puts the icon box flush
    /// with the **content** box (`st-entry.c:452-467`, zero extra offset), so it is the entry's
    /// 9px padding, plus the icon's own 4px padding, plus half the 16px glyph — **21px**.
    ///
    /// This was 16 before, which is why both glyphs sat visibly too near the pill's ends.
    pub const ICON_INSET: f64 = ENTRY_PAD + Self::ICON_PAD + Self::ICON_PX / 2.;
    /// `.search-entry-icon { margin-top: 2px }` (`_search-entry.scss:12`) — an explicit
    /// optical-centering nudge on top of the vertical centering `st_entry_allocate` already does.
    pub const ICON_NUDGE_Y: f64 = 2.;
    /// Distance from an edge to the text beside an icon: content box + the whole icon box
    /// (`4 + 16 + 4`) + [`ICON_SPACING`](Self::ICON_SPACING) — **39px**.
    const ICON_GUTTER: f64 = ENTRY_PAD + Self::ICON_PAD * 2. + Self::ICON_PX + Self::ICON_SPACING;
    /// Entry font (`%system_entry` inherits the 11pt base).
    const TEXT_PT: f64 = 11.;
    /// The caret bar's width. St's `caret-size` defaults to 1px.
    const CARET_W: f64 = 1.;

    /// The focus ring's stroke (`focus_ring` is 2px in `_drawing.scss`).
    const FOCUS_RING: f64 = 2.;

    /// Lay out a pill of `width` x `height` centered horizontally on `center_x`, top edge
    /// `top_y`. `height` is a parameter rather than [`Self::HEIGHT`] because the search entry
    /// rests as a puck bigger than the pill it opens into.
    ///
    /// The icon centres are only meaningful for [`EntryStyle::Search`]; a lockscreen entry has no
    /// glyphs, and `text_x` is its centre rather than a left edge.
    pub fn layout(
        center_x: f64,
        top_y: f64,
        width: f64,
        height: f64,
        style: EntryStyle,
    ) -> EntryLayout {
        let x = (center_x - width / 2.).round();
        let pill = Rectangle::new(Point::from((x, top_y.round())), Size::from((width, height)));
        // `.search-entry-icon`'s 2px `margin-top` rides on the centre, for every family: the
        // plain entries' glyphs get no such nudge in the theme, so only Search takes it.
        let cy = pill.loc.y
            + height / 2.
            + if style.has_primary_icon() {
                Self::ICON_NUDGE_Y
            } else {
                0.
            };
        let inset = Self::ICON_INSET;
        EntryLayout {
            pill,
            primary_icon: Point::from((pill.loc.x + inset, cy)),
            secondary_icon: Point::from((pill.loc.x + width - inset, cy)),
            text_x: pill.loc.x + style.text(width).0,
        }
    }

    /// Hit-test a point: the trailing clear disc (only when `has_clear`), else the
    /// field body, else `None`.
    pub fn hit(
        layout: &EntryLayout,
        pos: Point<f64, Logical>,
        has_trailing: bool,
    ) -> Option<EntryHit> {
        if !layout.pill.contains(pos) {
            return None;
        }
        if has_trailing {
            let d = pos - layout.secondary_icon;
            if d.x * d.x + d.y * d.y <= Self::ICON_PX * Self::ICON_PX {
                return Some(EntryHit::Trailing);
            }
        }
        Some(EntryHit::Field)
    }

    /// Bake the pill + selection wash + text/placeholder + caret into a pill-sized texture
    /// (composited by the caller at `layout.pill.loc`, the two glyphs on top).
    ///
    /// While `content.text` is empty the `placeholder` shows muted (a hint, never selected or
    /// caret-bearing); once there is text it shows in full white, with the selection painted
    /// behind it and a 1px caret bar at `content.cursor`.
    ///
    /// Text longer than the field **scrolls**: the run is offset so the caret stays inside the
    /// clip, pinned to the trailing edge once it would run past it, exactly far enough and no
    /// further. That replaces the MVP's hard clip, which simply hid whatever you typed past the
    /// pill's width.
    ///
    /// Caret and selection are placed by re-measuring the text before them
    /// ([`synoik_vk::text::measure_line_width_weighted`], memoized) and adding it to the run's own
    /// pen origin, so they land on advance boundaries the drawn glyphs agree with rather than on
    /// an ink-box estimate.
    #[track_caller]
    #[allow(clippy::too_many_arguments)]
    pub fn bake(
        renderer: &mut VulkanRenderer,
        cache: &mut BakeCache,
        scale: f64,
        width: f64,
        height: f64,
        content: EntryContent<'_>,
        entry_style: EntryStyle,
        focused: bool,
        // `has_trailing` reserves the trailing glyph's gutter, so long text stops before it
        // instead of running underneath.
        has_trailing: bool,
        // The system accent, for the styles whose focus ring is `focus_ring()`.
        accent: Rgba,
        revision: u64,
    ) -> anyhow::Result<TextureBuffer<VkTexture>> {
        let size = Size::<f64, Logical>::from((width, height));
        let (shown, map) = content.display();
        let empty = shown.is_empty();
        let display = if empty {
            content.placeholder.to_owned()
        } else {
            shown.clone()
        };
        // The caret is gated on **focus**, not on there being text: an empty focused entry is
        // exactly where the caret is the only thing left to see, and a field showing neither
        // text nor caret reads as dead. (GNOME draws the hint label beside a ClutterText that
        // still carries its cursor.) A *selection*, on the other hand, needs something to
        // select, and never spans a placeholder — that is a hint, not a value.
        let cursor = content.cursor.map(&map);
        let selection = (!empty)
            .then(|| content.selection.clone().map(|s| map(s.start)..map(s.end)))
            .flatten()
            .filter(|s| s.start < s.end);

        let (text_x, halign) = entry_style.text(width);
        // The text area: the entry's own gutter on the leading side (after the primary icon, if
        // this family has one), and the trailing glyph's gutter on the other when one is shown.
        let leading = text_x;
        let trailing = if has_trailing {
            Self::ICON_GUTTER
        } else {
            ENTRY_PAD
        };
        let avail = (width - leading - trailing).max(0.);
        let clip =
            Rectangle::<f64, Logical>::new(Point::from((leading, 0.)), Size::from((avail, height)));
        let ring = focused.then(|| entry_style.focus_ring(accent)).flatten();

        // Measurement happens in the same physical px the run is shaped at, so the advances
        // agree with the glyphs to the pixel.
        let font_px = (crate::ui::pt_to_px(Self::TEXT_PT) * scale) as f32;
        let advance = |upto: usize| {
            synoik_vk::text::measure_line_width_weighted(&display[..upto], font_px, false)
        };

        // Scroll just enough to keep the caret in view, and never past the end of the text —
        // so a short string never shifts, and a long one parks its tail at the trailing edge.
        //
        // Deliberately stateless (derived from the caret alone), which costs one nicety: with
        // overflowing text, walking Left from the end holds the caret against the clip's
        // trailing edge and slides the text under it, where GNOME would hold the viewport and
        // move the caret inside it. Fixing that means the *view* owning a scroll offset across
        // frames, which is state this bake does not have and does not want; the current
        // behavior is at least monotonic and never hides the caret.
        let avail_px = avail * scale;
        let total_px = advance(display.len());
        let scroll_px = match cursor {
            Some(at) if total_px > avail_px => {
                (advance(at) + Self::CARET_W * scale - avail_px).clamp(0., total_px - avail_px)
            }
            _ => 0.,
        };

        // Fold the caret and selection into the caller's revision here rather than asking every
        // caller to remember them: they change what is drawn without changing the text, so a
        // text-keyed revision would leave the caret frozen where it was baked.
        let revision = Revision::new()
            .of(revision)
            .of(cursor)
            .of(selection.clone())
            .px(scroll_px)
            .done();

        bake(
            renderer,
            cache,
            scale,
            size,
            revision,
            {
                let display = display.clone();
                move |r: &mut VulkanRenderer| {
                    let mut shaper = TextShaper::new(r, scale);
                    shaper.shape(&display, TextStyle::new(Self::TEXT_PT))
                }
            },
            move |frame, phys, shaped| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(entry_style.radius(height), entry_style.bg())?;
                if let Some(ring) = ring {
                    p.stroke_rounded(
                        Rectangle::from_size(Size::from((width, height))),
                        entry_style.radius(height),
                        Self::FOCUS_RING,
                        ring,
                    )?;
                }

                let anchor = leading - scroll_px / scale;
                let m = CaretMetrics::new(shaped, anchor, halign, scale, Self::TEXT_PT, false);
                // A fixed band, not the ink's height: an empty entry has no ink at all, and a
                // caret whose height came from the glyphs would vanish exactly when it is the
                // only thing left to see.
                let band = Rectangle::<f64, Logical>::new(
                    Point::from((0., ENTRY_PAD)),
                    Size::from((width, height - ENTRY_PAD * 2.)),
                );

                // Selection goes *behind* the glyphs. `selected-color` is `$fg_color`
                // (`_common.scss:180`), which is what unselected text already draws in, so the
                // run needs no second, differently-tinted pass.
                if let Some(sel) = &selection {
                    p.selection(
                        m.x_at(&display, sel.start),
                        m.x_at(&display, sel.end),
                        band,
                        style::selection_bg(accent),
                        Some(clip),
                    )?;
                }

                let color = if empty {
                    entry_style.placeholder()
                } else {
                    style::TEXT
                };
                p.text_band(shaped, anchor, halign, 0., height, color, clip)?;

                if let Some(at_byte) = cursor {
                    // With nothing typed, the run that got shaped is the **placeholder**, and the
                    // caret has no business riding its metrics: the pen sits left of that run's
                    // ink by the first glyph's left side bearing, so the bar landed outside the
                    // text clip and was shaved away to nothing. It is font-dependent, which is how
                    // it survived every test — the caret was there under the harness's fallback
                    // face and gone under Adwaita Sans on the seat.
                    //
                    // An empty entry's caret belongs where the first typed glyph's ink will start,
                    // which is the text origin. Not folded into `x_at`: with text present and the
                    // run scrolled, a caret at offset 0 is legitimately off the left edge and
                    // clipping it away is right.
                    let x = if empty {
                        leading
                    } else {
                        m.x_at(&display, at_byte)
                    };
                    p.caret(x, band, style::TEXT, Some(clip))?;
                }
                Ok(())
            },
        )
    }
}

/// Where a caret and a selection land inside a drawn run.
///
/// Both entry surfaces — the pill ([`Entry::bake`]) and the folder-rename box, which is
/// centred and bold and so cannot simply *be* an [`Entry`] — need the same three steps: recover
/// the run's pen origin from where its ink was anchored, measure the advance of the text before
/// an offset, and turn that into a bar. Doing it twice is how the two drift, so it lives here.
///
/// The pen origin matters: [`Painter::text`] and [`Painter::text_band`] anchor a run's **ink**,
/// and the glyph advances a caret rides are measured from the pen. Deriving the caret from the
/// ink box instead puts it off by the first glyph's left side bearing — invisible on `n`,
/// obvious on `j` or a leading space.
///
/// **Known limitation — RTL.** Offsets are turned into x by re-measuring `text[..offset]`, which
/// assumes logical order matches visual order. For a right-to-left or bidi run the glyphs are
/// laid out in visual order, so the caret lands at the LTR-prefix width rather than at the
/// insertion point and a selection washes the wrong glyphs. Fixing it needs an index→x mapping
/// out of the shaper (cosmic-text has the per-glyph byte ranges; `synoik_vk::text::ShapedRun`
/// does not expose them yet), not a change here. LTR runs are exact apart from kerning across
/// the caret boundary.
#[derive(Debug, Clone, Copy)]
pub struct CaretMetrics {
    /// Run-local pen origin, physical px.
    pen: f64,
    scale: f64,
    font_px: f32,
    bold: bool,
}

impl CaretMetrics {
    /// Recover the metrics for a run whose **ink** was anchored at logical `anchor_x` per
    /// `halign`. `pt` and `bold` must be the ones the run was shaped with, or the advances
    /// will not be the drawn glyphs'.
    pub fn new(
        shaped: &ShapedText,
        anchor_x: f64,
        halign: HAlign,
        scale: f64,
        pt: f64,
        bold: bool,
    ) -> Self {
        let (ink_x, _, ink_w, _) = shaped.ink_bounds();
        let anchor = anchor_x * scale;
        let ink_left = match halign {
            HAlign::Left => anchor,
            HAlign::Center => anchor - f64::from(ink_w) / 2.,
            HAlign::Right => anchor - f64::from(ink_w),
        };
        Self {
            pen: ink_left - f64::from(ink_x),
            scale,
            font_px: (crate::ui::pt_to_px(pt) * scale) as f32,
            bold,
        }
    }

    /// The logical x of byte offset `at` in `text` (which must be the string that was shaped).
    pub fn x_at(&self, text: &str, at: usize) -> f64 {
        let at = at.min(text.len());
        let advance =
            synoik_vk::text::measure_line_width_weighted(&text[..at], self.font_px, self.bold);
        (self.pen + advance) / self.scale
    }
}

/// What a cached bake is keyed by: `(scale, physical_w, physical_h)`. The revision rides beside
/// the value rather than in here, since a stale entry is replaced, never looked up.
type BakeKey = (NotNan<f64>, i32, i32);

/// A per-widget offscreen-texture cache for [`bake`], keyed by `(scale,
/// physical_size, revision)`. One lives (behind a `RefCell`) on each baking
/// widget; it clears itself when the renderer context changes.
#[derive(Default)]
pub struct BakeCache {
    context: Option<ContextId<VkTexture>>,
    /// The renderer's glyph epoch when these were baked. It moves only when a glyph upload failed
    /// and the residency was thrown away — at which point anything baked from it drew blank text,
    /// under a key the widget has no reason to change. Without this the blank survives as long as
    /// the widget's content does. See `VulkanRenderer::text_epoch`.
    text_epoch: u64,
    // key: (scale, physical_w, physical_h) -> (revision, buffer)
    //
    // The cached value is the [`TextureBuffer`], not the bare texture, and that is load-bearing:
    // the buffer is what carries the element `Id`, so wrapping a cached texture in a fresh buffer
    // each frame churns an *identity* even though not a pixel changed. Damage tracking then sees
    // the old element leave and a stranger arrive every frame, which throws away everything keyed
    // on that id — including each backdrop blur's whole chain, rebuilt from render
    // pass to pipelines. That cost ~15 GPU resource creations a frame for the length of every
    // overview transition (`ui/window_preview.rs`, `ui/dash.rs`), which is why `bake` hands back
    // the buffer rather than leaving the wrap to its callers.
    textures: HashMap<BakeKey, (u64, TextureBuffer<VkTexture>)>,
}

impl BakeCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A bake's `revision`, **derived from the values its paint closure reads** instead of maintained
/// beside them.
///
/// ```ignore
/// let revision = Revision::new()
///     .of(self.entries.len())
///     .of(&self.title)
///     .of(self.expanded)
///     .px(self.height)
///     .color(self.accent)
///     .done();
/// ```
///
/// # Why this exists
///
/// A hand-bumped `u64` is a *proxy* for "what this bake reads", kept somewhere else, with nothing
/// checking that the two agree. Every cache bug this codebase has had is that disagreement, in one
/// direction or the other:
///
/// - **Bumped when nothing baked changed** → a full GPU round trip on every frame of an animation.
///   The panel re-baked its whole bar for the length of every overview animation because opening
///   the overview checks the Activities button, whose *fade* made `are_animations_ongoing()` true
///   (`009213dd`); the app grid and the overview search re-shaped every label on every pointer move
///   because hover bumped `content_rev` (`c5336421`, `d396bd30`).
/// - **Not bumped when something did** → a stale texture that survives as long as the content does.
///   The calendar popover's background froze at its open-time height while the list kept growing,
///   because the height was not in the key (`128d112e`).
///
/// Deriving the key does not make either impossible, but it moves the mistake to where it can be
/// seen: the inputs are listed at the call site, immediately above the closure that reads them, so
/// adding one to the paint and adding one to the key are the same edit.
/// `docs/fork/widget-layer-design.md` §H1 prescribed this when `bake()` landed — "revision should
/// be **derived** … rather than hand-bumped, so it is correct on its own and size is pure
/// insurance".
///
/// Hashing, not equality, because `bake`'s cache key is a `u64`. A collision would serve a stale
/// texture, which at 64 bits over a handful of live variants is not a risk worth structuring
/// against — but it *is* the reason to prefer a signature tuple where a widget already has one
/// (`end_session`'s `Sig`), rather than converting it to this.
#[derive(Debug, Clone)]
pub struct Revision(std::collections::hash_map::DefaultHasher);

impl Default for Revision {
    fn default() -> Self {
        Self::new()
    }
}

impl Revision {
    pub fn new() -> Self {
        Self(std::collections::hash_map::DefaultHasher::new())
    }

    /// Fold in any hashable input. Order matters, which is what keeps `("a", "bc")` apart from
    /// `("ab", "c")`.
    #[must_use]
    pub fn of(mut self, value: impl std::hash::Hash) -> Self {
        std::hash::Hash::hash(&value, &mut self.0);
        self
    }

    /// Fold in a float — a size, a position, an animation progress. Floats are not `Hash`, and the
    /// two values that would otherwise misbehave are normalized: every NaN folds to one bit
    /// pattern (or a NaN input would miss the cache on **every** frame, which is the expensive
    /// failure this whole type exists to prevent), and `-0.0` folds to `0.0`, which is what the
    /// bake would draw anyway.
    #[must_use]
    pub fn px(self, value: f64) -> Self {
        let value = if value.is_nan() {
            f64::NAN
        } else {
            value + 0.0
        };
        self.of(value.to_bits())
    }

    /// Fold in a premultiplied or straight RGBA color.
    #[must_use]
    pub fn color(self, rgba: [f32; 4]) -> Self {
        rgba.iter().fold(self, |rev, c| rev.px(f64::from(*c)))
    }

    /// Fold in each item of an iterator, in order.
    #[must_use]
    pub fn each<T: std::hash::Hash>(self, items: impl IntoIterator<Item = T>) -> Self {
        items.into_iter().fold(self, Self::of)
    }

    /// The `revision` to hand [`bake`].
    pub fn done(&self) -> u64 {
        std::hash::Hasher::finish(&self.0)
    }
}

/// Convert a widget's logical size to the physical buffer size at `scale`
/// (clamped to at least 1×1). The single home for that rounding.
pub fn physical_size(scale: f64, logical: Size<f64, Logical>) -> Size<i32, Physical> {
    Size::from((
        to_physical_precise_round::<i32>(scale, logical.w).max(1),
        to_physical_precise_round::<i32>(scale, logical.h).max(1),
    ))
}

/// Bake a widget's chrome into a scale-sized offscreen `VkTexture`, caching by
/// `(scale, physical_size, revision)`. On a cache hit the stored texture is
/// cloned (the GPU image is `Arc`-shared) and **neither** closure runs.
///
/// The physical buffer is `round(logical_size × scale)`. Two phases, run only on a
/// cache miss:
/// - `prepare(renderer)` shapes every `GlyphRun` and returns them (or any bake inputs). Glyph
///   shaping needs `&mut VulkanRenderer` and cannot run while the frame is alive, so it must happen
///   here, before the frame opens.
/// - `paint(frame, phys, prepared)` clears + draws everything into the bound [`VulkanFrame`] (the
///   full-buffer rect is `Rectangle::from_size(phys)`). Widgets clear with their own color —
///   transparent for rounded popovers, a border color for square dialogs.
#[track_caller]
pub fn bake<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    logical_size: Size<f64, Logical>,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<P>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<TextureBuffer<VkTexture>> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;
    let phys = physical_size(scale, logical_size);
    let key = (scale_key, phys.w, phys.h);

    // The renderer context changing invalidates every cached GPU texture, and so does the glyph
    // residency being rebuilt — a texture baked from glyphs that never reached the atlas holds
    // blank text.
    let context = renderer.context_id();
    let text_epoch = renderer.text_epoch();
    if cache.context.as_ref() != Some(&context) || cache.text_epoch != text_epoch {
        cache.textures.clear();
        cache.context = Some(context);
        cache.text_epoch = text_epoch;
    }

    // Fold the style generation into every widget's revision, so a change to the base
    // font size re-bakes all of them. It cannot be left to the per-widget keys: a
    // fixed-size surface with text inside it (the panel bar) has an unchanged
    // `(scale, size, revision)` and would keep serving the old text.
    let revision = revision ^ crate::ui::style_generation();

    let fresh = matches!(cache.textures.get(&key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let prepared = prepare(renderer)?;
        let tex = bake_uncached(renderer, scale, logical_size, |frame, phys| {
            paint(frame, phys, &prepared)
        })?;
        // Widget bakes are transparent-cornered and composited by their caller, so there is no
        // opaque region to declare; `Transform::Normal` because a bake is drawn the way it was
        // painted.
        let buffer = TextureBuffer::from_texture(renderer, tex, scale, Transform::Normal, vec![]);
        cache.textures.insert(key, (revision, buffer));
    }

    // A clone keeps the `Id`: that is the point of caching the buffer.
    Ok(cache.textures.get(&key).map(|(_, b)| b.clone()).unwrap())
}

/// A GNOME `box-shadow` for a card, in logical px: gaussian `blur` radius (σ = blur/2), `offset`
/// `(dx, dy)`, `spread` (the shadow box grows by this before blurring, corners with it), and a
/// straight-alpha `color`. Consumed by [`bake_card_shadow`].
#[derive(Clone, Copy)]
pub struct DropShadowSpec {
    pub blur: f64,
    pub offset: (f64, f64),
    pub spread: f64,
    pub color: Rgba,
}

/// Bake (cached by `(scale, size, revision)`) a card's drop shadow into its own transparent
/// texture, and return it with the physical `(dx, dy)` to subtract from the card's on-screen
/// location so the shadow sits behind it. `card_size`/`radius` describe the card; `spec` is the
/// GNOME `box-shadow`. The buffer pads the card by the blur reach (~3σ) + `spread` all round (plus
/// the downward `offset` at the bottom), so the fringe never clips. The reusable form of the
/// screenshot-panel / notification-banner shadow — draws through [`Painter::drop_shadow`].
#[track_caller]
pub fn bake_card_shadow(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    spec: DropShadowSpec,
) -> anyhow::Result<(TextureBuffer<VkTexture>, Point<i32, Physical>)> {
    // Blur reach (~3σ) + a pixel of ceil headroom, plus the spread, is the pad on every side; the
    // downward offset only extends the bottom (top pad already covers a small upward offset).
    let reach = spec.blur * 1.5 + 1.;
    let pad = reach + spec.spread;
    let size = Size::<f64, Logical>::from((
        card_size.w + pad * 2.,
        card_size.h + pad * 2. + spec.offset.1.max(0.),
    ));
    // The (spread-inflated) shadow box, positioned so it has `reach` of blur room on top/left.
    let box_rect = Rectangle::new(
        Point::from((reach, reach)),
        Size::from((
            card_size.w + spec.spread * 2.,
            card_size.h + spec.spread * 2.,
        )),
    );
    let tex = bake(
        renderer,
        cache,
        scale,
        size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.drop_shadow(
                box_rect,
                radius + spec.spread,
                spec.blur,
                spec.offset,
                spec.color,
            )?;
            Ok(())
        },
    )?;
    let off = to_physical_precise_round::<i32>(scale, pad);
    Ok((tex, Point::from((off, off))))
}

/// Bake (cached by `(scale, size, revision)`) a card's 1px inset border into its own transparent
/// texture — a `stroke_rounded_full` ring at `radius`, `color` — to composite ON TOP of the card
/// (at the card's own origin, no offset). The `.popup-menu-content` border counterpart to
/// [`bake_card_shadow`]; a top overlay so it works for a multi-texture card (the calendar popover
/// stacks a column over its background box) without a seam. Width is GNOME's fixed 1px.
#[track_caller]
pub fn bake_card_border(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    color: Rgba,
) -> anyhow::Result<TextureBuffer<VkTexture>> {
    bake(
        renderer,
        cache,
        scale,
        card_size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.stroke_rounded_full(radius, 1., color)?;
            Ok(())
        },
    )
}

/// Bake (cached by `(scale, size, revision)`) a keyboard-focus ring as a transparent texture the
/// size of the focused control, to be composited **over** it.
///
/// For a focusable whose surface is baked somewhere the ring cannot reach — a notification card,
/// which is its own cached texture composited into a scrolling list — where threading a focus flag
/// through the card's bake and its cache key would mean re-baking the card's whole content on
/// every focus step. The ring is the same [`Painter::focus_ring`] every other surface draws.
pub fn bake_focus_ring(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    size: Size<f64, Logical>,
    radius: f64,
    base: Rgba,
) -> anyhow::Result<TextureBuffer<VkTexture>> {
    bake(
        renderer,
        cache,
        scale,
        size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.focus_ring(Rectangle::from_size(size), radius, base)?;
            Ok(())
        },
    )
}

/// Bake (cached by `(scale, size, revision)`) a card's rounded `.popup-menu-content` background
/// fill into its own texture — a `fill_rounded_full` at `radius`, `color`. Composited BEHIND the
/// content and ABOVE the drop shadow, this is the single home for the panel-popover box bg so the
/// three popovers (QS / date / input-source) can't drift: each content bakes with a transparent
/// bg and the shared popover chrome draws this one cited fill. Counterpart to [`bake_card_border`]
/// and [`bake_card_shadow`].
#[track_caller]
pub fn bake_card_fill(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    color: Rgba,
) -> anyhow::Result<TextureBuffer<VkTexture>> {
    bake(
        renderer,
        cache,
        scale,
        card_size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.fill_rounded_full(radius, color)?;
            Ok(())
        },
    )
}

/// The opaque sub-region of a rounded-rect texture of physical `size` with corner radius
/// `radius_px` (physical px): two overlapping bands that exclude the four transparent corner
/// squares, so occlusion never treats a cut-away corner as opaque (which would drop whatever shows
/// through the rounding). Under-reporting the small arc slivers is harmless. The single home for
/// this band math (the popover chrome fill and any rounded opaque surface share it).
pub fn rounded_opaque_regions(
    size: Size<i32, BufferCoord>,
    radius_px: i32,
) -> Vec<Rectangle<i32, BufferCoord>> {
    if radius_px > 0 && size.w > 2 * radius_px && size.h > 2 * radius_px {
        vec![
            Rectangle::new(
                Point::from((0, radius_px)),
                Size::from((size.w, size.h - 2 * radius_px)),
            ),
            Rectangle::new(
                Point::from((radius_px, 0)),
                Size::from((size.w - 2 * radius_px, size.h)),
            ),
        ]
    } else {
        vec![Rectangle::from_size(size)]
    }
}

/// A cache for [`bake_content`] — a content-sized bake whose physical size is not
/// known until its text is shaped, so it is keyed by `(scale, revision)` alone
/// (the revision determines the content, hence the size). Clears on context change.
#[derive(Default)]
pub struct ContentCache {
    context: Option<ContextId<VkTexture>>,
    /// The renderer's glyph epoch when these were baked. It moves only when a glyph upload failed
    /// and the residency was thrown away — at which point anything baked from it drew blank text,
    /// under a key the widget has no reason to change. Without this the blank survives as long as
    /// the widget's content does. See `VulkanRenderer::text_epoch`.
    text_epoch: u64,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl ContentCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Bake a **content-sized** widget (a dialog/notification box whose size is derived
/// from its shaped text's ink, not known up front). Cached by `(scale, revision)`.
///
/// `prepare(renderer)` shapes the text and returns the physical buffer size plus a
/// layout value `P`; `paint(frame, phys, prepared)` draws it. Both run only on a
/// cache miss. The caller reads the returned texture's own size to place it (these
/// widgets center themselves on screen from the baked size).
#[track_caller]
pub fn bake_content<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut ContentCache,
    scale: f64,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<(Size<i32, Physical>, P)>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;

    // Same two invalidations as `bake`: a new renderer context, and a rebuilt glyph residency
    // (which means anything cached here was baked with blank text).
    let context = renderer.context_id();
    let text_epoch = renderer.text_epoch();
    if cache.context.as_ref() != Some(&context) || cache.text_epoch != text_epoch {
        cache.textures.clear();
        cache.context = Some(context);
        cache.text_epoch = text_epoch;
    }

    let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let (phys, prepared) = prepare(renderer)?;
        let tex = bake_uncached_sized(renderer, phys, |frame| paint(frame, phys, &prepared))?;
        cache.textures.insert(scale_key, (revision, tex));
    }

    Ok(cache
        .textures
        .get(&scale_key)
        .map(|(_, t)| t.clone())
        .unwrap())
}

/// The offscreen dance for an already-known **physical** size (no logical→physical
/// step). Shared by [`bake_content`] and [`bake_uncached`]; also for a
/// content-sized widget with its own owner-driven cache that has already computed
/// its physical size (the screenshot help panel).
#[track_caller]
pub fn bake_uncached_sized(
    renderer: &mut VulkanRenderer,
    phys: Size<i32, Physical>,
    paint: impl FnOnce(&mut VulkanFrame) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    // Every offscreen bake funnels through here, and each one is a render pass,
    // a submit and a fence wait. Counting and timing them here (rather than at
    // each caller) is what lets a slow frame say how much of itself was
    // re-rasterization.
    //
    // `time_bake` is `#[track_caller]`, and so is every helper in this file between
    // a widget and this line, so the site it records is the *widget's* — not
    // whichever of `bake`/`bake_content`/`bake_uncached` it came through. Dropping
    // the attribute from any one of them collapses every widget that routes through
    // it onto a single line in this file, which reads like one very busy widget.
    let _timed = crate::frame_log::time_bake();

    let (w, h) = (phys.w.max(1), phys.h.max(1));
    let mut target =
        renderer.create_buffer(NATIVE_FOURCC, Size::<i32, BufferCoord>::from((w, h)))?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
        paint(&mut frame)?;
        // No `make_offscreen_sampleable` afterwards: finishing a frame that targets
        // an offscreen leaves it sampleable, with the layout transition riding this
        // submit instead of costing a command buffer, submit and fence wait of its
        // own. A bake's GPU work is negligible, so the round trips were most of what
        // a bake cost.
        let _sync = frame.finish()?;
    }
    Ok(target)
}

/// Bake once with no caching — for widgets that re-draw every frame while
/// animating and bypass their cache (the panel workspace-dot morph, the QS pill
/// fill-fade). Same contract as [`bake`]'s `paint`.
#[track_caller]
pub fn bake_uncached(
    renderer: &mut VulkanRenderer,
    scale: f64,
    logical_size: Size<f64, Logical>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let phys = physical_size(scale, logical_size);
    bake_uncached_sized(renderer, phys, |frame| paint(frame, phys))
}

// --- H2: logical/pt drawing --------------------------------------------------------------------
//
// A widget describes its chrome in LOGICAL units and GNOME points; `TextShaper`
// and `Painter` perform the one and only `× scale` conversion internally. No
// widget draw site multiplies by scale again — the multiply that got forgotten
// (the minuscule-text bug `3c7473be`) no longer exists at any call site.

/// A text style: a GNOME point size (routed through [`crate::ui::pt_to_px`]) and
/// weight. Color is chosen at draw time (the same shaped run can be drawn in more
/// than one color), so it is not part of the style.
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    /// GNOME point size (e.g. 11 for `%heading`). NOT pixels.
    pub pt: f64,
    pub bold: bool,
}

impl TextStyle {
    pub fn new(pt: f64) -> Self {
        Self { pt, bold: false }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

/// A shaped, rasterized run ready to draw — produced by [`TextShaper::shape`] at a
/// specific scale. Opaque wrapper over the physical [`GlyphRun`]. Drawn via
/// [`Painter::text`] (ink-box anchored + clipped to the buffer),
/// [`text_clipped`](Painter::text_clipped) (a custom clip rect — a header label
/// stopping short of a right-aligned time), or [`text_px`](Painter::text_px) (a
/// hand-computed physical origin — the right-anchored panel clock).
#[derive(Clone)]
pub struct ShapedText {
    run: GlyphRun,
}

impl ShapedText {
    /// Ink bounding box, physical px: `(x, y, w, h)`.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        self.run.ink_bounds()
    }

    /// The font's line-box height (ascent + descent) in **physical** px, or the ink height for a
    /// glyph-less run.
    ///
    /// This is the height a text row must have. Sizing a row by its point size instead clips
    /// descenders — a 20pt row is ~26.7px tall while the line box is nearer 32px, so `g`, `y` and
    /// `p` lose their tails and nothing else looks wrong.
    pub fn line_box_height(&self) -> i32 {
        let (_, ascent, descent) = self.run.line_box();
        if ascent + descent <= 0. {
            let (_, _, _, ih) = self.run.ink_bounds();
            return ih;
        }
        (ascent + descent).ceil() as i32
    }

    /// Physical-px top y at which to draw this run so its font line-box (ascent+descent about the
    /// baseline) is vertically centered in a band of `height_px` — GNOME/Pango's centering, which
    /// reserves descent space so caps sit a hair higher than ink centering. Pair with an x from
    /// [`ink_bounds`](Self::ink_bounds) or an advance width. Falls back to ink centering for a
    /// glyph-less run (no line-box metrics).
    pub fn line_box_centered_y(&self, height_px: i32) -> i32 {
        let (baseline, ascent, descent) = self.run.line_box();
        if ascent + descent <= 0. {
            let (_ix, iy, _iw, ih) = self.run.ink_bounds();
            return (height_px - ih) / 2 - iy;
        }
        // Center [baseline - ascent, baseline + descent] in the band, then offset so the run-local
        // baseline lands there: top = band_center - box_height/2 - (baseline - ascent).
        let box_top = (height_px as f32 - (ascent + descent)) / 2.;
        (box_top - (baseline as f32 - ascent)).round() as i32
    }
}

/// One span of a styled paragraph: its text, family, weight, and GNOME point size.
/// The pt → physical conversion happens in [`TextShaper::paragraph`].
#[derive(Debug, Clone, Copy)]
pub struct ParagraphSpan<'a> {
    pub text: &'a str,
    pub mono: bool,
    pub bold: bool,
    /// GNOME point size for this span (spans may differ, e.g. a title vs body).
    pub pt: f64,
}

impl<'a> ParagraphSpan<'a> {
    /// A plain sans span at `pt`.
    pub fn new(text: &'a str, pt: f64) -> Self {
        Self {
            text,
            mono: false,
            bold: false,
            pt,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

/// A shaped, wrapped, multi-span paragraph — the dialog/notification text block.
/// Its ink metrics are **physical** (content-sized widgets lay out in physical px
/// directly); draw it with [`Painter::paragraph`]. Cheap to clone (the glyph
/// atlas is ref-counted), so a widget can shape into a `Vec` then move rows around.
#[derive(Clone)]
pub struct ShapedParagraph {
    run: GlyphRun,
}

impl ShapedParagraph {
    /// Ink bounding box of the whole block, physical px: `(x, y, w, h)`.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        self.run.ink_bounds()
    }

    /// Ink bounding box of span index `i` (physical px) — e.g. to draw a keycap
    /// patch behind a monospace command span.
    pub fn span_ink_bounds(&self, i: u32) -> (i32, i32, i32, i32) {
        self.run.span_ink_bounds(i)
    }
}

/// Shapes text at physical (`× scale`) pixels — the miss-only prepare phase (it
/// needs `&mut VulkanRenderer`, which the live frame holds, so shaping must happen
/// before the frame opens). Hand one to a widget's `prepare` closure.
pub struct TextShaper<'a> {
    renderer: &'a mut VulkanRenderer,
    scale: f64,
}

impl<'a> TextShaper<'a> {
    pub fn new(renderer: &'a mut VulkanRenderer, scale: f64) -> Self {
        Self { renderer, scale }
    }

    /// Shape one line. `style.pt` → logical px (`pt_to_px`) → physical px
    /// (`× scale`) — the single font-size conversion.
    pub fn shape(&mut self, text: &str, style: TextStyle) -> anyhow::Result<ShapedText> {
        let px = (crate::ui::pt_to_px(style.pt) * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_run_weighted(text, px, style.bold)?;
        Ok(ShapedText { run })
    }

    /// Shape a wrapped, center-aligned, multi-span paragraph. `wrap` is the wrap
    /// width in **logical** px; `base_pt` is the line-height reference point size.
    /// Every span's pt is converted the same way as [`Self::shape`] — no call site
    /// touches a physical font size.
    pub fn paragraph(
        &mut self,
        spans: &[ParagraphSpan],
        wrap: f64,
        base_pt: f64,
    ) -> anyhow::Result<ShapedParagraph> {
        use synoik_vk::text::{SpanFamily, TextSpan};
        let to_px = |pt: f64| (crate::ui::pt_to_px(pt) * self.scale) as f32;
        let vk_spans: Vec<TextSpan> = spans
            .iter()
            .map(|s| TextSpan {
                text: s.text,
                family: if s.mono {
                    SpanFamily::Mono
                } else {
                    SpanFamily::Sans
                },
                bold: s.bold,
                px: to_px(s.pt),
            })
            .collect();
        let wrap_px = (wrap * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_paragraph(&vk_spans, wrap_px, to_px(base_pt))?;
        Ok(ShapedParagraph { run })
    }
}

/// Horizontal placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// How [`Painter::text`] anchors a run's ink box to its `at` point.
#[derive(Debug, Clone, Copy)]
pub struct Align {
    pub h: HAlign,
    pub v: VAlign,
}

impl Align {
    /// Left edge at `at.x`, vertically centered on `at.y` (a row label).
    pub const LEFT_MIDDLE: Align = Align {
        h: HAlign::Left,
        v: VAlign::Middle,
    };
    /// Right edge at `at.x`, vertically centered on `at.y` (a right-aligned label).
    pub const RIGHT_MIDDLE: Align = Align {
        h: HAlign::Right,
        v: VAlign::Middle,
    };
    /// Centered both ways on `at`.
    pub const CENTER: Align = Align {
        h: HAlign::Center,
        v: VAlign::Middle,
    };
    /// Ink top-left corner at `at` (a run baked into an exactly-ink-sized buffer).
    pub const TOP_LEFT: Align = Align {
        h: HAlign::Left,
        v: VAlign::Top,
    };
}

/// GNOME's button families (`_buttons.scss` / `_dialogs.scss`) — the styling a
/// [`Button`] renders with. All carry bold white text; they differ in fill + radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// `.button` (`%button`) — a subtle raised gray `mix($fg,$bg,9%)`, 8px radius.
    Normal,
    /// `.modal-dialog-button` (`%dialog_button`) — translucent white, 12px radius.
    /// The neutral dialog action button (Cancel / Log Out / …); accent shows only
    /// as the focus ring, matching GNOME 50.1's end-session dialog.
    Dialog,
    /// `.button.default` (`%default_button`) — solid accent fill, 8px radius. A
    /// suggested/primary action.
    Suggested,
    /// `.destructive-action` — solid red `#c01c28`, 8px radius.
    Destructive,
}

impl ButtonStyle {
    /// Corner radius, logical px (`$base_border_radius` 8; dialog buttons `*1.5` = 12).
    pub fn radius(self) -> f64 {
        match self {
            ButtonStyle::Dialog => 12.,
            _ => 8.,
        }
    }

    /// What the focus ring is drawn from. A [`Suggested`](Self::Suggested) button is
    /// `%default_button`, whose fill *is* the accent, so its ring swaps to
    /// `$accent_borders_color` (`_drawing.scss:313-316`) rather than disappearing into it.
    fn focus_ring_base(self, accent: Rgba) -> Rgba {
        match self {
            ButtonStyle::Suggested => style::accent_borders(accent),
            _ => accent,
        }
    }

    /// Base (non-hovered) fill; `accent` is the system accent, used by [`Suggested`].
    fn bg(self, accent: Rgba) -> Rgba {
        match self {
            ButtonStyle::Normal => style::BUTTON_BG,
            ButtonStyle::Dialog => style::DIALOG_BUTTON_BG,
            ButtonStyle::Suggested => accent,
            ButtonStyle::Destructive => style::DESTRUCTIVE_BG,
        }
    }
}

/// The battery indicator's colour for a tint role.
///
/// Panel foreground indicators on our dark chrome take the *lighter* palette step, the way
/// `$privacy_indicator_color` does (`if($variant=='light', $orange_4, $orange_3)`,
/// `_panel.scss:4`) — palette step 3/4, not the `$warning_color`/`$error_color` *background*
/// tokens, which are tuned to carry white text on top of them.
pub fn battery_tint(tint: crate::system_status::BatteryTint) -> Rgba {
    use crate::system_status::BatteryTint as T;
    match tint {
        T::Normal => style::TEXT,
        // $green_4, not $green_3: the palette's brightest green shouted next to white glyphs.
        T::Charging => style::rgb8(0x2e, 0xc2, 0x7e),
        T::Low => style::rgb8(0xf6, 0xd3, 0x2d),
        T::Critical => style::rgb8(0xe0, 0x1b, 0x24),
    }
}

/// How a battery overlay glyph is drawn: the glyph, its dilated silhouette for the rim and
/// shadow, its box, and the colour role it takes.
pub struct OverlayGlyph {
    pub icon: &'static str,
    pub halo: &'static str,
    /// Logical px. Taller than the body on purpose, so the glyph breaks the battery's outline
    /// instead of sitting trapped inside the charge.
    pub px: f64,
    pub tint: crate::system_status::BatteryTint,
}

/// The glyph for a battery overlay, if it has one.
///
/// All are ours, not theme icons (`resources/icons/battery-*-symbolic.svg`) — no theme ships
/// these alone, because every themed battery bakes them into a whole battery and we draw our own
/// body.
pub fn battery_overlay_glyph(
    overlay: crate::system_status::BatteryOverlay,
) -> Option<OverlayGlyph> {
    use crate::system_status::{BatteryOverlay as O, BatteryTint as T};
    let g = |icon, halo, px, tint| {
        Some(OverlayGlyph {
            icon,
            halo,
            px,
            tint,
        })
    };
    match overlay {
        O::Bolt => g(
            "battery-bolt-symbolic",
            "battery-bolt-halo-symbolic",
            Battery::OVERLAY,
            T::Normal,
        ),
        O::Cord => g(
            "battery-cord-symbolic",
            "battery-cord-halo-symbolic",
            Battery::CORD,
            T::Normal,
        ),
        // Red, like the housing it sits in: the alert is part of the critical reading, not a
        // neutral mark on top of it. Its rim is what keeps it legible against that red.
        O::Alert => g(
            "battery-alert-symbolic",
            "battery-alert-halo-symbolic",
            Battery::OVERLAY,
            T::Critical,
        ),
        O::None => None,
    }
}

/// The three layers of a battery overlay glyph — the glyph, a grey rim, and a shadow — centred on
/// `center` (relative to `origin`), topmost first.
///
/// The rim is a dilated silhouette of the same shape drawn behind the glyph. It is a second asset
/// because a symbolic carries one colour (its alpha is the coverage), so an outline cannot be a
/// stroke on the glyph itself. Without it a white glyph over a white fill is invisible — measured
/// on the seat, where a plug over a charged battery could not be seen at all. The shadow is the
/// rim's own colour at a low alpha, a hair larger and a pixel down: at panel size it is all but
/// invisible, and it is at the *small* sizes that it earns its keep, thickening the plug's prongs
/// enough to keep them apart.
pub fn battery_overlay_elements(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    glyph: &OverlayGlyph,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Vec<TextureRenderElement<VkTexture>> {
    let tint = battery_tint(glyph.tint);
    let shadow_center = center + Point::from((0., Battery::GLYPH_SHADOW_DY));
    [
        (glyph.icon, glyph.px, tint, center),
        (glyph.halo, glyph.px, Battery::GLYPH_RIM, center),
        (
            glyph.halo,
            glyph.px * Battery::GLYPH_SHADOW_SCALE,
            Battery::GLYPH_SHADOW,
            shadow_center,
        ),
    ]
    .into_iter()
    .filter_map(|(name, px, color, c)| {
        icon_element(renderer, icons, &[name], px, scale, color, origin, c)
    })
    .collect()
}

/// The dynamic battery indicator: a rounded body with a fill bar that tracks the charge, a nub,
/// and an optional critical glyph (`docs/fork/battery-indicator-design.md`).
///
/// A divergence we chose, not a GNOME port — 50.3 draws a 16px `battery-level-*-symbolic` glyph,
/// which quantises a continuous quantity into ten pictures that differ by a few pixels of interior
/// fill. A bar reads at a glance.
///
/// Carries no knowledge of UPower: the caller resolves state to a `tint` and the flags, so this
/// stays a shape the toolkit can draw and a test can measure. The charging bolt is **not** here —
/// it is `battery-bolt-symbolic` through the icon path, composited over the body by the caller,
/// like every other glyph in the status cluster.
#[derive(Debug, Clone, Copy)]
pub struct Battery {
    /// Charge, 0..=1. Clamped when drawn.
    pub fill: f64,
    /// The shell (housing) colour. Normally the panel foreground, like the neighbouring glyphs.
    pub body_tint: Rgba,
    /// The fill bar's colour — the channel that actually carries state, so colour appears *in*
    /// the charge rather than around it.
    pub fill_tint: Rgba,
}

impl Battery {
    /// Body box. ~1.6x a `.system-status-icon`, which is what makes the bar legible at all.
    pub const BODY_W: f64 = 26.;
    pub const BODY_H: f64 = 13.;
    /// Rounded rect, not a stadium: the mockup's corners are visibly tighter than `BODY_H / 2`.
    pub const RADIUS: f64 = 4.5;
    pub const STROKE: f64 = 1.5;
    /// Fill inset from the body's *outer* edge. Measured on the seat at scale 1.25: at 2.5 the
    /// gap inside the stroke rounded to a single physical pixel and the fill read as one thick
    /// green mass welded to the shell.
    pub const INSET: f64 = 3.;
    /// Concentric with [`Self::RADIUS`], so the fill's corners follow the shell's.
    pub const FILL_RADIUS: f64 = Self::RADIUS - Self::INSET;
    pub const NUB_W: f64 = 2.5;
    pub const NUB_H: f64 = 6.;
    /// Nearly flush with the body. At 1.5 this was a whole empty physical pixel on the seat and
    /// the nub read as a detached tick floating beside the battery rather than part of it.
    pub const NUB_GAP: f64 = 0.5;

    /// The width a cluster slot must reserve — body, gap, nub. This is why the panel cluster walks
    /// per-element widths instead of `i * QS_ICON`.
    pub const WIDTH: f64 = Self::BODY_W + Self::NUB_GAP + Self::NUB_W;
    /// The height the indicator occupies; the caller centres it in the bar.
    pub const HEIGHT: f64 = Self::BODY_H;

    /// The bolt's and the alert glyph's box, and the plug's (which is a shorter shape in its
    /// viewBox, so it needs less). Both taller than the body, so they cross its outline.
    pub const OVERLAY: f64 = 19.;
    pub const CORD: f64 = 18.;
    /// The overlay rim and its shadow — see [`battery_overlay_elements`].
    ///
    /// Midway between black and the grey the *dim* parts of the neighbouring symbolic icons read
    /// as (the unlit volume wave, the empty wifi arcs — panel foreground at Adwaita's 0.35 over
    /// the plate, `#899989` on a green wallpaper): an edge that is deliberate without being the
    /// only hard outline in the cluster, which is what black at 0.75 was.
    ///
    /// Opaque rather than a translucent white, because the rim also has to separate the glyph from
    /// a *white* fill bar — white at any alpha composites to white there and the plug dissolves,
    /// which is what put a dark rim here to begin with.
    pub const GLYPH_RIM: Rgba = style::rgb8(0x44, 0x4c, 0x44);
    /// The rim's colour again, low alpha — a black shadow under a grey rim reads as dirt in the
    /// plug's prongs.
    pub const GLYPH_SHADOW: Rgba = [
        Self::GLYPH_RIM[0],
        Self::GLYPH_RIM[1],
        Self::GLYPH_RIM[2],
        0.3,
    ];
    pub const GLYPH_SHADOW_SCALE: f64 = 1.14;
    pub const GLYPH_SHADOW_DY: f64 = 1.;

    /// The fill bar's width for a charge, inside the body. Floored at the fill *diameter*: a
    /// rounded rect narrower than its own corner diameter cannot exist, so 1% must still be a
    /// visible lozenge rather than a degenerate sliver.
    pub fn fill_width(fill: f64) -> f64 {
        let inner = Self::BODY_W - 2. * Self::INSET;
        // `f64::clamp` *propagates* NaN rather than trapping it, so a non-finite percentage would
        // otherwise sail through both clamps and reach the renderer as a NaN rect. Treat it as
        // empty: the reading is wrong either way, but a lozenge is a shape and NaN is not.
        if !fill.is_finite() {
            return 2. * Self::FILL_RADIUS;
        }
        let want = inner * fill.clamp(0., 1.);
        want.clamp(2. * Self::FILL_RADIUS, inner)
    }
}

/// A clickable button: a rounded [`ButtonStyle`] fill with a centered bold-white
/// label, the toolkit's standard hover wash, and an optional inset accent focus ring.
/// The owner holds the logical `rect` (from its own layout) and the interaction flags;
/// [`Painter::button`] draws it so every button behaves identically wherever it
/// appears. The single higher-level widget in the otherwise-primitive toolkit.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub rect: Rectangle<f64, Logical>,
    pub style: ButtonStyle,
    pub hovered: bool,
    /// Keyboard-focused / default action — draws the inset accent focus ring.
    pub focused: bool,
}

impl Button {
    pub fn new(rect: Rectangle<f64, Logical>, style: ButtonStyle) -> Self {
        Self {
            rect,
            style,
            hovered: false,
            focused: false,
        }
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Hit-test a point in the button's own logical coordinate space.
    pub fn contains(&self, p: Point<f64, Logical>) -> bool {
        self.rect.contains(p)
    }
}

/// GNOME's `.icon-button` (`_buttons.scss:18-38`) — a **circular** button whose whole content is
/// one symbolic glyph.
///
/// Distinct from [`Button`], which is a rounded rect with a text label: this one has
/// `border-radius: $forced_circular_radius`, so it is a circle, and its size is derived rather than
/// laid out — icon plus padding on every side. GNOME uses it for the login screen's a11y, cancel,
/// session-list and switch-user buttons (`_login-lock.scss:35-50`), the app-folder rename pencil,
/// and the notification close buttons, which is why it is a widget: those want one hover wash, one
/// focus ring and one geometry rule between them.
///
/// The fill is a [`ButtonStyle`] like any other button; the *shape* is what this type fixes.
#[derive(Debug, Clone, Copy)]
pub struct IconButton {
    pub rect: Rectangle<f64, Logical>,
    pub icon_px: f64,
    pub bg: Rgba,
    pub hovered: bool,
    pub focused: bool,
}

impl IconButton {
    /// `$scalable_icon_size` (`_common.scss:60`) — the glyph inside, at the default font.
    pub const ICON_PX: f64 = 16.;

    /// The button's diameter for a glyph of `icon_px` with `pad` on every side.
    ///
    /// `.icon-button`'s own padding is `$scaled_padding * 2` (12px); rules that extend it routinely
    /// override that, so it is a parameter rather than a constant — the lock screen's is
    /// `to_em(16px)` (`_login-lock.scss:44`).
    pub fn diameter(icon_px: f64, pad: f64) -> f64 {
        icon_px + pad * 2.
    }

    pub fn new(rect: Rectangle<f64, Logical>, icon_px: f64, bg: Rgba) -> Self {
        Self {
            rect,
            icon_px,
            bg,
            hovered: false,
            focused: false,
        }
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// The glyph's centre, in the same space as `rect`.
    pub fn icon_centre(&self) -> Point<f64, Logical> {
        Point::from((
            self.rect.loc.x + self.rect.size.w / 2.,
            self.rect.loc.y + self.rect.size.h / 2.,
        ))
    }

    /// Hit-test a point in the button's own logical space — **round**, not the bounding box.
    ///
    /// A circle inscribed in its box leaves a quarter of that box outside it, and a click landing
    /// in a corner an inch from any drawn pixel reads as a misfire, not as a generous target.
    pub fn contains(&self, p: Point<f64, Logical>) -> bool {
        let c = self.icon_centre();
        let r = self.rect.size.w.min(self.rect.size.h) / 2.;
        let (dx, dy) = (p.x - c.x, p.y - c.y);
        dx * dx + dy * dy <= r * r
    }
}

/// GNOME's `CheckBox` (`js/ui/checkBox.js`) — a tick box plus a label, the whole row clickable.
///
/// The glyph *frame* is paint ([`Painter::check_box`]); the tick itself is an icon element the
/// caller composites at [`CheckBox::glyph_centre`], the same split [`menu::Menu::ornaments`] uses,
/// because icons are textures rather than paint verbs. The tick is only drawn when `checked` —
/// unchecked is `color: transparent` in the SCSS (`_check-box.scss:21`), i.e. the glyph is still
/// laid out, just invisible; not drawing it at all is the same picture.
///
/// Sizes come from `_check-box.scss` via `gnome-style-reference.md` §check-box. `rect` is the whole
/// row (frame + spacing + label) so the hit target matches GNOME's, where the CheckBox *is* the
/// St.Button and clicking the label toggles it.
#[derive(Debug, Clone, Copy)]
pub struct CheckBox {
    /// The whole clickable row, in the owner's logical space.
    pub rect: Rectangle<f64, Logical>,
    pub checked: bool,
    pub hovered: bool,
    /// Keyboard-focused — draws the focus ring around the frame (not around the row).
    pub focused: bool,
    /// Pressed (pointer down on it).
    pub active: bool,
}

impl CheckBox {
    /// `icon-size: 14px` (`_check-box.scss:18`).
    pub const ICON_PX: f64 = 14.;
    /// `StIcon { padding: 1px; border: 2px }` — the frame is the glyph plus both.
    const GLYPH_PAD: f64 = 1.;
    const BORDER: f64 = 2.;
    /// `border-radius: 6px` on the glyph frame (`:22`).
    const RADIUS: f64 = 6.;
    /// `StBin { border-radius: 7px; padding: 2px }` — the focus ring's box, one step out
    /// from the frame (`:6-8`).
    const FOCUS_PAD: f64 = 2.;
    const FOCUS_RADIUS: f64 = 7.;

    /// The glyph frame's side: 14px icon + 1px padding + 2px border, per side.
    pub fn frame_px() -> f64 {
        Self::ICON_PX + (Self::GLYPH_PAD + Self::BORDER) * 2.
    }

    /// `StBoxLayout { spacing: .8em }` (`:4`) between the frame and the label. `em` resolves
    /// against the row's own font size, so it is a function of the label's point size rather
    /// than a constant.
    pub fn label_gap(label_pt: f64) -> f64 {
        super::pt_to_px(label_pt) * 0.8
    }

    /// Where the label starts, given the row's origin: past the frame and the gap.
    pub fn label_x(&self, label_pt: f64) -> f64 {
        self.rect.loc.x + Self::frame_px() + Self::label_gap(label_pt)
    }

    pub fn new(rect: Rectangle<f64, Logical>, checked: bool) -> Self {
        Self {
            rect,
            checked,
            hovered: false,
            focused: false,
            active: false,
        }
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// The glyph frame, vertically centred on the row (`y_align: START` on the bin but the label
    /// is `CENTER`, and with a single-line label the two agree).
    pub fn frame_rect(&self) -> Rectangle<f64, Logical> {
        let side = Self::frame_px();
        Rectangle::new(
            Point::from((
                self.rect.loc.x,
                self.rect.loc.y + (self.rect.size.h - side) / 2.,
            )),
            Size::from((side, side)),
        )
    }

    /// The tick's centre, for the caller to composite `check-symbolic` at [`Self::ICON_PX`].
    pub fn glyph_centre(&self) -> Point<f64, Logical> {
        let f = self.frame_rect();
        Point::from((f.loc.x + f.size.w / 2., f.loc.y + f.size.h / 2.))
    }

    /// Hit-test in the row's own logical space — the **whole row**, label included.
    pub fn contains(&self, p: Point<f64, Logical>) -> bool {
        self.rect.contains(p)
    }
}

impl Painter<'_, '_, '_> {
    /// Draw a [`CheckBox`]'s frame: the border (or the accent fill when checked) and the focus
    /// ring. The tick glyph and the label are composited by the caller.
    ///
    /// Unchecked the frame is a 2px border and nothing else — `background-color` is unset, so the
    /// dialog behind shows through. Checked it is a solid accent fill with **no** border
    /// (`border-color: transparent`, `:37`): filling *and* stroking would thicken the box by 2px
    /// the moment it is ticked.
    pub fn check_box(&mut self, c: &CheckBox, accent: Rgba) -> anyhow::Result<()> {
        let frame = c.frame_rect();

        if c.checked {
            // `:checked { background-color: -st-accent-color }`, lightened 5% on hover and
            // darkened 7% on press (`:40-47`).
            let bg = match (c.active, c.hovered) {
                (true, _) => style::darken(accent, 0.07),
                (false, true) => style::lighten(accent, 0.05),
                (false, false) => accent,
            };
            self.fill_rounded(frame, CheckBox::RADIUS, bg)?;
        } else {
            // `border: 2px solid transparentize(white, .85)`, tightening to .8 on hover and
            // .7 on press (`:23-31`).
            let alpha = match (c.active, c.hovered) {
                (true, _) => 0.3,
                (false, true) => 0.2,
                (false, false) => 0.15,
            };
            self.stroke_rounded(
                frame,
                CheckBox::RADIUS,
                CheckBox::BORDER,
                [1., 1., 1., alpha],
            )?;
        }

        if c.focused {
            // `:focus StBin { box-shadow: inset 0 0 0 2px accent @35% }` — on the *bin*, which is
            // the frame grown by its 2px padding, at the bin's own 7px radius.
            let pad = CheckBox::FOCUS_PAD;
            let ring = Rectangle::new(
                Point::from((frame.loc.x - pad, frame.loc.y - pad)),
                Size::from((frame.size.w + pad * 2., frame.size.h + pad * 2.)),
            );
            let color = [accent[0], accent[1], accent[2], 0.35];
            self.stroke_rounded(ring, CheckBox::FOCUS_RADIUS, 2., color)?;
        }
        Ok(())
    }
}

impl Painter<'_, '_, '_> {
    /// Paint an [`IconButton`]'s chrome — the circle, its hover wash and its focus ring. The glyph
    /// itself is composited by the caller (icons are textures, not paint ops); use
    /// [`IconButton::icon_centre`] to place it.
    /// Draw an [`IconLabelButton`]'s fill — `%osd_button_flat` with a checked state
    /// (`_screenshot.scss:26-38`). The caller composites the glyph and the caption on top.
    ///
    /// The base fill is [`style::OSD_BG`], not transparent: see [`style::OSD_FLAT_HOVER`] for why
    /// a "flat" OSD button is the panel's own colour rather than a hole in it.
    pub fn icon_label_button(&mut self, b: &IconLabelButton, accent: Rgba) -> anyhow::Result<()> {
        // `:active` outranks `:checked` outranks `:hover` — a press must read as a press even on
        // the button that is already selected.
        let bg = match (b.active, b.checked, b.hovered) {
            (true, _, _) => style::OSD_FLAT_ACTIVE,
            (false, true, _) => style::OSD_FLAT_CHECKED,
            (false, false, true) => style::OSD_FLAT_HOVER,
            (false, false, false) => style::OSD_BG,
        };
        self.fill_rounded(b.rect, IconLabelButton::RADIUS, bg)?;
        if b.focused {
            let ring = [accent[0], accent[1], accent[2], 0.8];
            self.stroke_rounded(b.rect, IconLabelButton::RADIUS, 2., ring)?;
        }
        Ok(())
    }

    /// Draw a [`Segmented`] container and its segment fills (`_screenshot.scss:83-110`).
    ///
    /// `checked` is the index that is selected — it inverts to a solid [`style::OSD_FG`] pill, and
    /// the caller must tint that segment's glyph [`style::OSD_BG`] to match (`:105`). `hovered` is
    /// the segment under the pointer, if any.
    pub fn segmented(
        &mut self,
        rect: Rectangle<f64, Logical>,
        segments: usize,
        checked: usize,
        hovered: Option<usize>,
    ) -> anyhow::Result<()> {
        // `background-color: transparentize($osd_fg_color, 0.9)` — white at 10%.
        let radius = rect.size.h / 2.;
        self.fill_rounded(rect, radius, [1., 1., 1., 0.1])?;

        for i in 0..segments {
            let seg = Segmented::segment_rect(rect, i);
            let seg_radius = seg.size.h / 2.;
            if i == checked {
                self.fill_rounded(seg, seg_radius, style::OSD_FG)?;
            } else if hovered == Some(i) {
                // `:hover { background-color: transparentize($osd_fg_color, 0.8) }` — white at 20%.
                self.fill_rounded(seg, seg_radius, [1., 1., 1., 0.2])?;
            }
        }
        Ok(())
    }

    pub fn icon_button(&mut self, b: &IconButton, accent: Rgba) -> anyhow::Result<()> {
        // `border-radius: $forced_circular_radius` on a square box.
        let radius = b.rect.size.w.min(b.rect.size.h) / 2.;
        self.fill_rounded(b.rect, radius, b.bg)?;
        if b.hovered {
            self.fill_rounded(b.rect, radius, style::HOVER_WASH)?;
        }
        if b.focused {
            // An icon button's disc is the neutral button fill, never the accent, so the plain
            // accent ring stands out on it.
            self.focus_ring(b.rect, radius, accent)?;
        }
        Ok(())
    }
}

/// GNOME's `.screenshot-ui-type-button` (`_screenshot.scss:26-38`) — a flat OSD button whose
/// content is a large symbolic glyph stacked over a caption.
///
/// Distinct from [`Button`] (rounded rect, one centred label) and [`IconButton`] (circle, one
/// glyph): this one is a *column*, so its height comes from the icon plus the caption plus the
/// spacing between them, and it carries a **checked** state — which is what earns it a place here,
/// since [`ButtonStyle`] has no checked fill.
///
/// The glyph and the label are composited by the caller (icons are render elements, not paint
/// verbs); this type owns the geometry and [`Painter::icon_label_button`] owns the fill.
///
/// GNOME defines its `IconLabelButton` inside `screenshot.js:53-73` and uses it only for the three
/// screenshot type buttons — this is not a shape that recurs upstream. It lives in the toolkit
/// because a control with hover, checked and focus states must not be hand-drawn in a caller.
#[derive(Debug, Clone, Copy)]
pub struct IconLabelButton {
    pub rect: Rectangle<f64, Logical>,
    pub hovered: bool,
    pub checked: bool,
    /// Held down — `%osd_button_flat`'s `:active` step (`_drawing.scss:182`), a lighter fill than
    /// hover. Distinct from `checked`: this one lasts only as long as the press.
    pub active: bool,
    pub focused: bool,
}

impl IconLabelButton {
    /// `> StIcon { icon-size: $large_icon_size }` (`_screenshot.scss:36`).
    pub const ICON_PX: f64 = 32.;
    /// `min-width: 48px` (`_screenshot.scss:28`).
    ///
    /// Inert at the default glyph size and kept only so the rule is represented: a 32px glyph
    /// inside 18px of horizontal padding is already 68px, so nothing this button can contain
    /// brings it under 48. Do not "fix" the floor into a width.
    pub const MIN_WIDTH: f64 = 48.;
    /// `padding: $base_padding * 2 $base_padding * 3` (`_screenshot.scss:29`) — 12px vertical,
    /// 18px horizontal.
    pub const PAD_Y: f64 = 12.;
    pub const PAD_X: f64 = 18.;
    /// `.icon-label-button-container { spacing: $scaled_padding }` (`_screenshot.scss:33`).
    pub const SPACING: f64 = 6.;
    /// `border-radius: $screenshot_ui_panel_border_radius - $screenshot_ui_panel_padding`
    /// (`_screenshot.scss:30`) = 32 - 18.
    pub const RADIUS: f64 = 14.;
    /// `%caption` — the container's font (`_screenshot.scss:32`).
    pub const LABEL_PT: f64 = 9.;

    /// The button's logical size for a label of `label_w` × `label_h`, honouring `MIN_WIDTH`.
    pub fn size(label_w: f64, label_h: f64) -> Size<f64, Logical> {
        let w = (label_w.max(Self::ICON_PX) + Self::PAD_X * 2.).max(Self::MIN_WIDTH);
        let h = Self::ICON_PX + Self::SPACING + label_h + Self::PAD_Y * 2.;
        Size::from((w, h))
    }

    pub fn new(rect: Rectangle<f64, Logical>) -> Self {
        Self {
            rect,
            hovered: false,
            checked: false,
            active: false,
            focused: false,
        }
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Centre of the glyph — horizontally centred, sitting on top of the caption.
    pub fn icon_centre(&self) -> Point<f64, Logical> {
        Point::from((
            self.rect.loc.x + self.rect.size.w / 2.,
            self.rect.loc.y + Self::PAD_Y + Self::ICON_PX / 2.,
        ))
    }

    /// Centre of the caption line, below the glyph.
    pub fn label_centre(&self, label_h: f64) -> Point<f64, Logical> {
        Point::from((
            self.rect.loc.x + self.rect.size.w / 2.,
            self.rect.loc.y + Self::PAD_Y + Self::ICON_PX + Self::SPACING + label_h / 2.,
        ))
    }

    pub fn contains(&self, p: Point<f64, Logical>) -> bool {
        self.rect.contains(p)
    }
}

/// GNOME's `.screenshot-ui-shot-cast-container` (`_screenshot.scss:83-110`) — a pill of
/// translucent white holding two or more segments, of which exactly one is checked.
///
/// A geometry-and-paint primitive like [`Switch`]: the owner holds which segment is selected and
/// where the pill sits, and composites the glyphs; [`Painter::segmented`] draws the container and
/// the segment fills so the selected-inverts-to-solid behaviour is written once.
///
/// Unique in 50.3 (the quick-settings split toggle is a different construct,
/// `_quick-settings.scss:57-77`), so it is here for the composition, not because it recurs.
pub struct Segmented;

impl Segmented {
    /// `padding: $base_padding * 0.5` and `spacing: $base_padding * 0.5`
    /// (`_screenshot.scss:86-87`).
    pub const PAD: f64 = 3.;
    pub const SPACING: f64 = 3.;
    /// `padding: $base_padding $base_padding * 2` (`_screenshot.scss:96`) — 6px vertical,
    /// 12px horizontal, around a `$base_icon_size` glyph (`:102`).
    pub const SEG_PAD_Y: f64 = 6.;
    pub const SEG_PAD_X: f64 = 12.;
    pub const ICON_PX: f64 = 16.;

    /// One segment's logical size: the glyph plus its padding.
    pub fn segment_size() -> Size<f64, Logical> {
        Size::from((
            Self::ICON_PX + Self::SEG_PAD_X * 2.,
            Self::ICON_PX + Self::SEG_PAD_Y * 2.,
        ))
    }

    /// The container's logical size for `n` segments.
    pub fn size(n: usize) -> Size<f64, Logical> {
        let seg = Self::segment_size();
        let n_f = n as f64;
        Size::from((
            seg.w * n_f + Self::SPACING * (n_f - 1.).max(0.) + Self::PAD * 2.,
            seg.h + Self::PAD * 2.,
        ))
    }

    /// Segment `i`'s rect inside a container placed at `rect`.
    pub fn segment_rect(rect: Rectangle<f64, Logical>, i: usize) -> Rectangle<f64, Logical> {
        let seg = Self::segment_size();
        Rectangle::new(
            Point::from((
                rect.loc.x + Self::PAD + (seg.w + Self::SPACING) * i as f64,
                rect.loc.y + Self::PAD,
            )),
            seg,
        )
    }
}

/// GNOME's `.toggle-switch` (`_switches.scss:6-52`) — the pill-and-handle control a
/// `PopupSwitchMenuItem` puts at the right end of a menu row (`popupMenu.js:501-524`).
///
/// A geometry-and-paint primitive, not a stateful widget: the owner holds the on/off
/// state (it lives in a settings model) and the rect (it comes from the owner's row
/// layout), and calls [`Painter::toggle_switch`] so every switch looks the same
/// wherever one appears.
///
/// No slide animation yet: the only switch so far lives in the accessibility menu,
/// which closes on click (`popupMenu.js:539-550`), so the travel is never seen.
pub struct Switch;

impl Switch {
    /// `$switch_width` (`_switches.scss:3`).
    pub const WIDTH: f64 = 46.;
    /// `$switch_handle_size` (`_switches.scss:4`).
    pub const HANDLE: f64 = 20.;
    /// `.handle { margin: 3px }` (`_switches.scss:31`) — so the track is
    /// `HANDLE + 2 * MARGIN` = 26px tall.
    pub const MARGIN: f64 = 3.;
    /// Track height: the handle plus its margin on both sides.
    pub const HEIGHT: f64 = Self::HANDLE + 2. * Self::MARGIN;

    /// The control's logical size.
    pub fn size() -> Size<f64, Logical> {
        Size::from((Self::WIDTH, Self::HEIGHT))
    }
}

/// Off-state track fill: `transparentize(white, .85)` (`_switches.scss:19`).
const SWITCH_OFF_BG: Rgba = [1., 1., 1., 0.15];
/// Off-state track fill while hovered: `transparentize(white, .8)` (`_switches.scss:22`).
const SWITCH_OFF_BG_HOVER: Rgba = [1., 1., 1., 0.2];
/// Handle fill when off: `mix(white, $bg_color, 80%)` over `$bg_color` #36363a
/// (`_switches.scss:35`).
const SWITCH_HANDLE_OFF: Rgba = [0.96, 0.96, 0.96, 1.];
/// Handle fill when on: plain white (`_switches.scss:50`).
const SWITCH_HANDLE_ON: Rgba = [1., 1., 1., 1.];
/// `box-shadow: 0 2px 4px transparentize(black, .8)` under the handle
/// (`_switches.scss:36`), approximated as a single offset dark disc behind it.
const SWITCH_HANDLE_SHADOW: Rgba = [0., 0., 0., 0.2];

/// GNOME's shared `BarLevel` (`js/ui/barLevel.js`) — the rounded progress bar the
/// OSD shows under its icon, and the same drawing the quick-settings sliders use.
///
/// A geometry-and-paint primitive like [`Switch`]: the owner holds the value and the
/// rect and calls [`Painter::bar_level`]. The metrics below are the `.level` node
/// inside `.osd-window` (`_osd.scss:22-34`); the colors are per-call
/// ([`BarLevelStyle`]) because each host node re-declares them.
pub struct BarLevel;

impl BarLevel {
    /// `-barlevel-height: $osd_levelbar_height` (`_osd.scss:3,26`).
    pub const HEIGHT: f64 = 6.;
    /// `min-width: 160px` (`_osd.scss:25`).
    pub const MIN_WIDTH: f64 = 160.;
    /// `-barlevel-overdrive-separator-width: $base_padding * 0.5` (`_osd.scss:31`).
    pub const OVERDRIVE_SEPARATOR: f64 = 3.;
}

/// The four theme colors a [`BarLevel`] draws with (`barLevel.js:110-113,155`).
#[derive(Debug, Clone, Copy)]
pub struct BarLevelStyle {
    /// `-barlevel-background-color` — the unfilled track.
    pub track: Rgba,
    /// `-barlevel-active-background-color` — the filled part below `overdrive_start`.
    pub fill: Rgba,
    /// `-barlevel-overdrive-color` — the filled part above it.
    pub overdrive: Rgba,
    /// The node's foreground color, which is what the separator is drawn in while
    /// the value has not reached overdrive (`barLevel.js:215-218`).
    pub separator: Rgba,
}

/// A `%card`-styled `St.Button` — the dateMenu's launcher cards
/// (`.events-button` / `.world-clocks-button` / `.weather-button`, all
/// `@extend %card`, `_calendar.scss:153-157`): a rounded [`style::CARD_BG`] card
/// that lightens on hover (`%card:hover`, [`style::HOVER_WASH`]) and launches an
/// app on click. This owns only the hover state plus the two easy-to-get-wrong
/// shared bits — the texture-cache revision packing and the hover-wash paint — so
/// each section keeps its own content bake and geometry.
#[derive(Default)]
pub struct CardButton {
    hovered: bool,
    focused: bool,
}

impl CardButton {
    pub fn hovered(&self) -> bool {
        self.hovered
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Set the keyboard-focus state; returns whether it changed.
    pub fn set_focused(&mut self, focused: bool) -> bool {
        let changed = self.focused != focused;
        self.focused = focused;
        changed
    }

    /// Set the hover state from whether the pointer is over the card; returns
    /// whether it changed (so the caller can request a redraw / re-bake).
    pub fn set_hovered(&mut self, over: bool) -> bool {
        let changed = self.hovered != over;
        self.hovered = over;
        changed
    }

    /// Pack a texture-cache revision: the content revision in the low 30 bits, the focus bit at
    /// 30, the hover bit at 31, and a physical clip-height key in the high 32 — so a re-cap OR a
    /// hover OR a focus change re-bakes with/without its wash and ring (mirrors `bg_texture`).
    pub fn revision(&self, content_rev: u64, height_key: u64) -> u64 {
        (content_rev & 0x3FFF_FFFF)
            | ((self.focused as u64) << 30)
            | ((self.hovered as u64) << 31)
            | (height_key << 32)
    }

    /// Paint the `%card:hover` lighten wash over the card when `hovered` — call
    /// right after filling the card background. Takes the bool explicitly (not
    /// `&self`) because the caller captures it into the `move` bake closure.
    pub fn paint_hover(
        painter: &mut Painter,
        hovered: bool,
        card: Rectangle<f64, Logical>,
        radius: f64,
    ) -> anyhow::Result<()> {
        if hovered {
            painter.fill_rounded(card, radius, style::HOVER_WASH)?;
        }
        Ok(())
    }

    /// Paint the keyboard-focus ring over the card when `focused` — call right after
    /// [`paint_hover`](Self::paint_hover). Takes the bool explicitly for the same reason.
    pub fn paint_focus(
        painter: &mut Painter,
        focused: bool,
        card: Rectangle<f64, Logical>,
        radius: f64,
        accent: Rgba,
    ) -> anyhow::Result<()> {
        if focused {
            painter.focus_ring(card, radius, accent)?;
        }
        Ok(())
    }
}

/// A scale-correct drawing surface over a bound [`VulkanFrame`]. Every verb takes
/// **logical** coordinates/sizes (and points, for text); the single `× scale`
/// conversion lives here. Construct one inside a [`bake`] `paint` closure.
/// An edge of a box — `St.Side` (`st-types.h`), in its order, so a port reads like the JS it came
/// from. Today only [`Painter::triangle`] takes one, naming the edge an arrow's apex points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Top = 0,
    Right = 1,
    Bottom = 2,
    Left = 3,
}

pub struct Painter<'a, 'frame, 'buffer> {
    frame: &'a mut VulkanFrame<'frame, 'buffer>,
    scale: f64,
    full: Rectangle<i32, Physical>,
}

impl<'a, 'frame, 'buffer> Painter<'a, 'frame, 'buffer> {
    /// `phys` is the full baked-buffer size (as handed to the `paint` closure); it
    /// scopes every draw's damage.
    pub fn new(
        frame: &'a mut VulkanFrame<'frame, 'buffer>,
        scale: f64,
        phys: Size<i32, Physical>,
    ) -> Self {
        Self {
            frame,
            scale,
            full: Rectangle::from_size(phys),
        }
    }

    fn px(&self, v: f64) -> i32 {
        to_physical_precise_round::<i32>(self.scale, v)
    }

    fn rect_px(&self, r: Rectangle<f64, Logical>) -> Rectangle<i32, Physical> {
        Rectangle::new(
            Point::from((self.px(r.loc.x), self.px(r.loc.y))),
            Size::from((self.px(r.size.w), self.px(r.size.h))),
        )
    }

    /// Damage covering all of `dst` — what a `Painter` draw always wants, since it paints the
    /// whole shape and lets the buffer bound it.
    ///
    /// `VulkanFrame`'s damage argument is **element-local** (smithay's convention): it is clipped
    /// to `dst`'s size and only then translated by `dst`'s origin. A `Painter` states its clip in
    /// *buffer* coordinates, so passing `self.full` reads as "the whole buffer" — which coincides
    /// with element-local coverage only while `dst` starts at or after the buffer's origin. A rect
    /// that runs past the **top or left** edge — which is exactly how the toolkit expresses a
    /// per-corner `border-radius`, letting the corners that should stay square fall outside — has
    /// its scissor silently shrunk by the overflow, so a shape hanging 52 px off the top-left
    /// drew into a 6 px sliver. Cover the element and let `damage_scissors` clamp to the
    /// framebuffer, which is the clip that was meant all along.
    fn damage_of(dst: Rectangle<i32, Physical>) -> [Rectangle<i32, Physical>; 1] {
        [Rectangle::from_size(dst.size)]
    }

    /// Clear the whole buffer to `color` (a transparent clear for rounded popovers,
    /// a border color for square dialogs).
    pub fn clear(&mut self, color: Rgba) -> anyhow::Result<()> {
        // A clear writes its value into the buffer verbatim — no blend — so it has to arrive in the
        // buffer's own (premultiplied) convention, not the toolkit's straight one. Every clear
        // color in the tree today is opaque or fully transparent, where the two agree; this keeps a
        // future translucent clear from silently storing straight alpha.
        self.frame.clear(
            Color32F::from(premultiply(color)),
            &Self::damage_of(self.full),
        )?;
        Ok(())
    }

    /// Fill the whole buffer with `color`, corners cut by `radius` (logical). For a
    /// card whose size is content-derived (a dialog/notification), where the entire
    /// baked buffer *is* the rounded card.
    pub fn fill_rounded_full(&mut self, radius: f64, color: Rgba) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        self.frame
            .render_rounded_rect(color, r, self.full, &Self::damage_of(self.full))?;
        Ok(())
    }

    /// Stroke the whole buffer's edge: an inset ring of `width` logical px, corners cut by
    /// `radius` (logical). The stroke counterpart to [`fill_rounded_full`](Self::fill_rounded_full)
    /// — a 1px border on a card whose buffer *is* the rounded surface (an OSD panel).
    pub fn stroke_rounded_full(
        &mut self,
        radius: f64,
        width: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let w = (width * self.scale) as f32;
        self.frame
            .stroke_rounded_rect(color, r, w, self.full, &Self::damage_of(self.full))?;
        Ok(())
    }

    /// Stroke `rect` (logical) with `color`: an inset ring of `width` logical px along the inside
    /// of the edge, corners cut by `radius` (logical; inner corners concentric). A focus ring or
    /// outline — the stroke counterpart to [`fill_rounded`](Self::fill_rounded).
    pub fn stroke_rounded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        width: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let w = (width * self.scale) as f32;
        let dst = self.rect_px(rect);
        self.frame
            .stroke_rounded_rect(color, r, w, dst, &Self::damage_of(dst))?;
        Ok(())
    }

    /// Fill the isoceles triangle inscribed in `rect` (logical) with `color`: its base spans one
    /// edge, its apex the midpoint of the opposite one, `side` naming the edge it points at.
    ///
    /// GNOME's `SwitcherPopup.drawArrow` (`js/ui/switcherPopup.js:661-704`) — the app switcher's
    /// multi-window chevron and the switcher list's scroll arrows. That function strokes the path
    /// in `border-color` and then fills it in `color`; `.switcher-arrow`
    /// (`_switcher-popup.scss:62-70`) sets both to the same value in both states, so one fill is
    /// exact. If a caller ever needs the two to differ, this grows a stroke arm rather than the
    /// caller painting a second triangle on top.
    pub fn triangle(
        &mut self,
        rect: Rectangle<f64, Logical>,
        side: Side,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let dst = self.rect_px(rect);
        self.frame
            .render_triangle(color, side as u8, dst, &Self::damage_of(dst))?;
        Ok(())
    }

    /// Fill `rect` (logical) with `color`, corners cut by `radius` (logical; 0 = a
    /// plain rectangle, e.g. a separator rule).
    pub fn fill_rounded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let dst = self.rect_px(rect);
        self.frame
            .render_rounded_rect(color, r, dst, &Self::damage_of(dst))?;
        Ok(())
    }

    /// [`fill_rounded`](Self::fill_rounded) with a horizontal alpha ramp: full `color` at
    /// `from`, transparent at `to`, both fractions of `rect`'s width (0 = its left edge) and in
    /// either order. GNOME's `background-gradient-direction: horizontal` where the two stops
    /// share an RGB and differ only in alpha, which is every gradient in the theme so far.
    ///
    /// Rounding is all four corners, as everywhere else. GNOME's per-corner `border-radius`
    /// (the page hints round only their inner pair) is expressed by letting the rect run past
    /// the bake buffer on the side that should stay square — the corners fall outside and are
    /// clipped, which is a real rounded rect rather than a painted-on curve.
    pub fn fill_rounded_faded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        color: Rgba,
        from: f64,
        to: f64,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let dst = self.rect_px(rect);
        self.frame.render_rounded_rect_faded(
            color,
            r,
            dst,
            &Self::damage_of(dst),
            (from as f32, to as f32),
        )?;
        Ok(())
    }

    /// Draw an [`AppIcon`] tile's **state** layer (not the icon pixels — those ride
    /// on top as an [`app_icon_element`]). Normal is a no-op: a flat `.overview-icon`
    /// tile shares its parent's background (`_drawing.scss:175-177`), so nothing is
    /// drawn. Hovered fills the tile with `hover_bg`, the surface-specific hover color
    /// (GNOME's flat+`always_dark` `st-lighten`, `_drawing.scss:186-189,270-274` — the
    /// lighten *direction* is the caller's, read from the SCSS).
    pub fn app_tile(&mut self, tile: &AppIcon, hover_bg: Rgba) -> anyhow::Result<()> {
        if tile.hovered {
            self.fill_rounded(tile.rect, tile.radius, hover_bg)?;
        }
        Ok(())
    }

    /// Paint one labelled `.overview-tile` into a card bake: its selection/hover wash
    /// (when `active`) and its caption. `rel` is the tile box relative to the bake
    /// origin; the icon pixels are composited separately on top as an
    /// [`app_icon_element`]. Shared by the search results and the app grid.
    pub fn labelled_tile(
        &mut self,
        rel: Rectangle<f64, Logical>,
        label: &[ShapedText],
        metrics: &TileMetrics,
        active: bool,
        text_color: Rgba,
    ) -> anyhow::Result<()> {
        if active {
            self.app_tile(
                &AppIcon {
                    rect: rel,
                    hovered: true,
                    radius: metrics.radius,
                },
                style::HOVER_WASH,
            )?;
        }
        // Label centered under the icon. A second line goes below the tile box, so the
        // clip is the tile widened downward by the lines past the first — clipping to
        // the box itself would cut a two-line caption in half.
        let lx = rel.loc.x + rel.size.w / 2.;
        let ly = rel.loc.y + metrics.pad + metrics.icon_px + metrics.label_gap;
        let extra = metrics.label_h * (label.len().max(1) as f64 - 1.);
        let clip = Rectangle::new(rel.loc, Size::from((rel.size.w, rel.size.h + extra)));
        for (i, line) in label.iter().enumerate() {
            self.text_band(
                line,
                lx,
                HAlign::Center,
                ly + i as f64 * metrics.label_h,
                metrics.label_h,
                text_color,
                clip,
            )?;
        }
        Ok(())
    }

    /// Draw a tile caption — one band of `line_h` per line, each centered in a box
    /// `w` wide starting at the bake's origin. The lines come from
    /// [`tile_label_lines`]; a collapsed caption is one of them, an expanded one is
    /// several. Meant for a bake sized exactly `w × lines.len()*line_h`, which is why
    /// it anchors at the origin rather than taking a tile rect.
    pub fn caption(
        &mut self,
        lines: &[ShapedText],
        w: f64,
        line_h: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        for (i, line) in lines.iter().enumerate() {
            let top = i as f64 * line_h;
            let band = Rectangle::new(Point::from((0., top)), Size::from((w, line_h)));
            self.text_band(line, w / 2., HAlign::Center, top, line_h, color, band)?;
        }
        Ok(())
    }

    /// Draw GNOME's `box-shadow`: a gaussian-blurred rounded rect behind a card.
    /// `rect`/`radius` are the casting box (logical); `blur` is the CSS blur radius
    /// (logical px; the gaussian σ = blur/2); `offset` shifts the shadow (logical —
    /// GNOME's panel shadows are `0 <dy>`); `color` is straight-alpha (premultiplied
    /// downstream). Draw this BEFORE the card fill so the card sits on top. The fringe
    /// bleeds ~`blur`·1.5 (3σ) beyond `rect` + `offset`, so the bake buffer must carry
    /// that much transparent padding around the card or the shadow clips at the edge
    /// (the OSD-panel callers size the buffer for it).
    pub fn drop_shadow(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        blur: f64,
        offset: (f64, f64),
        color: Rgba,
    ) -> anyhow::Result<()> {
        let sigma = (blur * self.scale / 2.) as f32;
        let mut box_dst = self.rect_px(rect);
        box_dst.loc.x += self.px(offset.0);
        box_dst.loc.y += self.px(offset.1);
        let r = (radius * self.scale) as f32;
        self.frame.render_drop_shadow(
            color,
            r,
            sigma,
            self.scale as f32,
            box_dst,
            &Self::damage_of(box_dst),
        )?;
        Ok(())
    }

    /// Draw a shaped run, anchoring its ink box to `at` (logical) per `align`,
    /// tinted `color`. Clipped to the whole buffer.
    pub fn text(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.render_run(shaped, at, align, color, self.full)
    }

    /// Like [`text`](Self::text), but clips the glyphs to `clip` (logical) instead of
    /// the whole buffer — for a run that must not overrun a sibling (a header label
    /// stopping short of a right-aligned time, a body column, a button-width label).
    /// This is what lets content-driven widgets draw every run through the `Painter`
    /// rather than reaching for `VulkanFrame::render_glyphs`.
    pub fn text_clipped(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
        clip: Rectangle<f64, Logical>,
    ) -> anyhow::Result<()> {
        let clip = self.rect_px(clip);
        self.render_run(shaped, at, align, color, clip)
    }

    /// Shared origin math for [`text`](Self::text)/[`text_clipped`](Self::text_clipped):
    /// place the run's ink box at `at` per `align`, then draw clipped to `clip` (physical),
    /// damaging the whole buffer.
    fn render_run(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
        clip: Rectangle<i32, Physical>,
    ) -> anyhow::Result<()> {
        let (ix, iy, iw, ih) = shaped.run.ink_bounds();
        let ax = self.px(at.x);
        let ay = self.px(at.y);
        let ox = match align.h {
            HAlign::Left => ax - ix,
            HAlign::Center => ax - ix - iw / 2,
            HAlign::Right => ax - ix - iw,
        };
        let oy = match align.v {
            VAlign::Top => ay - iy,
            VAlign::Middle => ay - iy - ih / 2,
            VAlign::Bottom => ay - iy - ih,
        };
        self.frame.render_glyphs(
            &shaped.run,
            Point::from((ox, oy)),
            color,
            clip,
            &Self::damage_of(clip),
        )?;
        Ok(())
    }

    /// Draw `shaped` horizontally per `halign` at logical `x`, vertically centered by
    /// its font **line box** within the band `[band_top, band_top + band_h]` (logical),
    /// clipped to `clip` (logical). Unlike [`text`](Self::text)'s ink-box centering,
    /// line-box centering keeps baselines aligned across side-by-side cells regardless
    /// of which strings carry descenders (the grid-row idiom, as the panel does for its
    /// clock/labels) — see [`ShapedText::line_box_centered_y`].
    #[allow(clippy::too_many_arguments)]
    pub fn text_band(
        &mut self,
        shaped: &ShapedText,
        x: f64,
        halign: HAlign,
        band_top: f64,
        band_h: f64,
        color: Rgba,
        clip: Rectangle<f64, Logical>,
    ) -> anyhow::Result<()> {
        let (ix, _iy, iw, _ih) = shaped.run.ink_bounds();
        let ax = self.px(x);
        let ox = match halign {
            HAlign::Left => ax - ix,
            HAlign::Center => ax - ix - iw / 2,
            HAlign::Right => ax - ix - iw,
        };
        let oy = self.px(band_top) + shaped.line_box_centered_y(self.px(band_h));
        let clip = self.rect_px(clip);
        self.frame.render_glyphs(
            &shaped.run,
            Point::from((ox, oy)),
            color,
            clip,
            &Self::damage_of(clip),
        )?;
        Ok(())
    }

    /// Draw a shaped run at a precomputed **physical** glyph-layout `origin`, tinted
    /// `color`, clipped to the whole buffer. The physical-coordinate counterpart to
    /// [`text`](Self::text) for a run whose placement isn't a simple ink-box anchor —
    /// the panel clock is placed from its right-anchored button's padding (tabular
    /// figures keep that button's width, and so the label, from jittering as the seconds
    /// tick), so its origin is computed by hand rather than via [`Align`].
    pub fn text_px(
        &mut self,
        shaped: &ShapedText,
        origin: Point<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.frame.render_glyphs(
            &shaped.run,
            origin,
            color,
            self.full,
            &Self::damage_of(self.full),
        )?;
        Ok(())
    }

    /// Fill a **physical** sub-rect with a solid `color` (no rounding). The
    /// physical-coordinate counterpart to [`fill_rounded`](Self::fill_rounded) for
    /// content-sized widgets whose chrome (a dialog's border/interior, a keycap
    /// patch) is laid out in physical px next to a [`ShapedParagraph`].
    pub fn fill_rect_px(
        &mut self,
        rect: Rectangle<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        // Premultiplied for the same reason as [`clear`](Self::clear) — this is a clear too.
        self.frame
            .clear(Color32F::from(premultiply(color)), &[rect])?;
        Ok(())
    }

    /// A caret bar of `Entry::CARET_W` at logical `x`, filling `band` vertically, clipped to
    /// `clip`. Blends (an SDF fill), unlike [`fill_rect_px`](Self::fill_rect_px)'s clear — a
    /// caret sits *on* the entry's fill, and clearing would punch a hole in it.
    pub fn caret(
        &mut self,
        x: f64,
        band: Rectangle<f64, Logical>,
        color: Rgba,
        clip: Option<Rectangle<f64, Logical>>,
    ) -> anyhow::Result<()> {
        self.selection(x, x + Entry::CARET_W, band, color, clip)
    }

    /// The selection wash between logical `x0` and `x1`, filling `band` vertically, clipped to
    /// `clip`. A zero-or-negative span draws nothing.
    pub fn selection(
        &mut self,
        x0: f64,
        x1: f64,
        band: Rectangle<f64, Logical>,
        color: Rgba,
        clip: Option<Rectangle<f64, Logical>>,
    ) -> anyhow::Result<()> {
        if x1 <= x0 {
            return Ok(());
        }
        let rect = Rectangle::new(
            Point::from((x0, band.loc.y)),
            Size::from((x1 - x0, band.size.h)),
        );
        let rect = match clip {
            Some(clip) => match rect.intersection(clip) {
                Some(rect) => rect,
                None => return Ok(()),
            },
            None => rect,
        };
        self.fill_rounded(rect, 0., color)
    }

    /// A crisp separator hairline filling the logical `rect` (one dimension is its 1px thickness).
    /// Snapped to device pixels and painted with [`fill_rect_px`](Self::fill_rect_px) — a *clear*,
    /// so a hairline keeps full coverage where an SDF `fill_rounded` would anti-alias both edges
    /// and halve a 1px line (`_message-list.scss` / `_popovers.scss` `$borders_color` all but
    /// vanished as a rounded fill). Because it clears (replaces, not blends), pass an **opaque**
    /// color when drawing over opaque content — use [`style::over`] to pre-blend
    /// [`style::BORDERS`] onto the surface; over a transparent bake layer the translucent
    /// `BORDERS` itself is correct (it blends when the layer composites). The single home for both
    /// the calendar column separator and the QS group separators.
    pub fn hairline(&mut self, rect: Rectangle<f64, Logical>, color: Rgba) -> anyhow::Result<()> {
        let mut px = self.rect_px(rect);
        px.size.w = px.size.w.max(1);
        px.size.h = px.size.h.max(1);
        self.fill_rect_px(px, color)
    }

    /// Draw a shaped paragraph block with its layout frame's top-left at `origin`
    /// (**physical** — paragraphs are physical-native; see [`ShapedParagraph`]),
    /// tinted `color`, clipped to the whole buffer. The physical-coordinate
    /// counterpart to [`text`](Self::text) for wrapped, multi-span text blocks.
    pub fn paragraph(
        &mut self,
        shaped: &ShapedParagraph,
        origin: Point<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.frame.render_glyphs(
            &shaped.run,
            origin,
            color,
            self.full,
            &Self::damage_of(self.full),
        )?;
        Ok(())
    }

    /// Draw a shaped paragraph with per-span colors: `colors[i]` tints span `i`
    /// (`origin` **physical**, clipped to the whole buffer). For runs whose spans
    /// carry distinct colors — the MRU scope panel's selected/unselected tokens.
    pub fn paragraph_spans(
        &mut self,
        shaped: &ShapedParagraph,
        origin: Point<i32, Physical>,
        colors: &[Rgba],
    ) -> anyhow::Result<()> {
        self.frame.render_glyphs_spans(
            &shaped.run,
            origin,
            colors,
            self.full,
            &Self::damage_of(self.full),
        )?;
        Ok(())
    }

    /// Draw a [`Button`]: the rounded style fill (+ hover wash), an accent focus ring
    /// when focused, then the centered bold-white `label`. `accent` is the system
    /// accent — the focus-ring color, and the fill for [`ButtonStyle::Suggested`].
    ///
    /// The focus ring is GNOME's inset 2px accent stroke ([`stroke_rounded`](Self::stroke_rounded))
    /// on the button's own rect, drawn over the fill — faithful, and correct over a translucent
    /// [`ButtonStyle::Dialog`] fill with no masking.
    /// The keyboard-focus ring: a 2px inset stroke in `base` at 80%
    /// (`@include focus_ring($fc)` → `box-shadow: inset 0 0 0 2px st-transparentize($fc, .2)`,
    /// `_drawing.scss:56-66`).
    ///
    /// `base` is the accent for an ordinary surface, and [`style::accent_borders`] for one whose
    /// fill is already the accent — the same `$fc` swap the SCSS makes for the `default` button
    /// style (`_drawing.scss:313-316`), without which a checked quick-settings tile shows an
    /// accent ring on an accent fill and reads as unfocused.
    ///
    /// A verb rather than each caller's own stroke, because every focusable surface in the shell
    /// owes the same ring — a button, a quick-settings tile, a slider — and a ring that differs
    /// per surface reads as a different kind of focus.
    pub fn focus_ring(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        base: Rgba,
    ) -> anyhow::Result<()> {
        let ring = [base[0], base[1], base[2], 0.8];
        self.stroke_rounded(rect, radius, 2., ring)
    }

    pub fn button(&mut self, b: &Button, label: &ShapedText, accent: Rgba) -> anyhow::Result<()> {
        let radius = b.style.radius();
        self.fill_rounded(b.rect, radius, b.style.bg(accent))?;
        if b.hovered {
            self.fill_rounded(b.rect, radius, style::HOVER_WASH)?;
        }
        if b.focused {
            self.focus_ring(b.rect, radius, b.style.focus_ring_base(accent))?;
        }
        let center = Point::from((
            b.rect.loc.x + b.rect.size.w / 2.,
            b.rect.loc.y + b.rect.size.h / 2.,
        ));
        self.text_clipped(label, center, Align::CENTER, style::TEXT, b.rect)?;
        Ok(())
    }

    /// Draw a [`Switch`] filling `rect` (use [`Switch::size`] for it): the fully-rounded
    /// track, then the handle at whichever end `on` selects (`_switches.scss:6-52`).
    ///
    /// `accent` is the system accent — the on-state track fill (`background:
    /// -st-accent-color`, `_switches.scss:41`). `hovered` only lightens the *off* track,
    /// which is the direction the SCSS gives it (`:15-23`); the on state's hover is a
    /// 5% accent lighten we skip, since our switch has no hover state of its own yet
    /// (the whole row is the hover target).
    /// The dynamic battery indicator ([`Battery`]), drawn at `origin` (its top-left) in a box
    /// [`Battery::WIDTH`] x [`Battery::HEIGHT`].
    ///
    /// Draws the housing, the charge and the nub, and nothing else: the charging bolt, the mains
    /// plug and the critical glyph are all composited over this by the caller, through the icon
    /// path, so each can carry the rim and shadow that let it survive a white fill.
    ///
    /// The shell is a real [`Self::stroke_rounded`], never a fill-then-inner-fill: that idiom
    /// cannot round its corners and would put a hard edge where the mockup has a radius.
    pub fn battery(&mut self, origin: Point<f64, Logical>, b: &Battery) -> anyhow::Result<()> {
        let body = Rectangle::new(origin, Size::from((Battery::BODY_W, Battery::BODY_H)));
        self.stroke_rounded(body, Battery::RADIUS, Battery::STROKE, b.body_tint)?;

        // The bar grows from the body's left edge, inset so a gap shows inside the stroke.
        let fill = Rectangle::new(
            origin + Point::from((Battery::INSET, Battery::INSET)),
            Size::from((
                Battery::fill_width(b.fill),
                Battery::BODY_H - 2. * Battery::INSET,
            )),
        );
        self.fill_rounded(fill, Battery::FILL_RADIUS, b.fill_tint)?;

        // The nub, centred on the body's right edge.
        let nub = Rectangle::new(
            origin
                + Point::from((
                    Battery::BODY_W + Battery::NUB_GAP,
                    (Battery::BODY_H - Battery::NUB_H) / 2.,
                )),
            Size::from((Battery::NUB_W, Battery::NUB_H)),
        );
        self.fill_rounded(nub, Battery::NUB_W / 2., b.body_tint)?;
        Ok(())
    }

    pub fn toggle_switch(
        &mut self,
        rect: Rectangle<f64, Logical>,
        on: bool,
        hovered: bool,
        accent: Rgba,
    ) -> anyhow::Result<()> {
        // `border-radius: $forced_circular_radius` — a pill, so half the height.
        let track_radius = rect.size.h / 2.;
        let track = if on {
            accent
        } else if hovered {
            SWITCH_OFF_BG_HOVER
        } else {
            SWITCH_OFF_BG
        };
        self.fill_rounded(rect, track_radius, track)?;

        // The handle sits `MARGIN` from the track's near end, sized square.
        let size = rect.size.h - 2. * Switch::MARGIN;
        let x = if on {
            rect.loc.x + rect.size.w - Switch::MARGIN - size
        } else {
            rect.loc.x + Switch::MARGIN
        };
        let handle = Rectangle::new(
            Point::from((x, rect.loc.y + Switch::MARGIN)),
            Size::from((size, size)),
        );
        // `box-shadow: 0 2px 4px …` — the same disc, nudged down, behind the handle.
        let shadow = Rectangle::new(handle.loc + Point::from((0., 2.)), handle.size);
        self.fill_rounded(shadow, size / 2., SWITCH_HANDLE_SHADOW)?;
        let fill = if on {
            SWITCH_HANDLE_ON
        } else {
            SWITCH_HANDLE_OFF
        };
        self.fill_rounded(handle, size / 2., fill)?;
        Ok(())
    }

    /// Draw a [`BarLevel`] filling `rect`: the track, the fill up to `value`, and —
    /// when `overdrive_start < max` — the overdrive segment past it, separated by a
    /// [`BarLevel::OVERDRIVE_SEPARATOR`]-wide gap (`barLevel.js:117-220`).
    ///
    /// `value` is clamped to `0..=max` and `max` to at least 1, exactly like the
    /// property setters (`barLevel.js:57-80`). The cap radius is
    /// `min(width, height) / 2` (`:122`), and — this is the part that is easy to get
    /// wrong — the fill's end cap is a **full circle centered on** the progress
    /// position (`:194-210`), so the fill actually reaches `end_x + radius`, which is
    /// what makes a full bar reach the right edge.
    ///
    /// We differ from the cairo original in one harmless way: GNOME paints the track
    /// only over the *unfilled* remainder, we paint the whole pill and cover it. That
    /// is identical for opaque fills (every current caller) and it is also what makes
    /// the overdrive gap correct for free — the gap shows the track through, which is
    /// precisely the color GNOME paints the separator with once the value is in
    /// overdrive (`:215-218`), so that branch draws nothing.
    pub fn bar_level(
        &mut self,
        rect: Rectangle<f64, Logical>,
        value: f64,
        max: f64,
        overdrive_start: f64,
        style: BarLevelStyle,
    ) -> anyhow::Result<()> {
        let max = max.max(1.);
        let value = value.clamp(0., max);
        let overdrive_start = overdrive_start.clamp(1., max);
        let radius = rect.size.w.min(rect.size.h) / 2.;
        // The span the progress position travels across, inset by a cap at each end.
        let travel = (rect.size.w - 2. * radius).max(0.);

        self.fill_rounded(rect, radius, style.track)?;

        let seg = |from: f64, to: f64| {
            Rectangle::new(
                Point::from((rect.loc.x + from, rect.loc.y)),
                Size::from((to - from, rect.size.h)),
            )
        };
        let end_x = radius + travel * (value / max);
        let sep_x = radius + travel * (overdrive_start / max);
        let overdrive_active = overdrive_start < max;
        let half_sep = if overdrive_active {
            BarLevel::OVERDRIVE_SEPARATOR / 2.
        } else {
            0.
        };

        if value > 0. {
            if !overdrive_active || value <= overdrive_start {
                self.fill_rounded(seg(0., end_x + radius), radius, style.fill)?;
            } else {
                // Only the *outer* ends of these two segments are round: the edges
                // facing the separator are straight `lineTo`s in cairo
                // (`barLevel.js:163-172,177-190`). Ours come from `fill_rounded`,
                // which rounds all four corners, so each inner edge is squared off
                // again by a plain rect over its corner band — at 6px tall the
                // corners are full semicircles, and leaving them would make the 3px
                // separator read as a wide notch exactly when overdrive is showing.
                let fill_end = sep_x - half_sep;
                let over_start = sep_x + half_sep;
                let over_end = end_x + radius;
                self.fill_rounded(seg(0., fill_end), radius, style.fill)?;
                self.fill_rounded(seg((fill_end - radius).max(0.), fill_end), 0., style.fill)?;
                self.fill_rounded(seg(over_start, over_end), radius, style.overdrive)?;
                self.fill_rounded(
                    seg(over_start, (over_start + radius).min(over_end)),
                    0.,
                    style.overdrive,
                )?;
            }
        }

        // Below overdrive the separator is a solid foreground tick; above it, the gap
        // left between the two fills already shows the track (see the doc comment).
        if overdrive_active && value <= overdrive_start {
            self.fill_rounded(seg(sep_x - half_sep, sep_x + half_sep), 0., style.separator)?;
        }
        Ok(())
    }
}

/// Test-only scale-sweep harness (H4 in the design doc). Bakes a widget at scales
/// {1.0, 1.5, 2.0} and asserts the buffer is physically `round(logical × scale)`
/// and that the glyph **ink area grows with the square of the scale** — the
/// assertion that would have caught the input-source popover's minuscule text
/// (`3c7473be`), where text shaped at logical px kept a constant glyph size at
/// every scale.
///
/// Ink *area* (bright-pixel count), not the ink bounding box: a widget's ink bbox
/// spans its top row to its bottom row, so its height tracks the buffer (row
/// layout) regardless of per-glyph size and cannot see shrunk glyphs. Glyph ink
/// area scales as `font_px²`, i.e. `scale²` when the text is correctly sized — so
/// scale-1→2 area ≈ 4×; the bug (constant font_px) leaves it ≈ 1×.
///
/// `bake_at` bakes the widget at the given scale and returns its texture; pass the
/// widget's scale-independent `logical_size`.
#[cfg(test)]
pub fn assert_scale_correct(
    vk: &mut VulkanRenderer,
    logical_size: Size<f64, Logical>,
    mut bake_at: impl FnMut(&mut VulkanRenderer, f64) -> TextureBuffer<VkTexture>,
) {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{ExportMem, Texture};
    use smithay::utils::Rectangle;

    // (scale, bright-pixel count) collected across the sweep.
    let mut ink: Vec<(f64, u64)> = Vec::new();

    for scale in [1.0, 1.5, 2.0] {
        let expected = physical_size(scale, logical_size);
        // Readback binds the *texture*; the buffer is just the cached wrapper around it.
        let mut tex = bake_at(vk, scale).texture().clone();

        let size = tex.size();
        assert_eq!(
            (size.w, size.h),
            (expected.w, expected.h),
            "scale {scale}: buffer size {size:?} != round(logical × scale) {expected:?}",
        );

        // Read the baked pixels back and count "ink" — pixels clearly brighter than
        // the dark widget background (text is near-white; the dark rounded bg and
        // the low-alpha separator stay well under the threshold).
        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let count = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count() as u64;
        assert!(
            count > 20,
            "scale {scale}: expected visible glyph ink, got {count} bright pixels",
        );
        ink.push((scale, count));
    }

    // The regression pin: ink area must grow ~scale². Correct text quadruples from
    // scale 1 to 2; text shaped at logical px (the bug) stays ~flat (≈1×), far below
    // the band. A wide band absorbs glyph-hinting/anti-aliasing noise while leaving
    // the 4×-vs-1× gap unmissable (per the review — no reliance on exact linearity).
    let ratio = ink[2].1 as f64 / ink[0].1 as f64;
    assert!(
        (2.5..=6.0).contains(&ratio),
        "ink area should grow ~4× (scale²) from scale 1 to 2, got {ratio:.2} \
         (counts {ink:?}) — a ratio near 1 means text was shaped at logical px \
         instead of physical (the HiDPI bug class)",
    );
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem, Texture};
    use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size};

    use super::{
        bake_uncached_sized, ellipsized_line, step_rects, tile_label_lines, Dir, Painter, Revision,
        TileMetrics, TILE_LABEL_EXPAND_LINES, TILE_LABEL_LINES,
    };
    use crate::render_helpers::vulkan::VulkanRenderer;

    /// A 2x2 grid of 40x20 cells with a full-width row beneath it — the shape of a quick-settings
    /// menu in miniature.
    fn grid_rects() -> Vec<Rectangle<f64, Logical>> {
        let cell = |x: f64, y: f64, w: f64, h: f64| {
            Rectangle::new(Point::from((x, y)), Size::from((w, h)))
        };
        vec![
            cell(0., 0., 40., 20.),   // 0: top-left
            cell(50., 0., 40., 20.),  // 1: top-right
            cell(0., 30., 40., 20.),  // 2: bottom-left
            cell(50., 30., 40., 20.), // 3: bottom-right
            cell(0., 60., 90., 20.),  // 4: the full-width row
        ]
    }

    fn group() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 0.)), Size::from((90., 80.)))
    }

    /// Entering from nowhere measures against the **group's** edge, not against some first child:
    /// Down enters at the top, Up at the bottom, Right at the left (`st-widget.c:2127-2150`).
    #[test]
    fn spatial_navigation_enters_from_the_edge_the_direction_comes_from() {
        let r = grid_rects();
        // Down from nowhere: the top row, and of it the one nearest the group's horizontal centre.
        let first = step_rects(&r, group(), None, Dir::Down).unwrap();
        assert!(
            first == 0 || first == 1,
            "Down enters at the top, got {first}"
        );
        assert_eq!(
            step_rects(&r, group(), None, Dir::Up),
            Some(4),
            "Up enters at the bottom"
        );
    }

    /// The arrows move geometrically and do **not** wrap; Tab walks the chain in order and does.
    #[test]
    fn arrows_step_geometrically_and_only_tab_wraps() {
        let r = grid_rects();
        let g = group();
        assert_eq!(step_rects(&r, g, Some(0), Dir::Right), Some(1));
        assert_eq!(step_rects(&r, g, Some(0), Dir::Down), Some(2));
        assert_eq!(step_rects(&r, g, Some(1), Dir::Left), Some(0));
        assert_eq!(step_rects(&r, g, Some(2), Dir::Down), Some(4));

        assert_eq!(
            step_rects(&r, g, Some(4), Dir::Down),
            None,
            "Down at the bottom stays put — StFocusManager passes wrap_around = FALSE for arrows"
        );
        assert_eq!(step_rects(&r, g, Some(0), Dir::Up), None);
        assert_eq!(step_rects(&r, g, Some(0), Dir::Left), None);

        assert_eq!(
            step_rects(&r, g, Some(4), Dir::TabForward),
            Some(0),
            "Tab wraps"
        );
        assert_eq!(step_rects(&r, g, Some(0), Dir::TabBackward), Some(4));
    }

    /// A candidate that merely overlaps the source is not "past" it: the filter wants the whole
    /// box beyond the source's edge, give or take 0.1px (`st-widget.c:1950-1975`).
    #[test]
    fn a_merely_overlapping_rect_is_not_in_that_direction() {
        let r = vec![
            Rectangle::new(Point::from((0., 0.)), Size::from((40., 20.))),
            // Starts inside the first row's band: not "below" it.
            Rectangle::new(Point::from((0., 10.)), Size::from((40., 20.))),
        ];
        assert_eq!(step_rects(&r, group(), Some(0), Dir::Down), None);
    }

    /// A tile is `.overview-tile` padding around a `Shell.SquareBin`, so its **width**
    /// follows the *content height* — icon + spacing + one caption line
    /// (`shell-square-bin.c:14-30`, `iconGrid.js:62`). Deriving it from the icon alone
    /// made it 120 wide against GNOME's 145. Pinned in numbers because the whole grid
    /// and the search results card size off it.
    ///
    /// 145, not 144: the caption's line box at the base 11pt is 19 (`ceil(ascent) + ceil(descent)`
    /// — see [`crate::ui::line_height_px`]), and `label_h` was pinned at 18.
    ///
    /// A resting caption past the first line hangs *below* the tile — the tile stays
    /// square (reserving the second line in the box would cost a rung of the icon ladder
    /// on the small canvases the second line exists for), so the chrome drawn around a
    /// caption grows by the overhang instead.
    #[test]
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
    fn an_overview_tile_is_a_square_around_its_caption_box() {
        let m = TileMetrics::overview();
        // The caption band is the shared line box, not a number of its own — the tile is a
        // SquareBin, so a private copy here would resize the whole app grid behind everyone.
        assert_eq!(
            m.label_h,
            crate::ui::line_height_px(crate::ui::BASE_FONT_PT),
            "the caption band must be the shared line box at the caption's own size"
        );
        assert_eq!(m.label_w(), m.icon_px + m.label_gap + m.label_h);
        assert_eq!(m.label_w(), 121.);
        assert_eq!(m.size(), Size::from((145., 145.)));
        assert_eq!(m.size().w, m.label_w() + 2. * m.pad);

        // One line fits the box exactly; each further line hangs below it.
        assert_eq!(m.caption_overhang(1), 0.);
        assert_eq!(m.caption_overhang(2), m.label_h);
        let tile = Rectangle::new(Point::from((0., 0.)), m.size());
        assert_eq!(
            m.label_top(tile) + m.label_h,
            tile.size.h - m.pad,
            "one line ends at the tile's bottom padding"
        );
    }

    /// A folder tile's icon is a homogeneous 2×2 over the same icon box an app icon
    /// fills (`createFolderIcon`, `appDisplay.js:2138-2162`): four half-box cells,
    /// each with one member icon at 0.4× the box centered in it. The centering is the
    /// part worth pinning — it is what leaves the cross-shaped gap that reads as a
    /// folder, and cell-filling instead would make the four icons touch.
    #[test]
    fn a_folder_tile_composes_four_sub_icons_centered_in_a_two_by_two() {
        let m = TileMetrics::overview();
        let tile = Rectangle::new(Point::from((0., 0.)), m.size());
        let sub = m.folder_subicon_px();
        assert_eq!(sub, 38., "floor(0.4 * 96)");

        let centers: Vec<Point<f64, Logical>> =
            (0..4).map(|i| m.folder_subicon_center(tile, i)).collect();
        let icon = m.icon_center(tile);
        // Left-to-right then top-to-bottom, a quarter box out from the icon center.
        let q = m.icon_px / 4.;
        assert_eq!(centers[0], Point::from((icon.x - q, icon.y - q)));
        assert_eq!(centers[1], Point::from((icon.x + q, icon.y - q)));
        assert_eq!(centers[2], Point::from((icon.x - q, icon.y + q)));
        assert_eq!(centers[3], Point::from((icon.x + q, icon.y + q)));

        // Centered in their half-box cells, so the composition sits inside the icon
        // box with a gap down the middle rather than filling it edge to edge.
        let left = centers[0].x - sub / 2.;
        let right = centers[1].x + sub / 2.;
        assert!(left > icon.x - m.icon_px / 2., "inset from the box: {left}");
        assert!(
            right < icon.x + m.icon_px / 2.,
            "inset from the box: {right}"
        );
        assert!(
            centers[1].x - sub / 2. > centers[0].x + sub / 2.,
            "the two columns do not touch"
        );
    }

    /// A single-line label is cut with an ellipsis rather than allowed to overflow.
    ///
    /// Content text is not ours to bound — a window title can be a whole file path — so the label
    /// that shows it has to end somewhere. St's answer is `PANGO_ELLIPSIZE_END` on every `StLabel`
    /// (`st-label.c:331`), and the switcher's title band relies on it.
    #[test]
    fn a_content_label_ellipsizes_instead_of_overflowing() {
        let pt = crate::ui::BASE_FONT_PT;
        let long = "A Very Long Window Title That Nobody Would Ever Choose \
                    But Every Editor Writes Anyway.txt";

        let cut = ellipsized_line(long, pt, 200.);
        assert!(cut.ends_with('…'), "cut at the end: {cut:?}");
        assert!(cut.len() < long.len(), "and shorter than the input");
        assert!(!cut.contains('\n'), "one line, never two: {cut:?}");

        // A title that fits is left exactly as it is — no stray ellipsis, no reflow.
        assert_eq!(ellipsized_line("Terminal", pt, 400.), "Terminal");
        assert_eq!(ellipsized_line("", pt, 400.), "");
    }

    /// A **resting** caption never splits a word: on a narrow tile (a small canvas shrinks
    /// the icon, and the caption box with it) "Graphics" must read "Graphic…", not
    /// "Graphi/cs". Expanding is the opposite — it exists to show the whole name, so there
    /// a word wider than the tile breaks across lines (Pango `WORD_CHAR`) rather than
    /// losing characters. Live report, 2026-07-28, on a 1024x665 canvas.
    #[test]
    fn a_resting_caption_ellipsizes_a_long_word_instead_of_splitting_it() {
        let pt = crate::ui::BASE_FONT_PT;
        // The caption box of a tile whose icon has stepped well down the ladder.
        let narrow = TileMetrics {
            icon_px: 32.,
            ..TileMetrics::overview()
        }
        .label_w();

        let resting = tile_label_lines("Graphics", pt, narrow, TILE_LABEL_LINES, false);
        assert_eq!(resting.len(), 1, "one word stays on one line: {resting:?}");
        assert!(
            resting[0].ends_with('…') && !resting[0].contains(' '),
            "…ellipsized, not split: {resting:?}"
        );

        // Two words still wrap at the space — that is the whole point of the second line.
        let two = tile_label_lines("Image Editors", pt, narrow, TILE_LABEL_LINES, false);
        assert_eq!(two.len(), 2, "a space is a break point: {two:?}");
        assert!(!two[0].ends_with('…'), "and needs no ellipsis: {two:?}");

        // A first line that had to be cut is the LAST line: "Chrome Web Store" in a box
        // too narrow for "Chrome" reads "Chr…", not "Chr…/We…".
        let cut_first = tile_label_lines("Chrome Web Store", pt, 40., TILE_LABEL_LINES, false);
        assert_eq!(
            cut_first.len(),
            1,
            "nothing follows an ellipsized line: {cut_first:?}"
        );
        assert!(cut_first[0].ends_with('…'));

        // Expanded may split it, because there is nowhere else for the characters to go.
        let expanded = tile_label_lines("Graphics", pt, narrow, TILE_LABEL_EXPAND_LINES, true);
        assert!(
            expanded.len() > 1 && !expanded.concat().contains('…'),
            "expanded breaks the word rather than cutting it: {expanded:?}"
        );
    }

    /// A name that does not fit is wrapped to [`TILE_LABEL_LINES`] and ellipsized past
    /// them at rest, and wrapped whole (no ellipsis) expanded — `_updateMultiline`,
    /// `appDisplay.js:1891-1924`, with the resting line count our own divergence.
    /// Before this, a long name was hard-clipped mid-glyph by the label band.
    #[test]
    fn a_long_tile_caption_ellipsizes_collapsed_and_wraps_expanded() {
        let name = "Passwords and Keys";
        let w = TileMetrics::overview().label_w();
        let pt = crate::ui::BASE_FONT_PT;

        let collapsed = tile_label_lines(name, pt, w, TILE_LABEL_LINES, false);
        assert_eq!(collapsed.len(), 2, "at rest a long name wraps to two lines");
        assert!(
            !collapsed.concat().contains('…'),
            "which this name fits in whole: {collapsed:?}"
        );
        // Longer than two lines still ends in an ellipsis rather than losing text.
        let long = tile_label_lines(
            "Passwords and Keys and Certificates",
            pt,
            w,
            TILE_LABEL_LINES,
            false,
        );
        assert_eq!(long.len(), 2);
        assert!(long[1].ends_with('…'), "cut past the last line: {long:?}");

        // One line is still one line — what a search result asks for.
        let one = tile_label_lines(name, pt, w, 1, false);
        assert_eq!(one.len(), 1);
        assert!(one[0].ends_with('…'));

        let expanded = tile_label_lines(name, pt, w, TILE_LABEL_EXPAND_LINES, true);
        assert!(expanded.len() > 1, "expanded wraps: {expanded:?}");
        assert!(!expanded.concat().contains('…'), "and drops the ellipsis");
        assert_eq!(
            expanded.join(" ").split_whitespace().collect::<Vec<_>>(),
            name.split_whitespace().collect::<Vec<_>>(),
            "the whole name is readable"
        );

        // A name that fits is untouched in both states — it stays in the page bake.
        assert_eq!(
            tile_label_lines("Files", pt, w, TILE_LABEL_LINES, false),
            vec!["Files"]
        );
        assert_eq!(
            tile_label_lines("Files", pt, w, TILE_LABEL_EXPAND_LINES, true),
            vec!["Files"]
        );
    }

    /// The gaussian drop-shadow verb: a black shadow over a white buffer must darken the
    /// casting box to near-black, fade through mid-grey in the blur fringe just outside it,
    /// and leave the far corner (beyond ~3σ) untouched white. Pins `Painter::drop_shadow`'s
    /// SDF placement + blur falloff over the shared `render_shadow` material.
    #[test]
    fn drop_shadow_casts_a_fading_fringe() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping drop_shadow_casts_a_fading_fringe: no Vulkan device ({e})");
                return;
            }
        };

        let scale = 1.0;
        let size = Size::<i32, Physical>::from((100, 100));
        // Box spans 30..70; blur 10 → σ=5, so the fringe reaches ~15px (15..85 shades) and
        // the (95,95) corner stays white.
        let mut tex = bake_uncached_sized(&mut vk, size, |frame| {
            let mut p = Painter::new(frame, scale, size);
            p.clear([1., 1., 1., 1.])?;
            let box_rect =
                Rectangle::<f64, Logical>::new(Point::from((30., 30.)), Size::from((40., 40.)));
            p.drop_shadow(box_rect, 12., 10., (0., 0.), [0., 0., 0., 1.])?;
            Ok(())
        })
        .expect("bake");

        let tex_size = tex.size();
        let fb = vk.bind(&mut tex).expect("bind");
        let region = Rectangle::<i32, BufferCoord>::from_size(tex_size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Opaque white base + premultiplied-black shadow → each channel reads (1 − α)·255,
        // so a plain channel average is the shadow strength (0 = full shadow, 255 = none).
        let lum = |x: i32, y: i32| -> i32 {
            let i = ((y * 100 + x) * 4) as usize;
            (pixels[i] as i32 + pixels[i + 1] as i32 + pixels[i + 2] as i32) / 3
        };
        let center = lum(50, 50);
        let fringe = lum(72, 50);
        let corner = lum(95, 95);

        assert!(
            center < 60,
            "box center should be near-black shadow, got {center}"
        );
        assert!(corner > 200, "far corner should stay white, got {corner}");
        assert!(
            fringe > center + 20 && fringe < corner - 20,
            "fringe just outside the box should be mid-grey (blur falloff): \
             center {center}, fringe {fringe}, corner {corner}",
        );
    }

    /// The horizontal alpha ramp on the rounded-rect material: full colour at one end,
    /// nothing at the other, linear between. Also pins the "run the rect past the buffer
    /// to keep a corner square" idiom the page hints use — with the rect extending 40px
    /// beyond the right edge, the right corners are clipped away and that edge is full
    /// height, while the left ones are visibly cut by the radius.
    #[test]
    fn a_faded_rounded_rect_ramps_across_its_width() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping a_faded_rounded_rect_ramps_across_its_width: no device ({e})");
                return;
            }
        };

        let scale = 1.0;
        let size = Size::<i32, Physical>::from((100, 100));
        let mut tex = bake_uncached_sized(&mut vk, size, |frame| {
            let mut p = Painter::new(frame, scale, size);
            p.clear([0., 0., 0., 0.])?;
            // Opaque white, rounded 20, running 40px past the right edge; brightest at the
            // left edge (u=0) and gone by the right edge of the *drawn* rect (u=1 of 140).
            let rect =
                Rectangle::<f64, Logical>::new(Point::from((0., 0.)), Size::from((140., 100.)));
            p.fill_rounded_faded(rect, 20., [1., 1., 1., 1.], 0., 1.)?;
            Ok(())
        })
        .expect("bake");

        let tex_size = tex.size();
        let fb = vk.bind(&mut tex).expect("bind");
        let region = Rectangle::<i32, BufferCoord>::from_size(tex_size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
        let alpha = |x: i32, y: i32| pixels[((y * 100 + x) * 4 + 3) as usize] as i32;

        let (left, mid, right) = (alpha(2, 50), alpha(50, 50), alpha(98, 50));
        assert!(left > 220, "full colour at the ramp's start: {left}");
        assert!(
            (left - right) > 40 && mid > right && mid < left,
            "the ramp must fall monotonically across the width: {left} {mid} {right}"
        );
        // Left corners cut by the radius, right edge square (its corners fell outside).
        assert_eq!(alpha(1, 1), 0, "the left corner is rounded away");
        assert!(
            alpha(98, 1) > 0,
            "the right edge runs off the buffer square"
        );
    }

    /// The bake-attribution chain: a bake must be recorded against the *widget's*
    /// line, not against whichever helper in this file it travelled through.
    /// The fill bar is the whole point of the indicator, so its degenerate ends are pinned: a
    /// rounded rect narrower than its own corner diameter cannot exist, and an empty battery must
    /// still show *something* or the widget reads as broken rather than as flat.
    #[test]
    fn the_battery_fill_bar_never_collapses_below_its_own_corner_diameter() {
        use super::Battery;

        let inner = Battery::BODY_W - 2. * Battery::INSET;
        let floor = 2. * Battery::FILL_RADIUS;

        assert_eq!(
            Battery::fill_width(1.),
            inner,
            "full fills the body exactly"
        );
        assert_eq!(Battery::fill_width(0.5), inner / 2., "half is half");
        assert_eq!(
            Battery::fill_width(0.),
            floor,
            "an empty battery still draws a lozenge, not a zero-width sliver"
        );
        assert_eq!(
            Battery::fill_width(0.01),
            floor,
            "1% is below the corner diameter, so it clamps up to it"
        );

        // Out-of-range input is clamped, not trusted: UPower is not the only thing that can write
        // a percentage, and a bar wider than its body would paint outside the shell.
        assert_eq!(Battery::fill_width(1.5), inner);
        assert_eq!(Battery::fill_width(-1.), floor);
        assert_eq!(Battery::fill_width(f64::NAN), floor, "NaN must not escape");

        // The slot the panel reserves has to hold the nub too, or the neighbour overlaps it.
        assert_eq!(
            Battery::WIDTH,
            Battery::BODY_W + Battery::NUB_GAP + Battery::NUB_W
        );
    }

    ///
    /// This is the load-bearing half of `frame_log`'s bake reporting, and it
    /// degrades silently: drop `#[track_caller]` from one helper and every widget
    /// routing through it collapses onto a single line in `widget.rs`, which reads
    /// like one very busy widget rather than a broken instrument. Nothing else
    /// notices — the counts and the timings stay right.
    ///
    /// Each helper here is entered from a distinct line of *this* test, so a
    /// collapsed chain shows up as a site in `widget.rs` instead.
    #[test]
    fn bake_sites_name_the_widget_not_the_helper() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping bake_sites_name_the_widget_not_the_helper: no device ({e})");
                return;
            }
        };

        let size = Size::<i32, Physical>::from((8, 8));
        let logical = Size::<f64, Logical>::from((8., 8.));
        let paint_px = |_: &mut crate::render_helpers::vulkan::VulkanFrame| Ok(());

        let _ = crate::frame_log::take_bake_sites();

        let sized_line = line!() + 1;
        bake_uncached_sized(&mut vk, size, paint_px).expect("bake_uncached_sized");
        let uncached_line = line!() + 1;
        super::bake_uncached(&mut vk, 1.0, logical, |_, _| Ok(())).expect("bake_uncached");
        let cached_line = line!() + 2;
        let mut cache = super::BakeCache::default();
        super::bake(
            &mut vk,
            &mut cache,
            1.0,
            logical,
            0,
            |_| Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("bake");

        let sites = crate::frame_log::take_bake_sites();
        let here = file!();
        for (what, line) in [
            ("bake_uncached_sized", sized_line),
            ("bake_uncached", uncached_line),
            ("bake", cached_line),
        ] {
            assert!(
                sites.iter().any(|s| s.file == here && s.line == line),
                "{what} was attributed to {:?}, not to its caller {here}:{line} — \
                 a #[track_caller] is missing from the chain in widget.rs",
                sites
                    .iter()
                    .map(|s| format!("{}:{}", s.file, s.line))
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// Every input the paint closure reads has to move the key, or the widget serves a stale
    /// texture for as long as its content lives — the `128d112e` calendar freeze.
    #[test]
    fn a_changed_input_changes_the_revision() {
        let base = Revision::new()
            .of(3usize)
            .of("title")
            .px(120.)
            .color(ACCENT);
        assert_ne!(
            base.done(),
            Revision::new()
                .of(4usize)
                .of("title")
                .px(120.)
                .color(ACCENT)
                .done(),
            "a count change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("other")
                .px(120.)
                .color(ACCENT)
                .done(),
            "a text change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(121.)
                .color(ACCENT)
                .done(),
            "a size change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(120.)
                .color(RED)
                .done(),
            "a color change did not invalidate"
        );
        assert_eq!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(120.)
                .color(ACCENT)
                .done(),
            "the same inputs must hit the cache"
        );
    }

    /// Order is part of the key, so two fields cannot swap their contributions and cancel out.
    #[test]
    fn the_order_of_the_inputs_is_part_of_the_revision() {
        assert_ne!(
            Revision::new().of("ab").of("c").done(),
            Revision::new().of("a").of("bc").done(),
            "adjacent inputs ran together, so a boundary shift is invisible"
        );
    }

    /// A NaN that hashed as itself would miss the cache on **every** frame — a full GPU round trip
    /// per frame, which is the exact failure this type exists to prevent, arriving silently.
    #[test]
    fn a_nan_input_still_hits_its_own_cache_entry() {
        assert_eq!(
            Revision::new().px(f64::NAN).done(),
            Revision::new().px(f64::NAN).done(),
            "a NaN input re-bakes forever"
        );
        assert_eq!(
            Revision::new().px(-0.0).done(),
            Revision::new().px(0.0).done(),
            "-0.0 and 0.0 paint the same and must share an entry"
        );
        assert_ne!(
            Revision::new().px(f64::NAN).done(),
            Revision::new().px(0.0).done()
        );
    }

    /// `each` folds a sequence, and a reordering of that sequence is a different bake.
    #[test]
    fn a_reordered_sequence_is_a_different_revision() {
        assert_ne!(
            Revision::new().each(["one", "two"]).done(),
            Revision::new().each(["two", "one"]).done(),
            "reordering the entries did not invalidate — a list that only reorders serves stale"
        );
        assert_eq!(
            Revision::new().each(["one", "two"]).done(),
            Revision::new().of("one").of("two").done(),
            "`each` must be the same as folding the items by hand"
        );
    }

    /// The wiggle is one continuous path, and it starts and ends at rest.
    ///
    /// It is six separate eases stitched together, which is exactly the shape that hides a bug at
    /// the seams: an off-by-one in the leg index, or getting `autoReverse`'s direction backwards,
    /// leaves the curve *plausible* — still 6 px, still 390 ms — while teleporting 12 px between
    /// two frames. That reads as a stutter rather than as a wrong animation, so it is pinned by
    /// continuity rather than by sampled positions.
    #[test]
    fn the_wiggle_never_jumps() {
        let mut w = super::Wiggle::default();
        let t0 = std::time::Duration::from_secs(10);
        w.start(t0);

        // One physical frame at 120 Hz. The largest step a *linear* leg can take is one leg's full
        // 12 px travel over its 65 ms, so anything beyond that with room to spare is a seam.
        let step = std::time::Duration::from_micros(8333);
        let leg_speed = 2. * super::Wiggle::OFFSET / super::Wiggle::LEG.as_secs_f64();
        let budget = leg_speed * step.as_secs_f64() * 1.5;

        let mut now = t0;
        let mut previous = w.offset(now);
        assert_eq!(previous, 0., "it must start from rest");

        while now < t0 + super::Wiggle::TIME + step {
            now += step;
            let x = w.offset(now);
            assert!(
                (x - previous).abs() <= budget,
                "the wiggle jumped {:.2}px at {:?}, from {previous:.2} to {x:.2}",
                (x - previous).abs(),
                now - t0,
            );
            previous = x;
        }

        // The extremes land exactly on the leg boundaries, which a frame clock will not sample —
        // 65 ms is not a whole number of frames at any refresh rate we run at. So they are checked
        // where they are, rather than by watching a sweep go past them.
        let leg_end = |n: u32| w.offset(t0 + super::Wiggle::LEG * n);
        assert_eq!(leg_end(1), -super::Wiggle::OFFSET, "the accelerate leg");
        // ...then the wave, alternating, one boundary per leg.
        for n in 1..=super::Wiggle::LEGS {
            let want = if n % 2 == 0 {
                super::Wiggle::OFFSET
            } else {
                -super::Wiggle::OFFSET
            };
            assert_eq!(leg_end(n), want, "leg {n} does not alternate");
        }
        // The wave ends on a returning leg, so the decelerate always unwinds from the same side —
        // get that backwards and the message ends up parked 6 px off-centre.
        assert_eq!(
            leg_end(super::Wiggle::LEGS + 1),
            -super::Wiggle::OFFSET,
            "the decelerate leg must start where the wave left off"
        );
        // ...and it is back at rest at the end, rather than parked off-centre.
        assert_eq!(w.offset(t0 + super::Wiggle::TIME), 0.);
        assert_eq!(w.offset(t0 + super::Wiggle::TIME * 2), 0.);
        assert!(!w.is_animating(t0 + super::Wiggle::TIME));

        // Nothing moves when nothing asked it to.
        let at_rest = super::Wiggle::default();
        assert_eq!(at_rest.offset(now), 0.);
        assert!(!at_rest.is_animating(now));
    }

    const ACCENT: [f32; 4] = [0.21, 0.52, 0.89, 1.];
    const RED: [f32; 4] = [0.89, 0.21, 0.21, 1.];

    // The screenshot type button is a *column* — glyph over caption — so its height is the one
    // dimension a caller cannot guess from a font metric alone, and getting it wrong shifts every
    // control in the panel. Pinned against the SCSS arithmetic rather than a measured screenshot:
    // 32px glyph + 6px spacing + the caption, inside 12px of vertical padding.
    #[test]
    fn the_type_button_stacks_its_glyph_over_its_caption() {
        use super::IconLabelButton as B;

        let label_h = 13.;
        let size = B::size(30., label_h);
        assert_eq!(size.h, B::PAD_Y * 2. + B::ICON_PX + B::SPACING + label_h);

        // A caption narrower than the glyph leaves the *glyph* setting the width — and note that
        // `min-width` never binds here, since 32 + 2*18 already clears 48. The floor exists in the
        // SCSS, not in any button we can draw.
        assert_eq!(B::size(4., label_h).w, B::ICON_PX + B::PAD_X * 2.);
        assert!(B::size(4., label_h).w > B::MIN_WIDTH);
        // ...and a wide one grows by its own padding instead.
        assert_eq!(B::size(100., label_h).w, 100. + B::PAD_X * 2.);

        let b = B::new(Rectangle::new(
            Point::from((10., 20.)),
            B::size(30., label_h),
        ));
        // The glyph sits in the top band, the caption below it — not both centred on the button,
        // which is what a "centre the content" reflex would produce.
        assert_eq!(b.icon_centre().y, 20. + B::PAD_Y + B::ICON_PX / 2.);
        assert_eq!(
            b.label_centre(label_h).y,
            20. + B::PAD_Y + B::ICON_PX + B::SPACING + label_h / 2.
        );
        assert_eq!(b.icon_centre().x, b.label_centre(label_h).x);
    }

    // The segmented pill's arithmetic: padding outside, spacing only *between* segments. An
    // off-by-one here (spacing after the last segment) leaves the pill visibly lopsided.
    #[test]
    fn the_segmented_pill_spaces_only_between_its_segments() {
        use super::Segmented as S;

        let seg = S::segment_size();
        assert_eq!(seg.w, S::ICON_PX + S::SEG_PAD_X * 2.);
        assert_eq!(seg.h, S::ICON_PX + S::SEG_PAD_Y * 2.);

        assert_eq!(S::size(1).w, seg.w + S::PAD * 2.);
        assert_eq!(S::size(2).w, seg.w * 2. + S::SPACING + S::PAD * 2.);
        assert_eq!(S::size(2).h, seg.h + S::PAD * 2.);

        let rect = Rectangle::new(Point::from((5., 7.)), S::size(2));
        let a = S::segment_rect(rect, 0);
        let b = S::segment_rect(rect, 1);
        assert_eq!(a.loc, Point::from((5. + S::PAD, 7. + S::PAD)));
        assert_eq!(b.loc.x, a.loc.x + seg.w + S::SPACING);
        // The last segment ends exactly one padding short of the container edge.
        assert_eq!(b.loc.x + b.size.w + S::PAD, rect.loc.x + rect.size.w);
    }
}

#[cfg(test)]
mod check_box_tests {
    use smithay::utils::{Point, Rectangle, Size};

    use super::{style, CheckBox};

    fn row() -> CheckBox {
        CheckBox::new(
            Rectangle::new(Point::from((10., 100.)), Size::from((300., 24.))),
            false,
        )
    }

    /// The frame is the glyph plus its padding *and* its border, per side — 14 + (1 + 2) * 2. Read
    /// off `_check-box.scss:17-23`; deriving it from `icon-size` alone would draw a 14px box and
    /// leave the tick touching the border.
    #[test]
    fn frame_is_the_glyph_plus_padding_and_border() {
        assert_eq!(CheckBox::frame_px(), 20.);
        let f = row().frame_rect();
        assert_eq!(f.size, Size::from((20., 20.)));
        // Vertically centred on the row, horizontally at its leading edge.
        assert_eq!(f.loc.x, 10.);
        assert_eq!(f.loc.y, 100. + (24. - 20.) / 2.);
    }

    /// `spacing: .8em` is a function of the label's own point size, not a constant — a caller
    /// using a bigger label must get a proportionally bigger gap.
    #[test]
    fn label_gap_scales_with_the_label_size() {
        let small = CheckBox::label_gap(11.);
        let big = CheckBox::label_gap(22.);
        assert!((big - small * 2.).abs() < 1e-6, "{small} {big}");
        assert!((small - crate::ui::pt_to_px(11.) * 0.8).abs() < 1e-6);
        // The label clears the frame and the gap.
        assert_eq!(row().label_x(11.), 10. + 20. + small);
    }

    /// The whole row is the hit target, label included — GNOME's CheckBox *is* the St.Button, so
    /// clicking the text toggles it. Hit-testing only the 20px frame would make the label look
    /// clickable and not be.
    #[test]
    fn the_whole_row_is_clickable_not_just_the_frame() {
        let c = row();
        assert!(c.contains(Point::from((12., 110.))), "the frame");
        assert!(c.contains(Point::from((250., 110.))), "the label");
        assert!(!c.contains(Point::from((400., 110.))), "past the row");
        assert!(!c.contains(Point::from((250., 130.))), "below the row");
    }

    /// `lighten`/`darken` are HSL lightness shifts. Pinned because the obvious channel-multiply
    /// stand-in gets both directions wrong: it cannot lighten a saturated primary at all, and it
    /// darkens toward black rather than toward the requested lightness.
    #[test]
    fn lighten_and_darken_move_lightness_not_channels() {
        let blue: super::Rgba = [0., 0., 1., 1.];
        let lit = style::lighten(blue, 0.1);
        assert!(
            lit[0] > 0. && lit[1] > 0.,
            "lightening must raise the floor"
        );
        assert!((lit[2] - 1.).abs() < 1e-6, "and leave the ceiling alone");

        let dark = style::darken(blue, 0.1);
        assert!(
            dark[2] < 1. && dark[2] > 0.7,
            "darkened, not crushed: {dark:?}"
        );
        assert_eq!(dark[3], 1., "alpha is untouched");

        // Saturating at either end must not produce a NaN.
        assert_eq!(style::lighten([1., 1., 1., 1.], 0.5), [1., 1., 1., 1.]);
        assert_eq!(style::darken([0., 0., 0., 1.], 0.5), [0., 0., 0., 1.]);
    }
}
