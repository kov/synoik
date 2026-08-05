// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `widget::Menu` — a popup menu built from a model at runtime.
//!
//! Every menu in the shell so far has been built from statically-known structure: the rows are
//! known when the code is written, so each surface hand-rolls its own row enum, layout arithmetic
//! and hit test. `app_menu.rs` said as much in its own header — the metrics there are "the shared
//! popup-menu box model, not app-menu specific, and the next menu should lift them out rather than
//! restate them". This is that lift.
//!
//! It exists because app indicators need it: a `com.canonical.dbusmenu` menu is a **remote tree**
//! whose shape is only known at runtime, with submenus, checkmarks, radio groups, per-row icons
//! and rows that come and go while the menu is open (see `docs/fork/status-notifier-port.md`).
//! Nothing hand-rolled can absorb that.
//!
//! **Box model.** GNOME's `.popup-menu-item` (`_common.scss:135`, `_popovers.scss`), the same one
//! `app_menu` measured — lifted here verbatim so the two cannot drift.
//!
//! **Submenus expand in place**, as GNOME's `PopupSubMenuMenuItem` does (`popupMenu.js:1308`,
//! shipped in `status/keyboard.js` and `status/network.js`): the parent row grows a chevron and its
//! children appear indented beneath it, inside the one surface. The alternative — a second popover
//! beside the parent, which is what these clients' native toolkits do — would need a grab spanning
//! two surfaces and would look unlike every other menu in the shell.
//!
//! **Not done here:** a menu taller than the screen is not scrollable yet. Inline expansion is what
//! makes that reachable (a deep tree grows the surface), so it is the first thing to add; until
//! then a very deep expansion clips at the screen edge.

use std::cell::RefCell;
use std::collections::HashSet;

use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use super::{style, Align, BakeCache, Painter, ShapedText, TextShaper, TextStyle};
use crate::render_helpers::vulkan::{VulkanFrame, VulkanRenderer};

/// `.popup-menu-content` padding — `$base_padding` (`_popovers.scss:28`).
pub const CONTENT_PAD: f64 = 6.;
/// `.popup-menu-item` vertical padding — `$base_padding * 1.5` (`%menuitem`, `_common.scss:135`).
const ITEM_PAD_V: f64 = 9.;
/// `.popup-menu-item` horizontal padding — `$base_padding * 2`.
const ITEM_PAD_H: f64 = 12.;
/// An item's line box: the label, or the `$scalable_icon_size` an item's icon would occupy —
/// whichever is taller, and they are the same at this font size.
const ITEM_LINE_H: f64 = 16.;
/// A menu row's height, from the box model above.
pub const ROW_H: f64 = 2. * ITEM_PAD_V + ITEM_LINE_H; // 34
/// `$menuitem_border_radius = $base_border_radius * 1.5` (`_popovers.scss:6,39`).
const ITEM_RADIUS: f64 = 12.;
/// A separator row: the same item padding around a 1px rule (`PopupSeparatorMenuItem` is a
/// `popup-menu-item`, `popupMenu.js:300-306`; `.popup-separator-menu-item-separator { height: 1px
/// }`, `_popovers.scss:117-120`).
const SEP_H: f64 = 2. * ITEM_PAD_V + 1.;
/// The separator rule — `$borders_color`, white at 10% (`_popovers.scss:119`).
const SEPARATOR: [f32; 4] = [1., 1., 1., 0.1];
/// `.popup-menu-content` `border-radius: $modal_radius * 1.25` (`_popovers.scss:30`).
const RADIUS: f64 = 20.;
/// Between a section header's label and the rule that follows it.
const HEADER_RULE_GAP: f64 = 6.;
/// The shortest a section header's rule may get before it stops reading as one.
const HEADER_RULE_MIN: f64 = 24.;
/// A `.system-status-icon`-sized ornament or row icon.
pub const ICON: f64 = 16.;
/// Between an ornament/icon and the label it belongs to.
const ICON_GAP: f64 = 6.;
/// How far one nesting level indents an expanded submenu's children.
const INDENT: f64 = 16.;
/// A disabled row's text alpha. GNOME dims insensitive items with `@insensitive_fg_color`, which is
/// the foreground at 50% (`_common.scss`, `:disabled`).
const DISABLED_ALPHA: f32 = 0.5;

/// Row text: `.popup-menu` inherits the 11pt body size (`_popovers.scss:16-24`).
const TEXT_PT: f64 = 11.;
fn text_px() -> f64 {
    crate::ui::pt_to_px(TEXT_PT)
}
/// `.popup-menu { min-width: 15em }` (`_popovers.scss:17`), em against the row font.
fn min_w() -> f64 {
    15. * text_px()
}
/// `.app-menu { max-width: 27.25em }` (`_popovers.scss:143-144`). A longer label is ellipsized
/// rather than widening the menu.
fn max_w() -> f64 {
    27.25 * text_px()
}

/// A row's check state — GNOME's `PopupBaseMenuItem` *ornament*, drawn at the row's left
/// (`popupMenu.js` `setOrnament`), and DBusMenu's `toggle-type`/`toggle-state` pair
/// (`dbusMenu.js:82-83`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ornament {
    #[default]
    None,
    /// An independent on/off row.
    Check(bool),
    /// One of a group; only the selected one shows its mark.
    Radio(bool),
}

impl Ornament {
    /// Whether this ornament currently draws a mark.
    fn is_marked(self) -> bool {
        matches!(self, Self::Check(true) | Self::Radio(true))
    }

    /// Whether the row reserves ornament space at all. A menu with *no* ornamented rows does not
    /// indent its labels, so an ordinary menu looks like an ordinary menu.
    fn reserves_space(self) -> bool {
        self != Self::None
    }
}

/// One activatable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// The caller's identity for this row, handed back when it is activated. Opaque here — a
    /// DBusMenu item id, or an index into the caller's own table.
    pub id: u64,
    pub label: String,
    /// A disabled row is drawn dimmed and cannot be activated or focused (DBusMenu `enabled`).
    pub enabled: bool,
    pub ornament: Ornament,
    /// A themed icon name drawn at the row's left (DBusMenu `icon-name`).
    pub icon: Option<String>,
    /// Non-empty makes this a submenu row: a chevron, and children that expand in place.
    pub children: Vec<MenuEntry>,
}

impl MenuItem {
    /// A plain enabled row.
    pub fn new(id: u64, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            ornament: Ornament::None,
            icon: None,
            children: Vec::new(),
        }
    }

    pub fn with_ornament(mut self, ornament: Ornament) -> Self {
        self.ornament = ornament;
        self
    }

    pub fn with_children(mut self, children: Vec<MenuEntry>) -> Self {
        self.children = children;
        self
    }

    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    fn is_submenu(&self) -> bool {
        !self.children.is_empty()
    }
}

/// What a menu is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    /// A bare rule between groups.
    Separator,
    /// `PopupSeparatorMenuItem(text)` (`popupMenu.js:300-324`): a label followed by a rule that
    /// fills the rest of the row.
    SectionHeader(String),
}

/// One row as laid out: an entry, the depth it sits at, and the path to reach it.
#[derive(Debug, Clone)]
struct VisibleRow {
    /// Index path through the tree, so a click can be routed without a second search.
    path: Vec<usize>,
    depth: usize,
    height: f64,
}

/// What a click on the menu did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuHit {
    /// A row was activated; the caller decides what its id means.
    Activated(u64),
    /// A submenu row was expanded or collapsed. Nothing left the menu; the caller redraws.
    Toggled(u64),
    /// The click landed on the menu but on nothing actionable (padding, a separator, a disabled
    /// row). Consumed, because a menu swallows clicks inside itself.
    Nothing,
}

/// A popup menu built from a model.
pub struct Menu {
    entries: Vec<MenuEntry>,
    /// Ids of submenu rows currently expanded. Kept by id rather than by index so a model update
    /// that reorders or inserts rows does not silently expand a different submenu.
    expanded: HashSet<u64>,
    /// The row the pointer is over, as an index into [`Self::visible_rows`].
    hovered: Option<usize>,
    /// The keyboard-focused row, same indexing. Distinct from hover: GNOME's menus track both, and
    /// a pointer moving over a menu must not steal the keyboard's place in it.
    focused: Option<usize>,
    revision: u64,
    bg_cache: RefCell<BakeCache>,
}

impl Menu {
    pub fn new(entries: Vec<MenuEntry>) -> Self {
        Self {
            entries,
            expanded: HashSet::new(),
            hovered: None,
            focused: None,
            revision: 0,
            bg_cache: RefCell::new(BakeCache::new()),
        }
    }

    /// Replace the model, keeping which submenus are open and where the keyboard is.
    ///
    /// A remote menu can change under the user at any moment (`LayoutUpdated`), and a rebuild that
    /// collapsed everything would make an app that repaints its menu unusable. Returns whether
    /// anything changed.
    pub fn set_entries(&mut self, entries: Vec<MenuEntry>) -> bool {
        if entries == self.entries {
            return false;
        }
        // Resolve which *row* had the focus before the model is replaced — afterwards the old
        // index points at whatever moved into that position, which is how focus silently jumps to
        // a different row when a client inserts one above it.
        let focused_id = self.focused.and_then(|k| self.row_id(k));
        self.entries = entries;
        self.revision += 1;
        self.focused = focused_id.and_then(|id| self.index_of(id));
        self.hovered = None;
        true
    }

    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The rows currently on screen, in order: every top-level entry, and the children of each
    /// expanded submenu directly beneath it.
    fn visible_rows(&self) -> Vec<VisibleRow> {
        fn walk(
            entries: &[MenuEntry],
            expanded: &HashSet<u64>,
            prefix: &mut Vec<usize>,
            depth: usize,
            out: &mut Vec<VisibleRow>,
        ) {
            for (i, entry) in entries.iter().enumerate() {
                prefix.push(i);
                out.push(VisibleRow {
                    path: prefix.clone(),
                    depth,
                    height: match entry {
                        MenuEntry::Separator => SEP_H,
                        _ => ROW_H,
                    },
                });
                if let MenuEntry::Item(item) = entry {
                    if item.is_submenu() && expanded.contains(&item.id) {
                        walk(&item.children, expanded, prefix, depth + 1, out);
                    }
                }
                prefix.pop();
            }
        }

        let mut out = Vec::new();
        walk(&self.entries, &self.expanded, &mut Vec::new(), 0, &mut out);
        out
    }

    /// The entry at an index path.
    fn entry_at(&self, path: &[usize]) -> Option<&MenuEntry> {
        let (&first, rest) = path.split_first()?;
        let mut entry = self.entries.get(first)?;
        for &i in rest {
            let MenuEntry::Item(item) = entry else {
                return None;
            };
            entry = item.children.get(i)?;
        }
        Some(entry)
    }

    fn item_at(&self, path: &[usize]) -> Option<&MenuItem> {
        match self.entry_at(path)? {
            MenuEntry::Item(item) => Some(item),
            _ => None,
        }
    }

    /// The id of the visible row at `k`, if it is an item.
    fn row_id(&self, k: usize) -> Option<u64> {
        let rows = self.visible_rows();
        self.item_at(&rows.get(k)?.path).map(|item| item.id)
    }

    /// Where a given id currently sits among the visible rows.
    fn index_of(&self, id: u64) -> Option<usize> {
        let rows = self.visible_rows();
        rows.iter()
            .position(|row| self.item_at(&row.path).is_some_and(|item| item.id == id))
    }

    /// Whether *any* visible row carries an ornament. The whole menu indents together or not at
    /// all — a menu whose labels shift sideways when one row gains a checkmark reads as broken.
    fn reserves_ornament(&self) -> bool {
        self.visible_rows().iter().any(|row| {
            self.item_at(&row.path)
                .is_some_and(|item| item.ornament.reserves_space() || item.icon.is_some())
        })
    }

    /// The top y of visible row `k`, content space.
    fn row_top(&self, rows: &[VisibleRow], k: usize) -> f64 {
        CONTENT_PAD + rows[..k].iter().map(|r| r.height).sum::<f64>()
    }

    /// Visible row `k`'s hover/click band.
    fn row_rect(&self, rows: &[VisibleRow], k: usize, width: f64) -> Rectangle<f64, Logical> {
        // A nested row's band is inset, so the hover wash reads as belonging to its parent.
        let indent = rows[k].depth as f64 * INDENT;
        Rectangle::new(
            Point::from((CONTENT_PAD + indent, self.row_top(rows, k))),
            Size::from((width - 2. * CONTENT_PAD - indent, rows[k].height)),
        )
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let rows = self.visible_rows();
        let lead = self.leading_inset();
        let widest = rows
            .iter()
            .filter_map(|row| {
                let indent = row.depth as f64 * INDENT;
                match self.entry_at(&row.path)? {
                    MenuEntry::Item(item) => {
                        let w = measure(&item.label);
                        // A submenu row also has to fit its chevron.
                        let chevron = if item.is_submenu() {
                            ICON_GAP + ICON
                        } else {
                            0.
                        };
                        Some(indent + lead + w + chevron)
                    }
                    MenuEntry::SectionHeader(label) => {
                        Some(indent + measure(label) + HEADER_RULE_GAP + HEADER_RULE_MIN)
                    }
                    MenuEntry::Separator => None,
                }
            })
            .fold(0., f64::max);

        let width = (2. * (CONTENT_PAD + ITEM_PAD_H) + widest).clamp(min_w(), max_w());
        let height = self.row_top(&rows, rows.len()) + CONTENT_PAD;
        Size::from((width, height))
    }

    /// How far a row's label is pushed right to clear the ornament/icon column.
    fn leading_inset(&self) -> f64 {
        if self.reserves_ornament() {
            ICON + ICON_GAP
        } else {
            0.
        }
    }

    pub fn corner_radius(&self) -> f64 {
        RADIUS
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Every visible row's label, top to bottom, separators excluded — the menu as a person reads
    /// it, for the corpus. An expanded submenu's children are included, in place.
    pub fn labels(&self) -> Vec<&str> {
        self.visible_rows()
            .into_iter()
            .filter_map(|row| match self.entry_at(&row.path)? {
                MenuEntry::Item(item) => Some(item.label.as_str()),
                MenuEntry::SectionHeader(label) => Some(label.as_str()),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    /// The menu-local centre of the row labelled `label`, so a test can click a row by name rather
    /// than by arithmetic that would drift with the box model.
    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        let width = self.logical_size().w;
        let rows = self.visible_rows();
        let k = rows.iter().position(|row| {
            self.item_at(&row.path)
                .is_some_and(|item| item.label == label)
        })?;
        let rect = self.row_rect(&rows, k, width);
        Some(Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + rect.size.h / 2.,
        )))
    }

    /// Whether the submenu row with `id` is expanded.
    pub fn is_expanded(&self, id: u64) -> bool {
        self.expanded.contains(&id)
    }

    /// Expand or collapse a submenu row by id, returning whether anything changed.
    pub fn set_expanded(&mut self, id: u64, expanded: bool) -> bool {
        let changed = if expanded {
            self.expanded.insert(id)
        } else {
            self.expanded.remove(&id)
        };
        if changed {
            // The rows below just moved, so whatever the pointer was over is no longer under it.
            self.hovered = None;
            self.revision += 1;
        }
        changed
    }

    /// Route a menu-local click.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> MenuHit {
        let width = self.logical_size().w;
        let rows = self.visible_rows();
        for k in 0..rows.len() {
            if !self.row_rect(&rows, k, width).contains(pos) {
                continue;
            }
            let Some(item) = self.item_at(&rows[k].path) else {
                // A separator or header: inside the menu, but not a target.
                return MenuHit::Nothing;
            };
            if !item.enabled {
                return MenuHit::Nothing;
            }
            let (id, is_submenu) = (item.id, item.is_submenu());
            if is_submenu {
                // A submenu row toggles rather than activates: it has no action of its own.
                let now = !self.is_expanded(id);
                self.set_expanded(id, now);
                return MenuHit::Toggled(id);
            }
            return MenuHit::Activated(id);
        }
        MenuHit::Nothing
    }

    /// Update the hovered row (`None` clears). Returns whether it changed.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let width = self.logical_size().w;
        let rows = self.visible_rows();
        let hovered = pos.and_then(|p| {
            (0..rows.len()).find(|&k| {
                self.item_at(&rows[k].path).is_some_and(|i| i.enabled)
                    && self.row_rect(&rows, k, width).contains(p)
            })
        });
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.revision += 1;
        true
    }

    /// The focused row's id, if any.
    pub fn focused_id(&self) -> Option<u64> {
        self.focused.and_then(|k| self.row_id(k))
    }

    /// Move the keyboard focus by `delta` rows, skipping anything that cannot take it (separators,
    /// headers, disabled rows) and wrapping at the ends, as GNOME's menus do.
    ///
    /// Returns whether the focus moved.
    pub fn focus_step(&mut self, delta: isize) -> bool {
        let rows = self.visible_rows();
        let focusable: Vec<usize> = (0..rows.len())
            .filter(|&k| self.item_at(&rows[k].path).is_some_and(|i| i.enabled))
            .collect();
        if focusable.is_empty() {
            return false;
        }

        let next = match self
            .focused
            .and_then(|cur| focusable.iter().position(|&k| k == cur))
        {
            Some(pos) => {
                let len = focusable.len() as isize;
                focusable[(((pos as isize + delta) % len + len) % len) as usize]
            }
            // Entering from nowhere: down lands on the first row, up on the last.
            None if delta >= 0 => focusable[0],
            None => focusable[focusable.len() - 1],
        };

        if self.focused == Some(next) {
            return false;
        }
        self.focused = Some(next);
        self.revision += 1;
        true
    }

    /// Activate the focused row — Enter/Space. A focused submenu row expands instead.
    pub fn activate_focused(&mut self) -> MenuHit {
        let Some(id) = self.focused_id() else {
            return MenuHit::Nothing;
        };
        let rows = self.visible_rows();
        let is_submenu = rows
            .iter()
            .find_map(|row| self.item_at(&row.path).filter(|i| i.id == id))
            .is_some_and(|item| item.is_submenu());

        if is_submenu {
            let now = !self.is_expanded(id);
            self.set_expanded(id, now);
            return MenuHit::Toggled(id);
        }
        MenuHit::Activated(id)
    }

    /// Right/Left on a submenu row: open it, or close it. Returns whether anything changed —
    /// `false` means the key was not ours and the caller should do whatever it does with it (Left
    /// on a top-level row, for instance, is not a menu gesture).
    pub fn focus_expand(&mut self, expand: bool) -> bool {
        let Some(id) = self.focused_id() else {
            return false;
        };
        let rows = self.visible_rows();
        let Some(item) = rows
            .iter()
            .find_map(|row| self.item_at(&row.path).filter(|i| i.id == id))
        else {
            return false;
        };
        if !item.is_submenu() {
            return false;
        }
        self.set_expanded(id, expand)
    }

    /// Clear hover and focus — what closing and reopening a menu owes.
    pub fn reset_navigation(&mut self) {
        if self.hovered.is_some() || self.focused.is_some() {
            self.revision += 1;
        }
        self.hovered = None;
        self.focused = None;
    }

    /// The ornament/icon centres to composite over the baked card, in menu-local coordinates:
    /// `(icon names, centre)` per row that has one.
    ///
    /// Icons are *elements*, not paint verbs — the toolkit composites them over a baked card the
    /// way `input_source_menu` composites its check marks — so the caller draws these itself.
    pub fn ornaments(&self) -> Vec<(Vec<String>, Point<f64, Logical>)> {
        let width = self.logical_size().w;
        let rows = self.visible_rows();
        let mut out = Vec::new();

        for k in 0..rows.len() {
            let Some(item) = self.item_at(&rows[k].path) else {
                continue;
            };
            let rect = self.row_rect(&rows, k, width);
            let mid_y = rect.loc.y + rect.size.h / 2.;

            // The leading column: a checkmark if marked, else the row's own icon. A marked row
            // with an icon shows the mark — the state matters more than the decoration.
            let leading: Option<Vec<String>> = if item.ornament.is_marked() {
                Some(style::CHECK_ICONS.iter().map(|s| (*s).to_owned()).collect())
            } else {
                item.icon.clone().map(|name| vec![name])
            };
            if let Some(names) = leading {
                out.push((
                    names,
                    Point::from((rect.loc.x + ITEM_PAD_H + ICON / 2., mid_y)),
                ));
            }

            // The trailing chevron on a submenu row, pointing down when open and right when shut —
            // GNOME rotates the same arrow (`popupMenu.js:1330-1338`).
            if item.is_submenu() {
                let name = if self.is_expanded(item.id) {
                    "pan-down-symbolic"
                } else {
                    "pan-end-symbolic"
                };
                out.push((
                    vec![name.to_owned()],
                    Point::from((rect.loc.x + rect.size.w - ITEM_PAD_H - ICON / 2., mid_y)),
                ));
            }
        }
        out
    }

    /// Shape every row's label — the miss-only prepare phase for [`super::bake`].
    pub fn shape_rows(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<Vec<Option<ShapedText>>> {
        let mut shaper = TextShaper::new(renderer, scale);
        let style = TextStyle::new(TEXT_PT);
        self.visible_rows()
            .into_iter()
            .map(|row| match self.entry_at(&row.path) {
                Some(MenuEntry::Item(item)) => shaper.shape(&item.label, style).map(Some),
                Some(MenuEntry::SectionHeader(label)) => shaper.shape(label, style).map(Some),
                _ => Ok(None),
            })
            .collect()
    }

    /// Paint the menu's rows onto a transparent card. The popover chrome draws the box behind it.
    pub fn paint(
        &self,
        frame: &mut VulkanFrame,
        phys: Size<i32, Physical>,
        scale: f64,
        runs: &[Option<ShapedText>],
    ) -> anyhow::Result<()> {
        let size = self.logical_size();
        let rows = self.visible_rows();
        let lead = self.leading_inset();
        let mut p = Painter::new(frame, scale, phys);
        p.clear(style::TRANSPARENT)?;

        for (k, run) in runs.iter().enumerate().take(rows.len()) {
            let rect = self.row_rect(&rows, k, size.w);
            let entry = self.entry_at(&rows[k].path);

            match (entry, run) {
                (Some(MenuEntry::Separator), _) => {
                    let rule = Rectangle::new(
                        Point::from((rect.loc.x, rect.loc.y + (SEP_H - 1.) / 2.)),
                        Size::from((rect.size.w, 1.)),
                    );
                    p.fill_rounded(rule, 0., SEPARATOR)?;
                }
                (Some(MenuEntry::SectionHeader(label)), Some(run)) => {
                    let label_x = rect.loc.x + ITEM_PAD_H;
                    p.text(
                        run,
                        Point::from((label_x, rect.loc.y + rect.size.h / 2.)),
                        Align::LEFT_MIDDLE,
                        style::TEXT,
                    )?;
                    let rule_x = label_x + measure(label) + HEADER_RULE_GAP;
                    let rule_end = rect.loc.x + rect.size.w - ITEM_PAD_H;
                    if rule_end > rule_x {
                        let rule = Rectangle::new(
                            Point::from((rule_x, rect.loc.y + (rect.size.h - 1.) / 2.)),
                            Size::from((rule_end - rule_x, 1.)),
                        );
                        p.fill_rounded(rule, 0., SEPARATOR)?;
                    }
                }
                (Some(MenuEntry::Item(item)), Some(run)) => {
                    // Hover and keyboard focus draw the same wash: GNOME's menus give the focused
                    // item the hover look rather than a second style.
                    if self.hovered == Some(k) || self.focused == Some(k) {
                        p.fill_rounded(rect, ITEM_RADIUS, style::HOVER_WASH)?;
                    }
                    let mut color = style::TEXT;
                    if !item.enabled {
                        color[3] *= DISABLED_ALPHA;
                    }
                    p.text(
                        run,
                        Point::from((
                            rect.loc.x + ITEM_PAD_H + lead,
                            rect.loc.y + rect.size.h / 2.,
                        )),
                        Align::LEFT_MIDDLE,
                        color,
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Bake the menu's card. Callers composite [`Self::ornaments`] over the result.
    pub fn bake(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<crate::render_helpers::vulkan::VkTexture> {
        super::bake(
            renderer,
            &mut self.bg_cache.borrow_mut(),
            scale,
            self.logical_size(),
            self.revision,
            |renderer| self.shape_rows(renderer, scale),
            |frame, phys, runs| self.paint(frame, phys, scale, runs),
        )
    }
}

fn measure(label: &str) -> f64 {
    synoik_vk::text::measure_line_width_weighted(label, text_px() as f32, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, label: &str) -> MenuEntry {
        MenuEntry::Item(MenuItem::new(id, label))
    }

    /// A tree with one submenu, which is the shape a DBusMenu menu arrives in.
    fn tree() -> Vec<MenuEntry> {
        vec![
            item(1, "Open"),
            MenuEntry::Item(
                MenuItem::new(2, "Settings")
                    .with_children(vec![item(21, "Account"), item(22, "Network")]),
            ),
            MenuEntry::Separator,
            item(3, "Quit"),
        ]
    }

    #[test]
    fn a_submenus_children_appear_only_while_it_is_expanded() {
        let mut menu = Menu::new(tree());
        assert_eq!(menu.labels(), vec!["Open", "Settings", "Quit"]);

        assert!(menu.set_expanded(2, true));
        assert_eq!(
            menu.labels(),
            vec!["Open", "Settings", "Account", "Network", "Quit"],
            "children expand in place, between their parent and what follows it"
        );

        // The menu got taller, and stayed as wide as its widest row needs.
        assert!(menu.set_expanded(2, false));
        assert_eq!(menu.labels(), vec!["Open", "Settings", "Quit"]);
    }

    #[test]
    fn expanding_a_submenu_grows_the_menu_and_indents_its_children() {
        let mut menu = Menu::new(tree());
        let shut = menu.logical_size();
        menu.set_expanded(2, true);
        let open = menu.logical_size();
        assert!(open.h > shut.h, "an expanded submenu makes the menu taller");

        // The child rows are inset relative to their parent's row.
        let parent = menu.row_center("Settings").unwrap();
        let child = menu.row_center("Account").unwrap();
        assert!(child.y > parent.y);
        let rows = menu.visible_rows();
        let parent_rect = menu.row_rect(&rows, 1, open.w);
        let child_rect = menu.row_rect(&rows, 2, open.w);
        assert!(
            child_rect.loc.x > parent_rect.loc.x,
            "a child row is indented from its parent"
        );
    }

    #[test]
    fn clicking_a_submenu_row_toggles_it_rather_than_activating() {
        let mut menu = Menu::new(tree());
        let at = menu.row_center("Settings").unwrap();

        assert_eq!(menu.pointer_click(at), MenuHit::Toggled(2));
        assert!(menu.is_expanded(2));
        assert_eq!(menu.pointer_click(at), MenuHit::Toggled(2));
        assert!(!menu.is_expanded(2));

        // A leaf activates.
        let quit = menu.row_center("Quit").unwrap();
        assert_eq!(menu.pointer_click(quit), MenuHit::Activated(3));
    }

    #[test]
    fn a_disabled_row_can_be_neither_clicked_nor_focused() {
        let mut menu = Menu::new(vec![
            MenuEntry::Item(MenuItem::new(1, "Available")),
            MenuEntry::Item(MenuItem::new(2, "Unavailable").disabled()),
        ]);
        let at = menu.row_center("Unavailable").unwrap();
        assert_eq!(menu.pointer_click(at), MenuHit::Nothing);
        assert!(
            !menu.pointer_hover(Some(at)),
            "a disabled row does not hover"
        );

        // Stepping down twice wraps back to the only focusable row rather than landing on it.
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(1));
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(1));
    }

    #[test]
    fn a_click_on_a_separator_is_swallowed() {
        let mut menu = Menu::new(tree());
        let size = menu.logical_size();
        let rows = menu.visible_rows();
        // Row 2 is the separator.
        let rect = menu.row_rect(&rows, 2, size.w);
        let at = Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));
        assert_eq!(menu.pointer_click(at), MenuHit::Nothing);
    }

    #[test]
    fn keyboard_navigation_skips_what_cannot_be_focused_and_wraps() {
        let mut menu = Menu::new(tree());

        // Down from nowhere enters at the top; the separator is stepped over.
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(1));
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(2));
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(3), "the separator is not focusable");
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(1), "and it wraps");

        // Up from nowhere enters at the bottom.
        let mut menu = Menu::new(tree());
        menu.focus_step(-1);
        assert_eq!(menu.focused_id(), Some(3));
    }

    #[test]
    fn right_and_left_open_and_close_the_focused_submenu() {
        let mut menu = Menu::new(tree());
        menu.focus_step(1);
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(2));

        assert!(menu.focus_expand(true));
        assert!(menu.is_expanded(2));
        // Already open: nothing to do, and the caller learns the key was not consumed.
        assert!(!menu.focus_expand(true));
        assert!(menu.focus_expand(false));
        assert!(!menu.is_expanded(2));

        // On a leaf, neither direction is a menu gesture.
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(3));
        assert!(!menu.focus_expand(true));
    }

    #[test]
    fn enter_activates_a_leaf_and_expands_a_submenu() {
        let mut menu = Menu::new(tree());
        menu.focus_step(1);
        assert_eq!(menu.activate_focused(), MenuHit::Activated(1));

        menu.focus_step(1);
        assert_eq!(menu.activate_focused(), MenuHit::Toggled(2));
        assert!(menu.is_expanded(2));
    }

    #[test]
    fn a_model_update_keeps_open_submenus_and_the_keyboard_place() {
        let mut menu = Menu::new(tree());
        menu.set_expanded(2, true);
        menu.focus_step(1);
        assert_eq!(menu.focused_id(), Some(1));

        // The client repaints its menu with a row inserted at the top — a `LayoutUpdated` — and
        // neither the open submenu nor the focused row may be lost by it.
        let mut updated = vec![item(9, "Sign in")];
        updated.extend(tree());
        assert!(menu.set_entries(updated));

        assert!(menu.is_expanded(2), "the open submenu stays open");
        assert_eq!(
            menu.focused_id(),
            Some(1),
            "focus follows its row, not its index"
        );
        assert_eq!(
            menu.labels(),
            vec!["Sign in", "Open", "Settings", "Account", "Network", "Quit"]
        );

        // An identical model is not a change, so a client repainting nothing costs no redraw.
        let same = menu.entries().to_vec();
        assert!(!menu.set_entries(same));
    }

    #[test]
    fn the_ornament_column_is_reserved_for_the_whole_menu_at_once() {
        // No ornaments: labels sit at the plain padding.
        let plain = Menu::new(vec![item(1, "Open")]);
        assert_eq!(plain.leading_inset(), 0.);

        // One checked row indents every label, so nothing shifts sideways when a row's state
        // changes while the menu is open.
        let marked = Menu::new(vec![
            MenuEntry::Item(MenuItem::new(1, "Open")),
            MenuEntry::Item(MenuItem::new(2, "Mute").with_ornament(Ornament::Check(true))),
        ]);
        assert!(marked.leading_inset() > 0.);

        // An *unchecked* row still reserves the column — otherwise ticking it would move the text.
        let unmarked = Menu::new(vec![MenuEntry::Item(
            MenuItem::new(2, "Mute").with_ornament(Ornament::Check(false)),
        )]);
        assert_eq!(unmarked.leading_inset(), marked.leading_inset());
    }

    #[test]
    fn a_submenu_row_draws_a_chevron_that_turns_when_it_opens() {
        let mut menu = Menu::new(tree());
        let shut: Vec<String> = menu
            .ornaments()
            .into_iter()
            .flat_map(|(names, _)| names)
            .collect();
        assert!(shut.iter().any(|n| n == "pan-end-symbolic"));

        menu.set_expanded(2, true);
        let open: Vec<String> = menu
            .ornaments()
            .into_iter()
            .flat_map(|(names, _)| names)
            .collect();
        assert!(open.iter().any(|n| n == "pan-down-symbolic"));
        assert!(!open.iter().any(|n| n == "pan-end-symbolic"));
    }

    #[test]
    fn a_checked_row_shows_its_mark_and_an_unchecked_one_does_not() {
        let menu = Menu::new(vec![
            MenuEntry::Item(MenuItem::new(1, "On").with_ornament(Ornament::Check(true))),
            MenuEntry::Item(MenuItem::new(2, "Off").with_ornament(Ornament::Check(false))),
        ]);
        let marks: Vec<_> = menu.ornaments();
        assert_eq!(marks.len(), 1, "only the checked row draws a mark");
        assert!(marks[0].0.iter().any(|n| n == style::CHECK_ICONS[0]));
    }
}
