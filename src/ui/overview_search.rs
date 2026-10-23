// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The overview search — a `search-entry` at the top of the Activities overview, and the
//! results under it: the built-in **app** provider's grid, then one list section per remote
//! search provider (`js/ui/searchController.js`, `search.js`, `appDisplay.js`
//! `AppSearchProvider`, `remoteSearch.js`).
//!
//! **Model split.** Like the dash ([`crate::ui::dash`]), this owns only plain state: the
//! query, snapshots of the app results and the provider sections, the keyboard selection and
//! mouse hover. Searching is `Synoik`'s: [`OverviewSearch::handle_key`] edits the query and
//! returns a [`SearchOutcome`], `Synoik::sync_overview_search` runs the search after its
//! delay, and the results come back through [`OverviewSearch::set_results`] (apps, on the
//! spot) and [`OverviewSearch::set_section`] (each provider, as it answers — see
//! [`crate::search_providers`]). Activation hands the caller an [`Activation`] to perform.
//!
//! **Faithful behavior.** Tokenize = trim + split on whitespace (`searchController.js:19-24`).
//! The search runs 150 ms after the first keystroke that changed the terms; Enter runs a
//! pending one first (`search.js:741-772,879-886`). App provider:
//! `AppSystem.search(terms.join(' '))` → relevance-tier groups → filter `should_show` → concat
//! → cap [`MAX_RESULTS`] (`appDisplay.js:1801-1831`). Sections follow the providers' order and
//! show up to five rows with an "N more" header (`search.js:293-340`). The default selection
//! is the first result of the first section that has one (`_maybeSetInitialSelection`);
//! Enter activates it. Until nothing is showing *and* every provider has answered the status
//! reads "Searching", then "No results". Escape while active clears, it does not close
//! (`searchController.js:153-160`).
//!
//! **Divergences:** no `Shell.AppUsage` ordering within a relevance tier, and no
//! `SystemActions` results. The results do not scroll: a section that would run past the
//! bottom of the results view is left out. Rows draw themed and file icons only, not icon
//! pixels a provider sends inline, and the description is not highlighted with the terms.
//! A section header is not a keyboard stop. GNOME's stage-capture `reset-search` gesture (a
//! click outside the entry/results clears the search) is not ported: such a click falls
//! through to the picker and the query survives until the next overview enter. Like the
//! panel and dash, the search draws on **every** output with one shared selection/hover
//! (GNOME shows it on the primary monitor only); hit-testing is still per-output.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::search_providers::{ResultIcon, ResultMeta};
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
/// so it looks deliberate beside it instead of like a shrunken entry. This is its size at or
/// above the reference canvas: [`puck_diameter`] ramps it, as the dash beside it ramps.
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

/// **Adaptive chrome, rule 2 — ramped.** The puck's diameter on a canvas with this chrome ramp:
/// [`PUCK_D`] shrunk with the dash, but never below the pill it opens into, whose height is its
/// text plus padding and does not ramp.
pub fn puck_diameter(ramp: f64) -> f64 {
    (PUCK_D * ramp).round().max(Entry::HEIGHT)
}

/// The find glyph's size in the puck on a canvas with this chrome ramp: ramped with the puck,
/// never below the pill's glyph. One size per canvas, so the icon cache sees no new key per frame.
fn puck_icon_px(ramp: f64) -> f64 {
    (PUCK_ICON_PX * ramp).round().max(Entry::ICON_PX)
}

/// The entry's theme margins on a canvas with this chrome ramp: fixed logical constants, ramped.
fn entry_margins(ramp: f64) -> (f64, f64) {
    (
        (ENTRY_MARGIN_TOP * ramp).round(),
        (ENTRY_MARGIN_BOTTOM * ramp).round(),
    )
}

/// What the search entry asks [`crate::ui::overview_layout`] for: the control plus its margins,
/// which is what gnome-shell's `searchEntryBin` reports as its preferred height
/// (`overviewControls.js:165`).
///
/// The control here is the **puck**, not the pill: it is the taller of the two states, and a
/// band sized for the pill would leave the resting button overhanging the thumbnail strip. The
/// pill then centres inside the puck's footprint rather than sitting on GNOME's literal
/// `margin-top`, which is the price of [`PUCK_D`]'s divergence.
pub fn preferred_entry_height(ramp: f64) -> f64 {
    let (top, bottom) = entry_margins(ramp);
    top + puck_diameter(ramp) + bottom
}

/// How far the *control's* vertical middle sits below the top of the bin
/// [`preferred_entry_height`] reserves — the puck's centre, which is also the expanded
/// pill's (the pill centres inside the puck's footprint, see [`PUCK_D`]).
fn entry_control_mid_y(ramp: f64) -> f64 {
    entry_margins(ramp).0 + puck_diameter(ramp) / 2.
}

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
/// The status line's box: wide enough for the 20pt string, tall enough for its line.
const STATUS_W: f64 = ENTRY_WIDTH;
const STATUS_H: f64 = 40.;
/// `#searchResults` `margin: 0 $base_margin`.
const RESULTS_MARGIN: f64 = 4.;
/// `#searchResultsContent` `max-width` — the column the results are centered in.
const RESULTS_MAX_W: f64 = 1044.;
/// What separates two sections: `.search-section` `spacing` (18) down to its separator, whose
/// `height: $base_margin*2` (8) is used as a spacer.
const SECTION_GAP: f64 = SECTION_SPACING + 8.;
/// `.search-section-content` `margin: 0 $base_margin*3`.
const SECTION_MARGIN_X: f64 = 12.;
/// `.search-section-content` `border: 2px solid transparent` — invisible, but it takes room.
const CARD_BORDER: f64 = 2.;
/// `%search_section_content_item` — `%tile` padding (`$base_padding`) and
/// `border-radius: $base_border_radius*1.65`: a row and a section header.
const ITEM_PAD: f64 = 6.;
const ITEM_RADIUS: f64 = 8. * 1.65;
/// `ListSearchResult.ICON_SIZE`.
const ROW_ICON_PX: f64 = 24.;
/// A row: its icon between the item padding — taller than its one line of text.
const ROW_H: f64 = ROW_ICON_PX + 2. * ITEM_PAD;
/// `.list-search-results` `spacing: $base_padding`.
const ROW_SPACING: f64 = 6.;
/// `.list-search-result-title` `spacing: $base_padding*2` — icon to title.
const TITLE_SPACING: f64 = 12.;
/// `.list-search-result-content` `spacing: $base_padding` — title to description.
const DESCRIPTION_SPACING: f64 = 6.;
/// `ProviderInfo.PROVIDER_ICON_SIZE`.
const PROVIDER_ICON_PX: f64 = 32.;
/// `.list-search-provider-content` `spacing: $base_padding*2`.
const PROVIDER_SPACING: f64 = 12.;
/// `.list-search-provider-details` `width: 120px`.
const PROVIDER_DETAILS_W: f64 = 120.;
/// The header button: padding, icon, spacing, details, padding.
const PROVIDER_W: f64 = 2. * ITEM_PAD + PROVIDER_ICON_PX + PROVIDER_SPACING + PROVIDER_DETAILS_W;
/// `.search-provider-icon` `margin-right: $base_margin` — header to rows.
const PROVIDER_MARGIN: f64 = 4.;
/// One line of body text, for sizing a box around lines.
const LINE_H: f64 = 18.;

/// One app result — a plain-data snapshot (not a live catalog borrow), like
/// [`crate::ui::dash::DashEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultEntry {
    pub id: String,
    pub name: String,
    pub icon: AppIconRef,
}

/// One remote provider's section: a header naming the app, and up to
/// [`MAX_LIST_ROWS`](crate::search_providers::MAX_LIST_ROWS) rows (`ListSearchResults`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSection {
    /// The provider's position among the providers — sections are drawn in this order
    /// whatever order they answered in.
    pub order: usize,
    pub provider: String,
    /// The app's name and icon, for the header (`ProviderInfo`).
    pub name: String,
    pub icon: AppIconRef,
    pub rows: Vec<ResultMeta>,
    /// The header's "N more".
    pub more: usize,
}

/// What activating a result does — the caller performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// Launch an app result.
    App(String),
    /// A provider's result: `ActivateResult` on its provider, then copy its clipboard text.
    Result {
        provider: String,
        id: String,
        clipboard_text: Option<String>,
    },
    /// A section header: open the search in the provider's app (`launchSearch`).
    LaunchSearch { provider: String },
}

/// What a point over the search UI hit. Nothing is hittable while the search is
/// inactive (the entry is then a passive hint over the thumbnail strip); once active
/// the entry body consumes inertly as [`Background`](SearchHit::Background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchHit {
    /// The entry's trailing clear glyph.
    Clear,
    /// App result tile at index `.0`.
    Result(usize),
    /// Row `row` of list section `section`.
    Row { section: usize, row: usize },
    /// The header of list section `.0`.
    Provider(usize),
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
    /// Enter: activate the default result, if there is one (`activateDefault`). The caller
    /// first runs a search that is still waiting out its delay, so Enter right after typing
    /// acts on what was typed.
    ActivateDefault,
    /// Close the overview (Escape while inactive — normally unreachable here, see the
    /// key-routing note in `src/input/mod.rs`; the real inactive-Escape close is the
    /// hardcoded overview bind).
    Close,
}

/// A selectable result, by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    App(usize),
    Row { section: usize, row: usize },
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
    /// The providers' sections, in provider order.
    sections: Vec<ListSection>,
    /// Some provider has yet to answer — the status reads "Searching", not "No results".
    searching: bool,
    /// Keyboard-selected result, as an index over the app results and then every section's
    /// rows. Meaningful only when there is something to select.
    selected: usize,
    /// The selection was moved by the keyboard since the query last changed. Until it is,
    /// it follows the default result (the first one) as results arrive
    /// (`_maybeSetInitialSelection`); after, it stays on the result it was moved to.
    navigated: bool,
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
            sections: Vec::new(),
            searching: false,
            selected: 0,
            navigated: false,
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

    /// The editing model itself — what the clipboard bindings need (the selection).
    pub fn edit(&self) -> &TextEdit {
        &self.edit
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
        match self.item(self.selected)? {
            Item::App(i) => Some(self.results[i].id.as_str()),
            Item::Row { section, row } => Some(self.sections[section].rows[row].id.as_str()),
        }
    }

    /// What Enter activates (`activateDefault`): the selected result, if any.
    pub fn selected_activation(&self) -> Option<Activation> {
        if !self.is_active() {
            return None;
        }
        self.activation(self.item(self.selected)?)
    }

    /// What clicking `hit` activates, if anything.
    pub fn hit_activation(&self, hit: SearchHit) -> Option<Activation> {
        match hit {
            SearchHit::Result(i) => self.activation(Item::App(i)),
            SearchHit::Row { section, row } => self.activation(Item::Row { section, row }),
            SearchHit::Provider(section) => Some(Activation::LaunchSearch {
                provider: self.sections.get(section)?.provider.clone(),
            }),
            SearchHit::Clear | SearchHit::Field | SearchHit::Background => None,
        }
    }

    fn activation(&self, item: Item) -> Option<Activation> {
        Some(match item {
            Item::App(i) => Activation::App(self.results.get(i)?.id.clone()),
            Item::Row { section, row } => {
                let section = self.sections.get(section)?;
                let meta = section.rows.get(row)?;
                Activation::Result {
                    provider: section.provider.clone(),
                    id: meta.id.clone(),
                    clipboard_text: meta.clipboard_text.clone(),
                }
            }
        })
    }

    /// How many results can be selected: the app results, then every section's rows.
    fn item_count(&self) -> usize {
        self.results.len() + self.sections.iter().map(|s| s.rows.len()).sum::<usize>()
    }

    /// The result at flat index `i`.
    fn item(&self, i: usize) -> Option<Item> {
        if i < self.results.len() {
            return Some(Item::App(i));
        }
        let mut i = i - self.results.len();
        for (section, s) in self.sections.iter().enumerate() {
            if i < s.rows.len() {
                return Some(Item::Row { section, row: i });
            }
            i -= s.rows.len();
        }
        None
    }

    /// The flat index of `item`.
    fn index_of(&self, item: Item) -> usize {
        match item {
            Item::App(i) => i,
            Item::Row { section, row } => {
                self.results.len()
                    + self.sections[..section]
                        .iter()
                        .map(|s| s.rows.len())
                        .sum::<usize>()
                    + row
            }
        }
    }

    /// The selected result's identity — which provider, which id — so it can be found again
    /// after the results around it change.
    fn selected_key(&self) -> Option<(Option<String>, String)> {
        Some(match self.item(self.selected)? {
            Item::App(i) => (None, self.results[i].id.clone()),
            Item::Row { section, row } => (
                Some(self.sections[section].provider.clone()),
                self.sections[section].rows[row].id.clone(),
            ),
        })
    }

    /// Put the selection back on `key` after a results change, or on the default result if it
    /// was never moved or its result is gone.
    fn reselect(&mut self, key: Option<(Option<String>, String)>) {
        let found = key.filter(|_| self.navigated).and_then(|(provider, id)| {
            let item = match provider {
                None => Item::App(self.results.iter().position(|e| e.id == id)?),
                Some(provider) => {
                    let section = self.sections.iter().position(|s| s.provider == provider)?;
                    let row = self.sections[section]
                        .rows
                        .iter()
                        .position(|m| m.id == id)?;
                    Item::Row { section, row }
                }
            };
            Some(self.index_of(item))
        });
        self.selected = found.unwrap_or(0);
    }

    /// Whether no result of any kind is showing.
    pub fn has_no_results(&self) -> bool {
        self.results.is_empty() && self.sections.is_empty()
    }

    /// The providers' sections, in order.
    pub fn sections(&self) -> &[ListSection] {
        &self.sections
    }

    /// Replace (or with `None`, remove) one provider's section.
    pub fn set_section(&mut self, provider: &str, section: Option<ListSection>) {
        let key = self.selected_key();
        let old = self.sections.iter().position(|s| s.provider == provider);
        match (old, section) {
            (Some(i), Some(section)) if self.sections[i] == section => return,
            (None, None) => return,
            (Some(i), Some(section)) => self.sections[i] = section,
            (Some(i), None) => {
                self.sections.remove(i);
            }
            (None, Some(section)) => {
                let at = self
                    .sections
                    .iter()
                    .position(|s| s.order > section.order)
                    .unwrap_or(self.sections.len());
                self.sections.insert(at, section);
            }
        }
        self.reselect(key);
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// Drop every provider section (the provider set changed).
    pub fn clear_sections(&mut self) {
        if !self.sections.is_empty() {
            let key = self.selected_key();
            self.sections.clear();
            self.reselect(key);
            self.content_rev = self.content_rev.wrapping_add(1);
        }
    }

    /// Whether the search is still in progress — what the empty state reads.
    pub fn is_searching(&self) -> bool {
        self.searching
    }

    /// Whether a provider is still searching — what the empty state reads.
    pub fn set_searching(&mut self, searching: bool) {
        if self.searching != searching {
            self.searching = searching;
            self.content_rev = self.content_rev.wrapping_add(1);
        }
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
                // activateDefault. Consumed even with nothing to activate: falling through,
                // the hardcoded Return bind would toggle the overview.
                if self.is_active() {
                    SearchOutcome::ActivateDefault
                } else {
                    SearchOutcome::Handled
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
        self.navigated = false;
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    fn select_prev(&mut self) {
        self.set_selected(self.selected.saturating_sub(1));
    }

    fn select_next(&mut self) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        self.set_selected((self.selected + 1).min(count - 1));
    }

    fn set_selected(&mut self, i: usize) {
        // No `content_rev` bump: the selection wash is a separate element (the caller
        // always redraws on a handled key), so moving it never re-shapes the labels.
        self.selected = i;
        self.navigated = true;
    }

    /// Replace the result snapshot (from `sync_overview_search`), clamping the
    /// selection into range.
    pub fn set_results(&mut self, results: Vec<SearchResultEntry>) {
        if results == self.results {
            return;
        }
        let key = self.selected_key();
        self.results = results;
        self.reselect(key);
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// Clear the query and results (overview enter/reset, Escape-while-active).
    pub fn clear(&mut self) {
        if self.edit.is_empty()
            && self.results.is_empty()
            && self.sections.is_empty()
            && !self.searching
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
        self.sections.clear();
        self.searching = false;
        self.selected = 0;
        self.navigated = false;
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
        let puck = puck_diameter(area.ramp);
        let e = self.expand;
        let pill_w = puck + (full_w - puck) * e;
        // The puck shrinks to the pill's height about a **fixed centre**, so the control opens
        // symmetrically instead of hinging on one edge. That centre is the puck's own, since the
        // band is reserved for the puck (see `preferred_entry_height`).
        let pill_h = puck + (Entry::HEIGHT - puck) * e;
        let center_y = area.entry.loc.y + entry_control_mid_y(area.ramp);
        let right = area.entry.loc.x + area.entry.size.w;
        let entry = Entry::layout(
            right - pill_w / 2.,
            center_y - pill_h / 2.,
            pill_w,
            pill_h,
            // Geometry only — `Entry::layout` reads the style's radius and gutters, never its
            // fill, so the appearance it carries is not consulted here.
            widget::EntryStyle::Search(widget::style::Appearance::default()),
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
        let results = self.is_active().then(|| self.results_layout(area));

        SearchLayout {
            entry,
            find_icon,
            puck_icon_px: puck_icon_px(area.ramp),
            pill_w,
            pill_h,
            results,
        }
    }

    /// The results under an active search, top to bottom in `#searchResultsContent`: the app
    /// grid, then one card per provider section; or the status line when there are none.
    fn results_layout(&self, area: SearchArea) -> ResultsLayout {
        let avail = area.results.size.w - 2. * RESULTS_MARGIN;
        let column_w = avail.min(RESULTS_MAX_W);
        let column_x = area.results.loc.x + (area.results.size.w - column_w) / 2.;
        let top = (area.results.loc.y + SECTION_SPACING).round();
        let bottom = area.results.loc.y + area.results.size.h;

        if self.has_no_results() {
            // `_statusContainer`, centered in the whole results view.
            let size = Size::from((STATUS_W, STATUS_H));
            let status = Rectangle::new(
                Point::from((
                    (area.results.loc.x + (area.results.size.w - size.w) / 2.).round(),
                    (area.results.loc.y + (area.results.size.h - size.h) / 2.).round(),
                )),
                size,
            );
            return ResultsLayout {
                canvas: status,
                tiles: Vec::new(),
                sections: Vec::new(),
                status: Some(status),
            };
        }

        let mut y = top;
        let mut tiles = Vec::new();
        if !self.results.is_empty() {
            // The grid sits centered in its section, with no plate behind it: only
            // `ListSearchResults` builds a `.search-section-content` (`search.js:296-299`).
            let n = self.results.len() as f64;
            let grid_w = n * tile_w() + (n - 1.) * GRID_SPACING;
            let x0 = (column_x + (column_w - grid_w) / 2.).round();
            tiles = (0..self.results.len())
                .map(|i| {
                    Rectangle::new(
                        Point::from((x0 + i as f64 * (tile_w() + GRID_SPACING), y)),
                        Size::from((tile_w(), tile_h())),
                    )
                })
                .collect();
            y += tile_h() + LABEL_OVERHANG + SECTION_GAP;
        }

        let card_x = column_x + SECTION_MARGIN_X;
        let card_w = column_w - 2. * SECTION_MARGIN_X;
        let inset = CARD_BORDER + CARD_PAD;
        let mut sections = Vec::new();
        for section in &self.sections {
            let header_h = 2. * ITEM_PAD
                + PROVIDER_ICON_PX.max(if section.more > 0 { 2. } else { 1. } * LINE_H);
            let rows_h = section.rows.len() as f64 * ROW_H
                + section.rows.len().saturating_sub(1) as f64 * ROW_SPACING;
            let card_h = 2. * inset + header_h.max(rows_h);
            // Until the results scroll, a section that would run past the bottom is left
            // out rather than drawn over the dash.
            if y + card_h > bottom {
                break;
            }
            let card = Rectangle::new(Point::from((card_x, y)), Size::from((card_w, card_h)));
            let header = Rectangle::new(
                Point::from((card_x + inset, y + inset)),
                Size::from((PROVIDER_W, header_h)),
            );
            let rows_x = header.loc.x + PROVIDER_W + PROVIDER_MARGIN;
            let rows_w = card_x + card_w - inset - rows_x;
            let rows = (0..section.rows.len())
                .map(|r| {
                    Rectangle::new(
                        Point::from((rows_x, y + inset + r as f64 * (ROW_H + ROW_SPACING))),
                        Size::from((rows_w, ROW_H)),
                    )
                })
                .collect();
            sections.push(SectionLayout { card, header, rows });
            y += card_h + SECTION_GAP;
        }

        let canvas_bottom = sections
            .last()
            .map(|s| s.card.loc.y + s.card.size.h)
            .or_else(|| tiles.first().map(|t| t.loc.y + tile_h() + LABEL_OVERHANG))
            .unwrap_or(top);
        ResultsLayout {
            canvas: Rectangle::new(
                Point::from((column_x.round(), top)),
                Size::from((column_w.round(), canvas_bottom - top)),
            ),
            tiles,
            sections,
            status: None,
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
        if let Some(results) = &layout.results {
            for (i, tile) in results.tiles.iter().enumerate() {
                if tile.contains(pos) {
                    return Some(SearchHit::Result(i));
                }
            }
            for (section, s) in results.sections.iter().enumerate() {
                if s.header.contains(pos) {
                    return Some(SearchHit::Provider(section));
                }
                for (row, rect) in s.rows.iter().enumerate() {
                    if rect.contains(pos) {
                        return Some(SearchHit::Row { section, row });
                    }
                }
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
        self.result_rect(i, area)
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
        self.layout(area).results?.tiles.get(i).copied()
    }

    /// Section `i`'s plate — a geometry probe for the render test.
    #[cfg(test)]
    pub fn section_card(&self, i: usize, area: SearchArea) -> Option<Rectangle<f64, Logical>> {
        Some(self.layout(area).results?.sections.get(i)?.card)
    }

    /// Where `hit` is drawn — what the conformance corpus clicks.
    pub fn hit_center(&self, hit: SearchHit, area: SearchArea) -> Option<Point<f64, Logical>> {
        let results = self.layout(area).results?;
        let rect = match hit {
            SearchHit::Result(i) => *results.tiles.get(i)?,
            SearchHit::Row { section, row } => *results.sections.get(section)?.rows.get(row)?,
            SearchHit::Provider(section) => results.sections.get(section)?.header,
            SearchHit::Clear | SearchHit::Field | SearchHit::Background => return None,
        };
        Some(Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + rect.size.h / 2.,
        )))
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
        // `org.gnome.desktop.interface color-scheme`, for the two surfaces here that take the
        // shared plate: the entry pill and the results card.
        appearance: widget::style::Appearance,
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
        {
            let cache = &mut *cache;
            crate::ui::widget::sync_icon_upload_context(&mut cache.context, &cache.icons, context);
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
                layout.puck_icon_px,
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

        // --- Result icons (topmost, over their tiles and rows). ---
        if let Some(results) = &layout.results {
            let mut uploads = cache.icons.borrow_mut();
            let mut push_icon = |icon: &AppIconRef, px: f64, center: Point<f64, Logical>| {
                if let Some(el) = widget::app_icon_element(
                    renderer,
                    &mut uploads,
                    app_icons,
                    icon,
                    px,
                    scale,
                    Point::from((0., 0.)),
                    center,
                    results_alpha,
                ) {
                    elements.push(el);
                }
            };
            for (entry, tile) in self.results.iter().zip(&results.tiles) {
                let center = Point::from((
                    tile.loc.x + tile.size.w / 2.,
                    tile.loc.y + TILE_PAD + RESULT_ICON_PX / 2.,
                ));
                push_icon(&entry.icon, RESULT_ICON_PX, center);
            }
            for (section, sl) in self.sections.iter().zip(&results.sections) {
                let center = Point::from((
                    sl.header.loc.x + ITEM_PAD + PROVIDER_ICON_PX / 2.,
                    sl.header.loc.y + ITEM_PAD + PROVIDER_ICON_PX / 2.,
                ));
                push_icon(&section.icon, PROVIDER_ICON_PX, center);
                for (meta, row) in section.rows.iter().zip(&sl.rows) {
                    if let Some(icon) = row_icon(meta) {
                        push_icon(&icon, ROW_ICON_PX, row_icon_center(*row));
                    }
                }
            }
            drop(uploads);
            // A symbolic icon is recoloured to the text it sits with, as `St.Icon` does for any
            // `-symbolic` name — Settings' panels and Calculator's copy glyph are all symbolic.
            for (section, sl) in self.sections.iter().zip(&results.sections) {
                for (meta, row) in section.rows.iter().zip(&sl.rows) {
                    let Some(name) = symbolic_icon(meta) else {
                        continue;
                    };
                    let Some(tb) =
                        sym_icons.texture(renderer, name, ROW_ICON_PX, scale, style::TEXT)
                    else {
                        continue;
                    };
                    let logical = tb.logical_size();
                    let loc = row_icon_center(*row) - Point::from((logical.w / 2., logical.h / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb,
                        loc,
                        results_alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
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
            widget::EntryStyle::Search(appearance),
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
                .of(appearance.rev())
                .done(),
        ) {
            Ok(buffer) => {
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

        // --- The results in two z-layers baked over the results column, so a highlight
        //     change (every mouse move / arrow key) never re-shapes text. The text layer is
        //     keyed on `content_rev` alone; the plate layer — section plates and the
        //     selection/hover wash — re-bakes when the highlight moves, but that is only
        //     rounded fills. The result icons were pushed above both. ---
        if let Some(results) = &layout.results {
            self.render_results(
                renderer,
                &mut cache,
                scale,
                results,
                results_alpha,
                appearance,
                &mut elements,
            );
        }

        elements
    }
}

impl OverviewSearch {
    #[allow(clippy::too_many_arguments)]
    fn render_results(
        &self,
        renderer: &mut VulkanRenderer,
        cache: &mut SearchCache,
        scale: f64,
        results: &ResultsLayout,
        alpha: f32,
        appearance: widget::style::Appearance,
        elements: &mut Vec<TextureRenderElement<VkTexture>>,
    ) {
        let canvas = results.canvas;
        let origin = canvas.loc;
        let rel = |r: Rectangle<f64, Logical>| Rectangle::new(r.loc - origin, r.size);
        let mut push = |buffer| {
            elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                origin,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ));
        };

        // (1) Text — highlight-independent.
        // A search result's caption never expands — the provider builds its icons with
        // `expandTitleOnHover: false` (`appDisplay.js:1837-1841`) — so it is always the
        // resting, end-ellipsized form, at the same line count the grid rests at (see
        // [`LABEL_OVERHANG`], which is what makes room for it).
        let label_w = widget::TileMetrics::overview().label_w();
        let captions: Vec<(Rectangle<f64, Logical>, Vec<String>)> = self
            .results
            .iter()
            .zip(&results.tiles)
            .map(|(e, tile)| {
                let lines = widget::tile_label_lines(
                    &e.name,
                    LABEL_PT,
                    label_w,
                    widget::TILE_LABEL_LINES,
                    false,
                );
                (rel(*tile), lines)
            })
            .collect();
        let mut texts = Vec::new();
        for (section, sl) in self.sections.iter().zip(&results.sections) {
            section_texts(section, sl, origin, &mut texts);
        }
        if let Some(status) = results.status {
            texts.push(TextItem {
                text: if self.searching {
                    "Searching"
                } else {
                    "No results"
                }
                .to_owned(),
                pt: STATUS_PT,
                bold: true,
                x: rel(status).loc.x + status.size.w / 2.,
                align: widget::HAlign::Center,
                top: rel(status).loc.y,
                h: status.size.h,
                // `.search-statustext`: `transparentize($system_fg_color, .2)`.
                color: [1., 1., 1., 0.8],
            });
        }
        match widget::bake(
            renderer,
            &mut cache.results_bake,
            scale,
            canvas.size,
            self.content_rev,
            move |r| {
                let mut shaper = widget::TextShaper::new(r, scale);
                let captions = captions
                    .into_iter()
                    .map(|(rect, lines)| {
                        let shaped = lines
                            .iter()
                            .map(|line| shaper.shape(line, widget::TextStyle::new(LABEL_PT)))
                            .collect::<anyhow::Result<Vec<_>>>()?;
                        Ok((rect, shaped))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let texts = texts
                    .into_iter()
                    .map(|t| {
                        let mut style = widget::TextStyle::new(t.pt);
                        if t.bold {
                            style = style.bold();
                        }
                        Ok((shaper.shape(&t.text, style)?, t))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok((captions, texts))
            },
            move |frame, phys, (captions, texts)| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                let all = Rectangle::from_size(canvas.size);
                for (rect, label) in captions {
                    p.labelled_tile(
                        *rect,
                        label,
                        &widget::TileMetrics::overview(),
                        false,
                        style::TEXT,
                    )?;
                }
                for (shaped, t) in texts {
                    p.text_band(shaped, t.x, t.align, t.top, t.h, t.color, all)?;
                }
                Ok(())
            },
        ) {
            Ok(buffer) => push(buffer),
            Err(err) => tracing::error!("error baking the search results: {err:#}"),
        }

        // (2) Plates and the selection/hover wash.
        let plates: Vec<Rectangle<f64, Logical>> =
            results.sections.iter().map(|s| rel(s.card)).collect();
        // The wash covers a tile's caption, which may run past the tile box — GNOME's tile
        // allocation follows its label, so the highlight grows with it (the app grid does
        // the same for an expanded caption).
        let tile_wash = |i: usize| {
            let lines = widget::tile_label_lines(
                &self.results[i].name,
                LABEL_PT,
                label_w,
                widget::TILE_LABEL_LINES,
                false,
            )
            .len();
            let tile = rel(results.tiles[i]);
            (
                Rectangle::new(
                    tile.loc,
                    Size::from((tile.size.w, tile.size.h + LINE_H * (lines as f64 - 1.))),
                ),
                widget::TileMetrics::overview().radius,
            )
        };
        let wash_of = |hit: SearchHit| -> Option<(Rectangle<f64, Logical>, f64)> {
            match hit {
                SearchHit::Result(i) if i < results.tiles.len() => Some(tile_wash(i)),
                SearchHit::Row { section, row } => Some((
                    rel(*results.sections.get(section)?.rows.get(row)?),
                    ITEM_RADIUS,
                )),
                SearchHit::Provider(section) => {
                    Some((rel(results.sections.get(section)?.header), ITEM_RADIUS))
                }
                _ => None,
            }
        };
        let selected = self.item(self.selected).map(|item| match item {
            Item::App(i) => SearchHit::Result(i),
            Item::Row { section, row } => SearchHit::Row { section, row },
        });
        let washes: Vec<_> = [selected, self.hovered]
            .into_iter()
            .flatten()
            .filter_map(wash_of)
            .collect();
        let hover_key = |hit: Option<SearchHit>| match hit {
            Some(SearchHit::Result(i)) => 1 + i as u64,
            Some(SearchHit::Row { section, row }) => 1_000 + 100 * section as u64 + row as u64,
            Some(SearchHit::Provider(section)) => 100_000 + section as u64,
            _ => 0,
        };
        // The appearance rides the revision, or a Dark Style toggle would leave the plates on
        // the old fill until the content next changed.
        let rev = (self.content_rev << 41)
            | (appearance.rev() << 40)
            | ((hover_key(selected) & 0xF_FFFF) << 20)
            | (hover_key(self.hovered) & 0xF_FFFF);
        match widget::bake(
            renderer,
            &mut cache.bg_bake,
            scale,
            canvas.size,
            rev,
            |_| Ok(()),
            move |frame, phys, ()| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                for plate in &plates {
                    // `.search-section-content` — a plate on the overview backdrop, so it takes
                    // the shared translucent fill rather than the opaque
                    // `$system_overlay_bg_color` (`widget::style::Appearance::plate`).
                    p.fill_rounded(*plate, CARD_RADIUS, appearance.plate())?;
                }
                for (rect, radius) in &washes {
                    p.fill_rounded(*rect, *radius, style::HOVER_WASH)?;
                }
                Ok(())
            },
        ) {
            Ok(buffer) => push(buffer),
            Err(err) => tracing::error!("error baking the search plates: {err:#}"),
        }
    }
}

/// A run of text to place in the results' text layer.
struct TextItem {
    text: String,
    pt: f64,
    bold: bool,
    x: f64,
    align: widget::HAlign,
    /// The band the line is centered in, relative to the canvas.
    top: f64,
    h: f64,
    color: widget::Rgba,
}

/// A section's header and rows as text items, relative to `origin`.
fn section_texts(
    section: &ListSection,
    sl: &SectionLayout,
    origin: Point<f64, Logical>,
    texts: &mut Vec<TextItem>,
) {
    let pt = crate::ui::BASE_FONT_PT;
    let line_w = |s: &str| {
        synoik_vk::text::measure_line_width_weighted(s, crate::ui::pt_to_px(pt) as f32, false)
    };
    let mut push = |text: String, x: f64, top: f64, h: f64, color| {
        if !text.is_empty() {
            texts.push(TextItem {
                text,
                pt,
                bold: false,
                x: x - origin.x,
                align: widget::HAlign::Left,
                top: top - origin.y,
                h,
                color,
            });
        }
    };

    // The header: the app's name, and under it "N more" when there is more to see.
    let details_x = sl.header.loc.x + ITEM_PAD + PROVIDER_ICON_PX + PROVIDER_SPACING;
    let content_top = sl.header.loc.y + ITEM_PAD;
    let content_h = sl.header.size.h - 2. * ITEM_PAD;
    let name = widget::ellipsized_line(&section.name, pt, PROVIDER_DETAILS_W);
    if section.more > 0 {
        push(
            name,
            details_x,
            content_top,
            content_h - LINE_H,
            style::TEXT,
        );
        push(
            format!("{} more", section.more),
            details_x,
            content_top + content_h - LINE_H,
            LINE_H,
            style::TEXT,
        );
    } else {
        push(name, details_x, content_top, content_h, style::TEXT);
    }

    // The rows: the title after its icon, then the description's first line in what is left.
    for (meta, row) in section.rows.iter().zip(&sl.rows) {
        let title_x = row.loc.x + ITEM_PAD + ROW_ICON_PX + TITLE_SPACING;
        let avail = row.loc.x + row.size.w - ITEM_PAD - title_x;
        let title = widget::ellipsized_line(&meta.name, pt, avail);
        let title_w = line_w(&title).min(avail);
        push(title, title_x, row.loc.y, row.size.h, style::TEXT);
        if let Some(description) = &meta.description {
            let x = title_x + title_w + DESCRIPTION_SPACING;
            let avail = row.loc.x + row.size.w - ITEM_PAD - x;
            if avail > 0. {
                push(
                    widget::ellipsized_line(description, pt, avail),
                    x,
                    row.loc.y,
                    row.size.h,
                    // `.list-search-result-description`: `$system_insensitive_fg_color`.
                    style::MUTED,
                );
            }
        }
    }
}

/// Where a row's icon is centered.
fn row_icon_center(row: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    Point::from((
        row.loc.x + ITEM_PAD + ROW_ICON_PX / 2.,
        row.loc.y + row.size.h / 2.,
    ))
}

/// A row's icon name when it is symbolic, and so drawn in the text colour.
fn symbolic_icon(meta: &ResultMeta) -> Option<&str> {
    match meta.icon.as_ref()? {
        ResultIcon::Themed(names) => names
            .first()
            .map(String::as_str)
            .filter(|name| name.ends_with("-symbolic")),
        _ => None,
    }
}

/// The full-colour icon a row draws, if it is one this toolkit can draw. Symbolic ones are
/// [`symbolic_icon`]'s.
fn row_icon(meta: &ResultMeta) -> Option<AppIconRef> {
    if symbolic_icon(meta).is_some() {
        return None;
    }
    match meta.icon.as_ref()? {
        ResultIcon::Themed(names) => Some(AppIconRef::Themed(names.clone())),
        ResultIcon::File(path) => Some(AppIconRef::File(path.clone())),
        ResultIcon::Pixels { .. } => None,
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
    /// The find glyph's size at rest, on this canvas.
    puck_icon_px: f64,
    /// The pill's current size — what the chrome is baked at.
    pill_w: f64,
    pill_h: f64,
    /// Everything below the entry, while a search is active.
    results: Option<ResultsLayout>,
}

struct ResultsLayout {
    /// The box every result is drawn inside — the size the result layers are baked at.
    canvas: Rectangle<f64, Logical>,
    /// App result tiles.
    tiles: Vec<Rectangle<f64, Logical>>,
    /// The provider sections that fit.
    sections: Vec<SectionLayout>,
    /// "No results" / "Searching", when nothing is showing.
    status: Option<Rectangle<f64, Logical>>,
}

struct SectionLayout {
    /// The `.search-section-content` plate.
    card: Rectangle<f64, Logical>,
    /// The `ProviderInfo` button.
    header: Rectangle<f64, Logical>,
    rows: Vec<Rectangle<f64, Logical>>,
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
            SearchOutcome::ActivateDefault
        );
        assert_eq!(
            s.selected_activation(),
            Some(Activation::App("a.desktop".to_owned()))
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
        // Enter with no results has nothing to activate.
        assert_eq!(
            s.handle_key(
                Some(Keysym::Return),
                None,
                EditMods::default(),
                KeyTheme::default()
            ),
            SearchOutcome::ActivateDefault
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
                                                                                 // F5 (no text, not
                                                                                 // a nav key) must
                                                                                 // be Ignored so
                                                                                 // the caller
                                                                                 // doesn't eat it.
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
                search_entry_height: preferred_entry_height(1.),
                search_entry_width: entry_width(1.),
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
        let t0 = layout.results.as_ref().unwrap().tiles[0];
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
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
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

        // The grid is centered in the results column, with no plate around it.
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('a'), EditMods::default(), KeyTheme::default());
        s.set_results(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);
        let area = area_1080();
        let l = s.layout(area).results.unwrap();
        assert_eq!(l.tiles[0].size, Size::from((tile_w(), tile_h())));
        assert_eq!(l.tiles[1].loc.x - l.tiles[0].loc.x, tile_w() + GRID_SPACING);
        let middle = (l.tiles[0].loc.x + l.tiles[1].loc.x + tile_w()) / 2.;
        let column_middle = area.results.loc.x + area.results.size.w / 2.;
        assert!(
            (middle - column_middle).abs() <= 1.,
            "{middle} vs {column_middle}"
        );
        assert!(l.sections.is_empty());
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

    /// With nothing to show, the status line (`_statusContainer`) sits centered in the results
    /// view, and reads "Searching" while a provider has yet to answer.
    #[test]
    fn an_empty_search_shows_its_status_centered() {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some('z'), EditMods::default(), KeyTheme::default());
        let area = area_1080();
        let status = s.layout(area).results.unwrap().status.unwrap();
        let center =
            |r: Rectangle<f64, Logical>| (r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.);
        let (sx, sy) = center(status);
        let (ax, ay) = center(area.results);
        assert!((sx - ax).abs() <= 1. && (sy - ay).abs() <= 1.);
        s.set_section("p", Some(section(0, "p", &["one"])));
        assert!(s.layout(area).results.unwrap().status.is_none());
    }

    fn meta(id: &str) -> ResultMeta {
        ResultMeta {
            id: id.to_owned(),
            name: id.to_uppercase(),
            description: None,
            icon: None,
            clipboard_text: Some(format!("copied {id}")),
        }
    }

    fn section(order: usize, provider: &str, ids: &[&str]) -> ListSection {
        ListSection {
            order,
            provider: provider.to_owned(),
            name: provider.to_owned(),
            icon: AppIconRef::Fallback,
            rows: ids.iter().map(|id| meta(id)).collect(),
            more: 0,
        }
    }

    fn typed(query: char) -> OverviewSearch {
        let mut s = OverviewSearch::new();
        s.handle_key(None, Some(query), EditMods::default(), KeyTheme::default());
        s
    }

    fn down(s: &mut OverviewSearch) {
        s.handle_key(
            Some(Keysym::Down),
            None,
            EditMods::default(),
            KeyTheme::default(),
        );
    }

    /// Sections take their providers' order whatever order they answer in, and the
    /// selection runs through the app results and then every section's rows.
    #[test]
    fn sections_keep_provider_order_and_the_selection_runs_through_them() {
        let mut s = typed('a');
        s.set_results(vec![entry("app.desktop", "App")]);
        s.set_section("second", Some(section(1, "second", &["c"])));
        s.set_section("first", Some(section(0, "first", &["a", "b"])));
        let order: Vec<&str> = s.sections().iter().map(|s| s.provider.as_str()).collect();
        assert_eq!(order, ["first", "second"]);

        let mut seen = vec![s.selected_id().unwrap().to_owned()];
        for _ in 0..4 {
            down(&mut s);
            seen.push(s.selected_id().unwrap().to_owned());
        }
        assert_eq!(
            seen,
            ["app.desktop", "a", "b", "c", "c"],
            "clamped at the last row"
        );
        assert_eq!(
            s.selected_activation(),
            Some(Activation::Result {
                provider: "second".into(),
                id: "c".into(),
                clipboard_text: Some("copied c".into()),
            })
        );
    }

    /// Until the keyboard moves it, the selection is the default result — the first one — and
    /// follows it as sections arrive. After, it stays on its result while others come and go.
    #[test]
    fn the_selection_follows_the_default_until_moved_then_stays_put() {
        let mut s = typed('a');
        s.set_section("second", Some(section(1, "second", &["c"])));
        assert_eq!(s.selected_id(), Some("c"));
        s.set_section("first", Some(section(0, "first", &["a"])));
        assert_eq!(
            s.selected_id(),
            Some("a"),
            "an earlier section's row is the new default"
        );

        down(&mut s);
        assert_eq!(s.selected_id(), Some("c"));
        s.set_section("first", Some(section(0, "first", &["a", "b"])));
        assert_eq!(
            s.selected_id(),
            Some("c"),
            "a moved selection stays on its result"
        );
        s.set_section("second", None);
        assert_eq!(
            s.selected_id(),
            Some("a"),
            "its result gone, back to the default"
        );
    }

    /// Rows and headers hit-test, and activate as their provider's result and its search.
    #[test]
    fn rows_and_headers_hit_and_activate() {
        let mut s = typed('a');
        s.set_expand(1.);
        s.set_section("p", Some(section(0, "p", &["a", "b"])));
        let area = area_1080();
        for hit in [
            SearchHit::Row { section: 0, row: 1 },
            SearchHit::Provider(0),
        ] {
            let at = s.hit_center(hit, area).unwrap();
            assert_eq!(s.hit_test(at, area), Some(hit));
        }
        assert_eq!(
            s.hit_activation(SearchHit::Row { section: 0, row: 1 }),
            Some(Activation::Result {
                provider: "p".into(),
                id: "b".into(),
                clipboard_text: Some("copied b".into()),
            })
        );
        assert_eq!(
            s.hit_activation(SearchHit::Provider(0)),
            Some(Activation::LaunchSearch {
                provider: "p".into()
            })
        );
    }

    /// A section that would run past the bottom of the results view is left out rather than
    /// drawn over the dash.
    #[test]
    fn sections_that_do_not_fit_are_left_out() {
        let mut s = typed('a');
        for i in 0..20 {
            let id = format!("p{i}");
            s.set_section(&id, Some(section(i, &id, &["a", "b", "c", "d", "e"])));
        }
        let area = area_1080();
        let l = s.layout(area).results.unwrap();
        assert!(!l.sections.is_empty() && l.sections.len() < 20);
        let last = l.sections.last().unwrap().card;
        assert!(last.loc.y + last.size.h <= area.results.loc.y + area.results.size.h);
    }

    impl OverviewSearch {
        #[cfg(test)]
        fn selected(&self) -> usize {
            self.selected
        }
    }
}
