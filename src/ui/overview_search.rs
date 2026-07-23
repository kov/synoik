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
use std::collections::HashMap;

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, AppIcon, AppIconUploads, Entry, EntryHit, EntryLayout, Painter};
use crate::utils::output_size;

/// The built-in `AppSearchProvider` result cap (`this.maxResults = 6`,
/// `appDisplay.js:1760`).
pub const MAX_RESULTS: usize = 6;

/// The hint shown in the empty entry (`hint_text: _('Type to search')`,
/// `overviewControls.js:330`).
const PLACEHOLDER: &str = "Type to search";
/// Entry pill width, logical (`.search-entry` `width: 24em`; em = 11pt·4/3 ≈ 14.67px).
const ENTRY_WIDTH: f64 = 352.;
/// Entry top edge from the output top, logical — below the panel + `.search-entry`
/// `margin-top: $base_padding*2` (12px). Hardcoded until S5's `ControlsManagerLayout`.
const ENTRY_TOP: f64 = 48.;

/// Full-color app-icon side in a result tile, logical. S5-tunable (GNOME's app-grid
/// `IconGrid` sizes this dynamically); 64 matches the dash.
const RESULT_ICON_PX: f64 = 64.;
/// Result-tile label point size (`%caption`-ish app name under the icon).
const LABEL_PT: f64 = 10.;
/// `.overview-tile` padding (`$base_padding*2`=12).
const TILE_PAD: f64 = 12.;
/// Result-tile fixed width, logical (icon + room for a clipped label + padding).
const TILE_W: f64 = 96.;
/// Result-tile height: padding + icon + gap + one label line + padding.
const TILE_H: f64 = TILE_PAD + RESULT_ICON_PX + 6. + 18. + TILE_PAD; // 112
/// Gap between grid tiles (`.grid-search-results` `spacing: $base_padding*5`=30).
const GRID_SPACING: f64 = 30.;
/// `.search-section-content` padding (`$base_padding*2`=12).
const CARD_PAD: f64 = 12.;
/// `.search-section-content` corner radius (`$modal_radius*1.5`=24).
const CARD_RADIUS: f64 = 24.;
/// Gap from the entry's bottom to the results card top (`.search-entry` `margin-bottom`
/// 6 + `.search-section` `spacing` 18).
const CARD_GAP: f64 = 24.;
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
    results_bake: widget::BakeCache,
    /// Full-color result-icon uploads (shared key space with the dash's).
    icons: AppIconUploads,
    /// The entry's symbolic glyph uploads, keyed by `(scale, icon name)` — uploaded at
    /// full tint so the overview fade rides on the element alpha instead of the tint
    /// (which would thrash the `IconCache`; see `render`).
    glyphs: HashMap<(NotNan<f64>, &'static str), TextureBuffer<VkTexture>>,
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
        if i != self.selected {
            self.selected = i;
            self.content_rev = self.content_rev.wrapping_add(1);
        }
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

    /// Set the mouse-hovered element; returns whether it changed (→ redraw).
    pub fn set_hovered(&mut self, hovered: Option<SearchHit>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        self.content_rev = self.content_rev.wrapping_add(1);
        true
    }

    /// Drop cached icon uploads (icon-theme / installed change).
    pub fn clear_icon_uploads(&self) {
        let mut cache = self.cache.borrow_mut();
        cache.icons.clear();
        cache.glyphs.clear();
    }

    /// Lay out the entry pill + (when active) the results card + tiles for an output.
    fn layout(&self, size: Size<f64, Logical>) -> SearchLayout {
        let entry = Entry::layout(size.w / 2., ENTRY_TOP, ENTRY_WIDTH);

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
            let card_h = TILE_H + 2. * CARD_PAD;
            let card_x = ((size.w - card_w) / 2.).round();
            let card_y = (entry.pill.loc.y + Entry::HEIGHT + CARD_GAP).round();
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

    /// Which interactive element is under `pos` (logical, output coords) — only while
    /// a search is **active**. While inactive the entry is a passive hint and clicks
    /// pass straight through: until S5's `ControlsManagerLayout` the hardcoded entry
    /// position overlaps the workspace thumbnail strip, and an always-on hit region
    /// would eat thumbnail clicks. Once active the pill is an opaque drawn control, so
    /// its *body* consumes inertly ([`SearchHit::Background`]) — a visible control must
    /// never actuate the hidden thumbnail behind it.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Option<SearchHit> {
        if !self.is_active() {
            return None;
        }
        let layout = self.layout(size);
        match Entry::hit(&layout.entry, pos, true) {
            Some(EntryHit::Clear) => return Some(SearchHit::Clear),
            Some(EntryHit::Field) => return Some(SearchHit::Background),
            None => {}
        }
        let card = layout.card?;
        if !card.contains(pos) {
            return None;
        }
        for (i, tile) in layout.tiles.iter().enumerate() {
            if tile.contains(pos) {
                return Some(SearchHit::Result(i));
            }
        }
        Some(SearchHit::Background)
    }

    /// The entry pill box for an output of `size` — a geometry probe for the render
    /// test.
    #[cfg(test)]
    pub fn entry_pill(&self, size: Size<f64, Logical>) -> Rectangle<f64, Logical> {
        self.layout(size).entry.pill
    }

    /// The logical center of result tile `i` for an output of `size` — a geometry
    /// probe for the conformance corpus (which clicks real pixels routed through
    /// [`hit_test`](Self::hit_test)). `None` if out of range.
    #[cfg(test)]
    pub fn result_center(&self, i: usize, size: Size<f64, Logical>) -> Option<Point<f64, Logical>> {
        let layout = self.layout(size);
        layout
            .tiles
            .get(i)
            .map(|t| Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.)))
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
        progress: f64,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let scale = output.current_scale().fractional_scale();
        let size = output_size(output);
        let layout = self.layout(size);
        let alpha = progress as f32;

        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.glyphs.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::new();
        let active = self.is_active();

        // --- Entry glyphs (topmost): the find icon, and the clear icon when active. ---
        // Rasterized + uploaded ONCE per scale at full tint, with `progress` applied as
        // the element alpha. Folding the fade into the tint instead would miss the
        // `IconCache` key (`(name, px, color)`) on every animation frame, re-rasterizing
        // the SVG and accreting a cached buffer per alpha step — the trap the dash's
        // show-apps glyph documents.
        if let Ok(scale_key) = NotNan::new(scale) {
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
                let key = (scale_key, name);
                if let std::collections::hash_map::Entry::Vacant(slot) = cache.glyphs.entry(key) {
                    let Some(buffer) = sym_icons.buffer(name, Entry::ICON_PX, scale, color) else {
                        continue;
                    };
                    match TextureBuffer::from_memory_buffer(renderer, &buffer) {
                        Ok(tb) => {
                            slot.insert(tb);
                        }
                        Err(err) => {
                            tracing::error!("error uploading search entry glyph: {err:#}");
                            continue;
                        }
                    }
                }
                if let Some(tb) = cache.glyphs.get(&key) {
                    let logical = tb.logical_size();
                    let loc = center - Point::from((logical.w / 2., logical.h / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
            }
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
                &mut cache.icons,
                app_icons,
                &entry.icon,
                RESULT_ICON_PX,
                scale,
                Point::from((0., 0.)),
                center,
                alpha,
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

        // --- The results card chrome (labels + selection/hover wash, or "No results"). ---
        if active {
            if let Some(card) = layout.card {
                let origin = card.loc;
                let selected = self.selected;
                let hovered = self.hovered;
                let card_size = card.size;
                // Tile boxes relative to the card origin (for paint) and label strings
                // (for the shaping prepare) — kept in separate vecs so the two bake
                // closures don't both borrow one.
                let rel_rects: Vec<Rectangle<f64, Logical>> = layout
                    .tiles
                    .iter()
                    .map(|t| Rectangle::new(t.loc - origin, t.size))
                    .collect();
                let names: Vec<String> = self.results.iter().map(|e| e.name.clone()).collect();
                let empty = self.results.is_empty();
                match widget::bake(
                    renderer,
                    &mut cache.results_bake,
                    scale,
                    card_size,
                    self.content_rev,
                    move |r| {
                        let mut shaper = widget::TextShaper::new(r, scale);
                        let labels = names
                            .iter()
                            .map(|name| shaper.shape(name, widget::TextStyle::new(LABEL_PT)))
                            .collect::<anyhow::Result<Vec<_>>>()?;
                        let status =
                            if empty {
                                Some(shaper.shape(
                                    "No results",
                                    widget::TextStyle::new(STATUS_PT).bold(),
                                )?)
                            } else {
                                None
                            };
                        Ok((labels, status))
                    },
                    move |frame, phys, (labels, status)| {
                        let mut p = Painter::new(frame, scale, phys);
                        p.clear(style::TRANSPARENT)?;
                        p.fill_rounded_full(CARD_RADIUS, widget::style::OVERLAY_BG)?;
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
                        for (i, rel) in rel_rects.iter().enumerate() {
                            if i == selected || hovered == Some(SearchHit::Result(i)) {
                                p.app_tile(
                                    &AppIcon {
                                        rect: *rel,
                                        hovered: true,
                                    },
                                    widget::style::HOVER_WASH,
                                )?;
                            }
                        }
                        for (rel, label) in rel_rects.iter().zip(labels.iter()) {
                            // Label centered under the icon, clipped to the tile.
                            let lx = rel.loc.x + rel.size.w / 2.;
                            let ly = rel.loc.y + TILE_PAD + RESULT_ICON_PX + 6.;
                            p.text_band(
                                label,
                                lx,
                                widget::HAlign::Center,
                                ly,
                                18.,
                                style::TEXT,
                                *rel,
                            )?;
                        }
                        Ok(())
                    },
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
                            card.loc,
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ));
                    }
                    Err(err) => tracing::error!("error baking the search results: {err:#}"),
                }
            }
        }

        elements
    }
}

use widget::style;

/// Computed geometry for one output size.
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
    fn typing_builds_query_and_activates() {
        let mut s = OverviewSearch::new();
        assert!(!s.is_active());
        assert_eq!(
            s.handle_key(None, Some('f'), true),
            SearchOutcome::QueryChanged
        );
        assert_eq!(
            s.handle_key(None, Some('i'), true),
            SearchOutcome::QueryChanged
        );
        assert!(s.is_active());
        assert_eq!(s.query(), "fi");

        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        assert_eq!(
            s.handle_key(Some(Keysym::Return), None, true),
            SearchOutcome::Activate("a.desktop".to_owned())
        );
    }

    #[test]
    fn arrow_moves_selection_and_clamps() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        // Right → index 1, then clamp at the last.
        s.handle_key(Some(Keysym::Right), None, true);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        s.handle_key(Some(Keysym::Right), None, true);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Left back to 0, saturating.
        s.handle_key(Some(Keysym::Left), None, true);
        s.handle_key(Some(Keysym::Left), None, true);
        assert_eq!(s.selected_id(), Some("a.desktop"));
    }

    #[test]
    fn selection_never_underflows_on_empty() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), true);
        // No results seeded.
        s.handle_key(Some(Keysym::Right), None, true);
        s.handle_key(Some(Keysym::Left), None, true);
        assert_eq!(s.selected_id(), None);
        // Enter with no results is consumed, not an activate.
        assert_eq!(
            s.handle_key(Some(Keysym::Return), None, true),
            SearchOutcome::Handled
        );
    }

    #[test]
    fn escape_clears_when_active_else_closes() {
        let mut s = OverviewSearch::new();
        // Inactive Escape → Close (normally unreachable via the input gate).
        assert_eq!(
            s.handle_key(Some(Keysym::Escape), None, true),
            SearchOutcome::Close
        );

        s.handle_key(None, Some('x'), true);
        s.set_results(vec![entry("a.desktop", "A")]);
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(Some(Keysym::Escape), None, true),
            SearchOutcome::Cleared
        );
        assert!(!s.is_active());
        assert_eq!(s.query(), "");
        assert!(s.result_id(0).is_none());
    }

    #[test]
    fn backspace_empties_and_deactivates() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true);
        assert!(s.is_active());
        assert_eq!(
            s.handle_key(Some(Keysym::BackSpace), None, true),
            SearchOutcome::QueryChanged
        );
        assert!(!s.is_active());
    }

    #[test]
    fn query_change_resets_selection_to_first() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        s.handle_key(Some(Keysym::Right), None, true);
        assert_eq!(s.selected_id(), Some("b.desktop"));
        // Typing another char resets selection to the first.
        s.handle_key(None, Some('b'), true);
        assert_eq!(s.selected(), 0);
    }

    #[test]
    fn unhandled_key_is_ignored_not_consumed() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true); // active
                                             // F5 (no text, not a nav key) must be Ignored so the caller doesn't eat it.
        assert_eq!(
            s.handle_key(Some(Keysym::F5), None, true),
            SearchOutcome::Ignored
        );
    }

    #[test]
    fn hit_test_inactive_passes_through_active_entry_consumes() {
        let mut s = OverviewSearch::new();
        let size = Size::from((1920., 1080.));
        let layout = s.layout(size);
        let entry_center = Point::from((
            layout.entry.pill.loc.x + ENTRY_WIDTH / 2.,
            layout.entry.pill.loc.y + Entry::HEIGHT / 2.,
        ));
        // Inactive: nothing is hittable — the entry is a passive hint, so clicks pass
        // through to the thumbnail strip it overlaps pre-S5.
        assert_eq!(s.hit_test(entry_center, size), None);

        // Active: the entry is an opaque drawn pill, so its body must CONSUME (never
        // actuate the hidden thumbnail behind it); the clear glyph and tiles are live.
        s.handle_key(None, Some('a'), true);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let layout = s.layout(size);
        assert_eq!(
            s.hit_test(entry_center, size),
            Some(SearchHit::Background),
            "an active (opaque) entry body must consume its own clicks"
        );
        assert_eq!(
            s.hit_test(layout.entry.secondary_icon, size),
            Some(SearchHit::Clear)
        );
        let t0 = layout.tiles[0];
        let tc = Point::from((t0.loc.x + t0.size.w / 2., t0.loc.y + t0.size.h / 2.));
        assert_eq!(s.hit_test(tc, size), Some(SearchHit::Result(0)));
    }

    /// A modified key (Ctrl/Alt/Super held) must never act as its bare self: Ctrl+Escape
    /// must not clear, Alt+arrow must not move the selection, Super+Enter must not launch.
    #[test]
    fn modified_keys_are_ignored_while_active() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), true);
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);

        for raw in [
            Keysym::Escape,
            Keysym::Return,
            Keysym::Right,
            Keysym::BackSpace,
        ] {
            assert_eq!(
                s.handle_key(Some(raw), None, false),
                SearchOutcome::Ignored,
                "{raw:?} with a modifier held must be ignored, not acted on"
            );
        }
        // Untouched by all of the above.
        assert!(s.is_active());
        assert_eq!(s.query(), "a");
        assert_eq!(s.selected_id(), Some("a.desktop"));
    }

    /// The empty-state ("No results") card is sized for its own status string, not for
    /// a nonexistent tile — a tile-width card would clip the 20pt text.
    #[test]
    fn empty_results_card_is_wide_enough_for_the_status_text() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), true);
        let size = Size::from((1920., 1080.));
        let card = s.layout(size).card.expect("an active search has a card");
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
