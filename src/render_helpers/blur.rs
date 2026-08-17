// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use crate::ui::widget::style::Appearance;

/// How much to blur, straight from the config.
///
/// The blur itself is the renderer's own: [`BackdropBlur`] does the backdrop effect and
/// [`EffectBlur`] the xray effect-buffer, both reading these options. This module used to also hold
/// a GLES dual-Kawase implementation; it went with the rest of the GLES machinery, leaving the
/// options as the only shared vocabulary between the config and the renderer.
///
/// [`BackdropBlur`]: crate::render_helpers::vulkan::BackdropBlur
/// [`EffectBlur`]: crate::render_helpers::vulkan::effect_blur::EffectBlur
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct BlurOptions {
    pub passes: u8,
    pub offset: f64,
}

impl From<synoik_config::Blur> for BlurOptions {
    fn from(config: synoik_config::Blur) -> Self {
        Self {
            passes: config.passes,
            offset: config.offset,
        }
    }
}

/// GNOME's client-blur strength: `BACKGROUND_EFFECT_BLUR_RADIUS`
/// (mutter 51 `src/compositor/meta-surface-actor.c`), in **logical** pixels.
///
/// It is `2σ`, not σ — `clutter_blur_new` opens with `blur->sigma = radius / 2.0f`
/// (`clutter/clutter/clutter-blur.c`), which is the same convention
/// [`BlurRecipe::Gaussian`] carries and the same one `BlurChain::record_gaussian` expects. mutter
/// multiplies by the stage view's scale at paint time, so the on-screen blur is the same size
/// whatever the output scale; the caller owes that multiply here too.
pub const GNOME_CLIENT_BLUR_RADIUS: f64 = 24.0;

/// Which blur to run over a captured backdrop.
///
/// Two algorithms, because they answer to two different owners. The shell's own chrome — the panel
/// plate, the dash pill — keeps niri's dual-Kawase, tuned by hand against the surfaces it draws.
/// Anything a *client* asked to blur through `ext-background-effect-v1` gets GNOME's separable
/// gaussian at GNOME's radius, because as of 51 mutter implements that protocol itself and there is
/// now one right answer to "what does a client that asked for blur get".
///
/// The two are not interchangeable at the cache level: the Kawase chain renders its final upsample
/// straight into the bundle's output (`BlurChain::set_external_dst`) while the gaussian copies out
/// of level 0, so a bundle built for one cannot serve the other. [`Self::kind`] is what keeps them
/// apart in the cache key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlurRecipe {
    /// niri's dual-Kawase: `passes` down/up levels, taps `offset` half-texels apart.
    Kawase { passes: usize, offset: f64 },
    /// GNOME's separable gaussian. `radius` is `2σ` **in the blurred texture's own pixels**, so a
    /// caller whose intermediate is not at output resolution owes the conversion — the same debt
    /// `GaussianBackdrop` documents.
    Gaussian { radius: f64 },
}

/// A blur's cache identity: the algorithm plus how many pyramid rungs it needs. Two bundles with
/// the same [`BlurKind`] and the same intermediate size are interchangeable; two with different
/// kinds are not, whatever their size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlurKind {
    Kawase { passes: usize },
    Gaussian { passes: usize },
}

impl BlurRecipe {
    /// The cache identity for running this recipe over a `width`×`height` intermediate.
    ///
    /// The gaussian's rung count is a function of the radius *and* the texture, because GNOME's
    /// cascade keeps halving until the surviving σ is small enough to be cheap — so unlike the
    /// Kawase's, it cannot be read off the recipe alone.
    pub fn kind(&self, width: u32, height: u32) -> BlurKind {
        match *self {
            Self::Kawase { passes, .. } => BlurKind::Kawase {
                passes: passes.clamp(1, 31),
            },
            Self::Gaussian { radius } => BlurKind::Gaussian {
                // `.max(1)`: the horizontal pass needs a same-sized twin to land in, and only the
                // shrinking levels have one. Mirrors `GaussianBackdrop::new`.
                passes: synoik_vk::blur::downscale_levels(width, height, radius).max(1),
            },
        }
    }

    /// This recipe with its length scaled by `k` — what the intermediate-resolution ladder owes,
    /// since both parameters are in the intermediate's texels and the intermediate is a *resample*
    /// of the region rather than a copy of it.
    pub fn scaled(self, k: f64) -> Self {
        match self {
            Self::Kawase { passes, offset } => Self::Kawase {
                passes,
                offset: offset * k,
            },
            Self::Gaussian { radius } => Self::Gaussian { radius: radius * k },
        }
    }
}

impl From<BlurOptions> for BlurRecipe {
    fn from(options: BlurOptions) -> Self {
        Self::Kawase {
            passes: options.passes as usize,
            offset: options.offset,
        }
    }
}

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

/// The client-blur recipe for an appearance — the compositor's answer to a client that asked for
/// blur and, per the protocol, said nothing else about how it should look.
///
/// **Why a tint at all.** A blurred backdrop is whatever happened to be behind the window, at
/// whatever brightness. The client then draws its own text over it in colours it chose for its own
/// theme, and cannot know what it landed on. Without a wash pulling the backdrop toward the
/// desktop's appearance, light-mode text over a dark backdrop (or the reverse) is simply
/// unreadable, and no amount of blur fixes it — this is the whole of what KWin's merged contrast
/// matrix and every macOS material are for.
///
/// **Why the system appearance is the right signal**, weak as it is: it is the only one we have.
/// `ext-background-effect-v1` carries a region and nothing else, so unlike macOS — where the app
/// names a material and therefore its own light/dark intent — we cannot ask the client. A desktop
/// set to light is overwhelmingly running light apps, so `color-scheme` is the best available
/// proxy, and it is the same key the shell's own plate already follows.
///
/// The alphas are deliberately modest. A client asking for blur asked to be *seen through*; a wash
/// heavy enough to guarantee legibility over any backdrop would override the translucency the
/// client chose, which is the one thing it did get to decide.
pub fn client_finish(appearance: Appearance, noise: f32, saturation: f32) -> Finish {
    let tint = match appearance {
        // Pull toward white so a light client's dark text keeps something to sit on.
        Appearance::Light => [1.0, 1.0, 1.0, 0.20],
        // Not pure black: GNOME's own dark surfaces are a desaturated near-black
        // (`$system_bg_color` is `lighten(#222226, 5%)`), and a true black wash reads as a hole
        // punched in the desktop rather than as a material.
        Appearance::Dark => [0.09, 0.09, 0.106, 0.20],
    };
    Finish {
        noise,
        saturation,
        tint,
        contrast: 0.06,
    }
}
