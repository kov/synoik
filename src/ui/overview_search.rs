// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The overview search — a `search-entry` at the top of the Activities overview
//! feeding the built-in **app** search provider, showing a grid of app results that
//! Enter/click launches (`js/ui/searchController.js`, `search.js`, `appDisplay.js`
//! `AppSearchProvider`).
//!
//! **Model split.** Like the dash ([`crate::ui::dash`]), this owns only plain state:
//! the query string, a snapshot of result entries, the keyboard selection, and mouse
//! hover. It does NOT own the app catalog — [`OverviewSearch::handle_key`] mutates the
//! query and returns a [`SearchOutcome`]; `Synoik::sync_overview_search` runs
//! `AppSystem::search` and feeds results back via [`OverviewSearch::set_results`]
//! (GNOME's `SearchResultsView.setTerms` → `_doSearch` → provider). Activation returns
//! the selected app id for the caller to `launch` + `close_overview`.
//!
//! **Faithful behavior (cited 50.1).** Tokenize = trim + split on whitespace
//! (`searchController.js:19-24`). App provider: `AppSystem.search(terms.join(' '))` →
//! relevance-tier groups → filter `should_show` → concat → cap `MAX_RESULTS`=6
//! (`appDisplay.js:1760,1801-1831`). Default selection = first result
//! (`search.js:799-823`); Enter activates it (`search.js:865-872`), closing the
//! overview (`appDisplay.js:3077`). Escape while active clears (does not close)
//! (`searchController.js:153-160`).
//!
//! **Divergences (S4 MVP), revisited later:** no 150ms debounce — search runs
//! synchronously per keystroke (search is in-process/cheap; keeps it clock-free and
//! headless-testable; the Enter-forces-a-pending-search rule, `search.js:866-868`,
//! becomes moot). No `Shell.AppUsage` ordering within a relevance tier (S9+) — tier
//! order is `g_desktop_app_info_search`'s. No `SystemActions` results (S9+). Caret is
//! at the end only (no mid-string edit/selection), so Left/Up map to selection-prev
//! and Right/Down/Tab to selection-next (GNOME's Left-at-end-of-text nuance and RTL
//! arrow-swap need a caret/locale port). No key autorepeat (run_dialog parity). No
//! compose (`Multi_key`). Modified keys (Ctrl/Alt/Super) are refused outright rather
//! than caret-handled, so GNOME's Ctrl+Enter open-new-window is unimplemented — a
//! middle-click/Ctrl result activation launches plainly, the same divergence the dash
//! records. The results grid renders over the still-visible window picker, and the
//! entry sits at a hardcoded y over the thumbnail strip — S5 (`ControlsManager`) does
//! the faithful layout + picker↔results cross-fade. GNOME's stage-capture
//! `reset-search` gesture (a click outside the entry/results clears the search) is not
//! ported: such a click falls through to the picker and the query survives until the
//! next overview enter. Like the panel and dash, the search draws on **every** output
//! with one shared selection/hover (GNOME shows it on the primary monitor only);
//! hit-testing is still per-output.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::overview_layout::ControlsLayout;
use crate::ui::text_edit::{EditMods, EditOutcome, KeyTheme, TextEdit};
use crate::ui::widget::{
    self, AppIcon, Entry, EntryContent, EntryHit, EntryLayout, Painter, SharedAppIconUploads,
};

/// The built-in `AppSearchProvider` result cap (`this.maxResults = 6`,
/// `appDisplay.js:1760`).
pub const MAX_RESULTS: usize = 6;

/// The entry pill's width on a canvas with this chrome ramp. Public because the entry now
/// floats rather than centering in a full-width bin, so [`crate::ui::overview_layout`] has
/// to know how wide it is to anchor it (and to keep the thumbnails strip clear of it).
pub fn entry_width(ramp: f64) -> f64 {
    (ENTRY_WIDTH * ramp).round()
}

/// Entry pill width, logical (`.search-entry` `width: 24em`; em = 11pt·4/3 ≈ 14.67px).
///
/// **Adaptive chrome, rule 2 — ramped** (`docs/fork/adaptive-overview-chrome.md`): this is
/// the width on a canvas at or above the reference; [`entry_width`] shrinks it below that,
/// so the pill keeps the *share* of the screen GNOME gives it. Its **height** does not
/// ramp — that is the entry's text plus padding, and text is exempt.
const ENTRY_WIDTH: f64 = 352.;
/// The resting puck's diameter — square, so `$forced_circular_radius` makes it a **circle**
/// holding nothing but the find glyph.
///
/// Sized as a *button* rather than as a collapsed text field: it reads a touch smaller than a
/// dash icon ([`crate::ui::dash::ICON_PX`] = 64), the biggest round-ish target on the overview,
/// so it looks deliberate beside it instead of like a shrunken entry. It does **not** ramp with
/// the chrome — like the pill's height, this is the size of a hand target, not a share of the
/// canvas.
///
/// **Divergence (approved).** GNOME shows the full 24em pill at rest with a "Type to search"
/// hint inside it (`overviewControls.js:324-331`). We rest as a puck at the right end of the
/// same footprint and grow to GNOME's pill on click or on the first keystroke. Everything about
/// the expanded entry — width, height, radius, fill, icon insets, font — is still GNOME's; only
/// the resting state and the transition are ours.
const PUCK_D: f64 = 56.;
/// The find glyph's size inside the resting puck. Bigger than [`Entry::ICON_PX`], which is the
/// glyph *in the pill* — a 16px magnifier in a 56px disc reads as a mis-drawn icon.
///
/// Two fixed sizes cross-faded, never a size lerped with `expand`: [`IconCache`] is keyed on
/// `(name, px, color)`, so a per-frame px would re-rasterize the SVG every animation frame and
/// accrete a cache entry per step.
const PUCK_ICON_PX: f64 = 24.;
/// `.search-entry` `margin-top: $base_padding*2` (`_search-entry.scss:4`).
const ENTRY_MARGIN_TOP: f64 = 12.;
/// `.search-entry` `margin-bottom: $base_padding` (`_search-entry.scss:5`).
const ENTRY_MARGIN_BOTTOM: f64 = 6.;

/// What the search entry asks [`crate::ui::overview_layout`] for: the control plus its margins,
/// which is what gnome-shell's `searchEntryBin` reports as its preferred height
/// (`overviewControls.js:165`).
///
/// The control here is the **puck**, not the pill: it is the taller of the two states, and a
/// band sized for the pill would leave the resting button overhanging the thumbnail strip. The
/// pill then centres inside the puck's footprint rather than sitting on GNOME's literal
/// `margin-top`, which is the price of [`PUCK_D`]'s divergence.
pub const PREFERRED_ENTRY_HEIGHT: f64 = ENTRY_MARGIN_TOP + PUCK_D + ENTRY_MARGIN_BOTTOM;

/// How far the *control's* vertical middle sits below the top of the bin
/// [`PREFERRED_ENTRY_HEIGHT`] reserves — the puck's centre, which is also the expanded
/// pill's (the pill centres inside the puck's footprint, see [`PUCK_D`]).
///
/// Published because the overview's workspace row is anchored to it: the row's top sits on
/// the entry control's midline (`crate::ui::overview_layout::ControlsLayout::workspace_row`).
pub const ENTRY_CONTROL_MID_Y: f64 = ENTRY_MARGIN_TOP + PUCK_D / 2.;

/// Full-color app-icon side in a result tile, logical: `GridSearchResult` builds a
/// default `IconGrid.BaseIcon` (`search.js:144-146`), whose size is `ICON_SIZE`
/// (`iconGrid.js:11,83`) — bigger than the dash's 64.
const RESULT_ICON_PX: f64 = 96.;
/// Result-tile label point size. `.search-result` `@extend .overview-tile`
/// (`_search-results.scss:59`), which sets no `font-size` — so, like the grid, an app
/// name here renders at the inherited stage size.
const LABEL_PT: f64 = crate::ui::BASE_FONT_PT;
/// `.overview-tile` padding — the selection fill sits on the outer tile here, not
/// on the inner icon the way the dash does. See [`AppIcon::OVERVIEW_TILE_PADDING`].
const TILE_PAD: f64 = AppIcon::OVERVIEW_TILE_PADDING;
/// Result-tile side. The tile is `.overview-tile` padding around a `Shell.SquareBin`
/// whose preferred width is its preferred height (`shell-square-bin.c:14-30`), so it
/// is **square**, sized by icon + `.overview-icon-with-label` spacing (`$base_padding`,
/// `_app-grid.scss:31-35`) + one label line. A longer label ellipsizes rather than
/// widening the tile. Kept in step with [`widget::TileMetrics::overview`] by
/// `search_tiles_match_the_shared_overview_metrics`.
///
/// Taken from the shared metrics rather than restated: the `+ 18.` this used to spell out is a
/// caption *line box*, which rides the realized font, so a literal copy here quietly stopped
/// agreeing with the tile the app grid draws.
fn tile_side() -> f64 {
    widget::TileMetrics::overview().size().w
}
fn tile_w() -> f64 {
    tile_side()
}
fn tile_h() -> f64 {
    tile_side()
}
/// How far a resting caption hangs below its tile ([`widget::TileMetrics::caption_overhang`]).
/// The card is one bake, so it reserves this or the last line is simply not drawn.
///
/// A result caption is the grid's: GNOME's is one ellipsized line on both surfaces
/// (`expandTitleOnHover: false` only stops results *expanding* on hover — the resting
/// line count is `StLabel`'s, `st-label.c:331`), so the divergence the grid takes is the
/// same divergence here.
const LABEL_OVERHANG: f64 = 18. * (widget::TILE_LABEL_LINES as f64 - 1.);
/// Gap between grid tiles (`.grid-search-results` `spacing: $base_padding*5`=30).
const GRID_SPACING: f64 = 30.;
/// `.search-section-content` padding (`$base_padding*2`=12).
const CARD_PAD: f64 = 12.;
/// `.search-section-content` corner radius (`$modal_radius*1.5`=24).
const CARD_RADIUS: f64 = 24.;
/// Gap from the top of the results strip to the card (`.search-section` `spacing`).
const SECTION_SPACING: f64 = 18.;
/// "No results" status text size (`.search-statustext` `%title_1` — 20pt/800).
const STATUS_PT: f64 = 20.;
/// Width of the empty-state ("No results") card — wide enough for the 20pt status
/// string, which a tile-width card would clip. Matches the entry pill's width.
const STATUS_CARD_W: f64 = ENTRY_WIDTH;

/// One app result — a plain-data snapshot (not a live catalog borrow), like
/// [`crate::ui::dash::DashEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultEntry {
    pub id: String,
    pub name: String,
    pub icon: AppIconRef,
}

/// What a point over the search UI hit. Nothing is hittable while the search is
/// inactive (the entry is then a passive hint over the thumbnail strip); once active
/// the entry body consumes inertly as [`Background`](SearchHit::Background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchHit {
    /// The entry's trailing clear glyph.
    Clear,
    /// Result tile at index `.0`.
    Result(usize),
    /// The entry's body — clicking it focuses (and so expands) the entry, the way
    /// gnome-shell's `searchEntryBin` click grabs key focus.
    Field,
    /// The results card background (padding/gaps) — consumes the click, no action.
    Background,
}

/// What a key did to the search — the caller applies the side effects (the model has
/// no catalog / overview handle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchOutcome {
    /// Consumed, no further action (selection move, Enter with no results).
    Handled,
    /// Not a search key — the caller must NOT consume it (fall through to the overview
    /// binds). Keeps bare modifiers/F-keys from being swallowed while searching.
    Ignored,
    /// The query changed; the caller re-runs the search (`sync_overview_search`).
    QueryChanged,
    /// The search was cleared (Escape while active); re-sync to empty.
    Cleared,
    /// Launch this app id, then close the overview (Enter on the selected result).
    Activate(String),
    /// Close the overview (Escape while inactive — normally unreachable here, see the
    /// key-routing note in `src/input/mod.rs`; the real inactive-Escape close is the
    /// hardcoded overview bind).
    Close,
}

#[derive(Default)]
struct SearchCache {
    context: Option<ContextId<VkTexture>>,
    entry_bake: widget::BakeCache,
    /// The card background + selection/hover wash + "No results" status (below the
    /// labels). Re-bakes on a highlight change, but only rounded fills — no re-shape.
    bg_bake: widget::BakeCache,
    /// The tile labels (above the wash) — hover/selection-independent so a highlight
    /// change never re-shapes them.
    results_bake: widget::BakeCache,
    /// Full-color result-icon uploads (shared key space with the dash's).
    icons: SharedAppIconUploads,
}

/// The overview search model. Owned on `Synoik`; fed results by `sync_overview_search`.
pub struct OverviewSearch {
    /// The editable query — caret, selection and all (see [`crate::ui::text_edit`]).
    edit: TextEdit,
    /// Whether the entry is grown to GNOME's pill. Distinct from [`Self::is_active`]: you can
    /// be expanded with an empty query (you clicked the puck and have not typed yet).
    expanded: bool,
    /// The expand animation's progress, 0 = puck, 1 = pill. Pushed each frame by
    /// `Synoik::update_overview_search_expand`, so hit-testing follows the animating pill
    /// instead of snapping to its destination.
    expand: f64,
    results: Vec<SearchResultEntry>,
    /// Keyboard-selected (default) result index; meaningful only when `!results.empty()`.
    selected: usize,
    /// Mouse hover (separate from `selected` — GNOME's Enter follows keynav, never the
    /// pointer).
    hovered: Option<SearchHit>,
    /// Bumped on any change that affects the bake (query/results/selection/hover).
    content_rev: u64,
    cache: RefCell<SearchCache>,
}

impl Default for OverviewSearch {
    fn default() -> Self {
        Self::new()
    }
}

/// Tokenize a search string like GNOME's `getTermsForSearchString`
/// (`searchController.js:19-24`): trim, split on whitespace, drop empties.
pub fn tokenize(query: &str) -> Vec<&str> {
    query.split_whitespace().collect()
}

impl OverviewSearch {
    pub fn new() -> Self {
        Self {
            edit: TextEdit::new(),
            expanded: false,
            expand: 0.,
            results: Vec::new(),
            selected: 0,
            hovered: None,
            content_rev: 0,
            cache: RefCell::new(SearchCache::default()),
        }
    }

    /// Whether a search is active (`terms.length > 0`, `_setSearchActive`). Derived
    /// from the query, like GNOME's cached `_searchActive`.
    pub fn is_active(&self) -> bool {
        self.edit.text().split_whitespace().next().is_some()
    }

    /// Show (or clear) the input method's in-progress composition in the entry.
    pub fn set_preedit(&mut self, preedit: Option<String>) -> bool {
        self.edit.set_preedit(preedit)
    }

    pub fn query(&self) -> &str {
        self.edit.text()
    }

    /// Whether the entry is (or is becoming) the full pill rather than the resting puck.
    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Grow to the pill — a click on the entry, or the first keystroke. Idempotent.
    pub fn expand(&mut self) {
        self.expanded = true;
    }

    /// The animated expand progress Synoik pushes in, 0 = puck, 1 = pill.
    pub fn set_expand(&mut self, progress: f64) {
        self.expand = progress.clamp(0., 1.);
    }

    /// The selected result's id, if any (what Enter activates).
    pub fn selected_id(&self) -> Option<&str> {
        self.results.get(self.selected).map(|e| e.id.as_str())
    }

    /// The id of result `i`, if present (what a click activates).
    pub fn result_id(&self, i: usize) -> Option<&str> {
        self.results.get(i).map(|e| e.id.as_str())
    }

    /// The icon of result `i`, if present (what a drag of that tile carries).
    pub fn result_icon(&self, i: usize) -> Option<&crate::app_system::AppIconRef> {
        self.results.get(i).map(|e| &e.icon)
    }

    /// Feed a key while the overview search is engaged. Press-only: key releases are
    /// owned globally by `should_intercept_key` via the shared `suppressed_keys`, so
    /// this never sees (and must not track) releases.
    ///
    /// `plain` is "no Ctrl/Alt/Super held". A modified key never reaches GNOME's entry
    /// as a bare one (Clutter.Text ignores or caret-handles it), so we refuse them all
    /// — otherwise Ctrl+Escape would clear the search, Alt+arrows would move the
    /// selection, and Super+Enter would launch. They fall through unconsumed instead
    /// (harmlessly: `hardcoded_overview_bind` requires empty modifiers).
    pub fn handle_key(
        &mut self,
        raw: Option<Keysym>,
        text: Option<char>,
        mods: EditMods,
        // `org.gnome.desktop.interface gtk-key-theme`, read live at the call site — the same
        // way the other four entries take it. It used to be a field with a setter nothing
        // called, so this entry was the one that silently ignored the setting.
        theme: KeyTheme,
    ) -> SearchOutcome {
        // --- Result navigation comes first, because these keys mean navigation to the
        // *results view*, not to the caret (`searchController.js:274-311`).
        //
        // Tab / Shift-Tab always navigate. Down/Up always navigate — they are line motion
        // on a one-line entry, so the caret has nothing to do with them. Right navigates
        // only when the caret is already at the end with nothing selected, which is
        // gnome-shell's `cursor_position === -1` guard; otherwise it moves the caret.
        // Left is *not* a navigation key in gnome-shell at all, and is now a caret move
        // here too — a divergence from our own MVP, which mapped it to select-prev
        // because there was no caret to move.
        if mods.is_plain() {
            let at_end =
                self.edit.cursor() == self.edit.text().len() && self.edit.selection().is_none();
            let nav = match raw {
                Some(Keysym::Tab) => Some(!mods.shift),
                Some(Keysym::ISO_Left_Tab) => Some(false),
                Some(Keysym::Up | Keysym::KP_Up) => Some(false),
                Some(Keysym::Down | Keysym::KP_Down) => Some(true),
                Some(Keysym::Right | Keysym::KP_Right) if at_end => Some(true),
                _ => None,
            };
            if let Some(forward) = nav {
                if forward {
                    self.select_next();
                } else {
                    self.select_prev();
                }
                return SearchOutcome::Handled;
            }
        }

        // --- Everything else is the entry's. The shared model owns the bindings; this
        // only maps its outcome onto the search's own policy.
        let was = self.edit.text().to_owned();
        match self.edit.handle_key(raw, text, mods, theme) {
            EditOutcome::Activate => {
                // activateDefault: launch the selected result, else consume (must NOT
                // fall through, or the hardcoded Return bind would toggle the overview).
                match self.selected_id() {
                    Some(id) if self.is_active() => SearchOutcome::Activate(id.to_owned()),
                    _ => SearchOutcome::Handled,
                }
            }
            EditOutcome::Cancel => {
                // Escape tier 1: a live search clears but stays open. With nothing to
                // clear the entry collapses and the caller closes the next tier down.
                if self.is_active() || self.expanded {
                    let was_active = self.is_active();
                    self.clear();
                    if was_active {
                        return SearchOutcome::Cleared;
                    }
                    return SearchOutcome::Handled;
                }
                SearchOutcome::Close
            }
            EditOutcome::Changed => {
                // Typing is what grows the puck into the pill.
                self.expanded = true;
                if self.edit.text() != was {
                    self.on_query_changed();
                    SearchOutcome::QueryChanged
                } else {
                    SearchOutcome::Handled
                }
            }
            EditOutcome::Moved => SearchOutcome::Handled,
            // A key the search doesn't handle (bare modifier, F-key, …) — do not consume it.
            EditOutcome::Ignored => SearchOutcome::Ignored,
        }
    }

    fn on_query_changed(&mut self) {
        // GNOME re-selects the first result whenever results update; reset here so
        // Enter after a Backspace activates the new first result.
        self.selected = 0;
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    fn select_prev(&mut self) {
        self.set_selected(self.selected.saturating_sub(1));
    }

    fn select_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.set_selected((self.selected + 1).min(self.results.len() - 1));
    }

    fn set_selected(&mut self, i: usize) {
        // No `content_rev` bump: the selection wash is a separate element (the caller
        // always redraws on a handled key), so moving it never re-shapes the labels.
        self.selected = i;
    }

    /// Replace the result snapshot (from `sync_overview_search`), clamping the
    /// selection into range.
    pub fn set_results(&mut self, results: Vec<SearchResultEntry>) {
        if results == self.results {
            return;
        }
        self.results = results;
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// Clear the query and results (overview enter/reset, Escape-while-active).
    pub fn clear(&mut self) {
        if self.edit.is_empty()
            && self.results.is_empty()
            && self.hovered.is_none()
            && !self.expanded
        {
            return;
        }
        // Clearing puts the entry back to its resting puck: there is nothing left to hold it
        // open, and leaving an empty pill up would be a third state with no meaning.
        self.expanded = false;
        self.edit.clear();
        self.results.clear();
        self.selected = 0;
        self.hovered = None;
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// Set the mouse-hovered element; returns whether it changed (→ redraw). Does not
    /// bump `content_rev`: the hover wash is a separate element, so a hover change (every
    /// mouse move) repositions it without re-shaping the result labels.
    pub fn set_hovered(&mut self, hovered: Option<SearchHit>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        true
    }

    /// Draw from `shared` instead of this surface's own upload map, so an icon already
    /// on the GPU for another surface is not uploaded again (see [`SharedAppIconUploads`]).
    pub fn share_icon_uploads(&self, shared: &SharedAppIconUploads) {
        self.cache.borrow_mut().icons = shared.clone();
    }

    /// The map this surface draws from.
    pub fn icon_uploads(&self) -> SharedAppIconUploads {
        self.cache.borrow().icons.clone()
    }

    /// Drop cached icon uploads (icon-theme / installed change).
    pub fn clear_icon_uploads(&self) {
        self.cache.borrow().icons.borrow_mut().clear();
    }

    /// Drop one icon's uploads, so the next frame re-uploads it from the freshly
    /// decoded pixels — see [`widget::drop_app_icon_upload`].
    pub fn drop_icon_upload(&self, icon: &crate::app_system::AppIconRef, logical_px: u16) {
        crate::ui::widget::drop_app_icon_upload(
            &mut self.cache.borrow_mut().icons.borrow_mut(),
            icon,
            logical_px,
        );
    }

    /// Lay out the entry pill + (when active) the results card + tiles inside the
    /// boxes [`crate::ui::overview_layout`] allocated: the entry bin at the top of
    /// the work area, the results strip spanning everything between it and the dash.
    fn layout(&self, area: SearchArea) -> SearchLayout {
        // The pill grows leftward out of the puck: its **right** edge is pinned to the
        // right edge of the box the controls layout allocated, so nothing else has to move
        // as it opens.
        let full_w = entry_width(area.ramp);
        let e = self.expand;
        let pill_w = PUCK_D + (full_w - PUCK_D) * e;
        // The puck shrinks to the pill's height about a **fixed centre**, so the control opens
        // symmetrically instead of hinging on one edge. That centre is the puck's own, since the
        // band is reserved for the puck (see `PREFERRED_ENTRY_HEIGHT`).
        let pill_h = PUCK_D + (Entry::HEIGHT - PUCK_D) * e;
        let center_y = area.entry.loc.y + ENTRY_MARGIN_TOP + PUCK_D / 2.;
        let right = area.entry.loc.x + area.entry.size.w;
        let entry = Entry::layout(
            right - pill_w / 2.,
            center_y - pill_h / 2.,
            pill_w,
            pill_h,
            widget::EntryStyle::Search,
        );
        // The find glyph rides from the puck's centre to the pill's leading gutter. It has
        // to be lerped rather than read off `EntryLayout`, whose `primary_icon` is only the
        // *expanded* position — at puck width the two happen to be a pixel apart, and
        // taking the layout's would make the icon jump at the start of the animation
        // instead of sliding.
        let find_icon = Point::from((
            entry.pill.loc.x + (pill_w / 2.) + (Entry::ICON_INSET - pill_w / 2.) * e,
            entry.primary_icon.y,
        ));
        let active = self.is_active();
        let (card, tiles) = if !active {
            (None, Vec::new())
        } else {
            let card_w = if self.results.is_empty() {
                // The "No results" status card: sized for its own %title_1 text, not for
                // a (nonexistent) tile — a tile-width card would clip the string.
                STATUS_CARD_W
            } else {
                let n = self.results.len() as f64;
                n * tile_w() + (n - 1.) * GRID_SPACING + 2. * CARD_PAD
            };
            let card_h = tile_h() + LABEL_OVERHANG + 2. * CARD_PAD;
            let card_x = (area.results.loc.x + (area.results.size.w - card_w) / 2.).round();
            let card_y = (area.results.loc.y + SECTION_SPACING).round();
            let card = Rectangle::new(Point::from((card_x, card_y)), Size::from((card_w, card_h)));
            let tiles = (0..self.results.len())
                .map(|i| {
                    let tx = card_x + CARD_PAD + i as f64 * (tile_w() + GRID_SPACING);
                    Rectangle::new(
                        Point::from((tx, card_y + CARD_PAD)),
                        Size::from((tile_w(), tile_h())),
                    )
                })
                .collect();
            (Some(card), tiles)
        };

        SearchLayout {
            entry,
            find_icon,
            pill_w,
            pill_h,
            card,
            tiles,
        }
    }

    /// Which interactive element is under `pos` (logical, output coords).
    ///
    /// The entry pill is a visible, opaque control whether or not a search is
    /// active, so it always consumes: its body is inert
    /// ([`SearchHit::Background`]) rather than falling through to the workspace
    /// behind it, which would leave the overview. The clear icon only exists
    /// while there is something to clear.
    ///
    /// Divergence: gnome-shell focuses the entry on that click; we have no
    /// click-to-focus yet, and typing engages the search from anywhere in the
    /// overview regardless.
    pub fn hit_test(&self, pos: Point<f64, Logical>, area: SearchArea) -> Option<SearchHit> {
        let layout = self.layout(area);
        // The clear glyph is hittable only once the pill is fully open. Its hit disc is a
        // generous 32px across (a 16px glyph deserves a bigger target), which is fine inside
        // a 352px pill and catastrophic inside the 56px puck: the disc covers most of the puck,
        // so any click on a resting-but-active entry would land on Clear and wipe the query.
        // It is also exactly when the glyph finishes fading in, so what you can hit is what
        // you can see.
        let clear_live = self.is_active() && self.expand >= 1.;
        match Entry::hit(&layout.entry, pos, clear_live) {
            Some(EntryHit::Trailing) => return Some(SearchHit::Clear),
            // The body is a focusable control, resting puck or open pill alike: clicking it
            // grows the entry, the way clicking gnome-shell's `searchEntryBin` grabs key
            // focus. It still consumes when already open, so a click inside the pill never
            // falls through to the picker behind it and leaves the overview.
            Some(EntryHit::Field) => return Some(SearchHit::Field),
            None => {}
        }
        if let Some(card) = layout.card {
            if card.contains(pos) {
                for (i, tile) in layout.tiles.iter().enumerate() {
                    if tile.contains(pos) {
                        return Some(SearchHit::Result(i));
                    }
                }
                return Some(SearchHit::Background);
            }
        }

        // While searching, the results strip covers the whole space between the
        // entry and the dash and is reactive there (gnome-shell allocates its
        // `searchController` that strip and cross-fades it over the picker rather
        // than carving space out of it — `overviewControls.js:242-245,609-643`).
        // Without this a click beside the card would reach the faded-out picker
        // and be read as "clicked empty desktop", leaving the overview.
        if self.is_active() && area.results.contains(pos) {
            return Some(SearchHit::Background);
        }

        None
    }

    /// The entry pill box **as currently drawn** — a geometry probe for the render test.
    /// At rest this is the collapsed puck; use [`Self::expanded_entry_pill`] for the open
    /// pill's dimensions, which is what the adaptive-chrome ramp is about.
    #[cfg(test)]
    pub fn entry_pill(&self, area: SearchArea) -> Rectangle<f64, Logical> {
        self.layout(area).entry.pill
    }

    /// The pill at full expansion, whatever the entry is doing right now — the box the
    /// chrome ramp sizes.
    #[cfg(test)]
    pub fn expanded_entry_pill(&self, area: SearchArea) -> Rectangle<f64, Logical> {
        let mut open = OverviewSearch::new();
        open.expand = 1.;
        open.layout(area).entry.pill
    }

    /// The label-bake revision — a test probe for the invariant that a highlight change
    /// (hover / keyboard selection) does not invalidate it (which would re-shape the
    /// result labels every mouse move / arrow key).
    #[cfg(test)]
    pub fn content_rev(&self) -> u64 {
        self.content_rev
    }

    /// Result tile `i`'s box — a geometry probe for the render test, which needs
    /// the tile edges (not just its center) to sample the selection fill.
    #[cfg(test)]
    pub fn result_tile(&self, i: usize, area: SearchArea) -> Option<Rectangle<f64, Logical>> {
        self.layout(area).tiles.get(i).copied()
    }

    /// The logical center of result tile `i`, or `None` if out of range. Used by a
    /// drag, to keep the icon under the point it was grabbed by, and by the
    /// conformance corpus, which clicks real pixels routed through
    /// [`hit_test`](Self::hit_test).
    pub fn result_center(&self, i: usize, area: SearchArea) -> Option<Point<f64, Logical>> {
        self.result_rect(i, area)
            .map(|t| Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.)))
    }

    /// Result `i`'s tile rect — what a context menu anchors on.
    pub fn result_rect(&self, i: usize, area: SearchArea) -> Option<Rectangle<f64, Logical>> {
        self.layout(area).tiles.get(i).copied()
    }

    /// The search render elements for `output`, faded by overview `progress` (0..1).
    /// Icons/glyphs are pushed first (topmost); chrome bakes last (below) — the dash
    /// order.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        area: SearchArea,
        fade: SearchFade,
        // `org.gnome.desktop.interface accent-color`. The pill draws no focus ring of its own,
        // but its **selection** is `st-transparentize(-st-accent-color, 0.7)` like every other
        // entry's, so this is not the unused argument it used to be.
        accent: [u8; 3],
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let SearchFade { overview, search } = fade;
        let scale = output.current_scale().fractional_scale();
        let layout = self.layout(area);
        let alpha = overview as f32;
        // The entry bin is always on screen (gnome-shell's `searchEntryBin` is not
        // part of the cross-fade); only the results strip fades in over the picker.
        let results_alpha = (overview * search) as f32;

        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.borrow_mut().clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::new();
        let active = self.is_active();

        // --- Entry glyphs (topmost): the find icon, and the clear icon when active. ---
        // Asked for at FULL tint, with `progress` applied as the element alpha. Folding
        // the fade into the tint instead would miss the `IconCache` key
        // (`(name, px, color)`) on every animation frame — re-rasterizing the SVG *and*
        // re-uploading it, and accreting a cached entry per alpha step.
        // The clear glyph fades in with the pill it lives in — at puck width there is no
        // room for it, and it would sit on top of the find glyph.
        // The find glyph exists at two fixed sizes — the puck's and the pill's — cross-faded on
        // the same centre, for the reason `PUCK_ICON_PX` gives: a lerped px would re-rasterize
        // per frame.
        let expand = self.expand as f32;
        for (name, px, color, center, want, glyph_alpha) in [
            (
                "edit-find-symbolic",
                PUCK_ICON_PX,
                style::MUTED,
                layout.find_icon,
                true,
                1. - expand,
            ),
            (
                "edit-find-symbolic",
                Entry::ICON_PX,
                style::MUTED,
                layout.find_icon,
                true,
                expand,
            ),
            (
                "edit-clear-symbolic",
                Entry::ICON_PX,
                style::TEXT,
                layout.entry.secondary_icon,
                active,
                expand,
            ),
        ] {
            if !want || glyph_alpha <= 0. {
                continue;
            }
            let Some(tb) = sym_icons.texture(renderer, name, px, scale, color) else {
                continue;
            };
            let logical = tb.logical_size();
            let loc = center - Point::from((logical.w / 2., logical.h / 2.));
            elements.push(TextureRenderElement::from_texture_buffer(
                tb,
                loc,
                alpha * glyph_alpha,
                None,
                None,
                Kind::Unspecified,
            ));
        }

        // --- Result app icons (topmost, over their tiles). ---
        for (i, entry) in self.results.iter().enumerate() {
            let tile = layout.tiles[i];
            let center = Point::from((
                tile.loc.x + tile.size.w / 2.,
                tile.loc.y + TILE_PAD + RESULT_ICON_PX / 2.,
            ));
            if let Some(el) = widget::app_icon_element(
                renderer,
                &mut cache.icons.borrow_mut(),
                app_icons,
                &entry.icon,
                RESULT_ICON_PX,
                scale,
                Point::from((0., 0.)),
                center,
                results_alpha,
            ) {
                elements.push(el);
            }
        }

        // --- The entry pill chrome (text/placeholder + caret), baked. ---
        // The pill's own size — the bake is the chrome, so it has to be what the layout
        // placed, ramp and expansion included, or the two disagree.
        let pill_w = layout.pill_w;
        let pill_h = layout.pill_h;
        match Entry::bake(
            renderer,
            &mut cache.entry_bake,
            scale,
            pill_w,
            pill_h,
            // No placeholder inside the pill: at rest the control is a labelled-by-shape
            // button, and once the pill is open the caret is the invitation.
            EntryContent::of(&self.edit, "", self.expanded),
            widget::EntryStyle::Search,
            // The search entry's focus is its caller's inset-accent ring, not the pill's.
            false,
            self.is_active(),
            // Not a focus ring (this style has none) — the selection wash.
            widget::style::accent_rgba(accent),
            // The size is part of what was baked; a canvas change or a step of the expansion
            // must re-bake it.
            widget::Revision::new()
                .of(self.content_rev)
                .px(pill_w)
                .px(pill_h)
                .of(accent)
                .done(),
        ) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    layout.entry.pill.loc,
                    alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error baking the search entry: {err:#}"),
        }

        // --- The results card in two z-layers so a highlight change (every mouse move /
        //     arrow key) never re-shapes the labels. The labels bake is keyed on
        //     `content_rev` alone (results, never the highlight); the card bake — background
        //     + selection/hover wash + "No results" status — re-bakes when the highlight
        //     moves, but that is only rounded fills (no label shaping), so it is cheap. The
        //     wash rides along in the card bake (rather than as its own element) simply because
        //     it re-bakes on the same key as the background it sits on — one bake, not two.
        //     The result icons were pushed above both. ---
        if active {
            if let Some(card) = layout.card {
                let origin = card.loc;
                let card_size = card.size;
                let empty = self.results.is_empty();
                let mut push_card_layer = |renderer: &VulkanRenderer, texture| {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        vec![],
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        card.loc,
                        results_alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                };

                // (1) Labels — highlight-independent, so hover/selection never re-shape them.
                let rel_rects: Vec<Rectangle<f64, Logical>> = layout
                    .tiles
                    .iter()
                    .map(|t| Rectangle::new(t.loc - origin, t.size))
                    .collect();
                // A search result's caption never expands — the provider builds its icons
                // with `expandTitleOnHover: false` (`appDisplay.js:1837-1841`) — so it is
                // always the resting, end-ellipsized form, at the same line count the grid
                // rests at (see [`LABEL_OVERHANG`], which is what makes room for it).
                let label_w = widget::TileMetrics::overview().label_w();
                let names: Vec<Vec<String>> = self
                    .results
                    .iter()
                    .map(|e| {
                        widget::tile_label_lines(
                            &e.name,
                            LABEL_PT,
                            label_w,
                            widget::TILE_LABEL_LINES,
                            false,
                        )
                    })
                    .collect();
                let label_rects = rel_rects.clone();
                match widget::bake(
                    renderer,
                    &mut cache.results_bake,
                    scale,
                    card_size,
                    self.content_rev,
                    move |r| {
                        let mut shaper = widget::TextShaper::new(r, scale);
                        names
                            .iter()
                            .map(|lines| {
                                lines
                                    .iter()
                                    .map(|line| {
                                        shaper.shape(line, widget::TextStyle::new(LABEL_PT))
                                    })
                                    .collect::<anyhow::Result<Vec<_>>>()
                            })
                            .collect::<anyhow::Result<Vec<_>>>()
                    },
                    move |frame, phys, labels| {
                        let mut p = Painter::new(frame, scale, phys);
                        p.clear(style::TRANSPARENT)?;
                        for (rel, label) in label_rects.iter().zip(labels.iter()) {
                            p.labelled_tile(
                                *rel,
                                label,
                                &widget::TileMetrics::overview(),
                                false,
                                style::TEXT,
                            )?;
                        }
                        Ok(())
                    },
                ) {
                    Ok(texture) => push_card_layer(renderer, texture),
                    Err(err) => tracing::error!("error baking the search labels: {err:#}"),
                }

                // (2) Card background + selection/hover wash + "No results" status (bottom).
                //     Re-bakes on a highlight change, but only rounded fills — no re-shape.
                let selected = self.selected;
                let hovered = self.hovered;
                // The wash covers the caption, which may run past the tile box — GNOME's
                // tile allocation follows its label, so the highlight grows with it (the
                // app grid does the same for an expanded caption).
                let wash_extra: Vec<f64> = self
                    .results
                    .iter()
                    .map(|e| {
                        let lines = widget::tile_label_lines(
                            &e.name,
                            LABEL_PT,
                            label_w,
                            widget::TILE_LABEL_LINES,
                            false,
                        )
                        .len();
                        18. * (lines as f64 - 1.)
                    })
                    .collect();
                // Highlight packed into the bake revision so a move re-bakes the wash.
                let hover_idx = match hovered {
                    Some(SearchHit::Result(i)) => i as u64 + 1,
                    _ => 0,
                };
                let card_rev = (self.content_rev << 40)
                    | ((selected as u64 & 0xF_FFFF) << 20)
                    | (hover_idx & 0xF_FFFF);
                match widget::bake(
                    renderer,
                    &mut cache.bg_bake,
                    scale,
                    card_size,
                    card_rev,
                    move |r| {
                        if empty {
                            let mut shaper = widget::TextShaper::new(r, scale);
                            Ok(Some(shaper.shape(
                                "No results",
                                widget::TextStyle::new(STATUS_PT).bold(),
                            )?))
                        } else {
                            Ok(None)
                        }
                    },
                    move |frame, phys, status| {
                        let mut p = Painter::new(frame, scale, phys);
                        p.clear(style::TRANSPARENT)?;
                        // `.search-section-content` — a plate on the overview backdrop, so it
                        // takes the shared translucent fill rather than the opaque
                        // `$system_overlay_bg_color` (`widget::style::OVERVIEW_PLATE`).
                        p.fill_rounded_full(CARD_RADIUS, widget::style::OVERVIEW_PLATE)?;
                        // Selection always washes; a hovered result adds/overlaps one.
                        for (i, rel) in rel_rects.iter().enumerate() {
                            if i == selected || hovered == Some(SearchHit::Result(i)) {
                                let grown = Rectangle::new(
                                    rel.loc,
                                    Size::from((
                                        rel.size.w,
                                        rel.size.h + wash_extra.get(i).copied().unwrap_or(0.),
                                    )),
                                );
                                p.fill_rounded(
                                    grown,
                                    widget::TileMetrics::overview().radius,
                                    style::HOVER_WASH,
                                )?;
                            }
                        }
                        if let Some(status) = status {
                            // Centered "No results" (`.search-statustext`).
                            p.text_band(
                                status,
                                card_size.w / 2.,
                                widget::HAlign::Center,
                                0.,
                                card_size.h,
                                style::MUTED,
                                Rectangle::from_size(card_size),
                            )?;
                        }
                        Ok(())
                    },
                ) {
                    Ok(texture) => push_card_layer(renderer, texture),
                    Err(err) => tracing::error!("error baking the search card: {err:#}"),
                }
            }
        }

        elements
    }
}

use widget::style;

/// Computed geometry for one output size.
/// How opaque the search chrome is: `overview` is the overview's own fade-in
/// (everything the overview draws rides it), `search` is gnome-shell's
/// search cross-fade, which the *results* ride and the entry does not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchFade {
    pub overview: f64,
    pub search: f64,
}

/// The two boxes [`crate::ui::overview_layout`] allocates the search: the entry
/// bin at the top of the work area, and the results strip spanning everything
/// between the entry and the dash (gnome-shell's `searchController`, which
/// overlaps the thumbnails and the picker rather than carving space out of
/// them — `overviewControls.js:242-245`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchArea {
    pub entry: Rectangle<f64, Logical>,
    pub results: Rectangle<f64, Logical>,
    /// The canvas's chrome ramp (`ControlsLayout::chrome_ramp`).
    pub ramp: f64,
}

impl From<ControlsLayout> for SearchArea {
    fn from(l: ControlsLayout) -> Self {
        Self {
            entry: l.search_entry,
            results: l.search_results,
            ramp: l.chrome_ramp,
        }
    }
}

struct SearchLayout {
    entry: EntryLayout,
    /// The find glyph's centre, lerped between the puck's middle and the pill's gutter.
    find_icon: Point<f64, Logical>,
    /// The pill's current size — what the chrome is baked at.
    pill_w: f64,
    pill_h: f64,
    /// The results card (`None` when search inactive).
    card: Option<Rectangle<f64, Logical>>,
    /// Result-tile boxes (empty unless active with results).
    tiles: Vec<Rectangle<f64, Logical>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> SearchResultEntry {
        SearchResultEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            icon: AppIconRef::Fallback,
        }
    }

    #[test]
    fn tokenize_trims_and_splits() {
        assert_eq!(tokenize("  firefox  "), vec!["firefox"]);
        assert_eq!(tokenize("web browser"), vec!["web", "browser"]);
        assert!(tokenize("   ").is_empty());
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn highlight_change_does_not_bump_the_label_bake_revision() {
        // The label bake is keyed on `content_rev`; hover + keyboard selection must not
        // touch it, or every mouse move / arrow key re-shapes the result labels (the
        // stutter that bites once providers make the result set large). Query/result
        // changes, which DO change the labels, still bump it.
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let rev = s.content_rev();
        assert!(
            s.set_hovered(Some(SearchHit::Result(1))),
            "a new hover reports a change"
        );
        s.handle_key(
            Some(Keysym::Down),
            None,
            EditMods::default(),
            KeyTheme::default(),
        ); // move the keyboard selection
        assert_eq!(
            s.content_rev(),
            rev,
            "hover + selection must not invalidate the label bake"
        );
        // A query change re-shapes (the labels differ).
        s.handle_key(None, Some('b'), EditMods::default(), KeyTheme::default());
        assert_ne!(s.content_rev(), rev, "a query change re-bakes");
    }

    #[test]
    fn typing_builds_query_and_activates() {
        let mut s = OverviewSearch::new();
        assert!(!s.is_active());
        assert_eq!(
            s.handle_key(None, Some('f'), EditMods::default(), KeyTheme::default()),
            SearchOutcome::QueryChanged
        );
        assert_eq!(
            s.handle_key(None, Some('i'), EditMods::default(), KeyTheme::default()),
            SearchOutcome::QueryChanged
        );
        assert!(s.is_active());
        assert_eq!(s.query(), "fi");

        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        assert_eq!(
            s.handle_key(
                Some(Keysym::Return),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::Activate("a.desktop".to_owned())
        );
    }

    /// Down/Up always navigate the results; Right only does so with the caret already at
    /// the end (gnome-shell's `cursor_position === -1` guard, `searchController.js:274-311`).
    #[test]
    fn arrow_moves_selection_and_clamps() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        // Right at the end of the text → index 1, then clamp at the last.
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("b.desktop"));
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Up back to 0, saturating.
        s.handle_key(
            Some(Keysym::Up),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        s.handle_key(
            Some(Keysym::Up),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("a.desktop"));
    }

    /// Left is caret motion, never navigation — gnome-shell does not bind it in the entry at
    /// all, and once the caret has left the end of the text Right stops navigating too.
    #[test]
    fn left_moves_the_caret_and_parks_right_navigation() {
        let mut s = OverviewSearch::new();
        for c in "ab".chars() {
            s.handle_key(None, Some(c), EditMods::default(), KeyTheme::default());
        }
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        s.handle_key(
            Some(Keysym::Left),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(
            s.selected_id(),
            Some("a.desktop"),
            "Left must not have stepped the selection"
        );
        // Typing now lands mid-string, which the old end-only caret could not do.
        s.handle_key(None, Some('x'), EditMods::default(), KeyTheme::default());
        assert_eq!(s.query(), "axb");
        // The caret is no longer at the end, so Right moves it rather than navigating.
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("a.desktop"));
        // Back at the end, Right navigates again.
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("b.desktop"));
    }

    /// An emptied-but-open entry still shows its caret. Backspacing the last character leaves
    /// a focused pill with no text and (for this entry) no placeholder either — gating the
    /// caret on "has text" made it draw literally nothing, which reads as a dead control.
    #[test]
    fn an_emptied_entry_still_has_a_caret() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.handle_key(
            Some(Keysym::BackSpace),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.query(), "");
        assert!(
            s.is_expanded(),
            "backspacing to empty leaves the pill open — only Escape/clear collapses it"
        );
        let content = widget::EntryContent::of(&s.edit, "", s.is_expanded());
        assert_eq!(
            content.cursor,
            Some(0),
            "the caret must survive an empty entry, or the open pill draws nothing at all"
        );
    }

    /// The key theme reaches the entry. It used to be a field with a setter nothing called, so
    /// this entry — the one this whole change is about — silently ignored the setting while the
    /// other four honored it.
    #[test]
    fn the_entry_honors_the_key_theme() {
        let mut s = OverviewSearch::new();
        for c in "one two".chars() {
            s.handle_key(None, Some(c), EditMods::default(), KeyTheme::Emacs);
        }
        // Ctrl-w is Emacs-only: in the default theme it falls through unconsumed.
        assert_eq!(
            s.handle_key(Some(Keysym::w), None, EditMods::ctrl(), KeyTheme::Default),
            SearchOutcome::Ignored
        );
        assert_eq!(s.query(), "one two");
        assert_eq!(
            s.handle_key(Some(Keysym::w), None, EditMods::ctrl(), KeyTheme::Emacs),
            SearchOutcome::QueryChanged
        );
        assert_eq!(s.query(), "one ");
    }

    /// The GNOME editing combos reach the query, which the old `push`/`pop` model could not
    /// express at all: a modified key was refused outright.
    #[test]
    fn gnome_editing_combos_reach_the_query() {
        let mut s = OverviewSearch::new();
        for c in "one two".chars() {
            s.handle_key(None, Some(c), EditMods::default(), KeyTheme::default());
        }
        assert_eq!(
            s.handle_key(
                Some(Keysym::BackSpace),
                None,
                EditMods::ctrl(),
                KeyTheme::default()
            ),
            SearchOutcome::QueryChanged
        );
        assert_eq!(
            s.query(),
            "one ",
            "Ctrl-BackSpace deletes the previous word"
        );
        s.handle_key(
            Some(Keysym::Home),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::ctrl_shift(),
            KeyTheme::default(),
        );
        s.handle_key(None, Some('t'), EditMods::default(), KeyTheme::default());
        assert_eq!(
            s.query(),
            "t ",
            "Ctrl-Shift-Right selected a word; typing replaced it"
        );
    }

    #[test]
    fn selection_never_underflows_on_empty() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), EditMods::default(), KeyTheme::default());
        // No results seeded.
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        s.handle_key(
            Some(Keysym::Left),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), None);
        // Enter with no results is consumed, not an activate.
        assert_eq!(
            s.handle_key(
                Some(Keysym::Return),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::Handled
        );
    }

    #[test]
    fn escape_clears_when_active_else_closes() {
        let mut s = OverviewSearch::new();
        // Inactive Escape → Close (normally unreachable via the input gate).
        assert_eq!(
            s.handle_key(
                Some(Keysym::Escape),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::Close
        );

        s.handle_key(None, Some('x'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A")]);
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(
                Some(Keysym::Escape),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::Cleared
        );
        assert!(!s.is_active());
        assert_eq!(s.query(), "");
        assert!(s.result_id(0).is_none());
    }

    #[test]
    fn backspace_empties_and_deactivates() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(
                Some(Keysym::BackSpace),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::QueryChanged
        );
        assert!(!s.is_active());
    }

    #[test]
    fn query_change_resets_selection_to_first() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        s.handle_key(
            Some(Keysym::Right),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Typing another char resets selection to the first.
        s.handle_key(None, Some('b'), EditMods::default(), KeyTheme::default());
        assert_eq!(s.selected(), 0);
    }

    #[test]
    fn unhandled_key_is_ignored_not_consumed() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default()); // active
                                                                                 // F5 (no text, not a nav key) must be Ignored so the caller doesn't eat it.
        assert_eq!(
            s.handle_key(
                Some(Keysym::F5),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::Ignored
        );
    }

    /// The boxes `overview_layout` allocates the search on 1920×1080 with the
    /// 35px panel strut.
    fn area_1080() -> SearchArea {
        let controls = crate::ui::overview_layout::layout(
            Size::from((1920., 1080.)),
            35.,
            crate::ui::overview_layout::Measured {
                search_entry_height: PREFERRED_ENTRY_HEIGHT,
                search_entry_width: entry_width(1.),
                search_entry_mid_y: ENTRY_CONTROL_MID_Y,
                dash_preferred_height: crate::ui::dash::preferred_height(Size::from((
                    1920., 1080.,
                ))),
            },
            crate::ui::overview_layout::state::WINDOW_PICKER,
        );
        controls.into()
    }

    /// The entry pill is opaque whether or not a search is active, so it always
    /// consumes its own clicks — falling through would hit the workspace behind
    /// it and leave the overview. Only the clear glyph and the result tiles act.
    #[test]
    fn entry_body_consumes_inactive_and_active() {
        let mut s = OverviewSearch::new();
        let area = area_1080();
        let layout = s.layout(area);
        // The *current* pill, not the expanded one: at rest this is the puck, and
        // hit-testing has to follow the box that is actually drawn.
        let entry_center = Point::from((
            layout.entry.pill.loc.x + layout.entry.pill.size.w / 2.,
            layout.entry.pill.loc.y + layout.entry.pill.size.h / 2.,
        ));
        assert_eq!(
            s.hit_test(entry_center, area),
            Some(SearchHit::Field),
            "the idle entry puck must consume rather than fall through"
        );
        // No query yet, so there is nothing to clear: that glyph is not live.
        assert_eq!(
            s.hit_test(layout.entry.secondary_icon, area),
            Some(SearchHit::Field)
        );
        // Well away from the entry: no hit at all.
        assert_eq!(s.hit_test(Point::from((10., 600.)), area), None);

        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        // Mid-grow the clear glyph is not hittable yet: its 32px disc would cover most of the
        // puck, so a click meant for the field would wipe the query.
        s.set_expand(0.5);
        let half = s.layout(area);
        assert_eq!(
            s.hit_test(half.entry.secondary_icon, area),
            Some(SearchHit::Field),
            "the clear glyph must not be hittable before the pill is open"
        );
        s.set_expand(1.);
        let layout = s.layout(area);
        let entry_center = Point::from((
            layout.entry.pill.loc.x + layout.entry.pill.size.w / 2.,
            layout.entry.pill.loc.y + Entry::HEIGHT / 2.,
        ));
        assert_eq!(
            s.hit_test(entry_center, area),
            Some(SearchHit::Field),
            "an active (opaque) entry body must consume its own clicks"
        );
        assert_eq!(
            s.hit_test(layout.entry.secondary_icon, area),
            Some(SearchHit::Clear)
        );
        let t0 = layout.tiles[0];
        let tc = Point::from((t0.loc.x + t0.size.w / 2., t0.loc.y + t0.size.h / 2.));
        assert_eq!(s.hit_test(tc, area), Some(SearchHit::Result(0)));
    }

    /// The resting entry is a puck at the right end of its footprint; expanding grows it
    /// leftward to GNOME's 24em pill with the right edge pinned. This is the divergence, so
    /// it is pinned by geometry rather than by eye.
    #[test]
    fn the_entry_rests_as_a_puck_and_grows_leftward() {
        let mut s = OverviewSearch::new();
        let area = area_1080();
        let right = area.entry.loc.x + area.entry.size.w;

        let puck = s.layout(area).entry.pill;
        assert_eq!(puck.size.w, PUCK_D, "at rest it is a circle");
        assert_eq!(
            puck.size.h, PUCK_D,
            "…square, so the radius rounds it fully"
        );
        assert!(
            puck.size.h > Entry::HEIGHT,
            "and it is a button, bigger than the pill it opens into"
        );
        const {
            assert!(
                PUCK_D < crate::ui::dash::ICON_PX,
                "but still smaller than a dash icon"
            )
        };
        assert_eq!(puck.loc.x + puck.size.w, right, "parked at the right edge");
        // The find glyph is centred in the puck, not sitting in a leading gutter.
        assert_eq!(s.layout(area).find_icon.x, puck.loc.x + PUCK_D / 2.);

        // Typing expands it; the animation progress is pushed in by Synoik, so drive it here.
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        assert!(s.is_expanded());
        s.set_expand(1.);
        let pill = s.layout(area).entry.pill;
        assert_eq!(pill.size.w, entry_width(area.ramp), "grown to 24em");
        assert_eq!(pill.size.h, Entry::HEIGHT, "and back to GNOME's own height");
        assert_eq!(
            pill.loc.x + pill.size.w,
            right,
            "the right edge never moved — the pill grew leftward"
        );
        assert_eq!(
            s.layout(area).find_icon.x,
            pill.loc.x + Entry::ICON_INSET,
            "and the glyph has slid into the leading gutter"
        );

        // Halfway through, both the box and the glyph are between the two — an endpoint-only
        // check would pass on a pill that teleported.
        s.set_expand(0.5);
        let mid = s.layout(area);
        assert!(mid.entry.pill.size.w > PUCK_D && mid.entry.pill.size.w < pill.size.w);
        assert!(mid.entry.pill.size.h < PUCK_D && mid.entry.pill.size.h > Entry::HEIGHT);
        assert!(mid.find_icon.x > pill.loc.x + Entry::ICON_INSET);
        assert_eq!(mid.entry.pill.loc.x + mid.entry.pill.size.w, right);
        // The height shrinks about a fixed centre, so the control does not hinge on an edge.
        let mid_c = mid.entry.pill.loc.y + mid.entry.pill.size.h / 2.;
        assert_eq!(mid_c, puck.loc.y + puck.size.h / 2.);
        assert_eq!(mid_c, pill.loc.y + pill.size.h / 2.);

        // Clearing puts it back to the puck.
        s.clear();
        assert!(!s.is_expanded());
    }

    /// The reserved band is the *puck's*, so the resting button never overhangs the strip
    /// below it — the band used to be sized for the 40px pill, which the 56px puck outgrows.
    #[test]
    fn the_resting_puck_fits_inside_the_band_the_layout_reserves() {
        let s = OverviewSearch::new();
        let area = area_1080();
        let puck = s.layout(area).entry.pill;
        assert!(puck.loc.y >= area.entry.loc.y, "clear of the band's top");
        assert!(
            puck.loc.y + puck.size.h <= area.entry.loc.y + area.entry.size.h,
            "and of its bottom"
        );
    }

    /// A modified key (Alt/Super held) must never act as its bare self.
    #[test]
    fn modified_keys_are_ignored_while_active() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);

        // Super is never an entry's, and Alt is bound to nothing here — both must fall
        // through unconsumed. Ctrl is deliberately NOT in this list any more: Ctrl-BackSpace
        // and friends are real editing bindings now, covered by
        // `gnome_editing_combos_reach_the_query`.
        let logo = EditMods {
            logo: true,
            ..EditMods::default()
        };
        let alt = EditMods {
            alt: true,
            ..EditMods::default()
        };
        for mods in [logo, alt] {
            for raw in [
                Keysym::Escape,
                Keysym::Return,
                Keysym::Right,
                Keysym::BackSpace,
            ] {
                assert_eq!(
                    s.handle_key(Some(raw), None, mods, KeyTheme::default()),
                    SearchOutcome::Ignored,
                    "{raw:?} with {mods:?} held must be ignored, not acted on"
                );
            }
        }
        // Untouched by all of the above.
        assert!(s.is_active());
        assert_eq!(s.query(), "a");
        assert_eq!(s.selected_id(), Some("a.desktop"));
    }

    /// The result tile follows `.overview-tile`, not `%tile`: gnome-shell puts the
    /// button (and so the selection fill) on the outer tile, which overrides both
    /// the padding and the radius and wraps the label as well as the icon
    /// (`_app-grid.scss:21-37`, `search.js:142` extending it via
    /// `_search-results.scss:58-60`). The dash is the other case and keeps `%tile`.
    #[test]
    fn result_tile_follows_the_overview_tile_rule() {
        // BaseIcon's default `ICON_SIZE` (`iconGrid.js:11,83`), not the dash's 64.
        assert_eq!(RESULT_ICON_PX, 96.);
        // padding 12 + icon 96 + `.overview-icon-with-label` spacing 6 + one label
        // line 18 + padding 12 — square, because BaseIcon is a `Shell.SquareBin`.
        assert_eq!((tile_w(), tile_h()), (145., 145.));
        assert_eq!(TILE_PAD, AppIcon::OVERVIEW_TILE_PADDING);
        assert_ne!(
            AppIcon::OVERVIEW_TILE_RADIUS,
            AppIcon::RADIUS,
            "the two tile rules must stay distinct — collapsing them is the bug \
             this pins (the dash keeps %tile, the app grid and search do not)"
        );

        // The card grows with the bigger tiles, and still centers them — plus the room a
        // resting caption needs below its tile box ([`LABEL_OVERHANG`]).
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let l = s.layout(area_1080());
        let card = l.card.expect("an active search has a card");
        assert_eq!(
            card.size,
            Size::from((
                2. * tile_w() + GRID_SPACING + 2. * CARD_PAD,
                tile_h() + LABEL_OVERHANG + 2. * CARD_PAD
            ))
        );
        assert_eq!(l.tiles[0].size, Size::from((tile_w(), tile_h())));
        assert_eq!(l.tiles[0].loc.x, card.loc.x + CARD_PAD);
    }

    /// The search results and the app grid are the same `.overview-tile` in GNOME
    /// (`search.js:142` extends it), so their geometry has to come out identical. The
    /// two are derived separately — these constants for layout, [`widget::TileMetrics`]
    /// for painting — so a change to one that misses the other shows up here.
    #[test]
    fn search_tiles_match_the_shared_overview_metrics() {
        let m = widget::TileMetrics::overview();
        assert_eq!(RESULT_ICON_PX, m.icon_px);
        assert_eq!(TILE_PAD, m.pad);
        assert_eq!(Size::from((tile_w(), tile_h())), m.size());
        assert_eq!(LABEL_PT, crate::ui::BASE_FONT_PT);
    }

    /// The empty-state ("No results") card is sized for its own status string, not for
    /// a nonexistent tile — a tile-width card would clip the 20pt text.
    #[test]
    fn empty_results_card_is_wide_enough_for_the_status_text() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), EditMods::default(), KeyTheme::default());
        let card = s
            .layout(area_1080())
            .card
            .expect("an active search has a card");
        assert!(
            card.size.w >= STATUS_CARD_W,
            "the No-results card must fit its status text, got {}",
            card.size.w
        );
        assert!(card.size.w > tile_w() + 2. * CARD_PAD);
    }

    impl OverviewSearch {
        #[cfg(test)]
        fn selected(&self) -> usize {
            self.selected
        }
    }
}
