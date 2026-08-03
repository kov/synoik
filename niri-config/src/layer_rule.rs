use crate::appearance::{BackgroundEffectRule, BlockOutFrom, CornerRadius, ShadowRule};
use crate::utils::RegexEq;
use crate::window_rule::PopupsRule;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct LayerRule {
    pub matches: Vec<Match>,
    pub excludes: Vec<Match>,

    pub opacity: Option<f32>,
    pub block_out_from: Option<BlockOutFrom>,
    pub shadow: ShadowRule,
    pub geometry_corner_radius: Option<CornerRadius>,
    pub place_within_backdrop: Option<bool>,
    pub baba_is_float: Option<bool>,
    pub background_effect: BackgroundEffectRule,
    pub popups: PopupsRule,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Match {
    pub namespace: Option<RegexEq>,
    pub at_startup: Option<bool>,
    pub layer: Option<niri_ipc::Layer>,
}
