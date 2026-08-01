//! The live switcher surface: the state machine of [`super::SwitcherPopup`] plus the items,
//! geometry and textures needed to actually draw it.
//!
//! One holder serves both popups. GNOME has two `SwitcherPopup` subclasses, but everything that
//! is not *item art* — the grab, the timers, keynav, commit, placement, the panel — is identical
//! between them, so the split lives in [`Items`] rather than in two parallel surfaces.

use std::cell::RefCell;
use std::time::Duration;

use niri_config::Modifiers;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::AppIconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::switcher::app_switcher::{self, AppItem};
use crate::ui::switcher::{
    PanelLayout, SwitcherKey, SwitcherKind, SwitcherOutcome, SwitcherPopup, Visibility,
    ITEM_PADDING, ITEM_RADIUS, ITEM_SELECTED, LIST_BG, LIST_BORDER, LIST_BORDER_COLOR, LIST_FG,
    LIST_RADIUS,
};
use crate::ui::widget::{
    self, style, AppIconUploads, BakeCache, Painter, ShapedText, TextShaper, TextStyle,
};
use crate::window::mapped::MappedId;

/// `.alt-tab-app`'s label — `$base_font_size` (`_common.scss:30`), the shell's default. The
/// switcher sets no font size of its own, so the item label is plain body text.
const LABEL_PT: f64 = 11.;

/// What the switcher is cycling through.
///
/// The two variants are the two popup classes. An app item carries *all* of that app's windows
/// because committing on it activates one of them, and slice 5's thumbnail sub-list walks them.
#[derive(Debug, Clone)]
pub enum Items {
    Apps(Vec<AppItem>),
    /// `WindowSwitcherPopup` — slice 3.
    Windows(Vec<MappedId>),
}

impl Items {
    pub fn len(&self) -> usize {
        match self {
            Items::Apps(items) => items.len(),
            Items::Windows(ids) => ids.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn kind(&self) -> SwitcherKind {
        match self {
            Items::Apps(_) => SwitcherKind::Apps,
            Items::Windows(_) => SwitcherKind::Windows,
        }
    }

    /// The window a commit on item `n` activates.
    ///
    /// For an app that is its most recently used window — `_finish` calls
    /// `Main.activateWindow(this._items[i].cachedWindows[0])` when no specific window was picked
    /// (`altTab.js:283-291`), and `cachedWindows` is in tab-list order.
    fn window_at(&self, n: usize) -> Option<MappedId> {
        match self {
            Items::Apps(items) => items.get(n)?.windows.first().copied(),
            Items::Windows(ids) => ids.get(n).copied(),
        }
    }
}

/// Everything one item needs on screen, resolved once when the popup opens.
#[derive(Debug, Clone)]
pub struct ItemArt {
    pub icon: Option<AppIconRef>,
    pub label: String,
    pub arrow: bool,
}

/// Everything [`SwitcherUi::open`] needs, gathered by the caller.
///
/// A struct rather than a long argument list because the caller assembles these from four
/// different places — the app system, the tab list, the seat's modifier state and the output —
/// and positional arguments of the same type (two `Modifiers`, two `f64`) are trivially
/// swappable at a call site.
pub struct OpenRequest {
    pub items: Items,
    pub art: Vec<ItemArt>,
    /// `binding.is_reversed()` — the `-backward` half of the switch binding.
    pub backward: bool,
    /// The binding's modifiers, whose primary bit commits on release.
    pub mask: Modifiers,
    /// What is actually held right now, for the release race.
    pub held: Modifiers,
    pub output: Output,
    /// The **primary** monitor's logical rect — GNOME centers on it, not on the focused output.
    pub monitor: Rectangle<f64, Logical>,
    pub label_height: f64,
}

struct Open {
    state: SwitcherPopup,
    items: Items,
    art: Vec<ItemArt>,
    layout: PanelLayout,
    output: Output,
    icon_px: f64,
    /// Kept so the row can be re-centered when an item disappears.
    monitor: Rectangle<f64, Logical>,
    label_height: f64,
}

impl Open {
    fn content_size(&self) -> Size<f64, Logical> {
        Size::from((self.icon_px, self.icon_px + self.label_height))
    }
}

/// The switcher surface — at most one is open at a time, as in GNOME (a new `_startSwitcher`
/// destroys any popup already up, `windowManager.js:1699-1701`).
#[derive(Default)]
pub struct SwitcherUi {
    open: Option<Open>,
    chrome: RefCell<BakeCache>,
    icons: RefCell<AppIconUploads>,
}

impl SwitcherUi {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The output the popup is on, so the compositor knows what to redraw.
    pub fn output(&self) -> Option<&Output> {
        self.open.as_ref().map(|o| &o.output)
    }

    pub fn selected(&self) -> Option<usize> {
        self.open.as_ref().map(|o| o.state.selected())
    }

    /// Whether anything is actually on screen — a popup inside its open delay is live but
    /// invisible, and must not force a redraw or a backdrop.
    pub fn is_visible(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.state.visibility() != Visibility::Pending)
    }

    /// Open a switcher, or return the outcome if it finished immediately.
    ///
    /// An immediate finish is not an error: it is the modifier-already-released race
    /// (`switcherPopup.js:144-155`), and the caller must still activate the selection.
    pub fn open(&mut self, req: OpenRequest, now: Duration) -> Option<SwitcherOutcome> {
        debug_assert_eq!(req.items.len(), req.art.len());

        let OpenRequest {
            items,
            art,
            backward,
            mask,
            held,
            output,
            monitor,
            label_height,
        } = req;

        let state = SwitcherPopup::show(items.kind(), items.len(), backward, mask, held, now)?;

        let icon_px = app_switcher::icon_size(
            items.len(),
            label_height,
            app_switcher::available_width(monitor.size.w),
        );
        // Every item is the same square: the icon, with the label under it.
        let content = Size::from((icon_px, icon_px + label_height));
        let layout = PanelLayout::new(&vec![content; items.len()], monitor);

        let outcome = state.outcome();
        self.open = Some(Open {
            state,
            items,
            art,
            layout,
            output,
            icon_px,
            monitor,
            label_height,
        });

        if outcome.is_some() {
            // Nothing was ever drawn, so there is no fade to wait out.
            self.open = None;
        }
        outcome
    }

    /// Drive the timers. Returns an outcome the moment the session ends.
    pub fn advance(&mut self, now: Duration) -> Option<(SwitcherOutcome, Option<MappedId>)> {
        let open = self.open.as_mut()?;
        open.state.poll(now);
        self.take_outcome()
    }

    /// When [`advance`](Self::advance) next has something to do.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.open.as_ref()?.state.next_deadline()
    }

    pub fn key_press(
        &mut self,
        key: SwitcherKey,
        now: Duration,
    ) -> Option<(SwitcherOutcome, Option<MappedId>)> {
        let open = self.open.as_mut()?;
        open.state.key_press(key, now);
        self.take_outcome()
    }

    pub fn key_release(
        &mut self,
        held: Modifiers,
        now: Duration,
    ) -> Option<(SwitcherOutcome, Option<MappedId>)> {
        let open = self.open.as_mut()?;
        open.state.key_release(held, now);
        self.take_outcome()
    }

    /// Pointer motion over the panel. Returns whether the selection moved.
    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        let Some(item) = open.layout.item_at(pos) else {
            return false;
        };
        open.state.pointer_entered_item(item)
    }

    /// A window went away while the popup is up (`_itemRemoved`, `switcherPopup.js:269-284`).
    pub fn window_removed(&mut self, id: MappedId) -> Option<(SwitcherOutcome, Option<MappedId>)> {
        let open = self.open.as_mut()?;

        let index = match &mut open.items {
            Items::Windows(ids) => {
                let index = ids.iter().position(|&w| w == id)?;
                ids.remove(index);
                index
            }
            Items::Apps(apps) => {
                // A window closing only removes the *item* when it was the app's last one.
                let index = apps.iter().position(|a| a.windows.contains(&id))?;
                apps[index].windows.retain(|&w| w != id);
                if !apps[index].windows.is_empty() {
                    // The app is still running; only its arrow may need to go.
                    open.art[index].arrow = apps[index].windows.len() > 1;
                    return None;
                }
                apps.remove(index);
                index
            }
        };

        open.art.remove(index);
        open.state.item_removed(index);
        self.relayout();
        self.take_outcome()
    }

    /// Close without committing — used when something else takes over the screen (a modal, a
    /// session lock), matching `system-modal-opened` (`switcherPopup.js:56-57`).
    pub fn cancel(&mut self) {
        self.open = None;
    }

    fn take_outcome(&mut self) -> Option<(SwitcherOutcome, Option<MappedId>)> {
        let open = self.open.as_ref()?;
        let outcome = open.state.outcome()?;
        let target = match outcome {
            SwitcherOutcome::Commit => open.items.window_at(open.state.selected()),
            SwitcherOutcome::Cancel => None,
        };
        self.open = None;
        Some((outcome, target))
    }

    /// Re-measure after the item count changed, so the row re-centers instead of leaving a hole
    /// where the removed item was.
    ///
    /// The icon size is deliberately *not* recomputed: GNOME sets `_iconSize` once, on the first
    /// height request (`altTab.js:783-788`), so a row that loses an item does not grow its icons
    /// back. Re-laddering here would make windows closing under the popup resize every item.
    fn relayout(&mut self) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let content = open.content_size();
        open.layout = PanelLayout::new(&vec![content; open.items.len()], open.monitor);
    }

    /// Front-to-back, like every other UI `render`.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let Some(open) = self.open.as_ref() else {
            return Vec::new();
        };
        if open.output != *output || open.state.visibility() == Visibility::Pending {
            return Vec::new();
        }

        let scale = output.current_scale().fractional_scale();
        let mut elements = Vec::new();

        // The app icons ride above the panel, one texture each.
        let mut uploads = self.icons.borrow_mut();
        for (i, art) in open.art.iter().enumerate() {
            let Some(icon) = art.icon.as_ref() else {
                continue;
            };
            let Some(item) = open.layout.items.get(i) else {
                continue;
            };
            // The icon sits in the top of the square, with the label below it.
            let center = Point::from((item.size.w / 2., ITEM_PADDING + open.icon_px / 2.));
            if let Some(el) = widget::app_icon_element(
                renderer,
                &mut uploads,
                app_icons,
                icon,
                open.icon_px,
                scale,
                item.loc,
                center,
                1.,
            ) {
                elements.push(el);
            }
        }
        drop(uploads);

        // ...and the panel, its selection wash and every label are one bake behind them.
        let revision = widget::Revision::new()
            .of(open.state.selected())
            .px(open.icon_px)
            .each(open.art.iter().map(|a| (a.label.clone(), a.arrow)))
            .done();

        let panel = open.layout.panel;
        let label_height = open.label_height;
        let items: Vec<Rectangle<f64, Logical>> = open
            .layout
            .items
            .iter()
            .map(|r| Rectangle::new(r.loc - panel.loc, r.size))
            .collect();

        let baked = widget::bake(
            renderer,
            &mut self.chrome.borrow_mut(),
            scale,
            panel.size,
            revision,
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                open.art
                    .iter()
                    .map(|a| shaper.shape(&a.label, TextStyle::new(LABEL_PT)))
                    .collect::<anyhow::Result<Vec<ShapedText>>>()
            },
            |frame, phys, labels: &Vec<ShapedText>| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(LIST_RADIUS, LIST_BG)?;
                p.stroke_rounded_full(LIST_RADIUS, LIST_BORDER, LIST_BORDER_COLOR)?;

                // The selection wash, and nothing for hover — hovering moves the selection
                // instead, so a second highlight would show two current items at once.
                if let Some(rect) = items.get(open.state.selected()) {
                    p.fill_rounded(*rect, ITEM_RADIUS, ITEM_SELECTED)?;
                }

                // The app name under each icon, centered in the strip the icon leaves free.
                for (i, rect) in items.iter().enumerate() {
                    if let Some(label) = labels.get(i) {
                        p.text(
                            label,
                            Point::from((
                                rect.loc.x + rect.size.w / 2.,
                                rect.loc.y + rect.size.h - ITEM_PADDING - label_height / 2.,
                            )),
                            widget::Align::CENTER,
                            LIST_FG,
                        )?;
                    }
                }

                // MISSING: the multi-window chevron (`.switcher-arrow`). Its geometry and
                // visibility are modelled and tested (`app_switcher::arrow_rect`,
                // `AppItem::has_arrow`), but GNOME draws it with cairo line segments
                // (`switcherPopup.js:661-690`) and our Painter has no triangle verb — only
                // rounded rects, text and hairlines. Faking it from two hairlines is exactly the
                // "no faked chrome" smell, so the honest fix is a real polygon primitive in the
                // Painter, which is its own change rather than a rider on this one.
                Ok(())
            },
        );

        match baked {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    panel.loc,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error drawing the switcher panel: {err:#}"),
        }

        elements
    }
}
