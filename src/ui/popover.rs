//! Panel popovers: click-anchored popups under a top-panel button.
//!
//! GNOME's panel buttons (dateMenu, quickSettings, …) open a popup menu anchored
//! below the button that grabs input and dismisses on Escape or an outside click.
//! This is the shared mechanism for those; the first (and currently only) content
//! is the [`Calendar`]. Unlike the modal dialogs (run dialog, end-session), a
//! popover draws **no** full-screen dim — it's a floating anchored surface, like a
//! GNOME popup menu — but it *does* grab input while open.
//!
//! Reuses the overlay render pattern (offscreen `VkTexture` → `TextureBuffer` →
//! positioned `TextureRenderElement`, like `run_dialog.rs`). The net-new behavior
//! vs the existing overlays is outside-click dismissal.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture as _;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::calendar::Calendar;
use crate::ui::panel::PANEL_HEIGHT;
use crate::utils::output_size;

/// The content a popover hosts. One variant for now; quick-settings will slot in.
pub enum PopoverContent {
    Calendar(Calendar),
}

impl PopoverContent {
    fn logical_size(&self) -> Size<f64, Logical> {
        match self {
            PopoverContent::Calendar(c) => c.logical_size(),
        }
    }
}

/// A single panel popover, owned on `Niri` alongside the other overlays.
pub struct PanelPopover {
    open: bool,
    /// The output the popover is anchored on (drawn/hit-tested only there).
    output: Option<Output>,
    /// The panel button rect it hangs from, output-local logical.
    anchor: Rectangle<f64, Logical>,
    content: Option<PopoverContent>,
}

impl PanelPopover {
    pub fn new() -> Self {
        Self {
            open: false,
            output: None,
            anchor: Rectangle::default(),
            content: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The output the popover is anchored on, while open.
    pub fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    /// Toggle the dateMenu calendar: open it anchored at `anchor` on `output`, or
    /// close it if it's already open.
    pub fn toggle_calendar(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        week_start: u8,
        show_week_numbers: bool,
        accent: [u8; 3],
    ) {
        if self.open {
            self.close();
            return;
        }
        self.open = true;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::Calendar(Calendar::new(
            week_start,
            show_week_numbers,
            accent,
        )));
    }

    pub fn close(&mut self) {
        self.open = false;
        self.output = None;
        self.content = None;
    }

    /// Feed a key while the popover is open. Escape closes it; every other key is
    /// swallowed (a modal grab, like GNOME popup menus). Returns whether the key
    /// was consumed.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> bool {
        if !self.open {
            return false;
        }
        if pressed && raw == Some(Keysym::Escape) {
            self.close();
        }
        true
    }

    /// Feed a pointer click at output-local logical `pos` on `output`. A click
    /// inside the popover routes to the content; anywhere else (including another
    /// output) closes it. Returns whether the popover consumed the click.
    pub fn pointer_click(&mut self, output: &Output, pos: Point<f64, Logical>) -> bool {
        if !self.open {
            return false;
        }
        if self.output.as_ref() != Some(output) {
            self.close();
            return true;
        }
        let origin = self.location(output);
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let local = pos - origin;
        let inside = local.x >= 0. && local.y >= 0. && local.x < size.w && local.y < size.h;
        if inside {
            if let Some(PopoverContent::Calendar(cal)) = self.content.as_mut() {
                cal.pointer_click(local);
            }
            return true;
        }
        // Outside click — dismiss and consume it (GNOME's grab swallows the click
        // that closes the menu rather than also acting on what's beneath).
        self.close();
        true
    }

    /// The popover's top-left, output-local logical: centered under the anchor,
    /// clamped into the output, just below the panel; snapped to the pixel grid.
    fn location(&self, output: &Output) -> Point<f64, Logical> {
        let scale = output.current_scale().fractional_scale();
        let ow = output_size(output).w;
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let center_x = self.anchor.loc.x + self.anchor.size.w / 2.;
        let x = (center_x - size.w / 2.).clamp(0., (ow - size.w).max(0.));
        Point::from((x, PANEL_HEIGHT))
            .to_physical_precise_round(scale)
            .to_logical(scale)
    }

    /// The popover render element for `output`, or `None` when closed / on another
    /// output / on a draw failure.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
    ) -> Option<TextureRenderElement<VkTexture>> {
        if !self.open || self.output.as_ref() != Some(output) {
            return None;
        }
        let _span = tracy_client::span!("PanelPopover::render");
        let scale = output.current_scale().fractional_scale();

        let texture = match self.content.as_ref()? {
            PopoverContent::Calendar(cal) => match cal.texture(renderer, scale) {
                Ok(tex) => tex,
                Err(err) => {
                    tracing::error!("error drawing the calendar popover: {err:#}");
                    return None;
                }
            },
        };

        let opaque = vec![Rectangle::from_size(texture.size())];
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, opaque);
        let location = self.location(output);
        Some(TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            1.,
            None,
            None,
            Kind::Unspecified,
        ))
    }
}

impl Default for PanelPopover {
    fn default() -> Self {
        Self::new()
    }
}
