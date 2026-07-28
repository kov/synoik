//! The app-folder dialog — gnome-shell's `AppFolderDialog` (`appDisplay.js:2463-2916`).
//!
//! Clicking a folder tile in the app grid opens this: a shade over the whole monitor with a
//! 720×720 panel floating on it, the folder's name across the top and the folder's apps in
//! their own 3×3 paginated grid below. Clicking an app inside launches it; clicking outside
//! the panel, or pressing Escape, pops it back down.
//!
//! The inner view is **the app grid widget itself** — GNOME builds it from the same class
//! (`FolderView extends BaseAppView`, `FolderGrid extends AppGrid`, `appDisplay.js:2066-2112`),
//! differing only in the page modes and how a page spends its slack, so
//! [`AppGrid::folder_view`] configures those two and everything else (hover wash, captions,
//! pagination, dots, navigation arrows, batched icon uploads) comes along for free.
//!
//! Divergences from gnome-shell, deliberate:
//! - **Read-only.** Folder *editing* is out of scope for this slice, so there is no rename entry
//!   and no edit button beside the name. GNOME centers its label by balancing the edit button with
//!   an equally-sized ghost actor (`_addFolderNameEntry`, `appDisplay.js:2531-2601`); with neither
//!   present the label is simply centered in the band, which is the same place.
//! - The panel is **clamped** to the space the container leaves it. GNOME allocates its natural
//!   720² and lets a short screen clip it (which is why the container pads the top by the panel
//!   height at all, `_app-grid.scss:53-56`); we would have no way to reach what fell off.
//! - GNOME constrains the dialog to the **primary** monitor (`MonitorConstraint({primary: true})`,
//!   `appDisplay.js:2478`). We draw the overview's chrome on every output already, and the dialog
//!   follows that same divergence.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock, Curve};
use crate::app_system::AppIconRef;
use crate::niri_render_elements;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::app_grid::{AppGrid, AppGridEntry, FocusDir, PageArrow};
use crate::ui::panel::PANEL_HEIGHT;
use crate::ui::widget::{self, style, Align, Painter};

/// `$app_folder_size` (`_app-grid.scss:4,60-61`) — the panel is a fixed square.
const SIZE: f64 = 720.;
/// `border-radius: $modal_radius * 4` (`_app-grid.scss:63`), and `$modal_radius` is
/// `$base_border_radius * 2` = 16 (`_common.scss:33,40`), so 64 — four times the radius of an
/// ordinary popover, which is what makes the panel read as a rounded card rather than a window.
const RADIUS: f64 = 64.;
/// `padding: 0 1px` (`_app-grid.scss:71`) — the room the inset border occupies.
const PAD_X: f64 = 1.;
/// `box-shadow: inset 0 0 0 1px $system_borders_color` (`_app-grid.scss:72`).
const BORDER_W: f64 = 1.;
/// `.folder-name-container` `padding: $base_padding * 4 $base_padding * 6` with
/// `padding-bottom: 0` (`_app-grid.scss:75-78`).
const NAME_PAD_TOP: f64 = 24.;
const NAME_PAD_X: f64 = 36.;
/// The folder name is a `.folder-name-label`, i.e. `%title_1` — 20pt at weight 800
/// (`_app-grid.scss:79-82`, `_common.scss:246-249`).
const NAME_PT: f64 = 20.;
/// `DIALOG_SHADE_NORMAL` (`appDisplay.js:57`): black at alpha 204/255.
const SHADE: [f32; 4] = [0., 0., 0., 204. / 255.];
/// `DIALOG_SHADE_HIGHLIGHT` (`appDisplay.js:58`) as a fraction of [`SHADE`] — both are pure
/// black, so the lighter shade is the same buffer at less alpha.
const SHADE_HIGHLIGHT_FACTOR: f32 = 85. / 204.;
/// `POPDOWN_DIALOG_TIMEOUT` (`appDisplay.js:29`): how long a drag has to stay outside the
/// panel before the dialog gets out of its way.
pub const POPDOWN_DIALOG_MS: u64 = 500;
/// `FOLDER_DIALOG_ANIMATION_TIME` (`appDisplay.js:43`) — the whole open/close.
const ANIMATION_MS: u64 = 200;

niri_render_elements! {
    FolderDialogRenderElement => {
        Texture = TextureRenderElement<VkTexture>,
        SolidColor = SolidColorRenderElement,
    }
}

/// What a point over an open dialog hit. Everything inside the panel consumes the click;
/// everything outside it pops the dialog down, which is the whole of GNOME's click gesture
/// (`may-recognize` tests the point against `_viewBox.allocation`, `appDisplay.js:2480-2487`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogHit {
    /// An app tile inside the folder, at index `.0`.
    App(usize),
    /// A page-indicator dot for page `.0`.
    Page(usize),
    /// A page navigation arrow.
    Arrow(PageArrow),
    /// Inside the panel but not on a control — consumes the click, does nothing.
    Inside,
    /// Outside the panel: pop down.
    Outside,
}

/// The open folder's geometry within one output view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DialogLayout {
    /// The `.app-folder-dialog` panel.
    pub panel: Rectangle<f64, Logical>,
    /// The band the name label is centered in.
    pub name_band: Rectangle<f64, Logical>,
    /// The folder view's box — everything below the name, which is what the inner
    /// [`AppGrid`] lays its page out in.
    pub grid_area: Rectangle<f64, Logical>,
}

/// Lay the dialog out over an output `view` (logical, output coordinates).
pub fn layout(view: Rectangle<f64, Logical>) -> DialogLayout {
    // `.app-folder-dialog-container` pads the top by the panel height, so the panel is
    // centered in the work area rather than the screen (`_app-grid.scss:53-56`).
    let avail_y = view.loc.y + PANEL_HEIGHT;
    let avail_h = (view.size.h - PANEL_HEIGHT).max(0.);

    let w = SIZE.min(view.size.w);
    let h = SIZE.min(avail_h);
    let panel = Rectangle::new(
        Point::from((
            (view.loc.x + (view.size.w - w) / 2.).round(),
            (avail_y + (avail_h - h) / 2.).round(),
        )),
        Size::from((w, h)),
    );

    let name_h = crate::ui::line_height_px(NAME_PT);
    let name_band = Rectangle::new(
        Point::from((panel.loc.x + PAD_X + NAME_PAD_X, panel.loc.y + NAME_PAD_TOP)),
        Size::from(((w - 2. * (PAD_X + NAME_PAD_X)).max(0.), name_h)),
    );

    // The name container has no bottom padding, so the view starts right under the label.
    let below = NAME_PAD_TOP + name_h;
    let grid_area = Rectangle::new(
        Point::from((panel.loc.x + PAD_X, panel.loc.y + below)),
        Size::from(((w - 2. * PAD_X).max(0.), (h - below).max(0.))),
    );

    DialogLayout {
        panel,
        name_band,
        grid_area,
    }
}

/// Where the dialog is in its open/close animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Zooming out of the source tile. Already modal — GNOME takes the grab in `popup`,
    /// before the animation starts (`appDisplay.js:2875-2893`).
    Opening,
    Visible,
    /// Shrinking back into the source tile. The grab is already released, so a click or
    /// an Escape during it goes to whatever is underneath, exactly as in GNOME
    /// (`popdown` ungrabs and *then* animates, `appDisplay.js:2896-2915`).
    Closing,
}

/// The folder currently on screen.
struct OpenFolder {
    /// The `folder-children` id — what the grid tile that opened it is keyed by.
    id: String,
    /// The already-translated display name (`_getFolderName`, `appDisplay.js:97-104`).
    name: String,
    /// The folder's own view: an [`AppGrid`] in its `FolderGrid` configuration.
    view: AppGrid,
    phase: Phase,
    /// A **linear** 0→1 ramp over [`ANIMATION_MS`] — the transition's timeline, not its
    /// motion. gnome-shell runs three eases with different curves off one duration (the
    /// transform `EASE_OUT_EXPO`, the shade `EASE_OUT_QUAD`, the source icon a delayed
    /// half-length quad), so the curves are applied per-quantity in [`Progress`] rather
    /// than baked into the animation.
    timeline: Animation,
    /// The backdrop's drag highlight, 0 = `DIALOG_SHADE_NORMAL`, 1 = `DIALOG_SHADE_HIGHLIGHT`
    /// (`_setLighterBackground`, `appDisplay.js:2794-2805`): the shade lightens while a drag
    /// out of the folder is outside the panel, and settles back if it comes home.
    highlight: Animation,
}

/// How far along each of the transition's independently-curved quantities is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Progress {
    /// 0 = the panel sits exactly on the source tile, 1 = its resting place
    /// (`EASE_OUT_EXPO`, `appDisplay.js:2686,2729`).
    zoom: f64,
    /// The backdrop's ramp to `DIALOG_SHADE_NORMAL` (`EASE_OUT_QUAD`, `:2677,2714`).
    shade: f64,
    /// The panel's own opacity — part of the transform's ease on the way in, its own quad
    /// on the way out (`:2684-2686` vs `:2717-2721`).
    content: f64,
    /// What the *source tile in the grid* should be drawn at: it fades out while the
    /// dialog opens and back in as it shrinks home, so the panel appears to turn back
    /// into the icon (`_ensureFolderDialog`'s `open-state-changed` handler,
    /// `appDisplay.js:2441-2451`).
    ///
    /// The fade timing is GNOME's on both halves: out over `TIME / 2` as the dialog
    /// opens, and on the close delayed by `TIME / 2` and then `EASE_IN_QUAD`. What that
    /// delay needs to work is a panel still *moving* while the icon comes up — see the
    /// close's `zoom` curve, which is where we diverge instead.
    source: f64,
}

/// The timeline position at which [`Curve::EaseOutExpo`] reaches `y` — the inverse of
/// `1 - 2^(-10x)` (`animation::Curve::y`).
///
/// This is what lets an interrupted transition resume from where it *looks* like it is
/// rather than from where its clock is. Clutter gets that for free, because `ease()`
/// animates a property from its current value; our timeline runs 0→1 under the curve, so
/// reversing it means asking the curve where the current value sits.
/// The affine an in-flight zoom applies to everything inside the dialog's container.
///
/// gnome-shell moves ONE actor: the `.app-folder-dialog-container`, which fills the
/// monitor. It is translated so its top-left lands on the source icon's, and scaled by
/// `source.width / child.width` — per axis, so the scale is deliberately **not** uniform
/// (`_zoomAndFadeIn`, `appDisplay.js:2666-2672`). Everything inside rides along, which for
/// a flat set of axis-aligned quads is the same as mapping each one's box through this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zoom {
    view: Rectangle<f64, Logical>,
    sx: f64,
    sy: f64,
    tx: f64,
    ty: f64,
}

impl Zoom {
    /// The transform at `zoom` (0 = sitting on `source`, 1 = identity), over an output
    /// `view`. Both endpoints are interpolated linearly because Clutter eases the
    /// translation and the scale as independent properties under one curve.
    pub fn new(view: Rectangle<f64, Logical>, source: Rectangle<f64, Logical>, zoom: f64) -> Self {
        let lerp = |a: f64, b: f64| a + (b - a) * zoom;
        Self {
            view,
            sx: lerp(source.size.w / view.size.w, 1.),
            sy: lerp(source.size.h / view.size.h, 1.),
            tx: lerp(source.loc.x - view.loc.x, 0.),
            ty: lerp(source.loc.y - view.loc.y, 0.),
        }
    }

    /// Where a box in view coordinates lands.
    pub fn map(&self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((
                self.view.loc.x + self.tx + (rect.loc.x - self.view.loc.x) * self.sx,
                self.view.loc.y + self.ty + (rect.loc.y - self.view.loc.y) * self.sy,
            )),
            Size::from((rect.size.w * self.sx, rect.size.h * self.sy)),
        )
    }
}

/// The transition timeline, started `from` (0..1) of the way through — a linear ramp, since
/// the curves are applied per-quantity in [`Progress`].
///
/// An interrupted transition runs only the time it has *left*, which keeps each curve's
/// shape exact. Clutter instead re-eases from the current value over a full duration; the
/// two agree whenever nothing is interrupted, which is every ordinary open and close.
fn ease_from(clock: &Clock, from: f64) -> Animation {
    let from = from.clamp(0., 1.);
    let remaining = ((1. - from) * ANIMATION_MS as f64).round() as u64;
    Animation::ease(clock.clone(), from, 1., 0., remaining.max(1), Curve::Linear)
}

fn expo_x_for(y: f64) -> f64 {
    if y <= 0. {
        0.
    } else if y >= 1. {
        1.
    } else {
        -(1. - y).log2() / 10.
    }
}

/// The same for [`Curve::EaseOutQuad`] — `y = 1 - (1 - x)²`, so `x = 1 - √(1 - y)`. This is
/// the close's curve, so it is what a *popdown* interrupting an open has to invert.
fn quad_x_for(y: f64) -> f64 {
    if y <= 0. {
        0.
    } else if y >= 1. {
        1.
    } else {
        1. - (1. - y).sqrt()
    }
}

impl Progress {
    /// Everything at rest — the dialog fully open.
    const OPEN: Self = Self {
        zoom: 1.,
        shade: 1.,
        content: 1.,
        source: 0.,
    };

    /// Fully shrunk onto the source tile — a finished close.
    const SHUT: Self = Self {
        zoom: 0.,
        shade: 0.,
        content: 0.,
        source: 1.,
    };

    fn at(phase: Phase, x: f64) -> Self {
        let x = x.clamp(0., 1.);
        // Pin the endpoints rather than trusting the curve at them: `Curve::EaseOutExpo`
        // is `1 - 2^(-10x)` with no endpoint clamp, so `y(1)` is 0.99902, not 1.
        // `Animation` normally hides that behind its own "past the end" early-return,
        // which applying the curve by hand goes around.
        if x >= 1. {
            return match phase {
                Phase::Visible | Phase::Opening => Self::OPEN,
                Phase::Closing => Self::SHUT,
            };
        }
        match phase {
            Phase::Visible => Self::OPEN,
            Phase::Opening => Self {
                zoom: Curve::EaseOutExpo.y(x),
                shade: Curve::EaseOutQuad.y(x),
                content: Curve::EaseOutExpo.y(x),
                source: 1. - Curve::EaseOutQuad.y((x * 2.).min(1.)),
            },
            // Closing runs the same eases *forward* from the open state to the source, so
            // each quantity is its opening counterpart mirrored — not the opening curve
            // evaluated backwards, which would ease from the wrong end.
            //
            // **Divergence: the collapse is `EASE_OUT_QUAD`, not GNOME's `EASE_OUT_EXPO`.**
            // Exponential means 82% of the travel happens in the first quarter of the
            // animation: measured, GNOME's panel is within 3% of the tile by the halfway
            // mark and spends the whole back half as a stationary speck. That is exactly
            // when the source icon's delayed fade is coming up, so there is no motion left
            // to carry the eye across the hand-over and the panel reads as *vanishing*
            // rather than landing. Quad puts the panel's size on the same curve as its
            // opacity, so it arrives at the tile at the instant it finishes fading — the
            // continuity the delayed fade assumes.
            Phase::Closing => Self {
                zoom: 1. - Curve::EaseOutQuad.y(x),
                shade: 1. - Curve::EaseOutQuad.y(x),
                content: 1. - Curve::EaseOutQuad.y(x),
                // Delayed by TIME/2, then ease-in-quad — `EASE_IN_QUAD` is just `u²`,
                // and our `Curve` set has no ease-*in* member.
                source: {
                    let u = (x * 2. - 1.).max(0.);
                    u * u
                },
            },
        }
    }
}

#[derive(Default)]
struct DialogCache {
    /// The panel chrome — its rounded fill, inset border and name label. Keyed on the
    /// name and the panel size, so it survives paging and hovering inside the folder.
    panel: widget::BakeCache,
}

struct OutputData {
    shade: SolidColorBuffer,
}

/// The app-folder dialog. Owned on `Niri`; opened by a click on a folder tile.
pub struct FolderDialog {
    open: Option<OpenFolder>,
    cache: RefCell<DialogCache>,
    clock: Clock,
}

impl FolderDialog {
    pub fn new(clock: Clock) -> Self {
        Self {
            open: None,
            cache: RefCell::new(DialogCache::default()),
            clock,
        }
    }

    /// Whether the dialog holds the modal grab — i.e. whether it is up *and* not already
    /// on its way out. A closing dialog is still drawn but no longer takes input, which is
    /// what `popdown` ungrabbing before it animates means (`appDisplay.js:2896-2915`).
    pub fn is_open(&self) -> bool {
        matches!(
            self.open.as_ref().map(|o| o.phase),
            Some(Phase::Opening | Phase::Visible)
        )
    }

    /// Whether anything is on screen, including the close animation.
    pub fn is_visible(&self) -> bool {
        self.open.is_some()
    }

    /// The open folder's id, if any (including while it closes — the source tile's fade
    /// runs to the end of that animation).
    pub fn folder_id(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.id.as_str())
    }

    /// Pop the dialog up on `id` with `members` (`FolderIcon.open`, `appDisplay.js:2334-2343`).
    /// Re-opening the folder that is already up is a no-op, as `popup` is when `_isOpen` —
    /// but one caught mid-*close* re-opens from wherever it had shrunk to, rather than
    /// snapping back to the source tile first.
    pub fn popup(&mut self, id: &str, name: &str, members: Vec<AppGridEntry>) {
        if let Some(open) = &mut self.open {
            if open.id == id {
                if open.phase == Phase::Closing {
                    let from = Progress::at(Phase::Closing, open.timeline.clamped_value()).zoom;
                    open.phase = Phase::Opening;
                    open.timeline = ease_from(&self.clock, expo_x_for(from));
                }
                return;
            }
        }
        let mut view = AppGrid::folder_view(self.clock.clone());
        view.set_entries(members);
        self.open = Some(OpenFolder {
            id: id.to_owned(),
            name: name.to_owned(),
            view,
            phase: Phase::Opening,
            timeline: ease_from(&self.clock, 0.),
            highlight: Animation::ease(self.clock.clone(), 0., 0., 0., 0, Curve::EaseOutQuad),
        });
    }

    /// Pop the dialog down. Returns whether it had been open (→ redraw, and the caller's
    /// Escape stops there rather than falling through to closing the grid). A dialog
    /// already closing returns `false`, so a second Escape falls through to the grid.
    pub fn popdown(&mut self) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        if open.phase == Phase::Closing {
            return false;
        }
        // Interrupting the open: carry the zoom it had reached into the close, so it
        // shrinks from where it is instead of jumping out to full size first.
        let from = Progress::at(open.phase, open.timeline.clamped_value()).zoom;
        open.phase = Phase::Closing;
        open.timeline = ease_from(&self.clock, quad_x_for(1. - from));
        true
    }

    /// Drop the dialog outright, with no shrink. This is GNOME's `_zoomAndFadeOut`
    /// early-out for a source icon that is no longer mapped (`appDisplay.js:2701-2704`) —
    /// which is what leaving the app grid does to it.
    ///
    /// Divergence: gnome-shell's dialog lives in the `overviewGroup` and so keeps fading
    /// with it until the grid actually unmaps, where ours goes the frame the grid's *state*
    /// flips. The visible difference is the shade ending one fade earlier on the way out.
    /// Taken deliberately: animating into a vanishing overview leaves a ghost shrinking over
    /// a re-opened one if the overview comes back inside the 200 ms.
    pub fn hide(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Retire a finished close. Called once a frame from `Niri`'s refresh — the dialog
    /// keeps drawing itself until the shrink is over, so it cannot drop its own state.
    pub fn advance(&mut self) {
        if self
            .open
            .as_ref()
            .is_some_and(|o| o.phase == Phase::Closing && o.timeline.is_clamped_done())
        {
            self.open = None;
        } else if let Some(open) = &mut self.open {
            if open.phase == Phase::Opening && open.timeline.is_clamped_done() {
                open.phase = Phase::Visible;
            }
        }
    }

    /// Whether an open/close animation is still running (→ hold the redraw loop open).
    pub fn are_animations_ongoing(&self) -> bool {
        self.highlight_ongoing()
            || self
                .open
                .as_ref()
                .is_some_and(|o| o.phase != Phase::Visible && !o.timeline.is_clamped_done())
    }

    /// Where the transition currently is.
    fn progress(&self) -> Progress {
        self.open.as_ref().map_or(Progress::OPEN, |o| {
            Progress::at(o.phase, o.timeline.clamped_value())
        })
    }

    /// The grid tile that should be faded, and to what — the source folder's id and the
    /// alpha its tile draws at. `None` when nothing is up.
    pub fn source_fade(&self) -> Option<(&str, f64)> {
        let open = self.open.as_ref()?;
        Some((open.id.as_str(), self.progress().source))
    }

    /// Re-seat the open folder against a fresh grid: `members` as they now resolve, or
    /// `None` if the folder is gone (emptied, deleted, or every member became a favorite),
    /// in which case the dialog goes with it — GNOME destroys the `FolderIcon`, and the
    /// dialog is destroyed with its source (`appDisplay.js:2320-2325`).
    pub fn resync(&mut self, members: Option<(String, Vec<AppGridEntry>)>) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        let Some((name, members)) = members else {
            self.open = None;
            return true;
        };
        let mut changed = open.view.set_entries(members);
        if open.name != name {
            open.name = name;
            changed = true;
        }
        changed
    }

    /// Lighten the backdrop while a drag out of the folder is outside the panel, or let
    /// it settle back (`_setLighterBackground`, `appDisplay.js:2794-2805`). Returns
    /// whether it changed (→ redraw).
    pub fn set_drag_outside(&mut self, outside: bool) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        let to = f64::from(u8::from(outside));
        if open.highlight.to() == to {
            return false;
        }
        let from = open.highlight.clamped_value();
        open.highlight = Animation::ease(
            self.clock.clone(),
            from,
            to,
            0.,
            ANIMATION_MS,
            Curve::EaseOutQuad,
        );
        true
    }

    /// Whether the backdrop is still easing between its two shades (→ hold the redraw loop).
    fn highlight_ongoing(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| !o.highlight.is_clamped_done())
    }

    /// The icon of member `i` — what a drag of that tile carries.
    pub fn entry_icon(&self, i: usize) -> Option<&AppIconRef> {
        self.open.as_ref()?.view.entry_icon(i)
    }

    /// How many members the open folder still shows.
    pub fn member_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.view.entry_count())
    }

    /// Take member `id` out of the open folder's view — the local half of
    /// `FolderView.removeApp` (`appDisplay.js:2239-2272`), so the tile goes the moment the
    /// drop is accepted rather than when the settings reload catches up. Returns whether
    /// it was there.
    pub fn remove_member(&mut self, id: &str) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        open.view.remove_entry(id)
    }

    /// The center of member tile `i` in output coordinates — where a drag of it is
    /// picked up, so the icon does not jump under the pointer.
    pub fn entry_center(
        &self,
        i: usize,
        view: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let open = self.open.as_ref()?;
        open.view.entry_center(i, layout(view).grid_area)
    }

    /// The id of the app at tile `i` inside the folder.
    pub fn entry_id(&self, i: usize) -> Option<&str> {
        self.open.as_ref()?.view.entry_id(i)
    }

    /// The icon size the open folder's view will render at for an output `view` — its own
    /// 3×3 in the panel, so not the top-level grid's ([`AppGrid::metrics_for`]).
    pub fn icon_px(&self, view: Rectangle<f64, Logical>) -> Option<f64> {
        let open = self.open.as_ref()?;
        Some(open.view.metrics_for(layout(view).grid_area).icon_px)
    }

    /// The member icons, for the startup decode prewarm.
    pub fn icon_refs(&self) -> impl Iterator<Item = &AppIconRef> {
        self.open.iter().flat_map(|o| o.view.icon_refs())
    }

    pub fn clear_icon_uploads(&self) {
        if let Some(open) = &self.open {
            open.view.clear_icon_uploads();
        }
    }

    pub fn drop_icon_upload(&self, icon: &AppIconRef, logical_px: u16) {
        if let Some(open) = &self.open {
            open.view.drop_icon_upload(icon, logical_px);
        }
    }

    /// What is at `pos` (logical, output coordinates). `None` when nothing is open — the
    /// dialog is modal, so once it *is* open every point on the output belongs to it.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        view: Rectangle<f64, Logical>,
    ) -> Option<DialogHit> {
        // Not `self.open`: a *closing* dialog is still drawn but has already released its
        // grab, so it must not swallow the click that a point over it would otherwise
        // reach. Without this the whole screen stays modal for the 200 ms of the shrink.
        if !self.is_open() {
            return None;
        }
        let open = self.open.as_ref()?;
        let l = layout(view);
        if !l.panel.contains(pos) {
            return Some(DialogHit::Outside);
        }
        if let Some(i) = open.view.hit_test(pos, l.grid_area) {
            return Some(DialogHit::App(i));
        }
        if let Some(page) = open.view.indicator_hit(pos, l.grid_area) {
            return Some(DialogHit::Page(page));
        }
        if let Some(arrow) = open.view.arrow_hit(pos, l.grid_area) {
            return Some(DialogHit::Arrow(arrow));
        }
        Some(DialogHit::Inside)
    }

    /// Track the pointer over the open folder. Returns whether anything changed (→ redraw).
    pub fn set_pointer(
        &mut self,
        pos: Option<Point<f64, Logical>>,
        view: Rectangle<f64, Logical>,
    ) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        let (tile, arrow) = match pos {
            Some(pos) => (
                open.view.hit_test(pos, l.grid_area),
                open.view.arrow_hit(pos, l.grid_area),
            ),
            None => (None, None),
        };
        let mut changed = open.view.set_hovered(tile);
        changed |= open.view.set_arrow_hovered(arrow);
        changed
    }

    /// Go to `page` of the open folder (a dot click); returns whether it moved.
    pub fn set_page(&mut self, page: usize, view: Rectangle<f64, Logical>) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        open.view.set_page(page, l.grid_area)
    }

    /// Step a page (a navigation-arrow click); returns whether it moved.
    pub fn step_page(&mut self, arrow: PageArrow, view: Rectangle<f64, Logical>) -> bool {
        let Some(open) = &self.open else {
            return false;
        };
        let cur = open.view.current_page();
        let target = match arrow {
            PageArrow::Prev => cur.saturating_sub(1),
            PageArrow::Next => cur + 1,
        };
        self.set_page(target, view)
    }

    pub fn current_page(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.view.current_page())
    }

    /// Tab through the open folder's members. The dialog is its own focus group
    /// (`global.focus_manager.add_group(this)`, `appDisplay.js:2516`), so Tab cycles
    /// inside it and never reaches the grid behind.
    pub fn focus_tab(&mut self, forward: bool, view: Rectangle<f64, Logical>) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        open.view.focus_tab(forward, l.grid_area)
    }

    /// The keyboard-focused member of the open folder, if any (what Enter launches).
    pub fn focused(&self) -> Option<usize> {
        self.open.as_ref()?.view.focused()
    }

    /// Move the folder's keyboard focus one step. The dialog is its own focus group in
    /// GNOME (`global.focus_manager.add_group(this)`, `appDisplay.js:2516`), so the
    /// arrows stay inside it while it is up — which here is simply a matter of routing
    /// them to this view instead of the one behind it.
    pub fn focus_navigate(&mut self, dir: FocusDir, view: Rectangle<f64, Logical>) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        open.view.focus_navigate(dir, l.grid_area)
    }

    /// The logical center of the open folder's tile `k` on its current page — a geometry
    /// probe for the conformance corpus, which clicks real pixels.
    #[cfg(test)]
    pub fn tile_center(
        &self,
        k: usize,
        grid_area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.open.as_ref()?.view.tile_center(k, grid_area)
    }

    /// The zoom the next render will apply — a probe for the render test, which has to
    /// reproduce the transform to know where the panel went.
    #[cfg(test)]
    pub fn zoom_for_test(&self) -> f64 {
        self.progress().zoom
    }

    /// The same for tile `k`'s **icon** — the render test samples the drawn pixels there.
    #[cfg(test)]
    pub fn icon_center(
        &self,
        k: usize,
        grid_area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.open.as_ref()?.view.icon_center(k, grid_area)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        view: Rectangle<f64, Logical>,
        source: Option<Rectangle<f64, Logical>>,
        alpha: f32,
        accent: [u8; 3],
        push: &mut dyn FnMut(FolderDialogRenderElement),
    ) {
        let Some(open) = &self.open else { return };
        if alpha <= 0. {
            return;
        }
        let _span = tracy_client::span!("FolderDialog::render");

        let scale = output.current_scale().fractional_scale();
        let l = layout(view);
        let progress = self.progress();

        // The folder's own grid — its tiles, captions, hover wash, dots and arrows all come
        // from the same widget the app grid uses. Collected rather than pushed, because the
        // zoom below transforms every one of them as a set.
        let mut content = open.view.render(
            renderer,
            app_icons,
            sym_icons,
            output,
            l.grid_area,
            alpha,
            accent,
        );

        // The panel itself: the rounded overlay surface, its inset hairline border, and the
        // folder name centered across the top.
        let panel_size = l.panel.size;
        let name = open.name.clone();
        let name_center = Point::from((
            l.name_band.loc.x - l.panel.loc.x + l.name_band.size.w / 2.,
            l.name_band.loc.y - l.panel.loc.y + l.name_band.size.h / 2.,
        ));
        let revision = widget::Revision::new()
            .of(&name)
            .px(panel_size.w)
            .px(panel_size.h)
            .done();
        match widget::bake(
            renderer,
            &mut self.cache.borrow_mut().panel,
            scale,
            panel_size,
            revision,
            move |r| {
                let mut shaper = widget::TextShaper::new(r, scale);
                shaper.shape(&name, widget::TextStyle::new(NAME_PT).bold())
            },
            move |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(RADIUS, style::OVERLAY_BG)?;
                p.stroke_rounded_full(RADIUS, BORDER_W, style::BORDERS)?;
                p.text(label, name_center, Align::CENTER, style::TEXT)?;
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
                content.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    l.panel.loc,
                    alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error baking the folder dialog: {err:#}"),
        }

        // --- The zoom ([`Zoom`]; `_zoomAndFadeIn`/`_zoomAndFadeOut`, `appDisplay.js:2660-2746`).
        //
        // The shade below is the dialog *actor's* own `background_color`, not part of the
        // container this moves, so it only fades — it is never transformed.
        //
        // Transformed geometry is not reflected in `opaque_regions`, which is sound here
        // for the same reason it is in the popover's open/close scale: this branch only
        // runs while the content is translucent, and a translucent element reports no
        // opaque regions anyway.
        if progress.zoom < 1. {
            if let Some(src) = source {
                let zoom = Zoom::new(view, src, progress.zoom);
                for el in &mut content {
                    let mapped = zoom.map(Rectangle::new(el.location(), el.logical_size()));
                    el.set_location(mapped.loc);
                    el.set_size(mapped.size);
                }
            }
            // The panel's opacity rides the transform's ease on the way in and its own on
            // the way out; either way it multiplies the overview's fade.
            for el in &mut content {
                el.set_alpha(alpha * progress.content as f32);
            }
        }
        for element in content {
            push(FolderDialogRenderElement::Texture(element));
        }

        // The shade over everything behind, bottom-most.
        let size = view.size;
        let data = output.user_data().get_or_insert(|| {
            std::sync::Mutex::new(OutputData {
                shade: SolidColorBuffer::new(size, SHADE),
            })
        });
        let mut data = data.lock().unwrap();
        data.shade.resize(size);
        // Both shades are pure black, so the drag highlight is just less of this one.
        let highlight = self
            .open
            .as_ref()
            .map_or(0., |o| o.highlight.clamped_value()) as f32;
        let shade_alpha = 1. - highlight * (1. - SHADE_HIGHLIGHT_FACTOR);
        push(FolderDialogRenderElement::SolidColor(
            SolidColorRenderElement::from_buffer(
                &data.shade,
                view.loc,
                alpha * progress.shade as f32 * shade_alpha,
                Kind::Unspecified,
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)))
    }

    fn entry(id: &str) -> AppGridEntry {
        AppGridEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            icon: AppIconRef::Fallback,
            folder: None,
        }
    }

    fn open_dialog(n: usize) -> FolderDialog {
        let mut dialog = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        let members: Vec<_> = (0..n).map(|i| entry(&format!("app{i}.desktop"))).collect();
        dialog.popup("Utilities", "Utilities", members);
        // Land on the open state: every geometry/hit-test assertion below is about the
        // resting dialog, not its first animation frame.
        at_ms(&mut dialog, ANIMATION_MS);
        dialog
    }

    /// Move the dialog's clock to `ms` past zero and retire whatever that finishes — the
    /// pinned-clock idiom, since nothing here advances a clock on its own.
    fn at_ms(dialog: &mut FolderDialog, ms: u64) {
        dialog
            .clock
            .set_unadjusted(std::time::Duration::from_millis(ms));
        dialog.advance();
    }

    /// The phase ladder, on a pinned clock. The grab is taken with `popup` and released
    /// with `popdown`, so `is_open()` (which gates hit-testing and the Escape tier) is
    /// true through the whole *opening* animation and false through the whole *closing*
    /// one, while `is_visible()` covers both.
    #[test]
    fn the_phases_run_open_then_shut_and_the_grab_matches() {
        let mut d = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        d.popup("Utilities", "Utilities", vec![entry("a.desktop")]);

        assert!(d.is_open(), "the grab is taken before the zoom starts");
        assert!(d.are_animations_ongoing(), "…and the zoom is running");

        at_ms(&mut d, 100);
        assert!(d.is_open() && d.are_animations_ongoing(), "halfway open");

        at_ms(&mut d, ANIMATION_MS);
        assert!(d.is_open());
        assert!(
            !d.are_animations_ongoing(),
            "settled — the redraw loop can idle"
        );
        assert_eq!(d.progress(), Progress::OPEN);

        assert!(d.popdown(), "the first popdown closes it");
        assert!(
            !d.is_open(),
            "the grab is released at once, before the shrink"
        );
        assert!(d.is_visible(), "…but it is still on screen, shrinking");
        assert_eq!(
            d.hit_test(Point::from((5., 5.)), view()),
            None,
            "and it is no longer modal: a click over it reaches the grid behind"
        );
        assert!(!d.popdown(), "a second Escape falls through to the grid");
        assert!(d.are_animations_ongoing());

        at_ms(&mut d, ANIMATION_MS * 2);
        assert!(!d.is_visible(), "a finished close retires itself");
        assert!(!d.are_animations_ongoing());
    }

    /// Clicking the same folder again while it shrinks re-opens it — GNOME's `_isOpen` is
    /// already false during the zoom-out, so `popup` runs — and it re-opens from where it
    /// had shrunk to rather than snapping back to the tile first.
    #[test]
    fn reopening_mid_close_resumes_from_where_it_shrank_to() {
        let mut d = open_dialog(4);
        assert!(d.popdown());
        at_ms(&mut d, ANIMATION_MS + 40);
        let caught = d.progress().zoom;
        assert!(
            caught > 0.05 && caught < 0.95,
            "the test must catch it mid-shrink, got {caught}"
        );

        d.popup("Utilities", "Utilities", vec![entry("a.desktop")]);
        assert!(
            d.is_open(),
            "the click re-opens rather than being swallowed"
        );
        assert!(
            (d.progress().zoom - caught).abs() < 1e-6,
            "and it resumes from the size it had reached, not from the tile: \
             {} vs {caught}",
            d.progress().zoom
        );
    }

    /// Closing an interrupted *open* resumes from the size it had reached, not from full
    /// size. The two directions now run different curves, so their inverses are different
    /// too — crossing them is a one-character bug that shows only as a mid-animation jump.
    #[test]
    fn closing_mid_open_resumes_from_where_it_had_grown_to() {
        let mut d = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        d.popup("Utilities", "Utilities", vec![entry("a.desktop")]);
        at_ms(&mut d, 20);
        let caught = d.progress().zoom;
        assert!(
            caught > 0.05 && caught < 0.95,
            "the test must catch it mid-open, got {caught}"
        );

        assert!(d.popdown());
        assert!(
            (d.progress().zoom - caught).abs() < 1e-6,
            "the shrink starts at the size it had reached: {} vs {caught}",
            d.progress().zoom
        );
    }

    /// The curves, sampled *between* the endpoints — where a direction flip or a swapped
    /// curve actually shows. Endpoints are structurally blind to both.
    #[test]
    fn the_close_eases_out_from_the_open_state_not_backwards_into_it() {
        let open = Progress::at(Phase::Opening, 0.25);
        let close = Progress::at(Phase::Closing, 0.25);

        // The open is GNOME's EASE_OUT_EXPO and is front-loaded: a quarter of the way in
        // the panel has nearly finished travelling.
        assert!(open.zoom > 0.8, "opening rushes out: {}", open.zoom);
        assert!(open.shade < open.zoom, "the shade trails the zoom in");

        // On the close, the panel's size, its opacity and the shade now all run the one
        // quad, so they are locked together — the panel arrives at the tile at the very
        // instant it finishes fading, which is what makes the hand-over readable.
        assert!((close.zoom - close.content).abs() < 1e-9, "{close:?}");
        assert!((close.shade - close.content).abs() < 1e-9, "{close:?}");

        // The close is our slower quad. The point of diverging is that the panel is still
        // visibly travelling when the source icon's delayed fade starts at the halfway
        // mark — GNOME's expo has it within 3% of the tile by then, a stationary speck.
        assert!(
            Progress::at(Phase::Closing, 0.5).zoom > 0.2,
            "at the hand-over the panel must still have somewhere to go, got {}",
            Progress::at(Phase::Closing, 0.5).zoom
        );
        assert!(
            1. - Curve::EaseOutExpo.y(0.5) < 0.05,
            "…which is exactly what GNOME's expo does not leave"
        );
        // …and it is monotone home, never bouncing.
        let mut last = 1.;
        for step in 1..=20 {
            let z = Progress::at(Phase::Closing, f64::from(step) / 20.).zoom;
            assert!(
                z < last,
                "the collapse must not stall or bounce: {z} !< {last}"
            );
            last = z;
        }

        // The source tile keeps GNOME's timing on both halves: out over the first half of
        // the open, and in over the *second* half of the close after its TIME/2 delay.
        assert_eq!(Progress::at(Phase::Opening, 0.).source, 1.);
        assert_eq!(Progress::at(Phase::Opening, 0.5).source, 0.);
        assert_eq!(Progress::at(Phase::Closing, 0.5).source, 0.);
        assert_eq!(Progress::at(Phase::Closing, 1.).source, 1.);
        assert!(
            Progress::at(Phase::Closing, 0.75).source < 0.5,
            "ease-in: it starts slowly"
        );
    }

    /// The zoom maps the container so that at 0 it sits exactly on the source tile, per
    /// axis — the scale is deliberately non-uniform, as Clutter's is.
    #[test]
    fn at_zero_the_zoom_lands_the_view_on_the_source_tile() {
        let v = view();
        let src = Rectangle::new(Point::from((300., 500.)), Size::from((144., 144.)));

        let z = Zoom::new(v, src, 0.);
        assert_eq!(z.map(v), src, "the whole view collapses onto the tile");
        // A box inside the view lands proportionally inside the tile.
        let panel = layout(v).panel;
        let mapped = z.map(panel);
        assert!(src.contains_rect(mapped), "{mapped:?} outside {src:?}");
        // Non-uniform, as Clutter's is: each axis scales by its own ratio, so a square
        // panel inside a 16:9 view mapped onto a square tile comes out 16:9-squashed.
        assert!(
            (panel.size.w - panel.size.h).abs() < 1e-9 && (src.size.w - src.size.h).abs() < 1e-9,
            "the premise: both boxes are square"
        );
        let want = v.size.h / v.size.w;
        assert!(
            (mapped.size.w / mapped.size.h - want).abs() < 1e-9,
            "a uniform scale would keep it square: {mapped:?}"
        );

        assert_eq!(Zoom::new(v, src, 1.).map(panel), panel, "1 is identity");
    }

    /// The panel is `$app_folder_size` square, centered in the work area — the screen with
    /// the top panel's height taken off the top, which is what shifts it below center.
    #[test]
    fn the_panel_is_seven_hundred_and_twenty_square_centered_under_the_panel() {
        let l = layout(view());

        assert_eq!(l.panel.size, Size::from((720., 720.)));
        assert_eq!(l.panel.loc.x, (1920. - 720.) / 2.);
        // 35 + (1045 − 720)/2 = 35 + 162.5, rounded.
        assert_eq!(l.panel.loc.y, f64::round(35. + (1045. - 720.) / 2.));
    }

    /// The name band sits inside the horizontal padding, at the container's top padding, and
    /// the grid starts immediately under it — `.folder-name-container` has no bottom padding.
    #[test]
    fn the_name_band_pads_the_top_and_the_grid_starts_under_it() {
        let l = layout(view());
        let name_h = crate::ui::line_height_px(NAME_PT);

        assert_eq!(l.name_band.loc.x, l.panel.loc.x + 1. + 36.);
        assert_eq!(l.name_band.loc.y, l.panel.loc.y + 24.);
        assert_eq!(l.name_band.size.w, 720. - 2. * (1. + 36.));

        assert_eq!(l.grid_area.loc.x, l.panel.loc.x + 1.);
        assert_eq!(l.grid_area.loc.y, l.panel.loc.y + 24. + name_h);
        assert_eq!(l.grid_area.size.h, 720. - 24. - name_h);
    }

    /// A screen too short for 720 + the panel strut clamps the dialog rather than letting it
    /// run off the bottom, and it still starts below the panel.
    #[test]
    fn a_short_screen_clamps_the_panel_into_the_work_area() {
        let l = layout(Rectangle::new(
            Point::from((0., 0.)),
            Size::from((1024., 600.)),
        ));

        // Each axis clamps on its own — `width`/`height` are separate declarations, so a
        // screen short in one direction only does not shrink the other.
        assert_eq!(l.panel.size, Size::from((720., 600. - 35.)));
        assert_eq!(l.panel.loc.y, 35.);
        assert!(l.panel.loc.y + l.panel.size.h <= 600.);
    }

    /// A click inside the panel is consumed; a click anywhere else pops the dialog down —
    /// the whole of GNOME's click gesture. And with nothing open the dialog is transparent
    /// to hit-testing, so the grid behind it keeps its clicks.
    #[test]
    fn outside_the_panel_pops_down_and_a_closed_dialog_hits_nothing() {
        let dialog = open_dialog(4);
        let l = layout(view());

        assert_eq!(
            dialog.hit_test(Point::from((5., 5.)), view()),
            Some(DialogHit::Outside)
        );
        let corner = Point::from((l.panel.loc.x + 4., l.panel.loc.y + 4.));
        assert_eq!(dialog.hit_test(corner, view()), Some(DialogHit::Inside));

        let closed = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        assert_eq!(closed.hit_test(corner, view()), None);
    }

    /// The inner view is a 3×3 `FolderGrid`: nine apps fit one page, the tenth makes a second.
    #[test]
    fn the_folder_view_paginates_three_by_three() {
        let l = layout(view());

        let dialog = open_dialog(9);
        let open = dialog.open.as_ref().unwrap();
        assert_eq!(open.view.page_count(l.grid_area), 1);
        assert_eq!(open.view.visible_len(l.grid_area), 9);

        let dialog = open_dialog(10);
        let open = dialog.open.as_ref().unwrap();
        assert_eq!(open.view.page_count(l.grid_area), 2);
        assert_eq!(open.view.visible_len(l.grid_area), 9);
    }

    /// Every tile is hittable and resolves to its own app, and a hit maps back to the id the
    /// caller launches.
    #[test]
    fn each_tile_hits_its_own_app() {
        let dialog = open_dialog(9);
        let l = layout(view());
        let open = dialog.open.as_ref().unwrap();

        for i in 0..9 {
            let center = open.view.entry_center(i, l.grid_area).unwrap();
            assert_eq!(
                dialog.hit_test(center, view()),
                Some(DialogHit::App(i)),
                "tile {i}"
            );
            assert_eq!(dialog.entry_id(i), Some(format!("app{i}.desktop").as_str()));
        }
    }

    /// Re-opening the folder that is already up keeps its page — `popup` is a no-op while
    /// `_isOpen`, so a second click on the same tile must not reset the view.
    #[test]
    fn reopening_the_same_folder_keeps_its_page() {
        let mut dialog = open_dialog(10);
        assert!(dialog.set_page(1, view()));
        assert_eq!(dialog.current_page(), 1);

        dialog.popup("Utilities", "Utilities", vec![entry("app0.desktop")]);
        assert_eq!(dialog.current_page(), 1);

        // A *different* folder replaces the view outright.
        dialog.popup("System", "System", vec![entry("app0.desktop")]);
        assert_eq!(dialog.current_page(), 0);
        assert_eq!(dialog.folder_id(), Some("System"));
    }

    /// A resync that no longer finds the folder closes the dialog; one that does keeps it
    /// open with the new members.
    #[test]
    fn a_vanished_folder_takes_the_dialog_with_it() {
        let mut dialog = open_dialog(4);
        assert!(dialog.resync(Some(("Utilities".to_owned(), vec![entry("only.desktop")]))));
        assert!(dialog.is_open());
        assert_eq!(dialog.entry_id(0), Some("only.desktop"));
        assert_eq!(dialog.entry_id(1), None);

        assert!(dialog.resync(None));
        assert!(!dialog.is_open());
    }
}
