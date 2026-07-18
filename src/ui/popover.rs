//! Panel popovers: click-anchored popups under a top-panel button.
//!
//! GNOME's panel buttons (dateMenu, quickSettings, …) open a popup menu anchored
//! below the button that grabs input and dismisses on Escape or an outside click.
//! This is the shared mechanism for those; the contents are the [`Calendar`] and
//! the [`QuickSettings`] menu. Unlike the modal dialogs (run dialog, end-session),
//! a popover draws **no** full-screen dim — it's a floating anchored surface, like
//! a GNOME popup menu — but it *does* grab input while open.
//!
//! Reuses the overlay render pattern (offscreen `VkTexture` → `TextureBuffer` →
//! positioned `TextureRenderElement`, like `run_dialog.rs`). A content type may
//! contribute *several* elements (the quick-settings menu composites its icons on
//! top of its chrome), so [`render`](PanelPopover::render) returns a `Vec`. The
//! net-new behavior vs the existing overlays is outside-click dismissal.

use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::calendar::Calendar;
use crate::ui::panel::PANEL_HEIGHT;
use crate::ui::quick_settings::QuickSettings;
use crate::utils::output_size;

/// The side effect a popover click asks the caller (the input handler) to apply.
/// Keeps the content widgets pure — they never touch gsettings or spawn — while
/// still driving real behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopoverAction {
    /// The click was consumed but has no side effect (e.g. a calendar day, or a
    /// hit on empty menu space). The popover stays open.
    Consumed,
    /// Set `org.gnome.desktop.interface color-scheme` (Dark Style tile).
    SetDarkStyle(bool),
    /// Set the inverse of `org.gnome.desktop.notifications show-banners` (DND).
    SetDoNotDisturb(bool),
    /// Set `org.gnome.settings-daemon.plugins.color night-light-enabled`.
    SetNightLight(bool),
    /// Spawn a command (a system-row button); the popover closes.
    Spawn(Vec<String>),
}

/// The content a popover hosts.
pub enum PopoverContent {
    Calendar(Calendar),
    QuickSettings(QuickSettings),
}

impl PopoverContent {
    fn logical_size(&self) -> Size<f64, Logical> {
        match self {
            PopoverContent::Calendar(c) => c.logical_size(),
            PopoverContent::QuickSettings(qs) => qs.logical_size(),
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
    /// close it if it's already open (from the same button).
    pub fn toggle_calendar(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        week_start: u8,
        show_week_numbers: bool,
        accent: [u8; 3],
    ) {
        if self.is_showing::<CalendarTag>() {
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

    /// Toggle the quick-settings menu, anchored at `anchor` on `output`.
    pub fn toggle_quick_settings(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        toggles: crate::gnome::QuickToggles,
        accent: [u8; 3],
    ) {
        if self.is_showing::<QuickSettingsTag>() {
            self.close();
            return;
        }
        self.open = true;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::QuickSettings(QuickSettings::new(
            toggles, accent,
        )));
    }

    /// Whether the popover is open showing a particular content kind (so a second
    /// click on the *same* button toggles it closed, but clicking a different
    /// panel button swaps content instead of no-op-toggling).
    fn is_showing<T: ContentTag>(&self) -> bool {
        self.open && self.content.as_ref().is_some_and(T::matches)
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
    /// inside the popover routes to the content (returning its action); anywhere
    /// else (including another output) closes it. Returns `None` when the popover
    /// wasn't open (the caller handles the click normally), or `Some(action)` when
    /// it consumed the click.
    pub fn pointer_click(
        &mut self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<PopoverAction> {
        if !self.open {
            return None;
        }
        if self.output.as_ref() != Some(output) {
            self.close();
            return Some(PopoverAction::Consumed);
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
            let action = match self.content.as_mut() {
                Some(PopoverContent::Calendar(cal)) => {
                    cal.pointer_click(local);
                    PopoverAction::Consumed
                }
                Some(PopoverContent::QuickSettings(qs)) => qs.pointer_click(local),
                None => PopoverAction::Consumed,
            };
            // A system-row spawn closes the menu, like GNOME.
            if matches!(action, PopoverAction::Spawn(_)) {
                self.close();
            }
            return Some(action);
        }
        // Outside click — dismiss and consume it (GNOME's grab swallows the click
        // that closes the menu rather than also acting on what's beneath).
        self.close();
        Some(PopoverAction::Consumed)
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

    /// The popover render elements for `output`, or empty when closed / on another
    /// output. `icons` supplies the symbolic icons the quick-settings menu needs.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if !self.open || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        let _span = tracy_client::span!("PanelPopover::render");
        let scale = output.current_scale().fractional_scale();
        let origin = self.location(output);

        match self.content.as_ref() {
            Some(PopoverContent::Calendar(cal)) => match cal.texture(renderer, scale) {
                Ok(texture) => {
                    use smithay::backend::renderer::element::Kind;
                    use smithay::backend::renderer::Texture as _;
                    use smithay::utils::Transform;

                    use crate::render_helpers::texture::TextureBuffer;

                    let opaque = vec![Rectangle::from_size(texture.size())];
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        opaque,
                    );
                    vec![TextureRenderElement::from_texture_buffer(
                        buffer,
                        origin,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    )]
                }
                Err(err) => {
                    tracing::error!("error drawing the calendar popover: {err:#}");
                    Vec::new()
                }
            },
            Some(PopoverContent::QuickSettings(qs)) => qs.render(renderer, icons, scale, origin),
            None => Vec::new(),
        }
    }
}

impl Default for PanelPopover {
    fn default() -> Self {
        Self::new()
    }
}

/// Type-level tags for [`PanelPopover::is_showing`], so the toggle helpers can ask
/// "is *this* content already up?" without a public content discriminant.
trait ContentTag {
    fn matches(content: &PopoverContent) -> bool;
}
struct CalendarTag;
impl ContentTag for CalendarTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::Calendar(_))
    }
}
struct QuickSettingsTag;
impl ContentTag for QuickSettingsTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::QuickSettings(_))
    }
}
