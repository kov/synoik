// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The screen magnifier — gnome-shell's `Magnifier` and `ZoomRegion` (`js/ui/magnifier.js`),
//! turned on by `org.gnome.desktop.a11y.applications screen-magnifier-enabled` and configured by
//! `org.gnome.desktop.a11y.magnifier`.
//!
//! A zoom region shows the screen magnified by `mag-factor` in a *viewport*: the whole screen,
//! or one half of it (`screen-position`), optionally moving with the pointer (`lens-mode`). What
//! it shows is the *region of interest* around a centre that follows the pointer the way
//! `mouse-tracking` says. The view is drawn, not hit: the pointer and every click stay where they
//! are unmagnified, and the magnified pointer is drawn where the zoom puts that spot.
//!
//! **Divergence: one region per display.** GNOME's region magnifies the whole stage as one
//! surface. Here each display magnifies its own content in its own coordinates; the display under
//! the pointer follows it, and the others keep their last view.

use std::collections::HashMap;
use std::time::Duration;

use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Color32F;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

use crate::animation::{Animation, Clock, Curve};
use crate::render_helpers::off_plane::OffPlaneRenderElement;
use crate::render_helpers::offscreen::OffscreenBuffer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::RenderCtx;
use crate::synoik::{OutputRenderElements, State, Synoik};
use crate::ui::widget::{style, Rgba};
use crate::utils::timers::TimerToken;

/// How long the pointer must rest before focus and caret moves may scroll the view
/// (`POINTER_REST_TIME`).
const POINTER_REST_TIME: Duration = Duration::from_millis(1000);

/// `_updateCloneGeometry`'s ease, for a factor change or a scroll to a point.
const EASE_MS: u64 = 100;

/// `.magnifier-zoom-region { border: 2px solid -st-accent-color; &.full-screen { border-width: 0 }
/// }` (`_a11y.scss:20-24`).
pub const BORDER_WIDTH: f64 = 2.;

/// `DEFAULT_BACKGROUND_COLOR` (`background.js:109`): the `SystemBackground` behind the clone,
/// seen where the view scrolls past the screen's edge.
pub const BACKGROUND: Rgba = [40. / 255., 40. / 255., 40. / 255., 1.];

/// `GDesktopMagnifierScreenPosition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenPosition {
    None,
    #[default]
    FullScreen,
    TopHalf,
    BottomHalf,
    LeftHalf,
    RightHalf,
}

impl ScreenPosition {
    pub fn from_nick(nick: &str) -> Self {
        match nick {
            "none" => Self::None,
            "top-half" => Self::TopHalf,
            "bottom-half" => Self::BottomHalf,
            "left-half" => Self::LeftHalf,
            "right-half" => Self::RightHalf,
            _ => Self::FullScreen,
        }
    }
}

/// `GDesktopMagnifier{Mouse,Focus,Caret}TrackingMode` — the three share their nicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    None,
    Centered,
    Proportional,
    Push,
}

impl Tracking {
    pub fn from_nick(nick: &str) -> Self {
        match nick {
            "centered" => Self::Centered,
            "proportional" => Self::Proportional,
            "push" => Self::Push,
            _ => Self::None,
        }
    }
}

/// `org.gnome.desktop.a11y.magnifier`, with the schema's defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MagnifierSettings {
    /// Rounded to two decimals, as `_updateMagFactor` reads it.
    pub mag_factor: f64,
    pub screen_position: ScreenPosition,
    pub lens_mode: bool,
    /// `scroll-at-edges`: true lets the view scroll past the screen's edge. The region clamps
    /// when it is *false* (`setClampScrollingAtEdges(!scroll-at-edges)`).
    pub scroll_at_edges: bool,
    pub mouse_tracking: Tracking,
    pub focus_tracking: Tracking,
    pub caret_tracking: Tracking,
    pub invert_lightness: bool,
    pub color_saturation: f64,
    pub brightness: [f64; 3],
    pub contrast: [f64; 3],
    pub show_crosshairs: bool,
    pub crosshairs_thickness: f64,
    /// Straight alpha, from `cross-hairs-color`.
    pub crosshairs_color: Rgba,
    pub crosshairs_opacity: f64,
    pub crosshairs_length: f64,
    pub crosshairs_clip: bool,
}

impl Default for MagnifierSettings {
    fn default() -> Self {
        Self {
            mag_factor: 2.,
            screen_position: ScreenPosition::FullScreen,
            lens_mode: false,
            scroll_at_edges: false,
            mouse_tracking: Tracking::Proportional,
            focus_tracking: Tracking::Proportional,
            caret_tracking: Tracking::Centered,
            invert_lightness: false,
            color_saturation: 1.,
            brightness: [0.; 3],
            contrast: [0.; 3],
            show_crosshairs: false,
            crosshairs_thickness: 8.,
            crosshairs_color: [1., 0., 0., 1.],
            crosshairs_opacity: 0.66,
            crosshairs_length: 4096.,
            crosshairs_clip: false,
        }
    }
}

/// A `cross-hairs-color` string as `Cogl.Color.from_string` takes the hex forms: `#rgb`,
/// `#rgba`, `#rrggbb`, `#rrggbbaa`. Anything else is the schema's red.
pub fn parse_color(s: &str) -> Rgba {
    let red = [1., 0., 0., 1.];
    let Some(hex) = s.strip_prefix('#') else {
        return red;
    };
    let digit = |c: char| c.to_digit(16).map(|d| d as f32);
    let chars: Vec<char> = hex.chars().collect();
    let channels: Option<Vec<f32>> = match chars.len() {
        3 | 4 => chars
            .iter()
            .map(|&c| digit(c).map(|d| d * 17. / 255.))
            .collect(),
        6 | 8 => chars
            .chunks(2)
            .map(|p| Some((digit(p[0])? * 16. + digit(p[1])?) / 255.))
            .collect(),
        _ => None,
    };
    match channels.as_deref() {
        Some([r, g, b]) => [*r, *g, *b, 1.],
        Some([r, g, b, a]) => [*r, *g, *b, *a],
        _ => red,
    }
}

/// Where a region's content sits: a point `p` of the unmagnified screen is drawn at
/// `origin + p * mag` (`_screenToViewPort`, with the clone's rounded position folded into
/// `origin`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomTransform {
    pub origin: Point<f64, Logical>,
    pub mag: f64,
}

impl ZoomTransform {
    pub fn apply(&self, p: Point<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            self.origin.x + p.x * self.mag,
            self.origin.y + p.y * self.mag,
        ))
    }

    fn lerp(&self, to: &Self, t: f64) -> Self {
        let l = |a: f64, b: f64| a + (b - a) * t;
        Self {
            origin: Point::from((l(self.origin.x, to.origin.x), l(self.origin.y, to.origin.y))),
            mag: l(self.mag, to.mag),
        }
    }
}

/// What a frame draws for one region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomView {
    /// The `magnifier-zoom-region` box, border included.
    pub viewport: Rectangle<f64, Logical>,
    /// Inside the border: what the magnified content is clipped to.
    pub content: Rectangle<f64, Logical>,
    pub transform: ZoomTransform,
    /// `_isFullScreen`: the normal screen is hidden behind it (`uiGroup.set_opacity(0)`).
    pub full_screen: bool,
    /// The pointer is over the viewport: the system pointer is hidden and the magnified one is
    /// drawn (`hideSystemCursor`).
    pub pointer_over: bool,
}

#[derive(Debug)]
struct Ease {
    from: ZoomTransform,
    anim: Animation,
}

/// One `ZoomRegion`, in the coordinates of the display it magnifies.
#[derive(Debug)]
pub struct ZoomRegion {
    screen: Size<f64, Logical>,
    mouse_tracking: Tracking,
    clamp: bool,
    lens_mode: bool,
    screen_position: ScreenPosition,
    viewport: Rectangle<f64, Logical>,
    center: Point<f64, Logical>,
    mag: f64,
    following_cursor: bool,
    delayed: Option<Point<f64, Logical>>,
    /// The pointer, local to this display; `None` while it is on another.
    mouse: Option<Point<f64, Logical>>,
    /// The pointer image's logical size (`_mouseSourceActor.get_size()`), for push tracking.
    cursor_size: f64,
    target: ZoomTransform,
    ease: Option<Ease>,
}

impl ZoomRegion {
    /// A region set up as `_settingsInit` sets the first one up.
    pub fn new(
        screen: Size<f64, Logical>,
        settings: &MagnifierSettings,
        cursor_size: f64,
        clock: &Clock,
    ) -> Self {
        let mut region = Self {
            screen,
            mouse_tracking: Tracking::None,
            clamp: false,
            lens_mode: false,
            screen_position: ScreenPosition::FullScreen,
            viewport: Rectangle::from_size(screen),
            center: Point::from((screen.w / 2., screen.h / 2.)),
            mag: 1.,
            following_cursor: false,
            delayed: None,
            mouse: None,
            cursor_size,
            target: ZoomTransform {
                origin: Point::from((0., 0.)),
                mag: 1.,
            },
            ease: None,
        };
        // No ease for the first view: nothing was on screen to ease from.
        region.set_mag_factor(settings.mag_factor, clock);
        if settings.screen_position != ScreenPosition::None {
            region.set_screen_position(settings.screen_position, clock);
        }
        region.set_lens_mode(settings.lens_mode, clock);
        region.set_clamp_scrolling_at_edges(!settings.scroll_at_edges, clock);
        region.set_mouse_tracking_mode(settings.mouse_tracking);
        region.ease = None;
        region
    }

    pub fn screen(&self) -> Size<f64, Logical> {
        self.screen
    }

    pub fn mag_factor(&self) -> f64 {
        self.mag
    }

    pub fn center(&self) -> Point<f64, Logical> {
        self.center
    }

    pub fn viewport(&self) -> Rectangle<f64, Logical> {
        self.viewport
    }

    pub fn following_cursor(&self) -> bool {
        self.following_cursor
    }

    pub fn easing(&self) -> bool {
        self.ease.as_ref().is_some_and(|e| !e.anim.is_done())
    }

    pub fn set_cursor_size(&mut self, size: f64) {
        self.cursor_size = size;
    }

    /// `_monitorsChanged`: the display's size changed.
    pub fn set_screen(&mut self, screen: Size<f64, Logical>, clock: &Clock) {
        self.screen = screen;
        self.update_screen_position(clock);
    }

    /// `setMagFactor`.
    pub fn set_mag_factor(&mut self, mag: f64, clock: &Clock) {
        self.change_roi(
            RoiChange {
                mag: Some(mag),
                redo_cursor_tracking: self.following_cursor,
                animate: true,
                ..RoiChange::default()
            },
            clock,
        );
    }

    /// `setMouseTrackingMode`.
    pub fn set_mouse_tracking_mode(&mut self, mode: Tracking) {
        self.mouse_tracking = mode;
    }

    /// `setLensMode`.
    pub fn set_lens_mode(&mut self, lens_mode: bool, clock: &Clock) {
        self.lens_mode = lens_mode;
        if !lens_mode {
            self.set_screen_position(self.screen_position, clock);
        }
    }

    /// `setClampScrollingAtEdges`.
    pub fn set_clamp_scrolling_at_edges(&mut self, clamp: bool, clock: &Clock) {
        self.clamp = clamp;
        if clamp {
            self.change_roi(RoiChange::default(), clock);
        }
    }

    /// `setScreenPosition`, and the `set*Half` / `setFullScreenMode` it picks from.
    pub fn set_screen_position(&mut self, position: ScreenPosition, clock: &Clock) {
        let (w, h) = (self.screen.w, self.screen.h);
        let viewport = match position {
            ScreenPosition::None => return,
            ScreenPosition::FullScreen => Rectangle::from_size(self.screen),
            ScreenPosition::TopHalf => Rectangle::new((0., 0.).into(), (w, h / 2.).into()),
            ScreenPosition::BottomHalf => Rectangle::new((0., h / 2.).into(), (w, h / 2.).into()),
            ScreenPosition::LeftHalf => Rectangle::new((0., 0.).into(), (w / 2., h).into()),
            ScreenPosition::RightHalf => Rectangle::new((w / 2., 0.).into(), (w / 2., h).into()),
        };
        self.set_view_port(viewport, false, clock);
        self.screen_position = position;
    }

    /// `_updateScreenPosition`.
    fn update_screen_position(&mut self, clock: &Clock) {
        if self.screen_position == ScreenPosition::None {
            self.set_view_port(self.viewport, false, clock);
        } else {
            self.set_screen_position(self.screen_position, clock);
        }
    }

    /// `_setViewPort`.
    fn set_view_port(
        &mut self,
        viewport: Rectangle<f64, Logical>,
        from_roi_update: bool,
        clock: &Clock,
    ) {
        let width = viewport.size.w.min(self.screen.w).round();
        let height = viewport.size.h.min(self.screen.h).round();
        let x = viewport.loc.x.max(0.).min(self.screen.w - width).round();
        let y = viewport.loc.y.max(0.).min(self.screen.h - height).round();
        self.viewport = Rectangle::new((x, y).into(), (width, height).into());

        if !from_roi_update {
            self.change_roi(
                RoiChange {
                    redo_cursor_tracking: self.following_cursor,
                    ..RoiChange::default()
                },
                clock,
            );
        }
    }

    /// `getROI`: the part of the screen the view shows.
    pub fn roi(&self) -> Rectangle<f64, Logical> {
        let w = self.viewport.size.w / self.mag;
        let h = self.viewport.size.h / self.mag;
        Rectangle::new(
            (self.center.x - w / 2., self.center.y - h / 2.).into(),
            (w, h).into(),
        )
    }

    /// `_isFullScreen`: by geometry, not by `screen-position`.
    pub fn is_full_screen(&self) -> bool {
        self.viewport == Rectangle::from_size(self.screen)
    }

    /// `_isMouseOverRegion`.
    pub fn mouse_over(&self) -> bool {
        self.mouse.is_some_and(|m| {
            let vp = self.viewport;
            m.x >= vp.loc.x
                && m.x < vp.loc.x + vp.size.w
                && m.y >= vp.loc.y
                && m.y < vp.loc.y + vp.size.h
        })
    }

    /// `scrollToMousePos`: the pointer moved to `mouse` (local to this display). Returns whether
    /// it is over the view. The caller restarts the rest timer, which ends in
    /// [`Self::pointer_rested`].
    pub fn scroll_to_mouse_pos(&mut self, mouse: Point<f64, Logical>, clock: &Clock) -> bool {
        self.mouse = Some(mouse);
        self.following_cursor = true;
        if self.mouse_tracking != Tracking::None {
            self.change_roi(
                RoiChange {
                    redo_cursor_tracking: true,
                    ..RoiChange::default()
                },
                clock,
            );
        }
        self.mouse_over()
    }

    /// The pointer left this display.
    pub fn pointer_left(&mut self) {
        self.mouse = None;
    }

    /// `POINTER_REST_TIME` after the last move: a focus or caret move that waited is applied.
    pub fn pointer_rested(&mut self, clock: &Clock) {
        self.following_cursor = false;
        if let Some(p) = self.delayed.take() {
            self.scroll_contents_to_delayed(p, clock);
        }
    }

    /// `_scrollContentsToDelayed`.
    pub fn scroll_contents_to_delayed(&mut self, p: Point<f64, Logical>, clock: &Clock) {
        if self.following_cursor {
            self.delayed = Some(p);
        } else {
            self.scroll_contents_to(p, clock);
        }
    }

    /// `scrollContentsTo`: centre the view on `p`, easing there.
    pub fn scroll_contents_to(&mut self, p: Point<f64, Logical>, clock: &Clock) {
        if p.x < 0. || p.x > self.screen.w || p.y < 0. || p.y > self.screen.h {
            return;
        }
        self.following_cursor = false;
        self.change_roi(
            RoiChange {
                center: Some(p),
                animate: true,
                ..RoiChange::default()
            },
            clock,
        );
    }

    /// `_changeROI`.
    fn change_roi(&mut self, change: RoiChange, clock: &Clock) {
        if let Some(mag) = change.mag.filter(|m| *m > 0.) {
            self.mag = mag;
        }
        let mut center = change.center.unwrap_or(self.center);

        if change.redo_cursor_tracking && self.mouse_tracking != Tracking::None {
            if let Some(mouse) = self.mouse {
                center = self.center_from_point(self.mouse_tracking, mouse);
            }
        }

        if self.clamp {
            let roi_w = self.viewport.size.w / self.mag;
            let roi_h = self.viewport.size.h / self.mag;
            center.x = center.x.min(self.screen.w - roi_w / 2.).max(roi_w / 2.);
            center.y = center.y.min(self.screen.h - roi_h / 2.).max(roi_h / 2.);
        }
        self.center = center;

        // A lens moves the view to sit over what it shows; a full-screen view has nowhere to go.
        if self.lens_mode && !self.is_full_screen() {
            let vp = self.viewport;
            self.set_view_port(
                Rectangle::new(
                    (center.x - vp.size.w / 2., center.y - vp.size.h / 2.).into(),
                    vp.size,
                ),
                true,
                clock,
            );
        }

        self.update_clone_geometry(change.animate, clock);
    }

    /// `_updateCloneGeometry`: where the clone goes, eased over 100 ms when asked.
    fn update_clone_geometry(&mut self, animate: bool, clock: &Clock) {
        let (x, y) = self.screen_to_view_port(Point::from((0., 0.)));
        let target = ZoomTransform {
            origin: Point::from((x.round(), y.round())),
            mag: self.mag,
        };
        if animate && target != self.target {
            let from = self.shown();
            self.ease = Some(Ease {
                from,
                anim: Animation::ease(clock.clone(), 0., 1., 0., EASE_MS, Curve::EaseOutQuad),
            });
        } else if !animate {
            self.ease = None;
        }
        self.target = target;
    }

    /// `_screenToViewPort`, relative to the viewport's origin.
    fn screen_to_view_port(&self, p: Point<f64, Logical>) -> (f64, f64) {
        (
            self.viewport.size.w / 2. + (p.x - self.center.x) * self.mag,
            self.viewport.size.h / 2. + (p.y - self.center.y) * self.mag,
        )
    }

    /// The clone's transform as it is drawn now, mid-ease or not, relative to the viewport.
    fn shown(&self) -> ZoomTransform {
        match &self.ease {
            Some(ease) if !ease.anim.is_done() => {
                ease.from.lerp(&self.target, ease.anim.clamped_value())
            }
            _ => self.target,
        }
    }

    /// `_centerFromPoint{Proportional,Push,Centered}`.
    fn center_from_point(&self, mode: Tracking, p: Point<f64, Logical>) -> Point<f64, Logical> {
        let roi = self.roi();
        match mode {
            Tracking::None | Tracking::Centered => p,
            Tracking::Proportional => {
                let half_w = self.screen.w / 2.;
                let half_h = self.screen.h / 2.;
                // A constant padding after zooming, so divided by the factor.
                let padding = self.viewport.size.w.min(self.viewport.size.h) / 5. / self.mag;
                let x_proportion = (p.x - half_w) / half_w;
                let y_proportion = (p.y - half_h) / half_h;
                Point::from((
                    p.x - x_proportion * (roi.size.w / 2. - padding),
                    p.y - y_proportion * (roi.size.h / 2. - padding),
                ))
            }
            Tracking::Push => {
                let mut x = roi.loc.x + roi.size.w / 2.;
                let mut y = roi.loc.y + roi.size.h / 2.;
                let right = roi.loc.x + roi.size.w - self.cursor_size;
                let bottom = roi.loc.y + roi.size.h - self.cursor_size;
                if p.x < roi.loc.x {
                    x -= roi.loc.x - p.x;
                } else if p.x > right {
                    x += p.x - right;
                }
                if p.y < roi.loc.y {
                    y -= roi.loc.y - p.y;
                } else if p.y > bottom {
                    y += p.y - bottom;
                }
                Point::from((x, y))
            }
        }
    }

    /// What to draw.
    pub fn view(&self) -> ZoomView {
        let full_screen = self.is_full_screen();
        let border = if full_screen { 0. } else { BORDER_WIDTH };
        let vp = self.viewport;
        let content = Rectangle::new(
            (vp.loc.x + border, vp.loc.y + border).into(),
            (vp.size.w - 2. * border, vp.size.h - 2. * border).into(),
        );
        let shown = self.shown();
        ZoomView {
            viewport: vp,
            content,
            // The clone and the mouse are children of the group inside the border.
            transform: ZoomTransform {
                origin: Point::from((
                    content.loc.x + shown.origin.x,
                    content.loc.y + shown.origin.y,
                )),
                mag: shown.mag,
            },
            full_screen,
            pointer_over: self.mouse_over(),
        }
    }
}

/// What one display's magnified frame is drawn with, kept across frames so the damage tracker
/// sees the same elements: the frame it magnifies, and the solids around it.
#[derive(Debug)]
pub struct MagnifierSurface {
    offscreen: OffscreenBuffer,
    background: SolidColorBuffer,
    /// Top, bottom, left, right.
    border: [SolidColorBuffer; 4],
}

impl Default for MagnifierSurface {
    fn default() -> Self {
        let solid = || SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.]);
        Self {
            offscreen: OffscreenBuffer::default(),
            background: solid(),
            border: [solid(), solid(), solid(), solid()],
        }
    }
}

/// A straight-alpha colour as the premultiplied one the solids draw.
fn premultiplied(c: Rgba) -> Color32F {
    Color32F::new(c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3])
}

impl Synoik {
    /// The frame of a display the magnifier is on, in `_createActors` order from the top: the
    /// magnified pointer, the magnified screen, the `SystemBackground` behind it, the region's
    /// border; and under all that, unless the view covers the screen, the screen as it is.
    pub(crate) fn render_magnified(
        &self,
        mut ctx: RenderCtx,
        output: &Output,
        include_pointer: bool,
        view: ZoomView,
        push: &mut dyn FnMut(OutputRenderElements),
    ) {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let mut scene = Vec::new();
        self.render_inner(ctx.r(), output, false, &mut |elem| scene.push(elem));

        let content = view.content.to_physical_precise_round(scale);
        let pointer_here = include_pointer
            && self.pointer_visibility.is_visible()
            && self.output_under_cursor().as_ref() == Some(output);
        let mouse = self.pointer_position_on(output);
        let mag = view.transform.mag;

        if pointer_here && view.pointer_over {
            // `_mouseActor`: the pointer image scaled by the factor about its hotspot, at the
            // spot the zoom draws the pointer's position.
            let origin = mouse.to_physical_precise_round(scale);
            let shift = view.transform.apply(mouse).to_physical_precise_round(scale) - origin;
            self.render_pointer(ctx.renderer, output, &mut |elem| {
                let elem = RescaleRenderElement::from_element(elem, origin, mag);
                let elem = RelocateRenderElement::from_element(elem, shift, Relocate::Relative);
                if let Some(elem) = CropRenderElement::from_element(elem, scale, content) {
                    push(OffPlaneRenderElement(elem).into());
                }
            });
        }

        let mut surfaces = self.magnifier_surfaces.borrow_mut();
        let surface = surfaces.entry(output.name()).or_default();
        let magnified = match surface.offscreen.render(ctx.renderer, scale, &scene) {
            Ok((elem, _sync, _data)) => {
                let elem = RescaleRenderElement::from_element(elem, (0, 0).into(), mag);
                let origin = view.transform.origin.to_physical_precise_round(scale);
                let elem = RelocateRenderElement::from_element(elem, origin, Relocate::Relative);
                CropRenderElement::from_element(elem, scale, content).map(|elem| {
                    push(elem.into());
                })
            }
            Err(err) => {
                warn!("error magnifying {}: {err:?}", output.name());
                None
            }
        };

        surface
            .background
            .update(view.content.size, premultiplied(BACKGROUND));
        push(
            SolidColorRenderElement::from_buffer(
                &surface.background,
                view.content.loc,
                1.,
                Kind::Unspecified,
            )
            .into(),
        );

        if !view.full_screen {
            let accent = style::accent_rgba(self.gnome_settings.accent_color);
            let vp = view.viewport;
            let b = BORDER_WIDTH;
            let sides: [Rectangle<f64, Logical>; 4] = [
                Rectangle::new(vp.loc, (vp.size.w, b).into()),
                Rectangle::new(
                    (vp.loc.x, vp.loc.y + vp.size.h - b).into(),
                    (vp.size.w, b).into(),
                ),
                Rectangle::new(
                    (vp.loc.x, vp.loc.y + b).into(),
                    (b, vp.size.h - 2. * b).into(),
                ),
                Rectangle::new(
                    (vp.loc.x + vp.size.w - b, vp.loc.y + b).into(),
                    (b, vp.size.h - 2. * b).into(),
                ),
            ];
            for (buffer, side) in surface.border.iter_mut().zip(sides) {
                buffer.update(side.size, premultiplied(accent));
                push(
                    SolidColorRenderElement::from_buffer(buffer, side.loc, 1., Kind::Unspecified)
                        .into(),
                );
            }
        }
        drop(surfaces);

        // The screen as it is shows around a view that does not cover it — and is all there is if
        // the magnified frame could not be drawn.
        if !view.full_screen || magnified.is_none() {
            if pointer_here && !view.pointer_over {
                self.render_pointer(ctx.renderer, output, &mut |elem| push(elem.into()));
            }
            for elem in scene {
                push(elem);
            }
        }
    }
}

#[derive(Debug, Default)]
struct RoiChange {
    mag: Option<f64>,
    center: Option<Point<f64, Logical>>,
    redo_cursor_tracking: bool,
    animate: bool,
}

/// The magnifier: its settings and a region per display while it is on.
#[derive(Debug, Default)]
pub struct Magnifier {
    pub active: bool,
    pub settings: MagnifierSettings,
    /// By output name.
    regions: HashMap<String, ZoomRegion>,
    rest_timer: Option<TimerToken>,
}

impl Magnifier {
    pub fn region(&self, output: &Output) -> Option<&ZoomRegion> {
        if !self.active {
            return None;
        }
        self.regions.get(&output.name())
    }

    /// The view `output` draws, or `None` while the magnifier is off.
    pub fn view(&self, output: &Output) -> Option<ZoomView> {
        self.region(output).map(ZoomRegion::view)
    }

    pub fn easing(&self) -> bool {
        self.active && self.regions.values().any(ZoomRegion::easing)
    }
}

impl Synoik {
    /// Keep a region for every display while the magnifier is on, sized to it.
    fn sync_magnifier_regions(&mut self) {
        let mag = &mut self.magnifier;
        if !mag.active {
            mag.regions.clear();
            return;
        }
        let cursor_size = f64::from(self.gnome_settings.cursor_size);
        let clock = self.clock.clone();
        let outputs: Vec<(String, Size<f64, Logical>)> = self
            .global_space
            .outputs()
            .filter_map(|o| {
                let geo = self.global_space.output_geometry(o)?;
                Some((o.name(), geo.size.to_f64()))
            })
            .collect();
        mag.regions
            .retain(|name, _| outputs.iter().any(|(n, _)| n == name));
        for (name, size) in outputs {
            match mag.regions.get_mut(&name) {
                Some(region) => {
                    region.set_cursor_size(cursor_size);
                    if region.screen() != size {
                        region.set_screen(size, &clock);
                    }
                }
                None => {
                    let region = ZoomRegion::new(size, &mag.settings, cursor_size, &clock);
                    mag.regions.insert(name, region);
                }
            }
        }
    }
}

impl State {
    /// Follow `org.gnome.desktop.a11y.applications screen-magnifier-enabled` and
    /// `org.gnome.desktop.a11y.magnifier`, applying what changed the way the shell's settings
    /// handlers do.
    pub fn sync_magnifier(&mut self) {
        let active = self
            .synoik
            .gnome_settings
            .a11y
            .get(crate::gnome::A11yToggle::Zoom);
        let settings = self.synoik.gnome_settings.magnifier;
        let clock = self.synoik.clock.clone();
        let mag = &mut self.synoik.magnifier;
        let was_active = mag.active;
        let old = std::mem::replace(&mut mag.settings, settings);
        mag.active = active;

        if active && was_active {
            for region in mag.regions.values_mut() {
                if settings.mag_factor != old.mag_factor {
                    region.set_mag_factor(settings.mag_factor, &clock);
                }
                if settings.screen_position != old.screen_position {
                    region.set_screen_position(settings.screen_position, &clock);
                    if settings.screen_position != ScreenPosition::FullScreen {
                        region.set_lens_mode(settings.lens_mode, &clock);
                    }
                }
                if settings.lens_mode != old.lens_mode {
                    region.set_lens_mode(settings.lens_mode, &clock);
                }
                if settings.scroll_at_edges != old.scroll_at_edges {
                    region.set_clamp_scrolling_at_edges(!settings.scroll_at_edges, &clock);
                }
                if settings.mouse_tracking != old.mouse_tracking {
                    region.set_mouse_tracking_mode(settings.mouse_tracking);
                }
            }
        }

        if active != was_active || active {
            self.synoik.sync_magnifier_regions();
        }
        if !active {
            // The offscreens are a frame's worth of memory per display.
            self.synoik.magnifier_surfaces.borrow_mut().clear();
        }
        if active && !was_active {
            // The view starts on the pointer (`scrollContentsTo` at construction, then
            // `startTrackingMouse`).
            self.magnifier_pointer_moved();
        }
        if active || was_active {
            self.synoik.queue_redraw_all();
        }
    }

    /// The pointer moved: the region under it follows (`scrollToMousePos`), and the rest timer
    /// starts over.
    pub fn magnifier_pointer_moved(&mut self) {
        if !self.synoik.magnifier.active {
            return;
        }
        self.synoik.sync_magnifier_regions();
        let clock = self.synoik.clock.clone();
        let pointer = self.synoik.seat.get_pointer().unwrap().current_location();
        let under = self
            .synoik
            .output_under(pointer)
            .map(|(output, local)| (output.name(), local));
        for (name, region) in &mut self.synoik.magnifier.regions {
            match &under {
                Some((n, local)) if n == name => {
                    region.scroll_to_mouse_pos(*local, &clock);
                }
                _ => region.pointer_left(),
            }
        }

        if let Some(token) = self.synoik.magnifier.rest_timer.take() {
            self.synoik.cancel_timer(token);
        }
        let token = self.synoik.timer_after(POINTER_REST_TIME, |state| {
            let clock = state.synoik.clock.clone();
            let mag = &mut state.synoik.magnifier;
            mag.rest_timer = None;
            for region in mag.regions.values_mut() {
                region.pointer_rested(&clock);
            }
            state.synoik.queue_redraw_all();
            None
        });
        self.synoik.magnifier.rest_timer = Some(token);
        self.synoik.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(settings: MagnifierSettings) -> ZoomRegion {
        ZoomRegion::new(
            Size::from((1000., 800.)),
            &settings,
            24.,
            &Clock::with_time(Duration::ZERO),
        )
    }

    #[test]
    fn colors_parse_like_cogl_hex() {
        assert_eq!(parse_color("#ff0000"), [1., 0., 0., 1.]);
        assert_eq!(parse_color("#00ff0080"), [0., 1., 0., 128. / 255.]);
        assert_eq!(parse_color("#00f"), [0., 0., 1., 1.]);
        assert_eq!(parse_color("blue-ish"), [1., 0., 0., 1.]);
    }

    /// `_centerFromPointProportional`: the pointer sits as far across the view as it is across
    /// the screen, padded by a fifth of the view.
    #[test]
    fn proportional_tracking_keeps_the_pointer_proportionally_placed() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut r = region(MagnifierSettings::default());
        r.scroll_to_mouse_pos(Point::from((500., 400.)), &clock);
        assert_eq!(
            r.center(),
            Point::from((500., 400.)),
            "the middle stays in the middle"
        );

        r.scroll_to_mouse_pos(Point::from((750., 400.)), &clock);
        // roi 500 wide, padding 800/5/2 = 80: 750 - 0.5 * (250 - 80) = 665.
        assert_eq!(r.center().x, 665.);
        // Drawn where the proportion says, inside the padding: 500 + (750 - 665) * 2.
        let t = r.view().transform;
        assert_eq!(t.apply(Point::from((750., 400.))).x, 670.);
    }

    /// `scroll-at-edges` false (the default) clamps; true lets the view run past the edge.
    #[test]
    fn the_view_clamps_at_the_edges_unless_told_to_scroll() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut r = region(MagnifierSettings {
            mouse_tracking: Tracking::Centered,
            ..MagnifierSettings::default()
        });
        r.scroll_to_mouse_pos(Point::from((10., 10.)), &clock);
        assert_eq!(r.center(), Point::from((250., 200.)));

        let mut r = region(MagnifierSettings {
            mouse_tracking: Tracking::Centered,
            scroll_at_edges: true,
            ..MagnifierSettings::default()
        });
        r.scroll_to_mouse_pos(Point::from((10., 10.)), &clock);
        assert_eq!(r.center(), Point::from((10., 10.)));
    }

    #[test]
    fn push_tracking_moves_only_at_the_edge() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut r = region(MagnifierSettings {
            mouse_tracking: Tracking::Push,
            ..MagnifierSettings::default()
        });
        r.scroll_to_mouse_pos(Point::from((500., 400.)), &clock);
        let before = r.center();
        r.scroll_to_mouse_pos(Point::from((600., 450.)), &clock);
        assert_eq!(r.center(), before, "inside the view, the view stays");
        // The roi's right edge is 750, less the 24px cursor: 726.
        r.scroll_to_mouse_pos(Point::from((736., 450.)), &clock);
        assert_eq!(r.center().x, before.x + 10.);
    }

    #[test]
    fn a_half_screen_view_has_a_border_and_tracks_whether_the_pointer_is_over_it() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut r = region(MagnifierSettings {
            screen_position: ScreenPosition::TopHalf,
            ..MagnifierSettings::default()
        });
        r.scroll_to_mouse_pos(Point::from((500., 100.)), &clock);
        let v = r.view();
        assert!(!v.full_screen);
        assert_eq!(
            v.viewport,
            Rectangle::new((0., 0.).into(), (1000., 400.).into())
        );
        assert_eq!(
            v.content,
            Rectangle::new((2., 2.).into(), (996., 396.).into())
        );
        assert!(v.pointer_over);
        r.scroll_to_mouse_pos(Point::from((500., 600.)), &clock);
        assert!(!r.view().pointer_over);
    }

    /// A lens view moves to sit over what it magnifies.
    #[test]
    fn a_lens_follows_the_pointer() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut r = region(MagnifierSettings {
            screen_position: ScreenPosition::LeftHalf,
            lens_mode: true,
            mouse_tracking: Tracking::Centered,
            ..MagnifierSettings::default()
        });
        r.scroll_to_mouse_pos(Point::from((600., 400.)), &clock);
        assert_eq!(r.viewport().loc, Point::from((350., 0.)));
    }

    /// A factor change eases the clone over 100 ms; following the pointer does not.
    #[test]
    fn a_factor_change_eases_and_tracking_does_not() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut r = ZoomRegion::new(
            Size::from((1000., 800.)),
            &MagnifierSettings::default(),
            24.,
            &clock,
        );
        r.scroll_to_mouse_pos(Point::from((500., 400.)), &clock);
        assert!(!r.easing());

        r.set_mag_factor(4., &clock);
        assert!(r.easing());
        let start = r.view().transform.mag;
        assert_eq!(start, 2., "it starts from what was shown");
        clock.set_unadjusted(Duration::from_millis(50));
        let mid = r.view().transform.mag;
        assert!(mid > 2. && mid < 4., "mid-ease: {mid}");
        clock.set_unadjusted(Duration::from_millis(100));
        assert_eq!(r.view().transform.mag, 4.);

        r.scroll_to_mouse_pos(Point::from((520., 400.)), &clock);
        assert!(!r.easing());
    }
}
