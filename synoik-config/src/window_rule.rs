use synoik_ipc::ColumnDisplay;

use crate::appearance::{
    BackgroundEffect, BackgroundEffectRule, BlockOutFrom, BorderRule, CornerRadius, ShadowRule,
    TabIndicatorRule,
};
use crate::layout::DefaultPresetSize;
use crate::utils::{MergeWith, RegexEq};
use crate::FloatOrInt;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct WindowRule {
    pub matches: Vec<Match>,
    pub excludes: Vec<Match>,

    // Rules applied at initial configure.
    pub default_column_width: Option<DefaultPresetSize>,
    pub default_window_height: Option<DefaultPresetSize>,
    pub open_on_output: Option<String>,
    pub open_on_workspace: Option<String>,
    pub open_maximized: Option<bool>,
    pub open_maximized_to_edges: Option<bool>,
    pub open_fullscreen: Option<bool>,
    pub open_floating: Option<bool>,
    pub open_focused: Option<bool>,

    // Rules applied dynamically.
    pub min_width: Option<u16>,
    pub min_height: Option<u16>,
    pub max_width: Option<u16>,
    pub max_height: Option<u16>,

    pub focus_ring: BorderRule,
    pub border: BorderRule,
    pub shadow: ShadowRule,
    pub tab_indicator: TabIndicatorRule,
    pub draw_border_with_background: Option<bool>,
    pub opacity: Option<f32>,
    pub geometry_corner_radius: Option<CornerRadius>,
    pub clip_to_geometry: Option<bool>,
    pub baba_is_float: Option<bool>,
    pub block_out_from: Option<BlockOutFrom>,
    pub variable_refresh_rate: Option<bool>,
    pub default_column_display: Option<ColumnDisplay>,
    pub default_floating_position: Option<FloatingPosition>,
    pub scroll_factor: Option<FloatOrInt<0, 100>>,
    pub tiled_state: Option<bool>,
    pub background_effect: BackgroundEffectRule,
    pub popups: PopupsRule,
}

/// Rules for popup surfaces.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PopupsRule {
    pub opacity: Option<f32>,
    pub geometry_corner_radius: Option<CornerRadius>,
    pub background_effect: BackgroundEffectRule,
}

/// Resolved popup-specific rules.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ResolvedPopupsRules {
    /// Extra opacity to draw popups with.
    pub opacity: Option<f32>,

    /// Corner radius to assume the popups have.
    pub geometry_corner_radius: Option<CornerRadius>,

    /// Background effect configuration for popups.
    pub background_effect: BackgroundEffect,
}

impl MergeWith<PopupsRule> for ResolvedPopupsRules {
    fn merge_with(&mut self, part: &PopupsRule) {
        if let Some(x) = part.opacity {
            self.opacity = Some(x);
        }
        if let Some(x) = part.geometry_corner_radius {
            self.geometry_corner_radius = Some(x);
        }
        self.background_effect.merge_with(&part.background_effect);
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Match {
    pub app_id: Option<RegexEq>,
    pub title: Option<RegexEq>,
    pub is_active: Option<bool>,
    pub is_focused: Option<bool>,
    pub is_active_in_column: Option<bool>,
    pub is_floating: Option<bool>,
    pub is_window_cast_target: Option<bool>,
    pub is_urgent: Option<bool>,
    pub at_startup: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingPosition {
    pub x: FloatOrInt<-65535, 65535>,
    pub y: FloatOrInt<-65535, 65535>,
    pub relative_to: RelativeTo,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RelativeTo {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
    Left,
    Right,
}
