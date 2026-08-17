// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use crate::ui::widget::style::Appearance;

/// GNOME's client-blur strength: `BACKGROUND_EFFECT_BLUR_RADIUS`
/// (mutter 51 `src/compositor/meta-surface-actor.c`), in **logical** pixels.
///
/// It is `2σ`, not σ — `clutter_blur_new` opens with `blur->sigma = radius / 2.0f`
/// (`clutter/clutter/clutter-blur.c`), the convention every radius in this compositor carries and
/// the one `BlurChain::record_gaussian` expects. mutter multiplies by the stage view's scale at
/// paint time, so the on-screen blur is the same size whatever the output scale; every caller here
/// owes that multiply too.
///
/// The other radii, for orientation — there is one blur *implementation*, not one radius, because
/// upstream itself uses different ones for different jobs:
///
/// | surface | radius | source |
/// | --- | --- | --- |
/// | client (`ext-background-effect-v1`) | 24 | mutter's `BACKGROUND_EFFECT_BLUR_RADIUS` |
/// | xray effect buffer | 24 | stands in for exactly that backdrop |
/// | panel plate, dash pill | 30 | Blur my Shell's default ([`crate::ui::panel::BAR_BLUR_RADIUS`]) |
/// | lock wallpaper | 90 | GNOME's `BLUR_RADIUS`, `unlockDialog.js` |
/// | overview backdrop | 90 | ours, `OVERVIEW_BLUR_RADIUS` |
pub const GNOME_CLIENT_BLUR_RADIUS: f64 = 24.0;

/// The postprocess **finish**: what the compositor does to a captured backdrop *after* the blur, to
/// make it read as a material rather than a smeared photograph.
///
/// Grouped rather than passed as four scalars because they are one recipe — macOS's insight, and
/// the thing a "blur strength" slider cannot buy: an app names a *material* (`.sidebar`,
/// `.hudWindow`) and the system owns radius, tint, saturation and grain together, so every surface
/// stays coherent. `ext-background-effect-v1` gives us the same shape by giving the client no
/// parameters at all: it sends a region, and every value here is ours to decide.
///
/// [`Self::NONE`] is the identity and is what the shell's own chrome uses — the panel and dash
/// paint their own `$system_*` fills over the blur, so a tint underneath would only muddy a colour
/// the theme already specifies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finish {
    /// Grain, in output pixels.
    pub noise: f32,
    /// Saturation multiplier; `1.0` is the identity.
    pub saturation: f32,
    /// **Straight-alpha** tint composited over the blurred backdrop; alpha is the strength. The
    /// shader wants it premultiplied — [`Self::tint_premultiplied`] does that conversion, so the
    /// policy above can keep stating colours the way a stylesheet would.
    pub tint: [f32; 4],
    /// Contrast boost about mid-grey; `0.0` is the identity.
    pub contrast: f32,
}

impl Finish {
    /// No finish at all: the blurred backdrop, untouched.
    pub const NONE: Self = Self {
        noise: 0.,
        saturation: 1.,
        tint: [0.; 4],
        contrast: 0.,
    };

    /// The tint in the premultiplied form `PostprocessPush::tint` wants.
    pub fn tint_premultiplied(&self) -> [f32; 4] {
        let a = self.tint[3];
        [self.tint[0] * a, self.tint[1] * a, self.tint[2] * a, a]
    }
}

impl Default for Finish {
    fn default() -> Self {
        Self::NONE
    }
}

/// The client-blur recipe — the compositor's answer to a client that asked for blur and, per the
/// protocol, said nothing else about how it should look.
///
/// **GNOME's answer is "nothing extra".** mutter 51 runs the blur, a saturation boost and a grain,
/// and stops there (`meta_background_effect_paint_blur_region`); the client's own translucent
/// surface is composited over the result and owns its own contrast. So this is `saturation` and
/// `noise` and no more.
///
/// **What was here before, and why it might come back.** A blurred backdrop is whatever happened to
/// be behind the window, at whatever brightness, and the client draws its text over it in colours
/// chosen for its own theme without knowing what it landed on — the argument for a wash pulling the
/// backdrop toward the desktop's appearance, which is what KWin's merged contrast matrix and every
/// macOS material do. We shipped one: a 20%-alpha tint keyed on `color-scheme` (the only signal
/// available, since the protocol carries a region and nothing else) plus a small contrast boost.
/// That is a real divergence from upstream, deliberately taken, and dropping it is a decision to
/// re-take by eye rather than a settled question. The `Finish` still carries `tint` and `contrast`,
/// and `appearance` is still resolved into the effect's options, so re-arming it is one commit.
///
/// `appearance` is unused for the same reason it is still a parameter: what it selects is exactly
/// what upstream declines to do, and the plumbing that keeps a colour-scheme flip repainting every
/// blurred surface (`Options::appearance`) is what a restored tint would need.
pub fn client_finish(appearance: Appearance, noise: f32, saturation: f32) -> Finish {
    let _ = appearance;
    Finish {
        noise,
        saturation,
        ..Finish::NONE
    }
}
