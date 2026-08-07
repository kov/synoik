// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::sync::{Arc, Mutex};

use smithay::utils::{Logical, Point, Rectangle, Scale};
use smithay::wayland::compositor::{with_states, SurfaceData};
use synoik_config::CornerRadius;
use wayland_server::protocol::wl_surface::WlSurface;

use crate::handlers::background_effect::get_cached_blur_region;
use crate::render_helpers::blur::{client_finish, BlurOptions};
use crate::render_helpers::damage::ExtraDamage;
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::xray::{XrayElement, XrayPos};
use crate::render_helpers::RenderCtx;
use crate::synoik_render_elements;
use crate::ui::widget::style::Appearance;
use crate::utils::region::TransformedRegion;
use crate::utils::surface_geo;

#[derive(Debug)]
pub struct BackgroundEffect {
    nonxray: FramebufferEffect,
    /// Damage when options change.
    damage: ExtraDamage,
    /// Corner radius for clipping.
    ///
    /// Stored here in addition to `RenderParams` to damage when it changes.
    // FIXME: would be good to remove this duplication of radius.
    corner_radius: CornerRadius,
    blur_config: synoik_config::Blur,
    options: Options,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Options {
    pub blur: bool,
    pub xray: bool,
    pub noise: Option<f64>,
    pub saturation: Option<f64>,
    /// Which appearance the client-blur recipe was last resolved for
    /// ([`crate::render_helpers::blur::client_finish`]).
    ///
    /// **Resolved into the options rather than read at draw time on purpose.** `Options` is what
    /// `update_render_elements` compares to decide whether to `damage_all()`, so putting the
    /// appearance here is what makes a color-scheme flip repaint every blurred surface — with no
    /// window damage, no config reload and no explicit invalidation anywhere. Read at draw time it
    /// would change nothing until something else happened to damage the surface, which on a static
    /// desktop is never. `nothing_but_a_color_scheme_flip_redraws_a_blurred_surface` is the guard.
    pub appearance: Appearance,
}

impl Options {
    fn is_visible(&self) -> bool {
        self.xray
            || self.blur
            || self.noise.is_some_and(|x| x > 0.)
            || self.saturation.is_some_and(|x| x != 1.)
    }
}

/// Render-time parameters.
#[derive(Debug)]
pub struct RenderParams {
    /// Geometry of the background effect.
    pub geometry: Rectangle<f64, Logical>,
    /// Effect subregion, will be clipped to `geometry`.
    ///
    /// `subregion.iter()` should return `geometry`-relative rectangles.
    pub subregion: Option<TransformedRegion>,
    /// Geometry and radius for clipping in the same coordinate space as `geometry`.
    pub clip: Option<(Rectangle<f64, Logical>, CornerRadius)>,
    /// Scale to use for rounding to physical pixels.
    pub scale: f64,
}

impl RenderParams {
    fn fit_clip_radius(&mut self) {
        if let Some((geo, radius)) = &mut self.clip {
            // HACK: increase radius to avoid slight bleed on rounded corners.
            *radius = radius.expanded_by(1.);

            *radius = radius.fit_to(geo.size.w as f32, geo.size.h as f32);
        }
    }
}

synoik_render_elements! {
    BackgroundEffectElement => {
        FramebufferEffect = FramebufferEffectElement,
        Xray = XrayElement,
        ExtraDamage = ExtraDamage,
    }
}

impl BackgroundEffect {
    pub fn new() -> Self {
        Self {
            nonxray: FramebufferEffect::new(),
            damage: ExtraDamage::new(),
            corner_radius: CornerRadius::default(),
            blur_config: synoik_config::Blur::default(),
            options: Options::default(),
        }
    }

    /// Damage the background effect, for example when a blur subregion changes.
    pub fn damage(&mut self) {
        self.damage.damage_all();
        self.nonxray.damage();
    }

    pub fn update_config(&mut self, config: synoik_config::Blur) {
        if self.blur_config == config {
            return;
        }

        self.blur_config = config;
        self.damage.damage_all();
        self.nonxray.damage();
    }

    pub fn update_render_elements(
        &mut self,
        corner_radius: CornerRadius,
        effect: synoik_config::BackgroundEffect,
        has_blur_region: bool,
        appearance: Option<Appearance>,
    ) {
        // If the surface explicitly requests a blur region, default blur to true.
        let blur = if has_blur_region {
            effect.blur != Some(false)
        } else {
            effect.blur == Some(true)
        };

        // Blur turned off globally is off for everyone, including surfaces that asked through
        // `ext-background-effect-v1`. Folded in here rather than at render time so a surface whose
        // *only* effect was that blur stops being visible at all: leaving it visible-but-unblurred
        // would punch an unblurred see-through hole where the client asked for blur, which is
        // strictly worse than ignoring the request. `capabilities()` clears the blur bit to match.
        let blur = blur && !self.blur_config.off;

        let mut options = Options {
            blur,
            xray: effect.xray == Some(true),
            noise: effect.noise,
            saturation: effect.saturation,
            // `None` keeps what we were last told — a path that cannot name the live appearance
            // must not vote on it, or the two writers flip the value back and forth and damage
            // every blurred surface every frame. See `RenderCtx::appearance`.
            appearance: appearance.unwrap_or(self.options.appearance),
        };

        // If we have some background effect but xray wasn't explicitly set, default it to true
        // since it's cheaper.
        //
        // Only for effects the *shell* turned on. A client asking through
        // `ext-background-effect-v1` is asking for the real thing — the windows behind it, blurred
        // — and xray cannot give it that: the xray buffer holds only the background layer and the
        // wallpaper (`Synoik::fill_xray_elements`), so a window with another window behind it would
        // show the wallpaper through it. KWin and macOS both blur the true backdrop; so do we.
        if !has_blur_region && options.is_visible() && effect.xray.is_none() {
            options.xray = true;
        }

        if self.options == options && self.corner_radius == corner_radius {
            return;
        }

        self.options = options;
        self.corner_radius = corner_radius;
        self.damage.damage_all();
        self.nonxray.damage();
    }

    pub fn is_visible(&self) -> bool {
        self.options.is_visible()
    }

    pub fn render(
        &self,
        ctx: RenderCtx,
        ns: Option<usize>,
        mut params: RenderParams,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
        if !self.is_visible() {
            return;
        }

        if let Some(clip) = &mut params.clip {
            clip.1 = self.corner_radius;
        }
        params.fit_clip_radius();

        let damage = self.damage.render(params.geometry);

        // Use noise/saturation from options, falling back to blur defaults if blurred, and
        // to no effect if not blurred. `blur_config.off` was already folded into `options.blur`.
        let blur = self.options.blur;
        let blur_options = blur.then_some(BlurOptions::from(self.blur_config));
        let noise = if blur { self.blur_config.noise } else { 0. };
        let noise = self.options.noise.unwrap_or(noise) as f32;
        let saturation = if blur {
            self.blur_config.saturation
        } else {
            1.
        };
        let saturation = self.options.saturation.unwrap_or(saturation) as f32;

        if self.options.xray {
            let Some(xray) = ctx.xray else {
                return;
            };

            push(damage.into());
            xray.render(
                ctx,
                params,
                xray_pos,
                blur,
                noise,
                saturation,
                &mut |elem| push(elem.into()),
            );
        } else {
            // Render non-xray effect.
            // The appearance-aware client recipe: the tint and contrast that make a blurred
            // backdrop something a client's own text can sit on. Only here — the shell's chrome
            // paints its own `$system_*` fill over its blur and passes `Finish::NONE`.
            let finish = client_finish(self.options.appearance, noise, saturation);
            let elem = self.nonxray.render(ns, params, blur_options, finish);
            push(elem.into());
        }
    }
}

fn render_params_for_tile(
    geometry: Rectangle<f64, Logical>,
    scale: f64,
    clip_to_geometry: bool,
    block_out: bool,
    blur_region: Option<Arc<Vec<Rectangle<i32, Logical>>>>,
    surface_geo: Rectangle<f64, Logical>,
    surface_anim_scale: Scale<f64>,
) -> Option<RenderParams> {
    // Effects not requested by the surface itself are drawn to match the geometry.
    let mut clip = true;

    let mut effect_geometry = geometry;
    let mut subregion = None;
    if let Some(rects) = blur_region {
        if rects.is_empty() {
            // Surface has a set, but empty blur region.
            return None;
        } else {
            // If the surface itself requests the effects, apply different defaults.
            clip = clip_to_geometry;

            // Use geometry-shaped blur for blocked-out windows to avoid unintentionally
            // leaking any surface shapes. We render those windows as geometry-shaped solid
            // rectangles anyway.
            if block_out {
                clip = true;
            } else {
                let mut surface_geo = surface_geo.upscale(surface_anim_scale);
                surface_geo.loc += geometry.loc;

                subregion = Some(TransformedRegion {
                    rects,
                    scale: surface_anim_scale,
                    offset: surface_geo.loc,
                });

                surface_geo = surface_geo
                    .to_physical_precise_round(scale)
                    .to_logical(scale);
                effect_geometry = surface_geo;
            }
        }
    }

    // This corner radius is reset to self.corner_radius in render().
    let clip = clip.then_some((geometry, CornerRadius::default()));

    Some(RenderParams {
        geometry: effect_geometry,
        subregion,
        clip,
        scale,
    })
}

/// Test-only record of what [`render_for_tile`] resolved, so a test can watch the two clocks that
/// feed it without reimplementing the resolution.
///
/// The effect's placement is mixed from sources that update at different times: `tile_geometry` is
/// the layout's area for the window, which moves as soon as the compositor decides on a new size,
/// while `surface_geo` and the blur region are the *client's* committed state, which only lands
/// when it acks and attaches. A resize is exactly the interval where those two disagree, and an
/// end-state test cannot see it — both have settled by then.
#[cfg(test)]
pub mod trace {
    use std::cell::RefCell;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct EffectSample {
        /// The layout's area for this window — the compositor's clock.
        pub tile_geometry: Rectangle<f64, Logical>,
        /// The client's committed xdg window geometry — the client's clock.
        pub surface_geo: Rectangle<f64, Logical>,
        /// What the effect actually ended up covering.
        pub effect_geometry: Rectangle<f64, Logical>,
        /// Bounding box of the blur subregion, in the same space as `effect_geometry`.
        pub subregion_bbox: Option<Rectangle<f64, Logical>>,
        /// Which draw path this resolved to: xray (sample the background-only offscreen) or the
        /// real-backdrop framebuffer capture.
        pub xray: bool,
        /// Whether the blur is on at all.
        pub blur: bool,
    }

    /// What the framebuffer capture actually grabbed, as opposed to where the effect was drawn.
    ///
    /// `dst` is the element geometry the damage tracker handed to `capture_framebuffer`; `src` is
    /// the sub-rectangle of the framebuffer that was blitted out of it. If the blur's *content*
    /// trails the window while its geometry tracks correctly, the disagreement has to show up
    /// between these two.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct CaptureSample {
        pub dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
        pub src: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    }

    thread_local! {
        static SAMPLES: RefCell<Vec<EffectSample>> = const { RefCell::new(Vec::new()) };
        static CAPTURES: RefCell<Vec<CaptureSample>> = const { RefCell::new(Vec::new()) };
    }

    /// Drain what has been recorded since the last call.
    pub fn take() -> Vec<EffectSample> {
        SAMPLES.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    pub(super) fn record(sample: EffectSample) {
        SAMPLES.with(|s| s.borrow_mut().push(sample));
    }

    /// Drain the captures recorded since the last call.
    pub fn take_captures() -> Vec<CaptureSample> {
        CAPTURES.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    pub fn record_capture(sample: CaptureSample) {
        CAPTURES.with(|s| s.borrow_mut().push(sample));
    }
}

/// Per-surface background effect stored in its data map.
struct SurfaceBackgroundEffect(Mutex<BackgroundEffect>);

impl SurfaceBackgroundEffect {
    fn get(states: &SurfaceData) -> &Self {
        states
            .data_map
            .get_or_insert(|| SurfaceBackgroundEffect(Mutex::new(BackgroundEffect::new())))
    }
}

pub fn damage_surface(states: &SurfaceData) {
    if let Some(effect) = states.data_map.get::<SurfaceBackgroundEffect>() {
        effect.0.lock().unwrap().damage();
    }
}

// Silence, Clippy
// A Smithay user is talking
#[allow(clippy::too_many_arguments)]
pub fn render_for_tile(
    ctx: RenderCtx,
    ns: Option<usize>,
    geometry: Rectangle<f64, Logical>,
    scale: f64,
    clip_to_geometry: bool,
    surface: &WlSurface,
    surface_off: Point<f64, Logical>,
    surface_anim_scale: Scale<f64>,
    blur_config: synoik_config::Blur,
    radius: CornerRadius,
    effect: synoik_config::BackgroundEffect,
    should_block_out: bool,
    xray_pos: XrayPos,
    push: &mut dyn FnMut(BackgroundEffectElement),
) {
    with_states(surface, |states| {
        render_for_surface(
            ctx,
            ns,
            geometry,
            scale,
            clip_to_geometry,
            states,
            surface_off,
            surface_anim_scale,
            blur_config,
            radius,
            effect,
            should_block_out,
            xray_pos,
            push,
        );
    });
}

/// As [`render_for_tile`], but against a surface's already-borrowed [`SurfaceData`].
///
/// **A surface-tree walk must use this one.** `with_surface_tree_downward` holds a *mutable* lock
/// on each surface's user data while it calls the processor
/// (`smithay/src/wayland/compositor/tree.rs`), so calling `render_for_tile` from inside the walk
/// re-enters that lock on the same surface and deadlocks — no panic, no message, the compositor
/// simply stops. That is how the subsurface hook first behaved.
#[allow(clippy::too_many_arguments)]
pub fn render_for_surface(
    ctx: RenderCtx,
    ns: Option<usize>,
    geometry: Rectangle<f64, Logical>,
    scale: f64,
    clip_to_geometry: bool,
    states: &SurfaceData,
    surface_off: Point<f64, Logical>,
    surface_anim_scale: Scale<f64>,
    blur_config: synoik_config::Blur,
    radius: CornerRadius,
    effect: synoik_config::BackgroundEffect,
    should_block_out: bool,
    xray_pos: XrayPos,
    push: &mut dyn FnMut(BackgroundEffectElement),
) {
    {
        let background_effect = SurfaceBackgroundEffect::get(states);
        let mut background_effect = background_effect.0.lock().unwrap();

        let blur_region = get_cached_blur_region(states);
        let has_blur_region = blur_region.as_ref().is_some_and(|r| !r.is_empty());

        background_effect.update_config(blur_config);
        background_effect.update_render_elements(radius, effect, has_blur_region, ctx.appearance);

        if !background_effect.is_visible() {
            return;
        }

        let mut surface_geo = surface_geo(states).unwrap_or_default().to_f64();
        surface_geo.loc += surface_off;

        let Some(params) = render_params_for_tile(
            geometry,
            scale,
            clip_to_geometry,
            should_block_out,
            blur_region,
            surface_geo,
            surface_anim_scale,
        ) else {
            return;
        };

        #[cfg(test)]
        trace::record(trace::EffectSample {
            tile_geometry: geometry,
            surface_geo,
            effect_geometry: params.geometry,
            xray: background_effect.options.xray,
            blur: background_effect.options.blur,
            subregion_bbox: params.subregion.as_ref().and_then(|region| {
                region
                    .iter()
                    .map(|(a, b)| Rectangle::new(a, (b - a).to_size()))
                    .reduce(|acc, r| {
                        let min = Point::from((acc.loc.x.min(r.loc.x), acc.loc.y.min(r.loc.y)));
                        let max_x = (acc.loc.x + acc.size.w).max(r.loc.x + r.size.w);
                        let max_y = (acc.loc.y + acc.size.h).max(r.loc.y + r.size.h);
                        Rectangle::new(min, (max_x - min.x, max_y - min.y).into())
                    })
            }),
        });

        let xray_pos = xray_pos.offset(params.geometry.loc - geometry.loc);
        background_effect.render(ctx, ns, params, xray_pos, push);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve the options a surface ends up with, through the real entry point.
    fn resolve(
        blur_config: synoik_config::Blur,
        effect: synoik_config::BackgroundEffect,
        has_blur_region: bool,
    ) -> Options {
        let mut bg = BackgroundEffect::new();
        bg.update_config(blur_config);
        bg.update_render_elements(CornerRadius::default(), effect, has_blur_region, None);
        bg.options
    }

    /// A client asking through `ext-background-effect-v1` gets the real backdrop blurred, not the
    /// xray see-through — the xray buffer holds only the wallpaper and the background layer, so a
    /// window stacked over another window would show the wallpaper through it.
    #[test]
    fn a_client_blur_region_blurs_the_real_backdrop() {
        let options = resolve(
            synoik_config::Blur::default(),
            synoik_config::BackgroundEffect::default(),
            true,
        );
        assert!(options.blur);
        assert!(!options.xray, "client-requested blur must not use xray");
    }

    /// ...but a rule that explicitly asks for xray still gets it.
    #[test]
    fn an_explicit_xray_rule_beats_the_client_default() {
        let effect = synoik_config::BackgroundEffect {
            xray: Some(true),
            ..Default::default()
        };
        let options = resolve(synoik_config::Blur::default(), effect, true);
        assert!(options.xray);
    }

    /// An effect the *shell* turns on keeps defaulting to xray, which is the cheap path.
    #[test]
    fn a_shell_requested_effect_still_defaults_to_xray() {
        let effect = synoik_config::BackgroundEffect {
            blur: Some(true),
            ..Default::default()
        };
        let options = resolve(synoik_config::Blur::default(), effect, false);
        assert!(options.blur);
        assert!(options.xray);
    }

    /// Blur off globally means the client's request is ignored outright. It must not degrade into a
    /// visible-but-unblurred effect: that would punch a see-through hole where blur was asked for.
    #[test]
    fn blur_off_ignores_a_client_blur_region_entirely() {
        let blur_config = synoik_config::Blur {
            off: true,
            ..Default::default()
        };
        let options = resolve(
            blur_config,
            synoik_config::BackgroundEffect::default(),
            true,
        );
        assert!(!options.blur);
        assert!(!options.is_visible(), "must render nothing at all");
    }
}
