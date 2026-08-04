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
