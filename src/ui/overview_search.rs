//! The overview search — a `search-entry` at the top of the Activities overview
//! feeding the built-in **app** search provider, showing a grid of app results that
//! Enter/click launches (`js/ui/searchController.js`, `search.js`, `appDisplay.js`
//! `AppSearchProvider`).
//!
//! **Model split.** Like the dash ([`crate::ui::dash`]), this owns only plain state:
//! the query string, a snapshot of result entries, the keyboard selection, and mouse
//! hover. It does NOT own the app catalog — [`OverviewSearch::handle_key`] mutates the
//! query and returns a [`SearchOutcome`]; `Niri::sync_overview_search` runs
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
use crate::ui::widget::{
    self, AppIcon, Entry, EntryHit, EntryLayout, Painter, SharedAppIconUploads,
};

/// The built-in `AppSearchProvider` result cap (`this.maxResults = 6`,
/// `appDisplay.js:1760`).
pub const MAX_RESULTS: usize = 6;

/// The hint shown in the empty entry (`hint_text: _('Type to search')`,
/// `overviewControls.js:330`).
const PLACEHOLDER: &str = "Type to search";
/// Entry pill width, logical (`.search-entry` `width: 24em`; em = 11pt·4/3 ≈ 14.67px).
const ENTRY_WIDTH: f64 = 352.;
/// `.search-entry` `margin-top: $base_padding*2` (`_search-entry.scss:4`).
const ENTRY_MARGIN_TOP: f64 = 12.;
/// `.search-entry` `margin-bottom: $base_padding` (`_search-entry.scss:5`).
const ENTRY_MARGIN_BOTTOM: f64 = 6.;

/// What the search entry asks [`crate::ui::overview_layout`] for: the pill plus
/// its margins, which is what gnome-shell's `searchEntryBin` reports as its
/// preferred height (`overviewControls.js:165`).
pub const PREFERRED_ENTRY_HEIGHT: f64 = ENTRY_MARGIN_TOP + Entry::HEIGHT + ENTRY_MARGIN_BOTTOM;

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
/// widening the tile. Kept in step with [`widget::TileMetrics::OVERVIEW`] by
/// `search_tiles_match_the_shared_overview_metrics`.
const TILE_SIDE: f64 = TILE_PAD + RESULT_ICON_PX + 6. + 18. + TILE_PAD; // 144
const TILE_W: f64 = TILE_SIDE;
const TILE_H: f64 = TILE_SIDE;
/// How far a resting caption hangs below its tile box: every line past the first, at the
/// shared label height. The tile is sized for one line (that is the `Shell.SquareBin`
/// rule above), so a card holding [`widget::TILE_LABEL_LINES`] of caption has to reserve
/// the rest itself or the last line is clipped by the card's own bake.
///
/// A result caption is otherwise the app grid's: GNOME's is one ellipsized line on both
/// surfaces (`expandTitleOnHover: false` only stops results *expanding* on hover — the
/// resting line count is `StLabel`'s, `st-label.c:331`), so the divergence the grid takes
/// is the same divergence here, and letting the two disagree would be the odd choice.
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

/// The overview search model. Owned on `Niri`; fed results by `sync_overview_search`.
pub struct OverviewSearch {
    query: String,
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
            query: String::new(),
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
        self.query.split_whitespace().next().is_some()
    }

    pub fn query(&self) -> &str {
        &self.query
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
        plain: bool,
        shift: bool,
    ) -> SearchOutcome {
        if !plain {
            return SearchOutcome::Ignored;
        }
        match raw {
            Some(Keysym::Escape) => {
                if self.is_active() {
                    self.clear();
                    SearchOutcome::Cleared
                } else {
                    SearchOutcome::Close
                }
            }
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                // activateDefault: launch the selected result, else consume (must NOT
                // fall through, or the hardcoded Return bind would toggle the overview).
                match self.selected_id() {
                    Some(id) if self.is_active() => SearchOutcome::Activate(id.to_owned()),
                    _ => SearchOutcome::Handled,
                }
            }
            // Shift+Tab is `ISO_Left_Tab` only as a *modified* keysym; what reaches us is
            // the raw sym, which is a plain `Tab` — so the shift state is what actually
            // decides the direction, and matching `ISO_Left_Tab` alone stepped forward.
            // Shift+Tab is `ISO_Left_Tab` only as a *modified* keysym; what reaches us is
            // the raw sym, which is a plain `Tab` — so the shift state is what actually
            // decides the direction, and matching `ISO_Left_Tab` alone stepped forward.
            Some(Keysym::Tab) if shift => {
                self.select_prev();
                SearchOutcome::Handled
            }
            Some(
                Keysym::Left | Keysym::Up | Keysym::KP_Left | Keysym::KP_Up | Keysym::ISO_Left_Tab,
            ) => {
                self.select_prev();
                SearchOutcome::Handled
            }
            Some(
                Keysym::Right | Keysym::Down | Keysym::KP_Right | Keysym::KP_Down | Keysym::Tab,
            ) => {
                self.select_next();
                SearchOutcome::Handled
            }
            Some(Keysym::BackSpace) => {
                self.query.pop();
                self.on_query_changed();
                SearchOutcome::QueryChanged
            }
            _ => {
                if let Some(c) = text.filter(|c| !c.is_control()) {
                    self.query.push(c);
                    self.on_query_changed();
                    SearchOutcome::QueryChanged
                } else {
                    // A key the search doesn't handle (bare modifier, F-key, …) — do
                    // not consume it.
                    SearchOutcome::Ignored
                }
            }
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
        if self.query.is_empty() && self.results.is_empty() && self.hovered.is_none() {
            return;
        }
        self.query.clear();
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
        let entry = Entry::layout(
            area.entry.loc.x + area.entry.size.w / 2.,
            area.entry.loc.y + ENTRY_MARGIN_TOP,
            ENTRY_WIDTH,
        );

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
                n * TILE_W + (n - 1.) * GRID_SPACING + 2. * CARD_PAD
            };
            let card_h = TILE_H + LABEL_OVERHANG + 2. * CARD_PAD;
            let card_x = (area.results.loc.x + (area.results.size.w - card_w) / 2.).round();
            let card_y = (area.results.loc.y + SECTION_SPACING).round();
            let card = Rectangle::new(Point::from((card_x, card_y)), Size::from((card_w, card_h)));
            let tiles = (0..self.results.len())
                .map(|i| {
                    let tx = card_x + CARD_PAD + i as f64 * (TILE_W + GRID_SPACING);
                    Rectangle::new(
                        Point::from((tx, card_y + CARD_PAD)),
                        Size::from((TILE_W, TILE_H)),
                    )
                })
                .collect();
            (Some(card), tiles)
        };

        SearchLayout { entry, card, tiles }
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
        match Entry::hit(&layout.entry, pos, self.is_active()) {
            Some(EntryHit::Clear) => return Some(SearchHit::Clear),
            Some(EntryHit::Field) => return Some(SearchHit::Background),
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

    /// The entry pill box — a geometry probe for the render test.
    #[cfg(test)]
    pub fn entry_pill(&self, area: SearchArea) -> Rectangle<f64, Logical> {
        self.layout(area).entry.pill
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
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        area: SearchArea,
        fade: SearchFade,
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
        for (name, color, center, want) in [
            (
                "edit-find-symbolic",
                style::MUTED,
                layout.entry.primary_icon,
                true,
            ),
            (
                "edit-clear-symbolic",
                style::TEXT,
                layout.entry.secondary_icon,
                active,
            ),
        ] {
            if !want {
                continue;
            }
            let Some(tb) = sym_icons.texture(renderer, name, Entry::ICON_PX, scale, color) else {
                continue;
            };
            let logical = tb.logical_size();
            let loc = center - Point::from((logical.w / 2., logical.h / 2.));
            elements.push(TextureRenderElement::from_texture_buffer(
                tb,
                loc,
                alpha,
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
        match Entry::bake(
            renderer,
            &mut cache.entry_bake,
            scale,
            ENTRY_WIDTH,
            &self.query,
            PLACEHOLDER,
            self.content_rev,
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
                let label_w = widget::TileMetrics::OVERVIEW.label_w();
                let names: Vec<Vec<String>> = self
                    .results
                    .iter()
                    .map(|e| {
                        widget::tile_label_lines(
                            &e.name,
                            LABEL_PT,
                            label_w,
                            widget::TILE_LABEL_LINES,
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
                                &widget::TileMetrics::OVERVIEW,
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
                        p.fill_rounded_full(CARD_RADIUS, widget::style::OVERLAY_BG)?;
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
                                    widget::TileMetrics::OVERVIEW.radius,
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
}

impl From<ControlsLayout> for SearchArea {
    fn from(l: ControlsLayout) -> Self {
        Self {
            entry: l.search_entry,
            results: l.search_results,
        }
    }
}

struct SearchLayout {
    entry: EntryLayout,
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
        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let rev = s.content_rev();
        assert!(
            s.set_hovered(Some(SearchHit::Result(1))),
            "a new hover reports a change"
        );
        s.handle_key(Some(Keysym::Down), None, true, false); // move the keyboard selection
        assert_eq!(
            s.content_rev(),
            rev,
            "hover + selection must not invalidate the label bake"
        );
        // A query change re-shapes (the labels differ).
        s.handle_key(None, Some('b'), true, false);
        assert_ne!(s.content_rev(), rev, "a query change re-bakes");
    }

    #[test]
    fn typing_builds_query_and_activates() {
        let mut s = OverviewSearch::new();
        assert!(!s.is_active());
        assert_eq!(
            s.handle_key(None, Some('f'), true, false),
            SearchOutcome::QueryChanged
        );
        assert_eq!(
            s.handle_key(None, Some('i'), true, false),
            SearchOutcome::QueryChanged
        );
        assert!(s.is_active());
        assert_eq!(s.query(), "fi");

        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        assert_eq!(
            s.handle_key(Some(Keysym::Return), None, true, false),
            SearchOutcome::Activate("a.desktop".to_owned())
        );
    }

    #[test]
    fn arrow_moves_selection_and_clamps() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        // Right → index 1, then clamp at the last.
        s.handle_key(Some(Keysym::Right), None, true, false);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        s.handle_key(Some(Keysym::Right), None, true, false);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Left back to 0, saturating.
        s.handle_key(Some(Keysym::Left), None, true, false);
        s.handle_key(Some(Keysym::Left), None, true, false);
        assert_eq!(s.selected_id(), Some("a.desktop"));
    }

    #[test]
    fn selection_never_underflows_on_empty() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), true, false);
        // No results seeded.
        s.handle_key(Some(Keysym::Right), None, true, false);
        s.handle_key(Some(Keysym::Left), None, true, false);
        assert_eq!(s.selected_id(), None);
        // Enter with no results is consumed, not an activate.
        assert_eq!(
            s.handle_key(Some(Keysym::Return), None, true, false),
            SearchOutcome::Handled
        );
    }

    #[test]
    fn escape_clears_when_active_else_closes() {
        let mut s = OverviewSearch::new();
        // Inactive Escape → Close (normally unreachable via the input gate).
        assert_eq!(
            s.handle_key(Some(Keysym::Escape), None, true, false),
            SearchOutcome::Close
        );

        s.handle_key(None, Some('x'), true, false);
        s.set_results(vec![entry("a.desktop", "A")]);
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(Some(Keysym::Escape), None, true, false),
            SearchOutcome::Cleared
        );
        assert!(!s.is_active());
        assert_eq!(s.query(), "");
        assert!(s.result_id(0).is_none());
    }

    #[test]
    fn backspace_empties_and_deactivates() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true, false);
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(Some(Keysym::BackSpace), None, true, false),
            SearchOutcome::QueryChanged
        );
        assert!(!s.is_active());
    }

    #[test]
    fn query_change_resets_selection_to_first() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        s.handle_key(Some(Keysym::Right), None, true, false);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Typing another char resets selection to the first.
        s.handle_key(None, Some('b'), true, false);
        assert_eq!(s.selected(), 0);
    }

    #[test]
    fn unhandled_key_is_ignored_not_consumed() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true, false); // active
                                                    // F5 (no text, not a nav key) must be Ignored so the caller doesn't eat it.
        assert_eq!(
            s.handle_key(Some(Keysym::F5), None, true, false),
            SearchOutcome::Ignored
        );
    }

    /// The boxes `overview_layout` allocates the search on 1920×1080 with the
    /// 35px panel strut.
    fn area_1080() -> SearchArea {
        let controls = crate::ui::overview_layout::layout(
            Size::from((1920., 1080.)),
            35.,
            PREFERRED_ENTRY_HEIGHT,
            crate::ui::dash::PREFERRED_HEIGHT,
            54.,
            1.,
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
        let entry_center = Point::from((
            layout.entry.pill.loc.x + ENTRY_WIDTH / 2.,
            layout.entry.pill.loc.y + Entry::HEIGHT / 2.,
        ));
        assert_eq!(
            s.hit_test(entry_center, area),
            Some(SearchHit::Background),
            "the idle entry pill must consume rather than fall through"
        );
        // No query yet, so there is nothing to clear: that glyph is not live.
        assert_eq!(
            s.hit_test(layout.entry.secondary_icon, area),
            Some(SearchHit::Background)
        );
        // Well away from the entry: no hit at all.
        assert_eq!(s.hit_test(Point::from((10., 600.)), area), None);

        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let layout = s.layout(area);
        assert_eq!(
            s.hit_test(entry_center, area),
            Some(SearchHit::Background),
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

    /// A modified key (Ctrl/Alt/Super held) must never act as its bare self: Ctrl+Escape
    /// must not clear, Alt+arrow must not move the selection, Super+Enter must not launch.
    #[test]
    fn modified_keys_are_ignored_while_active() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);

        for raw in [
            Keysym::Escape,
            Keysym::Return,
            Keysym::Right,
            Keysym::BackSpace,
        ] {
            assert_eq!(
                s.handle_key(Some(raw), None, false, false),
                SearchOutcome::Ignored,
                "{raw:?} with a modifier held must be ignored, not acted on"
            );
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
        assert_eq!((TILE_W, TILE_H), (144., 144.));
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
        s.handle_key(None, Some('a'), true, false);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let l = s.layout(area_1080());
        let card = l.card.expect("an active search has a card");
        assert_eq!(
            card.size,
            Size::from((
                2. * TILE_W + GRID_SPACING + 2. * CARD_PAD,
                TILE_H + LABEL_OVERHANG + 2. * CARD_PAD
            ))
        );
        assert_eq!(l.tiles[0].size, Size::from((TILE_W, TILE_H)));
        assert_eq!(l.tiles[0].loc.x, card.loc.x + CARD_PAD);
    }

    /// The search results and the app grid are the same `.overview-tile` in GNOME
    /// (`search.js:142` extends it), so their geometry has to come out identical. The
    /// two are derived separately — these constants for layout, [`widget::TileMetrics`]
    /// for painting — so a change to one that misses the other shows up here.
    #[test]
    fn search_tiles_match_the_shared_overview_metrics() {
        let m = widget::TileMetrics::OVERVIEW;
        assert_eq!(RESULT_ICON_PX, m.icon_px);
        assert_eq!(TILE_PAD, m.pad);
        assert_eq!(Size::from((TILE_W, TILE_H)), m.size());
        assert_eq!(LABEL_PT, crate::ui::BASE_FONT_PT);
    }

    /// The empty-state ("No results") card is sized for its own status string, not for
    /// a nonexistent tile — a tile-width card would clip the 20pt text.
    #[test]
    fn empty_results_card_is_wide_enough_for_the_status_text() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), true, false);
        let card = s
            .layout(area_1080())
            .card
            .expect("an active search has a card");
        assert!(
            card.size.w >= STATUS_CARD_W,
            "the No-results card must fit its status text, got {}",
            card.size.w
        );
        assert!(card.size.w > TILE_W + 2. * CARD_PAD);
    }

    impl OverviewSearch {
        #[cfg(test)]
        fn selected(&self) -> usize {
            self.selected
        }
    }
}
