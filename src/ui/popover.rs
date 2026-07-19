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

use std::cell::RefCell;
use std::rc::Rc;

use niri_config::Config;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock};
use crate::render_helpers::icon::IconCache;

/// How far the popover slides in (logical px) as it fades open — gnome-shell's
/// `BoxPointer` `-arrow-rise` (`$base_padding` = 6px). It emerges from `rise` above
/// its resting spot (toward the panel) and settles down, reversing on close.
const POPOVER_RISE: f64 = 6.;

/// Resting gap between the popover and the panel / screen edge, logical px. gnome-shell's
/// `.popup-menu-boxpointer { -arrow-rise: $base_padding }` is documented as the "distance
/// from the panel & screen edge" (6px), so the menu doesn't sit flush against either.
const POPOVER_MARGIN: f64 = 6.;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::calendar::Calendar;
use crate::ui::panel::PANEL_HEIGHT;
use crate::ui::quick_settings::QuickSettings;
use crate::utils::output_size;

/// The side effect a popover click asks the caller (the input handler) to apply.
/// Keeps the content widgets pure — they never touch gsettings or spawn — while
/// still driving real behavior.
#[derive(Debug, Clone, PartialEq)]
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
    /// Set the default sink's perceptual volume `0..=1` (the QS volume slider).
    SetVolume(f64),
    /// Toggle the default sink's mute (clicking the slider's speaker icon).
    ToggleMute,
    /// Open the interactive screenshot UI (the screenshot system button); the
    /// popover closes.
    Screenshot,
    /// Spawn a command (a system-row button / the battery pill); popover closes.
    Spawn(Vec<String>),
}

impl PopoverAction {
    /// Whether applying this action dismisses the menu (GNOME closes quick
    /// settings when a system button is used, but keeps it open for a toggle).
    fn closes_menu(&self) -> bool {
        matches!(self, PopoverAction::Screenshot | PopoverAction::Spawn(_))
    }
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
    /// The shared animation clock, cloned into each open/close [`Animation`].
    clock: Clock,
    /// The live config, read for the popover open/close animation params on each toggle.
    config: Rc<RefCell<Config>>,
    /// The open/close fade progress (0 = hidden, 1 = fully shown). `None` = no animation
    /// has run yet (treated as fully shown). On close it runs current→0.
    anim: Option<Animation>,
    /// While closing, the content is kept and rendered (fading out) until the animation
    /// settles, then dropped by [`advance_animations`](Self::advance_animations).
    closing: bool,
}

impl PanelPopover {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            open: false,
            output: None,
            anchor: Rectangle::default(),
            content: None,
            clock,
            config,
            anim: None,
            closing: false,
        }
    }

    /// Whether the popover is showing (including while it fades out on close).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The panel button role whose menu is up, so the panel can keep that button's
    /// container in its active state. `None` once the popover starts closing, so the
    /// button de-highlights immediately as the menu fades out (like gnome-shell
    /// dropping `:checked` on dismiss).
    pub fn open_role(&self) -> Option<&'static str> {
        if !self.open || self.closing {
            return None;
        }
        match self.content.as_ref()? {
            PopoverContent::Calendar(_) => Some(crate::ui::panel::ROLE_DATE_MENU),
            PopoverContent::QuickSettings(_) => Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        }
    }

    /// Build an open/close fade animation from `from` to `to` using the configured
    /// `panel_popover_open_close` params (gnome-shell's `BoxPointer` timing).
    fn make_anim(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.panel_popover_open_close.0,
        )
    }

    /// The current fade progress in `[0, 1]` (1 when fully open with no animation).
    fn progress(&self) -> f32 {
        self.anim
            .as_ref()
            .map_or(1., |a| a.clamped_value().clamp(0., 1.) as f32)
    }

    /// Settle the open/close animation: once a close fade finishes, drop the content.
    pub fn advance_animations(&mut self) {
        if self.closing && self.anim.as_ref().is_none_or(|a| a.is_done()) {
            self.open = false;
            self.closing = false;
            self.output = None;
            self.content = None;
            self.anim = None;
        }
    }

    /// Whether an open/close fade is still running (keeps the redraw loop ticking).
    pub fn are_animations_ongoing(&self) -> bool {
        self.anim.as_ref().is_some_and(|a| !a.is_done())
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
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::Calendar(Calendar::new(
            week_start,
            show_week_numbers,
            accent,
        )));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Toggle the quick-settings menu, anchored at `anchor` on `output`. `battery`
    /// feeds the power pill (`None` hides it); `audio` feeds the volume slider.
    #[allow(clippy::too_many_arguments)]
    pub fn toggle_quick_settings(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        toggles: crate::gnome::QuickToggles,
        network: crate::system_status::NetworkStatus,
        battery: Option<crate::system_status::BatteryStatus>,
        audio: Option<crate::audio::AudioStatus>,
        accent: [u8; 3],
    ) {
        if self.is_showing::<QuickSettingsTag>() {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::QuickSettings(QuickSettings::new(
            toggles, network, battery, audio, accent,
        )));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Push a fresh audio snapshot to an open quick-settings popover (from the
    /// PipeWire watcher), so the volume slider tracks live changes. Returns whether
    /// it changed anything.
    pub fn set_audio(&mut self, audio: Option<crate::audio::AudioStatus>) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_audio(audio)
            }
            _ => false,
        }
    }

    /// Continue a quick-settings volume-slider drag at output-local `pos`; returns
    /// the action to apply, or `None` when not over a live slider drag.
    pub fn pointer_drag(
        &mut self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<PopoverAction> {
        if !self.open || self.closing || self.output.as_ref() != Some(output) {
            return None;
        }
        let local = pos - self.location(output);
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) => qs.pointer_drag(local),
            _ => None,
        }
    }

    /// End any quick-settings slider drag (pointer released).
    pub fn end_drag(&mut self) {
        if let Some(PopoverContent::QuickSettings(qs)) = &mut self.content {
            qs.end_drag();
        }
    }

    /// Whether the popover is open showing a particular content kind (so a second
    /// click on the *same* button toggles it closed, but clicking a different
    /// panel button swaps content instead of no-op-toggling). A popover that is
    /// fading out (`closing`) is not "showing", so its button re-opens it fresh.
    fn is_showing<T: ContentTag>(&self) -> bool {
        self.open && !self.closing && self.content.as_ref().is_some_and(T::matches)
    }

    /// Start the fade-out. The content stays and keeps rendering (fading) until the
    /// animation settles, when [`advance_animations`](Self::advance_animations) drops
    /// it. Idempotent while already closing.
    pub fn close(&mut self) {
        if !self.open || self.closing {
            return;
        }
        self.closing = true;
        let from = f64::from(self.progress());
        self.anim = Some(self.make_anim(from, 0.));
    }

    /// Feed a key while the popover is open. Escape closes it; every other key is
    /// swallowed (a modal grab, like GNOME popup menus). Returns whether the key
    /// was consumed. A closing (fading-out) popover no longer grabs input.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> bool {
        if !self.open || self.closing {
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
        // A closed or fading-out popover doesn't grab: let the caller handle the click
        // normally (so a click during the close fade still hits whatever is beneath).
        if !self.open || self.closing {
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
            // A system button (screenshot / settings / lock / power / pill)
            // closes the menu, like GNOME.
            if action.closes_menu() {
                self.close();
            }
            return Some(action);
        }
        // Outside click — dismiss and consume it (GNOME's grab swallows the click
        // that closes the menu rather than also acting on what's beneath).
        self.close();
        Some(PopoverAction::Consumed)
    }

    /// The popover's resting top-left, output-local logical: centered under the anchor,
    /// clamped into the output with a `POPOVER_MARGIN` inset from the screen edges, and
    /// sitting `POPOVER_MARGIN` below the panel (not flush); snapped to the pixel grid.
    fn location(&self, output: &Output) -> Point<f64, Logical> {
        let scale = output.current_scale().fractional_scale();
        let ow = output_size(output).w;
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let center_x = self.anchor.loc.x + self.anchor.size.w / 2.;
        // Keep a margin from both screen edges (upper bound falls back to the lower one
        // when the popover is wider than the margined area).
        let max_x = (ow - size.w - POPOVER_MARGIN).max(POPOVER_MARGIN);
        let x = (center_x - size.w / 2.).clamp(POPOVER_MARGIN, max_x);
        Point::from((x, PANEL_HEIGHT + POPOVER_MARGIN))
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
        let progress = self.progress();

        // Slide: emerge from `POPOVER_RISE` above the resting spot as it opens (and
        // slide back up on close), coupled with the fade — gnome-shell's BoxPointer.
        // Applied only here, not in `location`, so hit-testing uses the resting rect
        // (input is inactive until fully open anyway).
        let mut origin = self.location(output);
        origin.y -= POPOVER_RISE * (1. - f64::from(progress));

        let mut elements = match self.content.as_ref() {
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
        };

        // Fade + scale the whole popover by the open/close progress. gnome-shell's
        // BoxPointer opens from 0.96→1.0 scale about the panel-adjacent edge it emerges
        // from (its arrow); we pivot on the popover's top-center. Applied only while
        // animating (`progress < 1`), where the fade already makes every element
        // translucent — so the scaled geometry not being reflected in `opaque_regions`
        // is harmless (a translucent element reports none). At rest the elements are
        // untouched, so their opaque regions stay exact.
        if progress < 1. {
            let scale_f = 0.96 + 0.04 * f64::from(progress); // lerp(0.96, 1.0, progress)
            let menu_w = self
                .content
                .as_ref()
                .map(|c| c.logical_size().w)
                .unwrap_or_default();
            let pivot = Point::<f64, Logical>::from((origin.x + menu_w / 2., origin.y));
            for el in &mut elements {
                el.set_alpha(progress);
                let loc = el.location();
                let sz = el.logical_size();
                el.set_location(Point::from((
                    pivot.x + (loc.x - pivot.x) * scale_f,
                    pivot.y + (loc.y - pivot.y) * scale_f,
                )));
                el.set_size(Size::from((sz.w * scale_f, sz.h * scale_f)));
            }
        }
        elements
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
