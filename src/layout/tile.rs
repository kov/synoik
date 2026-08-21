// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use core::f64;
use std::rc::Rc;

use smithay::backend::renderer::element::{Element, Kind};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use synoik_config::utils::MergeWith as _;
use synoik_config::{Color, CornerRadius, GradientInterpolation, WindowingMode};
use synoik_ipc::WindowLayout;

use super::focus_ring::{FocusRing, FocusRingRenderElement};
use super::opening_window::{OpenAnimation, OpeningWindowRenderElement};
use super::shadow::Shadow;
use super::{
    HitType, LayoutElement, LayoutElementRenderElement, LayoutElementRenderSnapshot, Options,
    SizeFrac, RESIZE_ANIMATION_THRESHOLD,
};
use crate::animation::{Animation, Clock};
use crate::layout::SizingMode;
use crate::render_helpers::background_effect::BackgroundEffectElement;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};
use crate::render_helpers::resize::ResizeRenderElement;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::snapshot::NeutralSnapshot;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::synoik_render_elements;
use crate::utils::transaction::Transaction;
use crate::utils::{
    baba_is_float_offset, round_logical_in_physical, round_logical_in_physical_max1,
};
use crate::window::ResolvedWindowRules;

/// Toplevel window with decorations.
#[derive(Debug)]
pub struct Tile<W: LayoutElement> {
    /// The toplevel window itself.
    window: W,

    /// The border around the window.
    border: FocusRing,

    /// The focus ring around the window.
    focus_ring: FocusRing,

    /// The shadow around the window.
    shadow: Shadow,

    /// This tile's current sizing mode.
    ///
    /// This will update only when the `window` actually goes maximized or fullscreen, rather than
    /// right away, to avoid black backdrop flicker before the window has had a chance to resize.
    sizing_mode: SizingMode,

    /// The tiled edges the window has committed to, the other half of its sizing state. Tracked
    /// for the same reason as `sizing_mode`: a change in either is a transition worth animating.
    tiled_edges: [bool; 4],

    /// The black backdrop for fullscreen windows.
    fullscreen_backdrop: SolidColorBuffer,

    /// Whether the tile should float upon unfullscreening.
    pub(super) restore_to_floating: bool,

    /// The size that the window should assume when going floating.
    ///
    /// This is generally the last size the window had when it was floating. It can be unknown if
    /// the window starts out in the tiling layout or fullscreen.
    pub(super) floating_window_size: Option<Size<i32, Logical>>,

    /// The position that the tile should assume when going floating, relative to the floating
    /// space working area.
    ///
    /// This is generally the last position the tile had when it was floating. It can be unknown if
    /// the window starts out in the tiling layout.
    pub(super) floating_pos: Option<Point<f64, SizeFrac>>,

    /// The window size to restore when untiling (mutter's `saved_rect`).
    ///
    /// Kept separately from `floating_window_size`, which tracks the live
    /// floating geometry and gets overwritten while tiled.
    pub(super) tiled_restore_size: Option<Size<i32, Logical>>,

    /// The position to restore when untiling (mutter's `saved_rect`).
    pub(super) tiled_restore_pos: Option<Point<f64, SizeFrac>>,

    /// The size a restore asked the client for, held until the client acts on it.
    ///
    /// mutter's wayland `save_rect` reads the geometry the *compositor asked for* — it walks
    /// `pending_configurations` and only falls back to the window's own config
    /// (`meta_window_wayland_save_rect`, `src/wayland/meta-window-wayland.c:1151`). We cannot
    /// simply do the same: a GTK4 client answers an unmaximize by acking at its old size and
    /// only shrinking a commit later (see `448e2dc5`), so our configure has left the pending set
    /// while the window is still maximized-sized. Re-maximizing in that window would save the
    /// work-area size as the rect to come back to, and the next unmaximize would "restore" the
    /// window to the full screen. So we remember what we asked for instead.
    pub(super) restore_in_flight: Option<RestoreInFlight>,

    /// Whether the window was maximized when it went fullscreen (mutter's `saved_maximize`).
    ///
    /// A window's `SizingMode` is a single value with fullscreen on top, so this is the only place
    /// the maximized state underneath it survives; unfullscreening consults it to decide whether
    /// to land on maximized or on the saved rect.
    pub(super) saved_maximize: bool,

    /// Currently selected preset width index when this tile is floating.
    pub(super) floating_preset_width_idx: Option<usize>,

    /// Currently selected preset height index when this tile is floating.
    pub(super) floating_preset_height_idx: Option<usize>,

    /// The animation upon opening a window.
    open_animation: Option<OpenAnimation>,

    /// The animation of the window resizing.
    resize_animation: Option<ResizeAnimation>,

    /// The animation of a tile visually moving horizontally.
    move_x_animation: Option<MoveAnimation>,

    /// The animation of a tile visually moving vertically.
    move_y_animation: Option<MoveAnimation>,

    /// The animation of the tile's opacity.
    pub(super) alpha_animation: Option<AlphaAnimation>,

    /// The animation of the tile growing out of somewhere it is not — see [`GrowAnimation`].
    grow_animation: Option<GrowAnimation>,

    /// Offset during the initial interactive move rubberband.
    pub(super) interactive_move_offset: Point<f64, Logical>,

    /// Snapshot of the last render for use in the close animation.
    unmap_snapshot: Option<TileUnmapSnapshot>,

    /// The view size for the tile's workspace.
    ///
    /// Used as the fullscreen target size.
    view_size: Size<f64, Logical>,

    /// Scale of the output the tile is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
}

/// A restore the client has not acted on yet; see [`Tile::restore_in_flight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreInFlight {
    /// The window's size when the restore was requested. The client has acted once it differs.
    pub from_size: Size<i32, Logical>,
    /// The size the restore asked for — the rect to keep as the one to come back to.
    pub size: Size<i32, Logical>,
}

synoik_render_elements! {
    TileRenderElement => {
        LayoutElement = LayoutElementRenderElement,
        FocusRing = FocusRingRenderElement,
        SolidColor = SolidColorRenderElement,
        Opening = OpeningWindowRenderElement,
        Resize = ResizeRenderElement,
        Border = BorderRenderElement,
        Shadow = ShadowRenderElement,
        Offscreen = OffscreenRenderElement,
        ExtraDamage = ExtraDamage,
        BackgroundEffect = BackgroundEffectElement,
    }
}

/// A tile's stored unmap snapshot: renderer-neutral CPU buffers, one per block-out variant.
pub type TileUnmapSnapshot = NeutralSnapshot;

/// The renderer an unmap snapshot is captured through, threaded down the layout tree. A concrete
/// renderer rather than `&mut VulkanRenderer`: capturing reads back pixels, which is not
/// something the generic render path does.
pub type SnapshotRenderer<'a> = &'a mut crate::render_helpers::vulkan::VulkanRenderer;

#[derive(Debug)]
struct ResizeAnimation {
    anim: Animation,
    size_from: Size<f64, Logical>,
    snapshot: LayoutElementRenderSnapshot,
    /// The "current window" snapshot. Reused across frames — its texture is re-rendered in place,
    /// never reallocated (matters on Venus, where per-frame allocation exhausts host blobs).
    offscreen_vk: OffscreenBuffer,
    /// The pre-resize snapshot uploaded to a `VkTexture`, cached for the whole animation (the
    /// source `MemoryBuffer` in `snapshot.neutral` never changes), so it is imported once, not
    /// per frame.
    prev_vk: std::cell::RefCell<Option<crate::render_helpers::vulkan::VkTexture>>,
    tile_size_from: Size<f64, Logical>,
    // If the resize involved the fullscreen state at some point, this is the progress toward the
    // fullscreen state. Used for things like fullscreen backdrop alpha.
    //
    // Note that this can be set even if this specific resize is between two non-fullscreen states,
    // for example when issuing a new resize during an unfullscreen resize.
    fullscreen_progress: Option<Animation>,
    // Similar to above but for fullscreen-or-maximized.
    expanded_progress: Option<Animation>,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
    /// Whether the move is waiting for the resize it belongs to.
    ///
    /// A sizing-mode change moves and resizes as one transition, but only the move can start when
    /// the user asks for it: the resize waits on the client committing the new size, one or two
    /// commits later. Running the move meanwhile is what made a maximize read as "slide into the
    /// corner, *then* grow". While held the offset stays at `from` and the clock is not consulted;
    /// [`Tile::release_held_move`] restarts it alongside the resize.
    held: bool,
}

impl MoveAnimation {
    /// How much of the move is left, 1 at the start and 0 at the end. A held move has not started.
    fn progress(&self) -> f64 {
        if self.held {
            1.
        } else {
            self.anim.value()
        }
    }
}

#[derive(Debug)]
pub(super) struct AlphaAnimation {
    pub(super) anim: Animation,
    /// Whether the animation should persist after it's done.
    ///
    /// This is used by things like interactive move which need to animate alpha to
    /// semitransparent, then hold it at semitransparent for a while, until the operation
    /// completes.
    pub(super) hold_after_done: bool,
    /// Reused across the animation's frames (reallocation churns virtio-gpu blobs).
    offscreen_vk: OffscreenBuffer,
}

/// A tile drawn as if it were somewhere else, easing back to where it really is.
///
/// The mirror of the minimize shrink: an unminimized window grows out of the rect it was hidden
/// in (its dock icon), the same rect it shrank into. The tile is in the layout for the whole
/// animation — focusable, raisable, hit-testable from the first frame — and only its *drawing*
/// comes from elsewhere, so nothing downstream has to know this is running.
///
/// Deliberately not applied inside [`Tile::render`]: the picker derives its own scale from the
/// tile's natural size and applies it to natural-size elements, so a transform baked into the
/// tile would compose with the picker's and draw the window at the product of the two. The space
/// that positions the tile applies this one instead, and the picker simply does not.
#[derive(Debug)]
struct GrowAnimation {
    /// Where the tile is drawn at progress 0, in the same coordinates the space positions it in.
    from: Rectangle<f64, Logical>,
    anim: Animation,
}

/// The `border` config a window actually gets.
///
/// **GNOME draws no compositor-side window chrome.** A window is a window: focus
/// is communicated by raising it and by the client's own CSD headerbar, never by
/// an outline the shell paints around it — mutter has no border/focus-ring
/// concept at all, and gnome-shell's `.window-clone-border` exists only inside
/// the overview. niri's border and focus ring are its own idiom, so in GNOME
/// windowing mode they are forced off, config *and* window rules alike: leaving
/// the rules live would let a single rule paint an outline GNOME never draws.
///
/// This zeroes their geometry too — `visual_border_width` returns `None` once the
/// border is off, so the window keeps the whole tile.
fn border_config(options: &Options, rules: &ResolvedWindowRules) -> synoik_config::Border {
    if options.layout.windowing_mode == WindowingMode::Floating {
        return synoik_config::Border {
            off: true,
            ..options.layout.border
        };
    }
    options.layout.border.merged_with(&rules.border)
}

/// The `focus-ring` config a window actually gets — off in GNOME mode, for the
/// reasons on [`border_config`].
fn focus_ring_config(options: &Options, rules: &ResolvedWindowRules) -> synoik_config::FocusRing {
    if options.layout.windowing_mode == WindowingMode::Floating {
        return synoik_config::FocusRing {
            off: true,
            ..options.layout.focus_ring
        };
    }
    options.layout.focus_ring.merged_with(&rules.focus_ring)
}

impl<W: LayoutElement> Tile<W> {
    pub fn new(
        window: W,
        view_size: Size<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let rules = window.rules();
        let border_config = border_config(&options, rules);
        let focus_ring_config = focus_ring_config(&options, rules);
        let shadow_config = options.layout.shadow.merged_with(&rules.shadow);
        let sizing_mode = window.sizing_mode();
        let tiled_edges = window.committed_tiled_edges();

        Self {
            window,
            border: FocusRing::new(border_config.into()),
            focus_ring: FocusRing::new(focus_ring_config),
            shadow: Shadow::new(shadow_config),
            sizing_mode,
            tiled_edges,
            fullscreen_backdrop: SolidColorBuffer::new((0., 0.), [0., 0., 0., 1.]),
            restore_to_floating: false,
            floating_window_size: None,
            floating_pos: None,
            tiled_restore_size: None,
            tiled_restore_pos: None,
            restore_in_flight: None,
            saved_maximize: false,
            floating_preset_width_idx: None,
            floating_preset_height_idx: None,
            open_animation: None,
            resize_animation: None,
            move_x_animation: None,
            move_y_animation: None,
            alpha_animation: None,
            grow_animation: None,
            interactive_move_offset: Point::from((0., 0.)),
            unmap_snapshot: None,
            view_size,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        // If preset widths or heights changed, clear our stored preset index.
        if self.options.layout.preset_column_widths != options.layout.preset_column_widths {
            self.floating_preset_width_idx = None;
        }
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.floating_preset_height_idx = None;
        }

        self.view_size = view_size;
        self.scale = scale;
        self.options = options;

        let round_max1 = |logical| round_logical_in_physical_max1(self.scale, logical);

        let rules = self.window.rules();

        let mut border_config = border_config(&self.options, rules);
        border_config.width = round_max1(border_config.width);
        self.border.update_config(border_config.into());

        let mut focus_ring_config = focus_ring_config(&self.options, rules);
        focus_ring_config.width = round_max1(focus_ring_config.width);
        self.focus_ring.update_config(focus_ring_config);

        let shadow_config = self.options.layout.shadow.merged_with(&rules.shadow);
        self.shadow.update_config(shadow_config);

        self.window.update_config(self.options.blur);
    }

    pub fn update_shaders(&mut self) {
        self.border.update_shaders();
        self.focus_ring.update_shaders();
        self.shadow.update_shaders();
    }

    pub fn update_window(&mut self) {
        let prev_sizing_mode = self.sizing_mode;
        self.sizing_mode = self.window.sizing_mode();
        let prev_tiled_edges = self.tiled_edges;
        self.tiled_edges = self.window.committed_tiled_edges();

        if let Some(animate_from) = self.window.take_animation_snapshot() {
            let params = if let Some(resize) = self.resize_animation.take() {
                // Compute like in animated_window_size(), but using the snapshot geometry (since
                // the current one is already overwritten).
                let mut size = animate_from.size;

                let val = resize.anim.value();
                let size_from = resize.size_from;
                let tile_size_from = resize.tile_size_from;

                size.w = size_from.w + (size.w - size_from.w) * val;
                size.h = size_from.h + (size.h - size_from.h) * val;

                let mut tile_size = animate_from.size;
                if prev_sizing_mode.is_fullscreen() {
                    tile_size.w = f64::max(tile_size.w, self.view_size.w);
                    tile_size.h = f64::max(tile_size.h, self.view_size.h);
                } else if prev_sizing_mode.is_normal() && !self.border.is_off() {
                    let width = self.border.width();
                    tile_size.w += width * 2.;
                    tile_size.h += width * 2.;
                }

                tile_size.w = tile_size_from.w + (tile_size.w - tile_size_from.w) * val;
                tile_size.h = tile_size_from.h + (tile_size.h - tile_size_from.h) * val;

                let fullscreen_from = resize
                    .fullscreen_progress
                    .map(|anim| anim.clamped_value().clamp(0., 1.))
                    .unwrap_or(if prev_sizing_mode.is_fullscreen() {
                        1.
                    } else {
                        0.
                    });

                let expanded_from = resize
                    .expanded_progress
                    .map(|anim| anim.clamped_value().clamp(0., 1.))
                    .unwrap_or(if prev_sizing_mode.is_normal() { 0. } else { 1. });

                (size, tile_size, fullscreen_from, expanded_from)
            } else {
                let size = animate_from.size;

                // Compute like in tile_size().
                let mut tile_size = size;
                if prev_sizing_mode.is_fullscreen() {
                    tile_size.w = f64::max(tile_size.w, self.view_size.w);
                    tile_size.h = f64::max(tile_size.h, self.view_size.h);
                } else if prev_sizing_mode.is_normal() && !self.border.is_off() {
                    let width = self.border.width();
                    tile_size.w += width * 2.;
                    tile_size.h += width * 2.;
                }

                let fullscreen_from = if prev_sizing_mode.is_fullscreen() {
                    1.
                } else {
                    0.
                };

                let expanded_from = if prev_sizing_mode.is_normal() { 0. } else { 1. };

                (size, tile_size, fullscreen_from, expanded_from)
            };
            let (size_from, tile_size_from, fullscreen_from, expanded_from) = params;

            let change = self.window.size().to_f64().to_point() - size_from.to_point();
            let change = f64::max(change.x.abs(), change.y.abs());
            let tile_change = self.tile_size().to_f64().to_point() - tile_size_from.to_point();
            let tile_change = f64::max(tile_change.x.abs(), tile_change.y.abs());
            let change = f64::max(change, tile_change);
            if change > RESIZE_ANIMATION_THRESHOLD {
                let anim = Animation::new(
                    self.clock.clone(),
                    0.,
                    1.,
                    0.,
                    self.options.animations.window_resize.anim,
                );

                let fullscreen_to = if self.sizing_mode.is_fullscreen() {
                    1.
                } else {
                    0.
                };
                let expanded_to = if self.sizing_mode.is_normal() { 0. } else { 1. };
                let fullscreen_progress = (fullscreen_from != fullscreen_to)
                    .then(|| anim.restarted(fullscreen_from, fullscreen_to, 0.));
                let expanded_progress = (expanded_from != expanded_to)
                    .then(|| anim.restarted(expanded_from, expanded_to, 0.));

                self.resize_animation = Some(ResizeAnimation {
                    anim,
                    size_from,
                    snapshot: animate_from,
                    // Fresh per resize-start (persists across the animation's frames, reused
                    // there); prev_vk starts empty since the pre-resize
                    // snapshot just changed.
                    offscreen_vk: OffscreenBuffer::default(),
                    prev_vk: std::cell::RefCell::new(None),
                    tile_size_from,
                    fullscreen_progress,
                    expanded_progress,
                });
            } else {
                self.resize_animation = None;

                // Nothing to animate yet, but the window did take a new sizing mode on this
                // commit — so this is the ack, and the resize is on the commit after it. Hold the
                // arm for that one rather than spending it here on a window that has not moved.
                if prev_sizing_mode != self.sizing_mode || prev_tiled_edges != self.tiled_edges {
                    self.window.hold_animate_arm();
                }
            }
        }

        // The move half of a sizing-mode change waits for the resize half, so that the two run as
        // one transition rather than a slide followed by a grow. Release it once the resize has
        // started, or once nothing is coming that it could wait for.
        if self.resize_animation.is_some() || !self.window.animate_pending() {
            self.release_held_move();
        }

        let round_max1 = |logical| round_logical_in_physical_max1(self.scale, logical);

        let rules = self.window.rules();
        let mut border_config = border_config(&self.options, rules);
        border_config.width = round_max1(border_config.width);
        self.border.update_config(border_config.into());

        let mut focus_ring_config = focus_ring_config(&self.options, rules);
        focus_ring_config.width = round_max1(focus_ring_config.width);
        self.focus_ring.update_config(focus_ring_config);

        let shadow_config = self.options.layout.shadow.merged_with(&rules.shadow);
        self.shadow.update_config(shadow_config);
    }

    pub fn advance_animations(&mut self) {
        if let Some(open) = &mut self.open_animation {
            if open.is_done() {
                self.open_animation = None;
            }
        }

        if let Some(resize) = &mut self.resize_animation {
            if resize.anim.is_done() {
                self.resize_animation = None;
            }
        }

        for slot in [&mut self.move_x_animation, &mut self.move_y_animation] {
            let Some(move_) = slot.as_mut() else {
                continue;
            };

            // A held move waits for its resize, but not forever. If the client has not committed
            // the new size by the time the move would itself have finished, run it: a window left
            // parked away from where the layout has it is worse than a late move, and this is also
            // what settles the hold when animations are asked to complete instantly.
            if move_.held && move_.anim.is_done() {
                move_.anim = move_.anim.restarted(1., 0., 0.);
                move_.held = false;
            }

            if !move_.held && move_.anim.is_done() {
                *slot = None;
            }
        }

        if let Some(alpha) = &mut self.alpha_animation {
            if !alpha.hold_after_done && alpha.anim.is_done() {
                self.alpha_animation = None;
            }
        }

        if let Some(grow) = &mut self.grow_animation {
            if grow.anim.is_done() {
                self.grow_animation = None;
            }
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.are_transitions_ongoing() || self.window.rules().baba_is_float == Some(true)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.open_animation.is_some()
            || self.resize_animation.is_some()
            || self.move_x_animation.is_some()
            || self.move_y_animation.is_some()
            || self
                .alpha_animation
                .as_ref()
                .is_some_and(|alpha| !alpha.anim.is_done())
            || self.grow_animation.is_some()
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        let rules = self.window.rules();
        let animated_tile_size = self.animated_tile_size();
        let expanded_progress = self.expanded_progress();

        // Never paint a filled backdrop behind the window.
        //
        // niri defaulted this to `!has_ssd()`, i.e. ON for every CSD client — which is every
        // modern Wayland app. With the border off (our default) it falls through to the focus
        // ring at the bottom of this function, so the *focused* window got an opaque rect over
        // its whole geometry, silently destroying client translucency: a 50%-opacity terminal
        // measured a flat, zero-variance backdrop while focused and blended correctly the moment
        // it lost focus (and in the overview, which draws no focus ring). GNOME has no such
        // chrome, so there is nothing this can be right for.
        //
        // The window focus ring and border are niri-isms slated for removal outright; until then
        // this keeps them from occluding anything. Note `FocusRing` the *type* stays either way —
        // `ui::mru` and the overview's workspace thumbnails reuse it as a rounded-rect helper.
        let draw_border_with_background = rules.draw_border_with_background.unwrap_or(false);
        let border_width = self.visual_border_width().unwrap_or(0.);

        // Do the inverse of tile_size() in order to handle the unfullscreen animation for windows
        // that were smaller than the fullscreen size, and therefore their animated_window_size() is
        // currently much smaller than the tile size.
        let mut border_window_size = animated_tile_size;
        border_window_size.w -= border_width * 2.;
        border_window_size.h -= border_width * 2.;

        // FIXME: this takes into account the animation from normal sizing mode to
        // maximized/fullscreen, but it doesn't take into account the corner radius animation from
        // the window itself.
        //
        // Currently, an easy way to see the problem is to start from a window with a nonzero
        // radius, then go from windowed fullscreen (that forces 0 radius) to regular fullscreen.
        // At the start of the animation, windowed fullscreen becomes false, but the window hasn't
        // animated to the normal fullscreen yet, so the radius here jumps to its nonzero value,
        // even though it should remain zero throughout.
        //
        // Later, when windows get the surface shape protocol with radii, this issue will happen
        // when that changes between animated commits.
        let radius = self
            .window
            .geometry_corner_radius()
            .expanded_by(border_width as f32)
            .scaled_by(1. - expanded_progress as f32);
        self.border.update_render_elements(
            border_window_size,
            is_active,
            !draw_border_with_background,
            self.window.is_urgent(),
            Rectangle::new(
                view_rect.loc - Point::from((border_width, border_width)),
                view_rect.size,
            ),
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        let radius = if self.visual_border_width().is_some() {
            radius
        } else {
            self.window
                .geometry_corner_radius()
                .scaled_by(1. - expanded_progress as f32)
        };
        self.shadow.update_render_elements(
            animated_tile_size,
            is_active,
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        let draw_focus_ring_with_background = if self.border.is_off() {
            draw_border_with_background
        } else {
            false
        };
        let radius = radius.expanded_by(self.focus_ring.width() as f32);
        self.focus_ring.update_render_elements(
            animated_tile_size,
            is_active,
            !draw_focus_ring_with_background,
            self.window.is_urgent(),
            view_rect,
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        self.fullscreen_backdrop.resize(animated_tile_size);
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_x_animation {
            offset.x += move_.from * move_.progress();
        }
        if let Some(move_) = &self.move_y_animation {
            offset.y += move_.from * move_.progress();
        }

        offset += self.interactive_move_offset;

        offset
    }

    pub fn start_open_animation(&mut self) {
        self.open_animation = Some(OpenAnimation::new(Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_open.anim,
        )));
    }

    /// Draw the tile out of `from` for the next `config`'s worth of time — see [`GrowAnimation`].
    pub fn animate_grow_from(
        &mut self,
        from: Rectangle<f64, Logical>,
        config: synoik_config::Animation,
    ) {
        self.grow_animation = Some(GrowAnimation {
            from,
            anim: Animation::new(self.clock.clone(), 0., 1., 0., config),
        });
    }

    /// Where and how big to draw a growing tile that the space would put at `pos`.
    ///
    /// `None` when the tile is simply where it is, which is every tile almost all of the time.
    /// The target is read fresh each frame rather than captured at the start, so a tile that is
    /// moved or relaid out while it grows still lands on its real place.
    pub fn grow_transform(&self, pos: Point<f64, Logical>) -> Option<(Point<f64, Logical>, f64)> {
        let grow = self.grow_animation.as_ref()?;
        let natural = self.tile_size();
        if natural.w <= 0. {
            return None;
        }

        let t = grow.anim.clamped_value();
        let lerp = |a: f64, b: f64| a + (b - a) * t;
        let loc = Point::from((lerp(grow.from.loc.x, pos.x), lerp(grow.from.loc.y, pos.y)));
        Some((loc, lerp(grow.from.size.w / natural.w, 1.)))
    }

    pub fn resize_animation(&self) -> Option<&Animation> {
        self.resize_animation.as_ref().map(|resize| &resize.anim)
    }

    pub fn animate_move_from(&mut self, from: Point<f64, Logical>) {
        self.animate_move_x_from(from.x);
        self.animate_move_y_from(from.y);
    }

    pub fn animate_move_x_from(&mut self, from: f64) {
        self.animate_move_x_from_with_config(from, self.options.animations.window_movement.0);
    }

    pub fn animate_move_x_from_with_config(&mut self, from: f64, config: synoik_config::Animation) {
        let current_offset = self.render_offset().x;

        // Preserve the previous config if ongoing.
        let anim = self.move_x_animation.take().map(|move_| move_.anim);
        let anim = anim
            .map(|anim| anim.restarted(1., 0., 0.))
            .unwrap_or_else(|| Animation::new(self.clock.clone(), 1., 0., 0., config));

        self.move_x_animation = Some(MoveAnimation {
            anim,
            from: from + current_offset,
            held: false,
        });
    }

    pub fn animate_move_y_from(&mut self, from: f64) {
        self.animate_move_y_from_with_config(from, self.options.animations.window_movement.0);
    }

    pub fn animate_move_y_from_with_config(&mut self, from: f64, config: synoik_config::Animation) {
        let current_offset = self.render_offset().y;

        // Preserve the previous config if ongoing.
        let anim = self.move_y_animation.take().map(|move_| move_.anim);
        let anim = anim
            .map(|anim| anim.restarted(1., 0., 0.))
            .unwrap_or_else(|| Animation::new(self.clock.clone(), 1., 0., 0., config));

        self.move_y_animation = Some(MoveAnimation {
            anim,
            from: from + current_offset,
            held: false,
        });
    }

    pub fn offset_move_y_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_y_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.progress();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    /// Start a sizing-mode change's move, held until its resize starts.
    ///
    /// See [`MoveAnimation::held`]. The offset applies immediately — the tile's model position has
    /// already changed, so the window must keep rendering where it was — but it does not decay
    /// until [`Self::release_held_move`].
    pub fn hold_move_from(&mut self, from: Point<f64, Logical>, config: synoik_config::Animation) {
        let current_offset = self.render_offset();
        for (slot, from) in [
            (&mut self.move_x_animation, from.x + current_offset.x),
            (&mut self.move_y_animation, from.y + current_offset.y),
        ] {
            *slot = Some(MoveAnimation {
                anim: Animation::new(self.clock.clone(), 1., 0., 0., config),
                from,
                held: true,
            });
        }
    }

    /// Let a held move run, from wherever it was held.
    pub fn release_held_move(&mut self) {
        for move_ in [&mut self.move_x_animation, &mut self.move_y_animation]
            .into_iter()
            .flatten()
        {
            if move_.held {
                move_.anim = move_.anim.restarted(1., 0., 0.);
                move_.held = false;
            }
        }
    }

    pub fn stop_move_animations(&mut self) {
        self.move_x_animation = None;
        self.move_y_animation = None;
    }

    pub fn animate_alpha(&mut self, from: f64, to: f64, config: synoik_config::Animation) {
        let from = from.clamp(0., 1.);
        let to = to.clamp(0., 1.);

        let taken = self.alpha_animation.take();
        let current = taken.as_ref().map_or(from, |a| a.anim.clamped_value());

        // Reuse the existing offscreen buffer (reallocation churns virtio-gpu blobs).
        let offscreen_vk = taken.map(|a| a.offscreen_vk).unwrap_or_default();

        self.alpha_animation = Some(AlphaAnimation {
            anim: Animation::new(self.clock.clone(), current, to, 0., config),
            hold_after_done: false,
            offscreen_vk,
        });
    }

    pub fn ensure_alpha_animates_to_1(&mut self) {
        if let Some(alpha) = &self.alpha_animation {
            if alpha.anim.to() != 1. {
                // Cancel animation instead of starting a new one because the user likely wants to
                // see the tile right away.
                self.alpha_animation = None;
            }
        }
    }

    pub fn hold_alpha_animation_after_done(&mut self) {
        if let Some(alpha) = &mut self.alpha_animation {
            alpha.hold_after_done = true;
        }
    }

    pub fn window(&self) -> &W {
        &self.window
    }

    pub fn window_mut(&mut self) -> &mut W {
        &mut self.window
    }

    pub fn sizing_mode(&self) -> SizingMode {
        self.sizing_mode
    }

    fn fullscreen_progress(&self) -> f64 {
        if let Some(resize) = &self.resize_animation {
            if let Some(anim) = &resize.fullscreen_progress {
                return anim.clamped_value().clamp(0., 1.);
            }
        }

        if self.sizing_mode.is_fullscreen() {
            1.
        } else {
            0.
        }
    }

    fn expanded_progress(&self) -> f64 {
        if let Some(resize) = &self.resize_animation {
            if let Some(anim) = &resize.expanded_progress {
                return anim.clamped_value().clamp(0., 1.);
            }
        }

        if self.sizing_mode.is_normal() {
            0.
        } else {
            1.
        }
    }

    /// Returns `None` if the border is hidden and `Some(width)` if it should be shown.
    pub fn effective_border_width(&self) -> Option<f64> {
        if !self.sizing_mode.is_normal() {
            return None;
        }

        if self.border.is_off() {
            return None;
        }

        Some(self.border.width())
    }

    fn visual_border_width(&self) -> Option<f64> {
        if self.border.is_off() {
            return None;
        }

        let expanded_progress = self.expanded_progress();

        // Only hide the border when fully expanded to avoid jarring border appearance.
        if expanded_progress == 1. {
            return None;
        }

        // FIXME: would be cool to, like, gradually resize the border from full width to 0 during
        // fullscreening, but the rest of the code isn't quite ready for that yet. It needs to
        // handle things like computing intermediate tile size when an animated resize starts during
        // an animated unfullscreen resize.
        Some(self.border.width())
    }

    /// Returns the location of the window's visual geometry within this Tile.
    pub fn window_loc(&self) -> Point<f64, Logical> {
        self.centered_window_offset(self.animated_tile_size(), self.animated_window_size())
    }

    /// Where the window sits inside its tile, from the *model* sizes rather than the animated ones.
    ///
    /// Anything persisted has to use this one: a window closed mid-resize would otherwise be
    /// remembered at wherever the animation happened to have reached.
    pub fn window_offset(&self) -> Point<f64, Logical> {
        self.centered_window_offset(self.tile_size(), self.window_size())
    }

    /// Center the window within its tile.
    ///
    /// - Without borders, the sizes match, so this difference is zero.
    /// - Borders always match from all sides, so this difference is pre-rounded to physical.
    /// - In fullscreen, if the window is smaller than the tile, then it gets centered, otherwise
    ///   the tile size matches the window.
    /// - During animations, the window remains centered within the tile; this is important for the
    ///   to/from fullscreen animation.
    fn centered_window_offset(
        &self,
        tile_size: Size<f64, Logical>,
        window_size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let loc = Point::from((
            (tile_size.w - window_size.w) / 2.,
            (tile_size.h - window_size.h) / 2.,
        ));

        // Round to physical pixels.
        loc.to_physical_precise_round(self.scale)
            .to_logical(self.scale)
    }

    pub fn tile_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_size();

        if self.sizing_mode.is_fullscreen() {
            // Normally we'd just return the fullscreen size here, but this makes things a bit
            // nicer if a fullscreen window is bigger than the fullscreen size for some reason.
            size.w = f64::max(size.w, self.view_size.w);
            size.h = f64::max(size.h, self.view_size.h);
            return size;
        }

        if let Some(width) = self.effective_border_width() {
            size.w += width * 2.;
            size.h += width * 2.;
        }

        size
    }

    pub fn tile_expected_or_current_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_expected_or_current_size();

        if self.sizing_mode.is_fullscreen() {
            // Normally we'd just return the fullscreen size here, but this makes things a bit
            // nicer if a fullscreen window is bigger than the fullscreen size for some reason.
            size.w = f64::max(size.w, self.view_size.w);
            size.h = f64::max(size.h, self.view_size.h);
            return size;
        }

        if let Some(width) = self.effective_border_width() {
            size.w += width * 2.;
            size.h += width * 2.;
        }

        size
    }

    pub fn window_size(&self) -> Size<f64, Logical> {
        let mut size = self.window.size().to_f64();
        size = size
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);
        size
    }

    pub fn window_expected_or_current_size(&self) -> Size<f64, Logical> {
        let size = self.window.expected_size();
        let mut size = size.unwrap_or_else(|| self.window.size()).to_f64();
        size = size
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);
        size
    }

    pub fn animated_window_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_size();

        if let Some(resize) = &self.resize_animation {
            let val = resize.anim.value();
            let size_from = resize.size_from.to_f64();

            size.w = f64::max(1., size_from.w + (size.w - size_from.w) * val);
            size.h = f64::max(1., size_from.h + (size.h - size_from.h) * val);
            size = size
                .to_physical_precise_round(self.scale)
                .to_logical(self.scale);
        }

        size
    }

    pub fn animated_tile_size(&self) -> Size<f64, Logical> {
        let mut size = self.tile_size();

        if let Some(resize) = &self.resize_animation {
            let val = resize.anim.value();
            let size_from = resize.tile_size_from.to_f64();

            size.w = f64::max(1., size_from.w + (size.w - size_from.w) * val);
            size.h = f64::max(1., size_from.h + (size.h - size_from.h) * val);
            size = size
                .to_physical_precise_round(self.scale)
                .to_logical(self.scale);
        }

        size
    }

    pub fn buf_loc(&self) -> Point<f64, Logical> {
        let mut loc = Point::from((0., 0.));
        loc += self.window_loc();
        loc += self.window.buf_loc().to_f64();
        loc
    }

    /// Returns a partially-filled [`WindowLayout`].
    ///
    /// Only the sizing properties that a [`Tile`] can fill are filled.
    pub fn ipc_layout_template(&self) -> WindowLayout {
        WindowLayout {
            pos_in_scrolling_layout: None,
            tile_size: self.tile_size().into(),
            window_size: self.window().size().into(),
            surface_size: self.window().buf_size().into(),
            tile_pos_in_workspace_view: None,
            window_offset_in_tile: self.window_loc().into(),
        }
    }

    fn is_in_input_region(&self, mut point: Point<f64, Logical>) -> bool {
        point -= self.window_loc().to_f64();
        self.window.is_in_input_region(point)
    }

    fn is_in_activation_region(&self, point: Point<f64, Logical>) -> bool {
        let activation_region = Rectangle::from_size(self.tile_size());
        activation_region.contains(point)
    }

    pub fn hit(&self, point: Point<f64, Logical>) -> Option<HitType> {
        let offset = self.bob_offset();
        let point = point - offset;

        if self.is_in_input_region(point) {
            let win_pos = self.buf_loc() + offset;
            Some(HitType::Input { win_pos })
        } else if self.is_in_activation_region(point) {
            Some(HitType::Activate {
                is_tab_indicator: false,
            })
        } else {
            None
        }
    }

    pub fn request_tile_size(
        &mut self,
        mut size: Size<f64, Logical>,
        animate: bool,
        transaction: Option<Transaction>,
    ) {
        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();
            size.w = f64::max(1., size.w - width * 2.);
            size.h = f64::max(1., size.h - width * 2.);
        }

        // The size request has to be i32 unfortunately, due to Wayland. We floor here instead of
        // round to avoid situations where proportionally-sized columns don't fit on the screen
        // exactly.
        self.window.request_size(
            size.to_i32_floor(),
            SizingMode::Normal,
            animate,
            transaction,
        );
    }

    pub fn tile_width_for_window_width(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size + self.border.width() * 2.
        }
    }

    pub fn tile_height_for_window_height(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size + self.border.width() * 2.
        }
    }

    pub fn window_width_for_tile_width(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size - self.border.width() * 2.
        }
    }

    pub fn window_height_for_tile_height(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size - self.border.width() * 2.
        }
    }

    pub fn request_maximized(
        &mut self,
        size: Size<f64, Logical>,
        animate: bool,
        transaction: Option<Transaction>,
    ) {
        self.window.request_size(
            size.to_i32_round(),
            SizingMode::Maximized,
            animate,
            transaction,
        );
    }

    pub fn request_fullscreen(&mut self, animate: bool, transaction: Option<Transaction>) {
        self.window.request_size(
            self.view_size.to_i32_round(),
            SizingMode::Fullscreen,
            animate,
            transaction,
        );
    }

    pub fn min_size_nonfullscreen(&self) -> Size<f64, Logical> {
        let mut size = self.window.min_size().to_f64();

        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();

            size.w = f64::max(1., size.w);
            size.h = f64::max(1., size.h);

            size.w += width * 2.;
            size.h += width * 2.;
        }

        size
    }

    pub fn max_size_nonfullscreen(&self) -> Size<f64, Logical> {
        let mut size = self.window.max_size().to_f64();

        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();

            if size.w > 0. {
                size.w += width * 2.;
            }
            if size.h > 0. {
                size.h += width * 2.;
            }
        }

        size
    }

    pub fn bob_offset(&self) -> Point<f64, Logical> {
        if self.window.rules().baba_is_float != Some(true) {
            return Point::from((0., 0.));
        }

        let y = baba_is_float_offset(self.clock.now(), self.view_size.h);
        let y = round_logical_in_physical(self.scale, y);
        Point::from((0., y))
    }

    fn render_inner(
        &self,
        mut ctx: RenderCtx,
        location: Point<f64, Logical>,
        mut xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(TileRenderElement),
    ) {
        let _span = tracy_client::span!("Tile::render_inner");

        let scale = Scale::from(self.scale);
        let fullscreen_progress = self.fullscreen_progress();
        let expanded_progress = self.expanded_progress();

        let win_alpha = if self.window.is_ignoring_opacity_window_rule() {
            1.
        } else {
            let alpha = self.window.rules().opacity.unwrap_or(1.).clamp(0., 1.);

            // Interpolate towards alpha = 1. at fullscreen.
            let p = fullscreen_progress as f32;
            alpha * (1. - p) + 1. * p
        };

        // This is here rather than in render_offset() because render_offset() is currently assumed
        // by the code to be temporary. So, for example, interactive move will try to "grab" the
        // tile at its current render offset and reset the render offset to zero by cancelling the
        // tile move animations. On the other hand, bob_offset() is not resettable, so adding it in
        // render_offset() would cause obvious animation glitches.
        //
        // This isn't to say that adding it here is perfect; indeed, it kind of breaks view_rect
        // passed to update_render_elements(). But, it works well enough for what it is.
        let bob_offset = self.bob_offset();
        let location = location + bob_offset;
        xray_pos = xray_pos.offset(bob_offset);

        let window_loc = self.window_loc();
        let window_size = self.window_size();
        let animated_window_size = self.animated_window_size();
        let window_render_loc = location + window_loc;
        let area = Rectangle::new(window_render_loc, animated_window_size);
        xray_pos = xray_pos.offset(window_loc);

        let rules = self.window.rules();

        let radius = self
            .window
            .geometry_corner_radius()
            .scaled_by(1. - expanded_progress as f32);

        // Popups go on top, whether it's resize or not.
        self.window.render_popups(
            ctx.r(),
            window_render_loc,
            scale,
            win_alpha,
            xray_pos,
            &mut |elem| push(elem.into()),
        );

        // If we're resizing, try to render a shader, or a fallback.
        let mut pushed_resize = false;
        if let Some(resize) = &self.resize_animation {
            // Whether this window must be hidden from the target we're rendering for.
            //
            // The crossfade's "before" texture is the neutral snapshot taken when the resize
            // started. It holds the window's real contents and has no per-target variant, so
            // drawing the crossfade on a blocked-out target would show exactly what block-out
            // exists to hide. Skip it there (and the red fallback with it) and let the plain window
            // render further down substitute the block-out buffer, as it does with no resize
            // running: a blocked-out window loses its crossfade in a capture, rather than leaking.
            let blocked_out = ctx.target.should_block_out(resize.snapshot.block_out_from)
                || ctx.target.should_block_out(rules.block_out_from);

            // `tex_prev` comes from the neutral snapshot captured at resize-start (uploaded once);
            // `tex_next` is the current window rendered into the reused Vulkan offscreen.
            if !blocked_out {
                {
                    let mut vctx = ctx.r();
                    if let Some((prev_mem, prev_geo)) =
                        resize.snapshot.neutral.get().and_then(|o| o.as_ref())
                    {
                        if resize.prev_vk.borrow().is_none() {
                            match TextureBuffer::from_memory_buffer(vctx.renderer, prev_mem) {
                                Ok(tb) => {
                                    *resize.prev_vk.borrow_mut() = Some(tb.texture().clone());
                                }
                                Err(err) => {
                                    warn!("error uploading resize snapshot to Vulkan: {err:?}");
                                }
                            }
                        }
                        let prev_tex = resize.prev_vk.borrow().clone();

                        if let Some(prev_tex) = prev_tex {
                            let mut window_elements = Vec::new();
                            self.window.render_normal(
                                vctx.r(),
                                Point::from((0., 0.)),
                                scale,
                                1.,
                                xray_pos,
                                &mut |elem| window_elements.push(elem),
                            );

                            let current = resize
                                .offscreen_vk
                                .render(vctx.renderer, scale, &window_elements)
                                .map_err(|err| {
                                    warn!("error rendering window to Vulkan texture: {err:?}")
                                })
                                .ok();

                            if let Some((elem_current, _sync_point, mut data)) = current {
                                let texture_current = elem_current.texture().clone();
                                let texture_current_geo = elem_current.geometry(scale);

                                let use_custom = vctx.renderer.has_custom_shader(
                                    crate::render_helpers::vulkan::CustomShaderType::Resize,
                                );
                                let elem = ResizeRenderElement::new(
                                    area,
                                    scale,
                                    (prev_tex, *prev_geo),
                                    resize.snapshot.size,
                                    (texture_current, texture_current_geo),
                                    window_size,
                                    resize.anim.value() as f32,
                                    resize.anim.clamped_value().clamp(0., 1.) as f32,
                                    radius,
                                    win_alpha,
                                    use_custom,
                                );

                                data.id = elem.id().clone();
                                self.window.set_offscreen_data(Some(data));
                                push(elem.into());
                                pushed_resize = true;
                            }
                        }
                    }
                }
            }

            // Not for a blocked-out target: leaving `pushed_resize` false there falls through to
            // the plain window render below, which substitutes the block-out buffer.
            // Painting the red placeholder instead would replace a blocked-out window
            // with a solid red rectangle in every capture taken during a resize.
            if !pushed_resize && !blocked_out {
                let fallback_buffer = SolidColorBuffer::new(area.size, [1., 0., 0., 1.]);
                let elem = SolidColorRenderElement::from_buffer(
                    &fallback_buffer,
                    area.loc,
                    win_alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
                pushed_resize = true;
            }
        }

        // If we're not resizing, render the window itself.
        if !pushed_resize {
            let geo = Rectangle::new(window_render_loc, window_size);
            let radius = radius.fit_to(window_size.w as f32, window_size.h as f32);

            let clip = |elem| match elem {
                LayoutElementRenderElement::Wayland(elem) => {
                    LayoutElementRenderElement::Wayland(elem).into()
                }
                LayoutElementRenderElement::SolidColor(elem) => {
                    // In this branch we're rendering a blocked-out window with a solid
                    // color. We render it with a rounded corner shader because we assume
                    // the window's own CSD already has corners rounded to the
                    // user-provided radius, so our blocked-out rendering should match that
                    // radius.
                    if radius != CornerRadius::default() {
                        return BorderRenderElement::new(
                            geo.size,
                            Rectangle::from_size(geo.size),
                            GradientInterpolation::default(),
                            Color::from_color32f(elem.color()),
                            Color::from_color32f(elem.color()),
                            0.,
                            Rectangle::from_size(geo.size),
                            0.,
                            radius,
                            scale.x as f32,
                            1.,
                        )
                        .with_location(geo.loc)
                        .into();
                    }

                    // Otherwise, render the solid color as is.
                    LayoutElementRenderElement::SolidColor(elem).into()
                }
                elem @ LayoutElementRenderElement::BackgroundEffect(_) => {
                    // This is only used on popups for now. If subsurface blur is implemented, this
                    // will need to be handled somehow.
                    error!("background effect clipping is unimplemented");
                    elem.into()
                }
            };

            self.window.render_normal(
                ctx.r(),
                window_render_loc,
                scale,
                win_alpha,
                xray_pos,
                &mut |elem| push(clip(elem)),
            );
        }

        if fullscreen_progress > 0. {
            let alpha = fullscreen_progress as f32;

            // During the un/fullscreen animation, render a border element in order to use the
            // animated corner radius.
            if fullscreen_progress < 1. {
                let border_width = self.visual_border_width().unwrap_or(0.);
                let radius = self
                    .window
                    .geometry_corner_radius()
                    .expanded_by(border_width as f32)
                    .scaled_by(1. - expanded_progress as f32);

                let size = self.fullscreen_backdrop.size();
                let color = self.fullscreen_backdrop.color();
                let elem = BorderRenderElement::new(
                    size,
                    Rectangle::from_size(size),
                    GradientInterpolation::default(),
                    Color::from_color32f(color),
                    Color::from_color32f(color),
                    0.,
                    Rectangle::from_size(size),
                    0.,
                    radius,
                    scale.x as f32,
                    alpha,
                )
                .with_location(location);
                push(elem.into());
            } else {
                let elem = SolidColorRenderElement::from_buffer(
                    &self.fullscreen_backdrop,
                    location,
                    alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
            }
        }

        if let Some(width) = self.visual_border_width() {
            self.border
                .render(location + Point::from((width, width)), &mut |elem| {
                    push(elem.into())
                });
        }

        // Hide the focus ring when maximized/fullscreened. It's not normally visible anyway due to
        // being outside the monitor or obscured by a solid colored bar, but it is visible under
        // semitransparent bars in maximized state (which is a bit weird) and in the overview (also
        // a bit weird).
        if focus_ring && expanded_progress < 1. {
            self.focus_ring
                .render(location, &mut |elem| push(elem.into()));
        }

        if expanded_progress < 1. {
            self.shadow.render(location, &mut |elem| push(elem.into()));
        }

        let surface_anim_scale = animated_window_size / window_size;
        // Background effects (blur/xray behind the window) now render on both the GLES and the
        // owned Vulkan renderer — `render_background_effect` dispatches the effect buffer's
        // prepare per renderer (see `xray::prepare_effect_buffer`).
        self.window.render_background_effect(
            ctx.r(),
            area,
            self.scale,
            surface_anim_scale,
            radius,
            xray_pos,
            &mut |elem| push(elem.into()),
        );
    }

    pub fn render(
        &self,
        mut ctx: RenderCtx,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(TileRenderElement),
    ) {
        let _span = tracy_client::span!("Tile::render");

        let scale = Scale::from(self.scale);

        let tile_alpha = self
            .alpha_animation
            .as_ref()
            .map_or(1., |alpha| alpha.anim.clamped_value()) as f32;

        let mut pushed = false;
        self.window().set_offscreen_data(None);

        // The open / alpha animations render the tile through an offscreen. If that fails they
        // degrade: skip the offscreen effect and fall through to the plain render below, so the
        // window still shows (just without the animation).
        if let Some(open) = &self.open_animation {
            {
                let mut vctx = ctx.r();
                let mut elements = Vec::new();
                self.render_inner(
                    vctx.r(),
                    Point::new(0., 0.),
                    xray_pos,
                    focus_ring,
                    &mut |elem| elements.push(elem),
                );
                match open.render_vulkan(
                    vctx.renderer,
                    &elements,
                    self.animated_tile_size(),
                    location,
                    scale,
                    tile_alpha,
                ) {
                    Ok((elem, data)) => {
                        self.window().set_offscreen_data(Some(data));
                        push(elem.into());
                        pushed = true;
                    }
                    Err(err) => {
                        warn!("error rendering window opening animation on Vulkan: {err:?}");
                    }
                }
            }
        } else if let Some(alpha) = &self.alpha_animation {
            // Render the tile into a VkTexture offscreen and composite it at the animated alpha;
            // falls through to the plain render below if the offscreen does not render.
            {
                {
                    let mut vctx = ctx.r();
                    let mut elements = Vec::new();
                    self.render_inner(
                        vctx.r(),
                        Point::new(0., 0.),
                        xray_pos,
                        focus_ring,
                        &mut |elem| elements.push(elem),
                    );
                    match alpha.offscreen_vk.render(vctx.renderer, scale, &elements) {
                        Ok((elem, _sync, data)) => {
                            let offset = elem.offset();
                            let elem = elem.with_alpha(tile_alpha).with_offset(location + offset);

                            self.window().set_offscreen_data(Some(data));
                            push(elem.into());
                            pushed = true;
                        }
                        Err(err) => {
                            warn!(
                                "error rendering tile to Vulkan offscreen for alpha animation: \
                                 {err:?}"
                            );
                        }
                    }
                }
            }
        }

        if !pushed {
            self.render_inner(ctx, location, xray_pos, focus_ring, &mut |elem| push(elem));
        }
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        renderer: SnapshotRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
    ) {
        if self.unmap_snapshot.is_some() {
            return;
        }

        // A failed capture stores nothing: the close animation is skipped (it warned), rather than
        // half-captured. Storing a partial snapshot would let the blocked-out variant fall back to
        // the unblocked contents on a screencast.
        self.unmap_snapshot =
            self.render_snapshot_vulkan(renderer, xray, xray_has_blocked_out_layers, xray_pos);
    }

    pub fn take_unmap_snapshot(&mut self) -> Option<TileUnmapSnapshot> {
        self.unmap_snapshot.take()
    }

    /// Capture the tile's three block-out variants, rasterized into renderer-neutral CPU buffers.
    ///
    /// Returns `None` if any *needed* variant failed to capture, so a partial snapshot can never be
    /// stored: [`NeutralSnapshot::variant`] distinguishes "not needed" from "missing" only because
    /// this holds, and if it didn't, a blocked-out window would fall through to its real contents
    /// on a screencast.
    fn render_snapshot_vulkan(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
        mut xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
    ) -> Option<NeutralSnapshot> {
        let _span = tracy_client::span!("Tile::render_snapshot_vulkan");

        let scale = Scale::from(self.scale);
        let block_out_from = self.window.rules().block_out_from;

        let capture =
            |target: RenderTarget,
             xray: Option<&Xray>,
             renderer: &mut crate::render_helpers::vulkan::VulkanRenderer| {
                let mut elements = Vec::new();
                self.render(
                    RenderCtx {
                        target,
                        renderer,
                        xray,
                        // A snapshot capture cannot name the live appearance — see
                        // `RenderCtx::appearance` for why it must not guess one either.
                        appearance: None,
                    },
                    Point::from((0., 0.)),
                    xray_pos,
                    false,
                    &mut |elem| elements.push(elem),
                );
                rasterize_neutral(renderer, scale, &elements)
            };

        let contents = capture(RenderTarget::Output, xray.as_deref(), renderer)?;

        let mut contents_with_blocked_out_bg = None;

        // Do a bit of pointer surgery on Xray.
        //
        // The idea is to avoid the combinatorial combination of capturing snapshots for target
        // (Output, Screencast) × Xray target (Output, Screencast, ScreenCapture).
        //
        // Our main goals:
        // - Everything must look unblocked for RenderTarget::Output.
        // - If anything is potentially blocked-out, it must not show up on any screen capture.
        //
        // Right above we captured a fully-unblocked snapshot for the Output, so that's covered.
        //
        // Next, *only if Xray has any blocked-out surfaces* (which is a rare case), we will capture
        // a snapshot where the window itself is unblocked, but the Xray background is blocked. To
        // do this, we swap the Output target buffers in Xray with the Screencast target buffers
        // (which were prepared for us higher up the stack).
        //
        // Finally, we capture a fully blocked-out snapshot. If Xray has blocked-out surfaces, then
        // Xray's Screencast buffers are already filled-in, but if not, then we swap in the Output
        // buffers, to avoid an extra render. This is safe since we know there are no blocked
        // surfaces there.
        let output_idx = RenderTarget::Output as usize;
        let screencast_idx = RenderTarget::Screencast as usize;
        let mut screencast_background = None;
        let mut screencast_backdrop = None;
        let mut output_background = None;
        let mut output_backdrop = None;
        if let Some(xray) = &mut xray {
            screencast_background = Some(Rc::clone(&xray.background[screencast_idx]));
            screencast_backdrop = Some(Rc::clone(&xray.backdrop[screencast_idx]));
            output_background = Some(Rc::clone(&xray.background[output_idx]));
            output_backdrop = Some(Rc::clone(&xray.backdrop[output_idx]));

            if xray_has_blocked_out_layers {
                xray.background[output_idx] = screencast_background.clone().unwrap();
                xray.backdrop[output_idx] = screencast_backdrop.clone().unwrap();

                contents_with_blocked_out_bg = capture(RenderTarget::Output, Some(xray), renderer);
            } else {
                xray.background[screencast_idx] = output_background.clone().unwrap();
                xray.backdrop[screencast_idx] = output_backdrop.clone().unwrap();
            }
        }

        // Only bake the blocked-out variant if a target can actually select it: `should_block_out`
        // is never true when `block_out_from` is `None`, so skipping it is safe — and it's the
        // common case, which keeps this to a single render.
        let blocked_out_contents = block_out_from
            .is_some()
            .then(|| capture(RenderTarget::Screencast, xray.as_deref(), renderer))
            .flatten();

        // Put everything back to normal.
        if let Some(xray) = &mut xray {
            if xray_has_blocked_out_layers {
                xray.background[output_idx] = output_background.take().unwrap();
                xray.backdrop[output_idx] = output_backdrop.take().unwrap();
            } else {
                xray.background[screencast_idx] = screencast_background.take().unwrap();
                xray.backdrop[screencast_idx] = screencast_backdrop.take().unwrap();
            }
        }

        // Fail closed: a needed variant that didn't capture throws the whole snapshot away.
        if xray_has_blocked_out_layers && contents_with_blocked_out_bg.is_none() {
            warn!("could not capture the blocked-out-background closing snapshot; skipping it");
            return None;
        }
        if block_out_from.is_some() && blocked_out_contents.is_none() {
            warn!("could not capture the blocked-out closing snapshot; skipping it");
            return None;
        }

        Some(NeutralSnapshot {
            contents,
            contents_with_blocked_out_bg,
            blocked_out_contents,
            block_out_from,
            size: self.animated_tile_size(),
        })
    }

    pub fn border(&self) -> &FocusRing {
        &self.border
    }

    pub fn focus_ring(&self) -> &FocusRing {
        &self.focus_ring
    }

    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        assert_eq!(self.sizing_mode, self.window.sizing_mode());

        let scale = self.scale;
        let size = self.tile_size();
        let rounded = size.to_physical_precise_round(scale).to_logical(scale);
        assert_abs_diff_eq!(size.w, rounded.w, epsilon = 1e-5);
        assert_abs_diff_eq!(size.h, rounded.h, epsilon = 1e-5);
    }
}

/// Render `elements` through the owned Vulkan renderer into a renderer-neutral CPU buffer, together
/// with their encompassing geometry — the same convention `render_to_encompassing_texture` uses for
/// the GLES bake, so a `ClosingWindow` can place either at the same offset.
fn rasterize_neutral<E: smithay::backend::renderer::element::RenderElement<VulkanRenderer>>(
    renderer: &mut VulkanRenderer,
    scale: Scale<f64>,
    elements: &[E],
) -> Option<crate::render_helpers::snapshot::CapturedVariant> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
    use smithay::utils::Transform;

    use crate::render_helpers::{encompassing_geo, render_to_vec};

    let geo = encompassing_geo(scale, elements.iter());
    if geo.size.is_empty() {
        return None;
    }

    let relocated = elements.iter().rev().map(|ele| {
        RelocateRenderElement::from_element(ele, geo.loc.upscale(-1), Relocate::Relative)
    });

    match render_to_vec(
        renderer,
        geo.size,
        scale,
        Transform::Normal,
        Fourcc::Abgr8888,
        relocated,
    ) {
        Ok(data) => {
            let buffer_size = geo.size.to_logical(1).to_buffer(1, Transform::Normal);
            let buffer = MemoryBuffer::new(
                data,
                Fourcc::Abgr8888,
                buffer_size,
                scale,
                Transform::Normal,
            );
            Some((buffer, geo))
        }
        Err(err) => {
            warn!("error capturing closing-window snapshot via Vulkan: {err:?}");
            None
        }
    }
}
