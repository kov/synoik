use std::cell::RefCell;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;

use niri_config::{Action, Config};
use niri_ipc::SizeChange;
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::TouchSlot;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer, Texture};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::output::{Output, WeakOutput};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::layout::floating::DIRECTIONAL_MOVE_PX;
use crate::niri_render_elements;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::RenderTarget;
use crate::ui::widget::{self, Painter, ParagraphSpan, TextShaper};
use crate::utils::to_physical_precise_round;

/// Per-element cache of a neutral CPU buffer uploaded once to a `VkTexture` (see
/// [`upload_cached`]).
type VkCache = RefCell<Option<TextureBuffer<VkTexture>>>;

const SELECTION_BORDER: i32 = 2;

const PADDING: i32 = 8;
const RADIUS: i32 = 16;
/// Help-line font size, GNOME points; shaping routes it through [`ParagraphSpan`].
/// `font_px()` is its logical px, used only for the keycap-patch padding geometry.
const FONT_PT: f64 = 11.;
fn font_px() -> f64 {
    crate::ui::pt_to_px(FONT_PT)
}
/// Panel corner radius, logical px — GNOME `.screenshot-ui-panel` `$modal_radius * 2`
/// (`_screenshot.scss:9`). The whole panel is one rounded `%osd_panel` card.
const PANEL_RADIUS: f64 = 32.;

/// Dark `%osd_panel` background and the grey keycap patch (`#2C2C2C`). `PANEL_BORDER_COLOR`
/// is GNOME's `$osd_outer_borders_color` = white@2% (`_colors.scss:44`), a 1px inset stroke;
/// the panel reads against the screenshot via its drop shadow, not a heavy border.
const PANEL_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const PANEL_BORDER_COLOR: [f32; 4] = [1., 1., 1., 0.02];
const KEYCAP_BG: [f32; 4] = [0.172, 0.172, 0.172, 1.];
const TEXT_COLOR: [f32; 4] = [1., 1., 1., 1.];

/// Drop shadow — GNOME `.screenshot-ui-panel` `box-shadow: 0 2px 4px 0 $shadow_color`
/// (`_screenshot.scss:21`); `$shadow_color` (dark) is `rgba(0,0,0,0.2)`. Logical px.
const SHADOW_COLOR: [f32; 4] = [0., 0., 0., 0.2];
const SHADOW_BLUR: f64 = 4.;
const SHADOW_OFFSET_Y: f64 = 2.;

// Ideally the screenshot UI should support cross-output selections. However, that poses some
// technical challenges when the outputs have different scales and such. So, this implementation
// allows only single-output selections for now.
//
// As a consequence of this, selection coordinates are in output-local coordinate space.
#[allow(clippy::large_enum_variant)]
pub enum ScreenshotUi {
    Closed {
        last_selection: Option<(WeakOutput, Rectangle<i32, Physical>)>,
        clock: Clock,
        config: Rc<RefCell<Config>>,
    },
    Open {
        selection: (Output, Point<i32, Physical>, Point<i32, Physical>),
        output_data: HashMap<Output, OutputData>,
        button: Button,
        show_pointer: bool,
        open_anim: Animation,
        clock: Clock,
        config: Rc<RefCell<Config>>,
        path: Option<String>,
    },
}

/// State for moving the selection (as opposed to just drawing).
pub struct MoveState {
    // Cursor offset from selection.1 when starting the move.
    pointer_offset: Point<i32, Physical>,
    // If the move is initiated by a touch, this is the slot. If `None`, the move was initiated by
    // holding Space.
    touch_slot: Option<TouchSlot>,
}

pub enum Button {
    Up,
    Down {
        touch_slot: Option<TouchSlot>,
        on_capture_button: bool,
        last_pos: (Output, Point<i32, Physical>),
        move_state: Option<MoveState>,
    },
}

pub struct OutputData {
    size: Size<i32, Physical>,
    scale: f64,
    transform: Transform,
    // Output, screencast, screen capture.
    screenshot: [OutputScreenshot; 3],
    buffers: [SolidColorBuffer; 8],
    locations: [Point<i32, Physical>; 8],
    /// The help panel, drawn straight into `VkTexture`s by the owned renderer and cached across
    /// frames. Built lazily on the first render (no renderer is available at open time); the
    /// capture button's hit test reads its size, so the button isn't clickable until one frame
    /// has drawn.
    panel: RefCell<PanelCache>,
}

/// The two help-panel variants (show/hide-pointer), rebuilt on a scale or renderer-context change.
#[derive(Default)]
struct PanelCache {
    scale: f64,
    context: Option<ContextId<VkTexture>>,
    /// "…to show the pointer." — shown while the pointer is hidden.
    show: Option<VkTexture>,
    /// "…to hide the pointer." — shown while the pointer is visible.
    hide: Option<VkTexture>,
    /// The panel's drop shadow, baked once into its own transparent texture and composited
    /// *behind* the panel (both variants share the panel size, so one shadow serves both).
    shadow: Option<VkTexture>,
    /// The CPU-composed capture-button bitmap (composited over the panel as a separate element),
    /// and its once-per-context Vulkan upload.
    button: Option<MemoryBuffer>,
    button_vk: VkCache,
}

impl PanelCache {
    /// The panel's physical size (both variants share it), or `None` before the first draw.
    fn size(&self) -> Option<Size<i32, Buffer>> {
        let t = self.show.as_ref().or(self.hide.as_ref())?;
        Some(Size::from((t.width() as i32, t.height() as i32)))
    }
}

/// An output's screenshot captured through the owned Vulkan renderer up front, so a Vulkan session
/// never bakes (nor reads back) a GLES texture for the screenshot UI at all.
///
/// Captured once per render target — the frozen screen is drawn into screencasts and screen
/// captures too, and those differ by block-out rules. A missing `screen` drops the output; a
/// missing `pointer` only costs the cursor. Empty (via `Default`) on a GLES session.
#[derive(Default)]
pub struct ScreenshotNeutral {
    pub screen: Option<MemoryBuffer>,
    pub pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
}

/// One output's frozen screen (and pointer + its logical location), as renderer-neutral CPU
/// buffers. Saving to disk crops these on the CPU rather than reading back from the GPU.
pub struct OutputScreenshot {
    screen: MemoryBuffer,
    pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
    /// `screen` / `pointer` uploaded once to `VkTexture`s, cached across frames.
    screen_vk: VkCache,
    pointer_vk: VkCache,
}

niri_render_elements! {
    ScreenshotUiRenderElement => {
        Screenshot = CapturedTextureRenderElement,
        SolidColor = SolidColorRenderElement,
    }
}

impl Button {
    fn is_down(&self) -> bool {
        matches!(self, Self::Down { .. })
    }

    fn is_dragging_selection(&self) -> bool {
        matches!(
            self,
            Self::Down {
                on_capture_button: false,
                ..
            }
        )
    }
}

impl ScreenshotUi {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self::Closed {
            last_selection: None,
            clock,
            config,
        }
    }

    pub fn open(
        &mut self,
        // Output, screencast, screen capture.
        screenshots: HashMap<Output, [OutputScreenshot; 3]>,
        default_output: Output,
        show_pointer: bool,
        path: Option<String>,
    ) -> bool {
        if screenshots.is_empty() {
            // Every output's capture failed (each warned individually). Say so, or the keybind
            // just silently does nothing.
            warn!("no output could be captured; not opening the screenshot UI");
            return false;
        }

        let Self::Closed {
            last_selection,
            clock,
            config,
        } = self
        else {
            return false;
        };

        let last_selection = last_selection
            .take()
            .and_then(|(weak, sel)| weak.upgrade().map(|output| (output, sel)));
        let selection = match last_selection {
            Some(selection) if screenshots.contains_key(&selection.0) => selection,
            _ => {
                let output = default_output;
                let output_transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = output_transform.transform_size(output_mode.size);
                (
                    output,
                    Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    ),
                )
            }
        };

        let selection = (
            selection.0,
            selection.1.loc,
            selection.1.loc + selection.1.size - Size::from((1, 1)),
        );

        let output_data = screenshots
            .into_iter()
            .map(|(output, screenshot)| {
                let transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = transform.transform_size(output_mode.size);
                let scale = output.current_scale().fractional_scale();
                let buffers = [
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                ];
                let locations = [Default::default(); 8];

                let data = OutputData {
                    size,
                    scale,
                    transform,
                    screenshot,
                    buffers,
                    locations,
                    panel: RefCell::new(PanelCache::default()),
                };
                (output, data)
            })
            .collect();

        let open_anim = {
            let c = config.borrow();
            Animation::new(clock.clone(), 0., 1., 0., c.animations.screenshot_ui_open.0)
        };

        *self = Self::Open {
            selection,
            output_data,
            button: Button::Up,
            show_pointer,
            open_anim,
            clock: clock.clone(),
            config: config.clone(),
            path,
        };

        self.update_buffers();

        true
    }

    pub fn close(&mut self) -> bool {
        let Self::Open {
            selection,
            clock,
            config,
            ..
        } = self
        else {
            return false;
        };

        let last_selection = Some((
            selection.0.downgrade(),
            rect_from_corner_points(selection.1, selection.2),
        ));

        *self = Self::Closed {
            last_selection,
            clock: clock.clone(),
            config: config.clone(),
        };

        true
    }

    pub fn toggle_pointer(&mut self) {
        if let Self::Open { show_pointer, .. } = self {
            *show_pointer = !*show_pointer;
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, ScreenshotUi::Open { .. })
    }

    pub fn set_space_down(&mut self, down: bool) {
        if let Self::Open {
            selection,
            button:
                Button::Down {
                    move_state,
                    last_pos,
                    ..
                },
            ..
        } = self
        {
            if down {
                if move_state.is_none() {
                    *move_state = Some(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: None,
                    });
                }
            } else {
                // Only clear if moving with Space.
                if let Some(MoveState {
                    touch_slot: None, ..
                }) = move_state
                {
                    *move_state = None;
                }
            }
        }
    }

    pub fn move_left(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.x, b.x));
        a.x -= delta;
        b.x -= delta;

        self.update_buffers();
    }

    pub fn move_right(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.w - max(a.x, b.x) - 1);
        a.x += delta;
        b.x += delta;

        self.update_buffers();
    }

    pub fn move_up(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.y, b.y));
        a.y -= delta;
        b.y -= delta;

        self.update_buffers();
    }

    pub fn move_down(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.h - max(a.y, b.y) - 1);
        a.y += delta;
        b.y += delta;

        self.update_buffers();
    }

    /// Moves the screenshot selection to a different output.
    ///
    /// This preserves the relative position while keeping logical size. It is (intentionally) very
    /// similar to how floating windows move across monitors, but with one difference: floating
    /// windows can go partially outside the view, while the screenshot selection cannot. So, we
    /// clamp it to new output bounds, trying to preserve the size if possible.
    pub fn move_to_output(&mut self, new_output: Output) {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            return;
        };

        let (current_output, current_a, current_b) = selection;

        if current_output == &new_output {
            return;
        }

        let Some(target_data) = output_data.get(&new_output) else {
            return;
        };

        let current_data = &output_data[current_output];

        let current_rect: Rectangle<_, Physical> = Rectangle::new(
            Point::from((current_a.x.min(current_b.x), current_a.y.min(current_b.y))),
            Size::from((
                (current_a.x.max(current_b.x) - current_a.x.min(current_b.x) + 1),
                (current_a.y.max(current_b.y) - current_a.y.min(current_b.y) + 1),
            )),
        );
        let current_rect = current_rect.to_f64();

        let rel_x = current_rect.loc.x / current_data.size.w as f64;
        let rel_y = current_rect.loc.y / current_data.size.h as f64;

        let factor = target_data.scale / current_data.scale;
        let mut new_width = (current_rect.size.w * factor).round() as i32;
        let mut new_height = (current_rect.size.h * factor).round() as i32;

        new_width = new_width.clamp(1, target_data.size.w);
        new_height = new_height.clamp(1, target_data.size.h);

        let new_x = (rel_x * target_data.size.w as f64).round() as i32;
        let new_y = (rel_y * target_data.size.h as f64).round() as i32;

        let max_x = target_data.size.w - new_width;
        let max_y = target_data.size.h - new_height;
        let new_x = new_x.clamp(0, max_x);
        let new_y = new_y.clamp(0, max_y);

        let new_rect = Rectangle::new(
            Point::from((new_x, new_y)),
            Size::from((new_width, new_height)),
        );

        *selection = (
            new_output,
            new_rect.loc,
            new_rect.loc + new_rect.size - Size::from((1, 1)),
        );

        self.update_buffers();
    }

    pub fn set_width(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.w);
        let current_size = max(a.x, b.x) + 1 - min(a.x, b.x);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.w - min(a.x, b.x)) - 1;
        a.x = min(a.x, b.x);
        b.x = a.x + new_size;

        self.update_buffers();
    }

    pub fn set_height(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.h);
        let current_size = max(a.y, b.y) + 1 - min(a.y, b.y);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.h - min(a.y, b.y)) - 1;
        a.y = min(a.y, b.y);
        b.y = a.y + new_size;

        self.update_buffers();
    }

    pub fn advance_animations(&mut self) {}

    pub fn are_animations_ongoing(&self) -> bool {
        let Self::Open { open_anim, .. } = self else {
            return false;
        };

        !open_anim.is_done()
    }

    fn update_buffers(&mut self) {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to update buffers");
        };

        let (selection_output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);

        for (output, data) in output_data {
            let buffers = &mut data.buffers;
            let locations = &mut data.locations;
            let size = data.size;
            let scale = data.scale;

            if output == selection_output {
                // Check if the selection is still valid. If not, reset it back to default.
                if !Rectangle::from_size(size).contains_rect(rect) {
                    rect = Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    );
                    *a = rect.loc;
                    *b = rect.loc + rect.size - Size::from((1, 1));
                }

                let border = to_physical_precise_round(scale, SELECTION_BORDER);

                let resize = move |buffer: &mut SolidColorBuffer, w: i32, h: i32| {
                    let size = Size::<_, Physical>::from((w, h));
                    buffer.resize(size.to_f64().to_logical(scale));
                };

                resize(&mut buffers[0], rect.size.w + border * 2, border);
                resize(&mut buffers[1], rect.size.w + border * 2, border);
                resize(&mut buffers[2], border, rect.size.h);
                resize(&mut buffers[3], border, rect.size.h);

                resize(&mut buffers[4], size.w, rect.loc.y);
                resize(&mut buffers[5], size.w, size.h - rect.loc.y - rect.size.h);
                resize(&mut buffers[6], rect.loc.x, rect.size.h);
                resize(
                    &mut buffers[7],
                    size.w - rect.loc.x - rect.size.w,
                    rect.size.h,
                );

                locations[0] = Point::from((rect.loc.x - border, rect.loc.y - border));
                locations[1] = Point::from((rect.loc.x - border, rect.loc.y + rect.size.h));
                locations[2] = Point::from((rect.loc.x - border, rect.loc.y));
                locations[3] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));

                locations[5] = Point::from((0, rect.loc.y + rect.size.h));
                locations[6] = Point::from((0, rect.loc.y));
                locations[7] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));
            } else {
                buffers[0].resize((0., 0.));
                buffers[1].resize((0., 0.));
                buffers[2].resize((0., 0.));
                buffers[3].resize((0., 0.));

                buffers[4].resize(size.to_f64().to_logical(data.scale));
                buffers[5].resize((0., 0.));
                buffers[6].resize((0., 0.));
                buffers[7].resize((0., 0.));
            }
        }
    }

    /// The help panel's on-screen rect on `output`, or `None` if it has no panel.
    ///
    /// Test-only. The panel is drawn from real text, so its size depends on the font the machine
    /// actually has — a test cannot hardcode this rect. Measuring *inside* it is also the only way
    /// to tell the panel apart from the UI's other chrome: the four selection-border buffers alone
    /// score thousands of white pixels with the panel entirely absent. Returns `None` until the
    /// panel has drawn at least once (it is built lazily at render time).
    #[cfg(test)]
    pub fn panel_rect(&self, output: &Output) -> Option<Rectangle<i32, Physical>> {
        let Self::Open { output_data, .. } = self else {
            return None;
        };

        let output_data = output_data.get(output)?;
        let size = output_data.panel.borrow().size()?;

        Some(Rectangle::new(
            panel_location(output_data, size),
            Size::from((size.w, size.h)),
        ))
    }

    pub fn render_output(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target: RenderTarget,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        let _span = tracy_client::span!("ScreenshotUi::render_output");

        let Self::Open {
            output_data,
            show_pointer,
            button,
            open_anim,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to render it");
        };

        let Some(output_data) = output_data.get(output) else {
            return;
        };

        let scale = output_data.scale;
        let progress = open_anim.clamped_value().clamp(0., 1.) as f32;

        // The help panel goes on top. Built lazily here (no renderer exists at open time), so this
        // is also what first populates the size the capture-button hit test reads.
        output_data.ensure_panel(renderer);
        if let Some(size) = output_data.panel.borrow().size() {
            let alpha = if button.is_dragging_selection() {
                0.3
            } else {
                0.9
            };
            let location = panel_location(output_data, size).to_f64().to_logical(scale);

            // Earlier-pushed elements are composited on top (the screenshot goes last), so push the
            // capture button before the panel to keep it above the panel background.
            if let Some(elem) = output_data.button_element(renderer, size, alpha * progress) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
            if let Some(elem) =
                output_data.panel_element(renderer, *show_pointer, location, alpha * progress)
            {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
            // The drop shadow is pushed after the panel so it composites beneath it.
            if let Some(elem) = output_data.shadow_element(renderer, alpha * progress) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
        }

        for (buffer, loc) in zip(&output_data.buffers, &output_data.locations) {
            let elem = SolidColorRenderElement::from_buffer(
                buffer,
                loc.to_f64().to_logical(scale),
                progress,
                Kind::Unspecified,
            );
            push(elem.into());
        }

        // The screenshot itself goes last.
        let index = match target {
            RenderTarget::Output => 0,
            RenderTarget::Screencast => 1,
            RenderTarget::ScreenCapture => 2,
        };
        let screenshot = &output_data.screenshot[index];

        if *show_pointer {
            if let Some(pointer) = screenshot.pointer_element(renderer) {
                push(ScreenshotUiRenderElement::Screenshot(pointer));
            }
        }
        if let Some(buffer) = screenshot.buffer_element(renderer) {
            push(ScreenshotUiRenderElement::Screenshot(buffer));
        }
    }

    pub fn capture_from_neutral(&self) -> Option<(Size<i32, Physical>, Vec<u8>)> {
        let _span = tracy_client::span!("ScreenshotUi::capture_from_neutral");

        let Self::Open {
            selection,
            output_data,
            show_pointer,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to capture");
        };

        let data = &output_data[&selection.0];
        let OutputScreenshot {
            screen,
            pointer: screenshot_pointer,
            ..
        } = &data.screenshot[0];

        let rect = rect_from_corner_points(selection.1, selection.2);

        // The pointer neutral carries its top-left in logical output coordinates; map it back to
        // the physical space the frozen screen (and the selection rect) live in.
        let pointer = show_pointer
            .then(|| screenshot_pointer.as_ref())
            .flatten()
            .map(|(ptr, loc)| {
                let scale = Scale::from(data.scale);
                (ptr, loc.to_physical_precise_round(scale))
            });

        Some((rect.size, crop_screenshot_neutral(screen, rect, pointer)))
    }

    pub fn action(&self, raw: Keysym, mods: ModifiersState) -> Option<Action> {
        let Self::Open { button, .. } = self else {
            return None;
        };

        // Pressing Space while the button is down goes into origin moving rather than capture.
        if matches!(button, Button::Down { .. }) && raw == Keysym::space {
            return None;
        }

        action(raw, mods)
    }

    /// The current selection as a rectangle in **global logical** coordinates.
    ///
    /// This is what `org.gnome.Shell.Screenshot.SelectArea` hands back, so it has to be in the same
    /// space its caller then passes to `ScreenshotArea` — output-local physical is what the UI
    /// works in and is not it.
    pub fn selection_rect_global(&self) -> Option<Rectangle<i32, Logical>> {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            return None;
        };

        let (output, a, b) = selection;
        let rect = rect_from_corner_points(*a, *b);
        let scale = output_data.get(output)?.scale;
        let logical = rect.to_f64().to_logical(scale);
        let loc = output.current_location();

        Some(Rectangle::new(
            Point::from((
                loc.x + logical.loc.x.round() as i32,
                loc.y + logical.loc.y.round() as i32,
            )),
            Size::from((logical.size.w.round() as i32, logical.size.h.round() as i32)),
        ))
    }

    pub fn selection_output(&self) -> Option<&Output> {
        if let Self::Open {
            selection: (output, _, _),
            ..
        } = self
        {
            Some(output)
        } else {
            None
        }
    }

    pub fn output_size(&self, output: &Output) -> Option<(Size<i32, Physical>, f64, Transform)> {
        if let Self::Open { output_data, .. } = self {
            let data = output_data.get(output)?;
            Some((data.size, data.scale, data.transform))
        } else {
            None
        }
    }

    /// The pointer has moved to `point` relative to the current selection output.
    ///
    /// The point may be outside output bounds.
    pub fn pointer_motion(&mut self, point: Point<i32, Physical>, slot: Option<TouchSlot>) {
        let Self::Open {
            selection,
            output_data,
            button:
                Button::Down {
                    touch_slot,
                    on_capture_button,
                    last_pos,
                    move_state,
                },
            ..
        } = self
        else {
            return;
        };

        if *touch_slot != slot {
            return;
        }

        last_pos.1 = point;

        if *on_capture_button {
            return;
        }

        if let Some(move_state) = move_state {
            // The cursor offset is relative to selection.1.
            let delta = point - (selection.1 + move_state.pointer_offset);

            let desired = rect_from_corner_points(selection.1 + delta, selection.2 + delta);
            let bounds = Rectangle::from_size(output_data[&selection.0].size - desired.size);
            let clamped_loc = desired.loc.constrain(bounds);

            let delta = clamped_loc - rect_from_corner_points(selection.1, selection.2).loc;
            selection.1 += delta;
            selection.2 += delta;
        } else {
            let size = output_data[&selection.0].size;
            selection.2 = Point::new(point.x.clamp(0, size.w - 1), point.y.clamp(0, size.h - 1));
        }

        self.update_buffers();
    }

    pub fn pointer_down(
        &mut self,
        output: Output,
        point: Point<i32, Physical>,
        slot: Option<TouchSlot>,
        move_existing: bool,
    ) -> bool {
        let Self::Open {
            selection,
            output_data,
            button,
            ..
        } = self
        else {
            return false;
        };

        // Check if this is a second touch (different slot) while already dragging.
        if let Some(new_slot) = slot {
            if let Button::Down {
                on_capture_button: false,
                move_state,
                last_pos,
                ..
            } = button
            {
                if move_state.is_none() {
                    *move_state = Some(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: Some(new_slot),
                    });
                }
            }
        }

        if button.is_down() {
            return false;
        }

        if move_existing {
            if output != selection.0 {
                return false;
            }

            *button = Button::Down {
                touch_slot: slot,
                on_capture_button: false,
                last_pos: (output, point),
                move_state: Some(MoveState {
                    pointer_offset: point - selection.1,
                    touch_slot: slot,
                }),
            };
            return true;
        }

        let Some(output_data) = output_data.get(&output) else {
            return false;
        };

        if let Some(panel_size) = output_data.panel.borrow().size() {
            let location = panel_location(output_data, panel_size);

            if is_within_capture_button(output_data.scale, panel_size, point - location) {
                *button = Button::Down {
                    touch_slot: slot,
                    on_capture_button: true,
                    last_pos: (output, point),
                    move_state: None,
                };
                return false;
            }
        }

        *button = Button::Down {
            touch_slot: slot,
            on_capture_button: false,
            last_pos: (output.clone(), point),
            move_state: None,
        };

        let point = Point::new(
            point.x.clamp(0, output_data.size.w - 1),
            point.y.clamp(0, output_data.size.h - 1),
        );
        *selection = (output, point, point);

        self.update_buffers();

        true
    }

    pub fn pointer_up(&mut self, slot: Option<TouchSlot>) -> Option<bool> {
        let Self::Open {
            selection,
            output_data,
            button,
            ..
        } = self
        else {
            return None;
        };

        let Button::Down {
            touch_slot,
            on_capture_button,
            ref last_pos,
            ref mut move_state,
            ..
        } = *button
        else {
            return None;
        };

        if touch_slot != slot {
            // This is not our main touch, but it might be the move touch. If so, stop the move.
            if let Some(state) = move_state {
                if state.touch_slot.is_some_and(|m_slot| Some(m_slot) == slot) {
                    *move_state = None;
                }
            };

            return None;
        }

        let last_pos = last_pos.clone();
        *button = Button::Up;

        // Check if we released still on the capture button.
        if on_capture_button {
            let (output, point) = last_pos;

            #[allow(clippy::question_mark)]
            let Some(output_data) = output_data.get(&output) else {
                return None;
            };

            if let Some(panel_size) = output_data.panel.borrow().size() {
                let location = panel_location(output_data, panel_size);

                if is_within_capture_button(output_data.scale, panel_size, point - location) {
                    return Some(true);
                }
            }
        }

        // Check if the resulting selection is zero-sized, and try to come up with a small
        // default rectangle.
        let (output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);
        if rect.size.is_empty() || rect.size == Size::from((1, 1)) {
            let data = &output_data[output];
            rect = Rectangle::new(
                Point::from((rect.loc.x - 16, rect.loc.y - 16)),
                Size::from((32, 32)),
            )
            .intersection(Rectangle::from_size(data.size))
            .unwrap_or_default();
            *a = rect.loc;
            *b = rect.loc + rect.size - Size::from((1, 1));
        }

        self.update_buffers();

        Some(false)
    }
}

impl OutputScreenshot {
    /// The frozen screen as renderer-neutral captures (a Vulkan session).
    pub fn from_neutrals(
        screen: MemoryBuffer,
        pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
    ) -> Self {
        Self {
            screen,
            pointer,
            screen_vk: RefCell::new(None),
            pointer_vk: RefCell::new(None),
        }
    }

    /// The frozen-screen element, or `None` if it can't be drawn — a failed Vulkan upload. Callers
    /// must skip it rather than substitute anything: there is no second copy of the frozen screen.
    fn buffer_element(
        &self,
        renderer: &mut VulkanRenderer,
    ) -> Option<CapturedTextureRenderElement> {
        let Self {
            screen, screen_vk, ..
        } = self;

        let vk = &mut *renderer;
        let tb = upload_cached(vk, screen, screen_vk)?;
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                (0., 0.),
                1.,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    /// The composited-pointer element. `None` when no pointer was captured, or when it can't be
    /// drawn (see [`Self::buffer_element`]).
    fn pointer_element(
        &self,
        renderer: &mut VulkanRenderer,
    ) -> Option<CapturedTextureRenderElement> {
        let Self {
            pointer,
            pointer_vk,
            ..
        } = self;

        let vk = &mut *renderer;
        let (neutral, location) = pointer.as_ref()?;
        let tb = upload_cached(vk, neutral, pointer_vk)?;
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                *location,
                1.,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}

impl OutputData {
    /// Build both help-panel variants into `VkTexture`s if missing or stale (scale / renderer
    /// context change). Failures leave the variant `None` — the panel just doesn't draw.
    fn ensure_panel(&self, renderer: &mut VulkanRenderer) {
        let scale = self.scale;
        let context = renderer.context_id();
        {
            let cache = self.panel.borrow();
            if cache.show.is_some()
                && cache.scale == scale
                && cache.context.as_ref() == Some(&context)
            {
                return;
            }
        }

        let show = generate_panel(renderer, scale, "show")
            .map_err(|err| warn!("error rendering help panel: {err:?}"))
            .ok();
        let hide = generate_panel(renderer, scale, "hide")
            .map_err(|err| warn!("error rendering help panel: {err:?}"))
            .ok();
        // The shadow only needs the panel footprint (both variants share it).
        let shadow = show
            .as_ref()
            .or(hide.as_ref())
            .map(|t| Size::<i32, Physical>::from((t.width() as i32, t.height() as i32)))
            .and_then(|panel_size| {
                generate_panel_shadow(renderer, scale, panel_size)
                    .map_err(|err| warn!("error rendering help-panel shadow: {err:?}"))
                    .ok()
            });
        *self.panel.borrow_mut() = PanelCache {
            scale,
            context: Some(context),
            show,
            hide,
            shadow,
            button: Some(button_bitmap(scale)),
            button_vk: RefCell::new(None),
        };
    }

    /// The capture-button element, positioned at the panel's left-centre. `None` if the button
    /// bitmap or its upload is missing.
    fn button_element(
        &self,
        renderer: &mut VulkanRenderer,
        panel_size: Size<i32, Buffer>,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let scale = self.scale;
        let padding: i32 = to_physical_precise_round(scale, PADDING);
        let radius: i32 = to_physical_precise_round(scale, RADIUS);

        let cache = self.panel.borrow();
        let buffer = cache.button.as_ref()?;
        let tb = upload_cached(renderer, buffer, &cache.button_vk)?;

        // Panel-local physical offset of the button's top-left, then into output-logical space.
        let loc =
            panel_location(self, panel_size) + Point::from((padding, panel_size.h / 2 - radius));
        let location = loc.to_f64().to_logical(scale);

        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                location,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    /// The panel's drop-shadow element, placed so its baked panel-footprint aligns with the panel
    /// on screen (the buffer pads the footprint by the blur bleed, so the element sits up-left of
    /// the panel by that margin). Composited *behind* the panel. `None` until the shadow is baked.
    fn shadow_element(
        &self,
        renderer: &mut VulkanRenderer,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let scale = self.scale;
        let (texture, panel_size) = {
            let cache = self.panel.borrow();
            (cache.shadow.clone()?, cache.size()?)
        };
        let (margin, _) = shadow_pad(scale);
        let loc = (panel_location(self, panel_size) - Point::from((margin, margin)))
            .to_f64()
            .to_logical(scale);
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                loc,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    /// The help-panel element (show/hide variant chosen by `show_pointer`), or `None` if the panel
    /// hasn't been built (see [`Self::ensure_panel`]).
    fn panel_element(
        &self,
        renderer: &mut VulkanRenderer,
        show_pointer: bool,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let cache = self.panel.borrow();
        let texture = if show_pointer {
            &cache.hide
        } else {
            &cache.show
        };
        let texture = texture.clone()?;
        let buffer = TextureBuffer::from_texture(
            renderer,
            texture,
            self.scale,
            Transform::Normal,
            Vec::new(),
        );
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}

/// Upload a neutral CPU buffer to a `VkTexture` once, caching it for reuse across frames (a
/// full-screen re-upload every frame churns virtio-gpu blobs).
fn upload_cached(
    vk: &mut VulkanRenderer,
    neutral: &MemoryBuffer,
    cache: &VkCache,
) -> Option<TextureBuffer<VkTexture>> {
    if cache.borrow().is_none() {
        match TextureBuffer::from_memory_buffer(vk, neutral) {
            Ok(tb) => *cache.borrow_mut() = Some(tb),
            Err(err) => warn!("error uploading screenshot overlay to Vulkan: {err:?}"),
        }
    }
    cache.borrow().clone()
}

fn action(raw: Keysym, mods: ModifiersState) -> Option<Action> {
    if raw == Keysym::Escape {
        return Some(Action::CancelScreenshot);
    }

    if mods.alt || mods.shift {
        return None;
    }

    if !mods.ctrl && (raw == Keysym::space || raw == Keysym::Return) {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: true,
        });
    }
    if mods.ctrl && raw == Keysym::c {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: false,
        });
    }

    if !mods.ctrl && raw == Keysym::p {
        return Some(Action::ScreenshotTogglePointer);
    }

    None
}

pub fn rect_from_corner_points(
    a: Point<i32, Physical>,
    b: Point<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let x1 = min(a.x, b.x);
    let y1 = min(a.y, b.y);
    let x2 = max(a.x, b.x);
    let y2 = max(a.y, b.y);
    // We're adding + 1 because the pointer is clamped to output size - 1, so to get the full
    // screen worth of selection we must add back that + 1.
    Rectangle::from_extremities((x1, y1), (x2 + 1, y2 + 1))
}

/// Crop `rect` out of the frozen-screen `neutral` buffer (tightly-packed `Abgr8888`, origin at the
/// output's top-left) and, if `pointer` is given, composite that premultiplied buffer on top at its
/// physical origin. Pure CPU — the owned-Vulkan save-to-disk path, so it never reads back through
/// GLES. Returns `rect.size.w * rect.size.h * 4` bytes; out-of-bounds source pixels stay zero.
pub(crate) fn crop_screenshot_neutral(
    neutral: &MemoryBuffer,
    rect: Rectangle<i32, Physical>,
    pointer: Option<(&MemoryBuffer, Point<i32, Physical>)>,
) -> Vec<u8> {
    let (fw, fh) = (neutral.size().w, neutral.size().h);
    let (rw, rh) = (rect.size.w.max(0), rect.size.h.max(0));
    let src = neutral.data();
    let mut out = vec![0u8; (rw * rh * 4) as usize];

    // Crop: copy the in-bounds horizontal span of each row wholesale.
    for y in 0..rh {
        let sy = rect.loc.y + y;
        if sy < 0 || sy >= fh {
            continue;
        }
        let sx0 = rect.loc.x.max(0);
        let sx1 = (rect.loc.x + rw).min(fw);
        if sx1 <= sx0 {
            continue;
        }
        let src_off = ((sy * fw + sx0) * 4) as usize;
        let dst_off = ((y * rw + (sx0 - rect.loc.x)) * 4) as usize;
        let n = ((sx1 - sx0) * 4) as usize;
        out[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
    }

    // Composite the pointer on top: premultiplied `Abgr8888` "over" (out = src + dst·(255−a)/255).
    if let Some((ptr, ptr_origin)) = pointer {
        let (pw, ph) = (ptr.size().w, ptr.size().h);
        let pdata = ptr.data();
        for py in 0..ph {
            let cy = ptr_origin.y - rect.loc.y + py;
            if cy < 0 || cy >= rh {
                continue;
            }
            for px in 0..pw {
                let cx = ptr_origin.x - rect.loc.x + px;
                if cx < 0 || cx >= rw {
                    continue;
                }
                let si = ((py * pw + px) * 4) as usize;
                let a = pdata[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                let di = ((cy * rw + cx) * 4) as usize;
                for c in 0..4 {
                    let d = out[di + c] as u32;
                    let v = pdata[si + c] as u32 + (d * inv + 127) / 255;
                    out[di + c] = v.min(255) as u8;
                }
            }
        }
    }

    out
}

/// What the capture button will act on — GNOME's three type buttons
/// (`screenshot.js:1305-1348`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureType {
    /// A dragged rectangle. GNOME's `Selection`, and the only mode niri's picker ever had.
    #[default]
    Selection,
    /// The whole output.
    Screen,
    /// A window chosen from the selector. Not yet implemented — see slice 2 in
    /// `docs/fork/screenshot-ui-port.md`; the button is built but hidden until it works.
    Window,
}

impl CaptureType {
    /// The type row, left to right, in GNOME's order (`screenshot.js:1305-1342`).
    pub const ROW: [CaptureType; 3] = [
        CaptureType::Selection,
        CaptureType::Screen,
        CaptureType::Window,
    ];

    /// The button's glyph.
    pub fn icon(self) -> &'static str {
        match self {
            CaptureType::Selection => "screenshot-ui-area-symbolic",
            CaptureType::Screen => "screenshot-ui-display-symbolic",
            CaptureType::Window => "screenshot-ui-window-symbolic",
        }
    }

    /// The button's caption.
    pub fn label(self) -> &'static str {
        match self {
            CaptureType::Selection => "Selection",
            CaptureType::Screen => "Screen",
            CaptureType::Window => "Window",
        }
    }

    /// Whether the mode is offered yet. `Window` is built and laid out but not drawn — **our**
    /// scaffolding, not GNOME's practice: GNOME never hides `_windowButton` (it hides the *cast*
    /// button, and on a runtime capability, `screenshot.js:1524`). Delete this the moment slice 2
    /// lands rather than growing more callers.
    pub fn is_available(self) -> bool {
        self != CaptureType::Window
    }
}

/// A clickable control in the panel. The panel is one baked texture, so hit-testing is geometry
/// against [`PanelLayout`] rather than a tree walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Type(CaptureType),
    /// The shot/cast segment at this index. Only index 0 (shot) exists until slice 4.
    ShotCast(usize),
    Capture,
    ShowPointer,
    Close,
}

/// Where every control sits inside the panel, in panel-local **logical** coordinates.
///
/// One layout feeds the bake, the icon placement and the hit test, so a control cannot be drawn
/// somewhere the pointer does not find it. Built from the caption sizes, which need a shaper —
/// hence it is produced during the bake and cached, and the panel is not clickable until one frame
/// has drawn (the same caveat the capture button already carried).
#[derive(Debug, Clone, Copy)]
pub struct PanelLayout {
    pub size: Size<f64, Logical>,
    /// Indexed by [`CaptureType::ROW`].
    pub type_buttons: [Rectangle<f64, Logical>; 3],
    pub shot_cast: Rectangle<f64, Logical>,
    pub capture: Rectangle<f64, Logical>,
    pub show_pointer: Rectangle<f64, Logical>,
}

impl PanelLayout {
    /// `padding: $screenshot_ui_panel_padding` = `$base_padding * 3` (`_screenshot.scss:3,10`).
    const PAD: f64 = 18.;
    /// `padding-bottom: … - $base_padding` — trimmed "to accommodate the large capture button"
    /// (`_screenshot.scss:11-12`).
    const PAD_BOTTOM: f64 = 12.;
    /// `spacing: $base_padding * 2` between the type row and the bottom row
    /// (`_screenshot.scss:14`).
    const ROW_SPACING: f64 = 12.;
    /// `.screenshot-ui-type-button-container` spacing (`screenshot.js:1299`).
    const TYPE_SPACING: f64 = 12.;
    /// `.screenshot-ui-capture-button`: a `$large_icon_size` box inside `$base_margin` padding and
    /// a 4px border (`_screenshot.scss:40-45`).
    pub const CAPTURE_DIAMETER: f64 = 32. + 4. * 2. + 4. * 2.;
    /// `.screenshot-ui-show-pointer-button` extends `.icon-button`, whose padding is
    /// `$scaled_padding * 2` (`_buttons.scss:18-38`).
    const SHOW_POINTER_PAD: f64 = 12.;

    /// Lay the panel out around captions of `label_w` (one per type button) and `label_h`.
    ///
    /// The bottom row is a **BinLayout** in GNOME (`screenshot.js:1350`): the shot/cast pill, the
    /// capture button and the show-pointer button all occupy the same band, aligned start, centre
    /// and end. Laying them out in sequence instead would push the capture button off-centre,
    /// which is the mistake this arithmetic exists to prevent.
    pub fn new(label_w: [f64; 3], label_h: f64) -> Self {
        let type_sizes = label_w.map(|w| widget::IconLabelButton::size(w, label_h));

        // `homogeneous: true` (`screenshot.js:1300`) — every type button is as wide as the widest.
        let type_w = type_sizes.iter().fold(0f64, |acc, s| acc.max(s.w));
        let type_h = type_sizes.iter().fold(0f64, |acc, s| acc.max(s.h));
        let type_row_w = type_w * 3. + Self::TYPE_SPACING * 2.;

        let shot_cast_size = widget::Segmented::size(1);
        let show_pointer_d =
            widget::IconButton::diameter(widget::IconButton::ICON_PX, Self::SHOW_POINTER_PAD);
        let bottom_h = Self::CAPTURE_DIAMETER
            .max(shot_cast_size.h)
            .max(show_pointer_d);

        // The bottom row must fit its three children side by side even though they are stacked in
        // a bin — otherwise the pill and the toggle would overlap the capture button on a narrow
        // panel.
        let bottom_min_w =
            shot_cast_size.w + Self::CAPTURE_DIAMETER + show_pointer_d + Self::ROW_SPACING * 2.;
        let content_w = type_row_w.max(bottom_min_w);

        let size = Size::from((
            content_w + Self::PAD * 2.,
            Self::PAD + type_h + Self::ROW_SPACING + bottom_h + Self::PAD_BOTTOM,
        ));

        let type_y = Self::PAD;
        let type_x0 = Self::PAD + (content_w - type_row_w) / 2.;
        let type_buttons = [0usize, 1, 2].map(|i| {
            Rectangle::new(
                Point::from((type_x0 + (type_w + Self::TYPE_SPACING) * i as f64, type_y)),
                Size::from((type_w, type_h)),
            )
        });

        let bottom_y = Self::PAD + type_h + Self::ROW_SPACING;
        // Centre each child on the band, then align it start / centre / end.
        let centred = |w: f64, h: f64, x: f64| {
            Rectangle::new(
                Point::from((x, bottom_y + (bottom_h - h) / 2.)),
                Size::from((w, h)),
            )
        };
        let shot_cast = centred(shot_cast_size.w, shot_cast_size.h, Self::PAD);
        let capture = centred(
            Self::CAPTURE_DIAMETER,
            Self::CAPTURE_DIAMETER,
            (size.w - Self::CAPTURE_DIAMETER) / 2.,
        );
        let show_pointer = centred(
            show_pointer_d,
            show_pointer_d,
            size.w - Self::PAD - show_pointer_d,
        );

        Self {
            size,
            type_buttons,
            shot_cast,
            capture,
            show_pointer,
        }
    }

    /// The control at `p`, in panel-local logical coordinates.
    ///
    /// The capture button and the show-pointer toggle are **round**, so they are hit-tested as
    /// circles: a click in the corner of a circular button's box reads as a misfire, not as a
    /// generous target (the same rule [`widget::IconButton::contains`] follows).
    pub fn control_at(&self, p: Point<f64, Logical>) -> Option<Control> {
        let in_circle = |r: Rectangle<f64, Logical>| {
            let radius = r.size.w.min(r.size.h) / 2.;
            let cx = r.loc.x + r.size.w / 2.;
            let cy = r.loc.y + r.size.h / 2.;
            let (dx, dy) = (p.x - cx, p.y - cy);
            dx * dx + dy * dy <= radius * radius
        };

        if in_circle(self.capture) {
            return Some(Control::Capture);
        }
        if in_circle(self.show_pointer) {
            return Some(Control::ShowPointer);
        }
        for (i, ty) in CaptureType::ROW.iter().enumerate() {
            if ty.is_available() && self.type_buttons[i].contains(p) {
                return Some(Control::Type(*ty));
            }
        }
        if self.shot_cast.contains(p) {
            let seg = widget::Segmented::segment_rect(self.shot_cast, 0);
            if seg.contains(p) {
                return Some(Control::ShotCast(0));
            }
        }
        None
    }
}

fn panel_location(output_data: &OutputData, panel_size: Size<i32, Buffer>) -> Point<i32, Physical> {
    let scale = output_data.scale;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let x = max(0, (output_data.size.w - panel_size.w) / 2);
    let y = max(0, output_data.size.h - panel_size.h - padding * 2);
    Point::from((x, y))
}

fn is_within_capture_button(
    scale: f64,
    panel_size: Size<i32, Buffer>,
    pos_within_panel: Point<i32, Physical>,
) -> bool {
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let radius = to_physical_precise_round::<i32>(scale, RADIUS) - 2;

    let xc = padding + radius;
    let yc = panel_size.h / 2;
    let pos = pos_within_panel;

    (pos.x - xc) * (pos.x - xc) + (pos.y - yc) * (pos.y - yc) <= radius * radius
}

/// Compose the concentric capture-button bitmap: a white centre disk and a white outer ring with a
/// transparent gap between them (and outside), so it composites over the dark panel like the old
/// cairo shutter icon. A pure CPU distance test (no cairo); tagged at `scale` so it draws at the
/// same logical size regardless of the output scale.
fn button_bitmap(scale: f64) -> MemoryBuffer {
    let radius: i32 = to_physical_precise_round(scale, RADIUS);
    let stroke: i32 = to_physical_precise_round::<i32>(scale, 2).max(1);
    let side = (radius * 2).max(1);
    let d = side as usize;
    let c = f64::from(radius); // centre, since side == 2·radius
    let r = f64::from(radius);
    let ring_inner = f64::from((radius - stroke).max(0));
    let disk_outer = f64::from((radius - stroke * 2).max(0));

    // 1 below `edge`, 0 above, ~1px transition.
    let below = |dist: f64, edge: f64| (edge - dist + 0.5).clamp(0., 1.);
    // 1 within [inner, outer], 0 outside.
    let band = |dist: f64, inner: f64, outer: f64| {
        f64::min((dist - inner + 0.5).clamp(0., 1.), below(dist, outer))
    };

    let mut data = vec![0u8; d * d * 4];
    for y in 0..d {
        for x in 0..d {
            let dx = x as f64 + 0.5 - c;
            let dy = y as f64 + 0.5 - c;
            let dist = (dx * dx + dy * dy).sqrt();
            let cov = f64::max(below(dist, disk_outer), band(dist, ring_inner, r));
            // Premultiplied opaque white × coverage → all four channels equal.
            let v = (cov * 255.).round().clamp(0., 255.) as u8;
            let i = (y * d + x) * 4;
            data[i] = v;
            data[i + 1] = v;
            data[i + 2] = v;
            data[i + 3] = v;
        }
    }

    // The pixels are physical-sized (`radius` is already scaled), so tag the buffer at the output
    // scale — matching the panel texture — or it composites `scale`× too big on a HiDPI output.
    MemoryBuffer::new(
        data,
        Fourcc::Argb8888,
        Size::from((side, side)),
        Scale::from(scale),
        Transform::Normal,
    )
}

/// Draw the screenshot help panel straight into a `VkTexture`: a dark box with a grey border, the
/// concentric-circle capture button on the left, and two left-aligned help lines with `Space`/`P`
/// keycaps on grey patches. `verb` is the pointer line's verb ("show" or "hide"). No cairo/pango.
/// Physical `(margin, offset_y)` of the panel drop shadow at `scale`: the blur bleed (~3σ) and the
/// downward offset. The shadow buffer pads the panel footprint by `margin` on top/left/right and
/// `margin + offset_y` at the bottom, and the element is placed at `panel_location − (margin,
/// margin)`.
fn shadow_pad(scale: f64) -> (i32, i32) {
    let sigma = SHADOW_BLUR * scale / 2.;
    let margin = (sigma * 3.).ceil() as i32;
    let offset_y = (SHADOW_OFFSET_Y * scale).round() as i32;
    (margin, offset_y)
}

/// Bake the panel's drop shadow into its own transparent `VkTexture` via [`Painter::drop_shadow`],
/// sized to hold the blur bleed + offset around a `panel_size` card. Composited behind the panel.
fn generate_panel_shadow(
    renderer: &mut VulkanRenderer,
    scale: f64,
    panel_size: Size<i32, Physical>,
) -> anyhow::Result<VkTexture> {
    let (margin, offset_y) = shadow_pad(scale);
    let size = Size::<i32, Physical>::from((
        panel_size.w + margin * 2,
        panel_size.h + margin * 2 + offset_y,
    ));
    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear([0., 0., 0., 0.])?;
        // The panel footprint sits at (margin, margin) in the buffer; `drop_shadow` shifts it down
        // by the offset and blurs it. Logical coords (the verb re-multiplies by scale).
        let box_logical = Rectangle::new(
            Point::from((f64::from(margin) / scale, f64::from(margin) / scale)),
            Size::from((
                f64::from(panel_size.w) / scale,
                f64::from(panel_size.h) / scale,
            )),
        );
        p.drop_shadow(
            box_logical,
            PANEL_RADIUS,
            SHADOW_BLUR,
            (0., SHADOW_OFFSET_Y),
            SHADOW_COLOR,
        )?;
        Ok(())
    })
}

fn generate_panel(
    renderer: &mut VulkanRenderer,
    scale: f64,
    verb: &str,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("screenshot_ui::generate_panel");

    let px = (font_px() * scale) as f32;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let radius: i32 = to_physical_precise_round(scale, RADIUS);
    // 2 px between the two lines, matching the old cairo line spacing.
    let line_gap: i32 = to_physical_precise_round(scale, 2);
    // Breathing room around the grey keycap patches.
    let kpad_x = (px * 0.28).round() as i32;
    let kpad_y = (px * 0.12).round() as i32;

    // Each line is its own single-line run so the two share a left edge (the paragraph builder
    // center-aligns; we strip that per line via its ink-x). The keycap is span index 1.
    // `TextShaper` owns the pt → physical-px multiply — no `* scale` on the shaping path.
    let pointer_line = format!(" to {verb} the pointer.");
    const WRAP: f64 = 100_000.;
    let lines = [
        vec![
            ParagraphSpan::new("Press ", FONT_PT),
            ParagraphSpan::new(" Space ", FONT_PT).mono(),
            ParagraphSpan::new(" to save the screenshot.", FONT_PT),
        ],
        vec![
            ParagraphSpan::new("Press ", FONT_PT),
            ParagraphSpan::new(" P ", FONT_PT).mono(),
            ParagraphSpan::new(&pointer_line, FONT_PT),
        ],
    ];
    let runs = {
        let mut shaper = TextShaper::new(renderer, scale);
        lines
            .iter()
            .map(|spans| shaper.paragraph(spans, WRAP, FONT_PT))
            .collect::<Result<Vec<_>, _>>()?
    };
    let ink: Vec<(i32, i32, i32, i32)> = runs.iter().map(|r| r.ink_bounds()).collect();

    let text_w = ink.iter().map(|b| b.2).max().unwrap_or(0);
    let line_bottom = ink.iter().map(|b| b.1 + b.3).max().unwrap_or(0);
    let row_advance = line_bottom + line_gap;
    let text_h = row_advance * (lines.len() as i32 - 1) + line_bottom;

    let width = text_w + radius * 2 + padding * 3;
    let height = max(text_h, radius * 2) + padding * 2;
    let text_x = radius * 2 + padding * 2;

    let size = Size::<i32, Physical>::from((width, height));
    let full = Rectangle::from_size(size);

    // A rounded `%osd_panel` card: transparent-cleared, filled to `PANEL_RADIUS`, then a 1px
    // white@2% inset border stroke. The capture button and the drop shadow are composited as
    // separate elements over/under it (see `OutputData::button_element` and the `Shadow` element
    // in `render_output`), so the buffer stays exactly panel-sized.
    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear([0., 0., 0., 0.])?;
        p.fill_rounded_full(PANEL_RADIUS, PANEL_BG)?;
        p.stroke_rounded_full(PANEL_RADIUS, 1., PANEL_BORDER_COLOR)?;

        // Two help lines, left-aligned at `text_x`.
        for (i, run) in runs.iter().enumerate() {
            let (ix, _, _, _) = ink[i];
            let y_line = padding + i as i32 * row_advance;
            let origin = Point::<i32, Physical>::from((text_x - ix, y_line));

            // Grey keycap patch behind span 1 (" Space " / " P ").
            let (sx, sy, sw, sh) = run.span_ink_bounds(1);
            if sw > 0 && sh > 0 {
                let patch = Rectangle::new(
                    Point::from((origin.x + sx - kpad_x, origin.y + sy - kpad_y)),
                    Size::from((sw + kpad_x * 2, sh + kpad_y * 2)),
                );
                if let Some(patch) = patch.intersection(full) {
                    p.fill_rect_px(patch, KEYCAP_BG)?;
                }
            }
            p.paragraph(run, origin, TEXT_COLOR)?;
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Plausible caption metrics at scale 1 — the three labels differ in width, which is what makes
    // the homogeneous rule observable.
    const LABEL_W: [f64; 3] = [58., 42., 48.];
    const LABEL_H: f64 = 13.;

    fn layout() -> PanelLayout {
        PanelLayout::new(LABEL_W, LABEL_H)
    }

    // GNOME's bottom row is a BinLayout (`screenshot.js:1350`), so the capture button is centred on
    // the *panel*, with the pill and the toggle aligned to the ends of the same band. Laying the
    // three out in sequence — the reflex, and what a BoxLayout would give — puts the capture button
    // wherever the pill's width leaves it, which is visibly off-centre and the whole reason this
    // arithmetic exists.
    #[test]
    fn the_capture_button_is_centred_on_the_panel_not_after_the_pill() {
        let l = layout();

        let capture_centre = l.capture.loc.x + l.capture.size.w / 2.;
        assert!(
            (capture_centre - l.size.w / 2.).abs() < 0.001,
            "capture button at {capture_centre}, panel centre {}",
            l.size.w / 2.
        );

        // The pill starts at the left padding and the toggle ends at the right padding.
        assert_eq!(l.shot_cast.loc.x, PanelLayout::PAD);
        assert_eq!(
            l.show_pointer.loc.x + l.show_pointer.size.w,
            l.size.w - PanelLayout::PAD
        );

        // All three share the band, so their vertical centres agree.
        let mid = |r: Rectangle<f64, Logical>| r.loc.y + r.size.h / 2.;
        assert!((mid(l.capture) - mid(l.shot_cast)).abs() < 0.001);
        assert!((mid(l.capture) - mid(l.show_pointer)).abs() < 0.001);

        // ...and they do not overlap, which a bin layout alone would not guarantee.
        assert!(l.shot_cast.loc.x + l.shot_cast.size.w <= l.capture.loc.x);
        assert!(l.capture.loc.x + l.capture.size.w <= l.show_pointer.loc.x);
    }

    // `homogeneous: true` (`screenshot.js:1300`): every type button takes the width of the widest,
    // so the row does not jitter as the captions change with translation.
    #[test]
    fn the_type_buttons_are_homogeneous_and_evenly_spaced() {
        let l = layout();

        let w = l.type_buttons[0].size.w;
        for b in &l.type_buttons {
            assert_eq!(b.size.w, w);
            assert_eq!(b.size.h, l.type_buttons[0].size.h);
            assert_eq!(b.loc.y, PanelLayout::PAD);
        }
        for i in 1..3 {
            assert_eq!(
                l.type_buttons[i].loc.x - l.type_buttons[i - 1].loc.x,
                w + PanelLayout::TYPE_SPACING
            );
        }

        // The widest caption sets the width, not the first one.
        let widest = LABEL_W.iter().cloned().fold(0f64, f64::max);
        assert_eq!(w, widget::IconLabelButton::size(widest, LABEL_H).w);
    }

    // A click in the corner of a circular button's bounding box is an inch from any drawn pixel.
    // Treating it as a hit is the kind of generosity that fires the shutter when the user meant to
    // drag a selection.
    #[test]
    fn the_round_controls_are_hit_tested_as_circles() {
        let l = layout();

        let centre = |r: Rectangle<f64, Logical>| {
            Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
        };
        assert_eq!(l.control_at(centre(l.capture)), Some(Control::Capture));
        assert_eq!(
            l.control_at(centre(l.show_pointer)),
            Some(Control::ShowPointer)
        );

        // The box corner of the capture button is outside the circle.
        let corner = Point::from((l.capture.loc.x + 1., l.capture.loc.y + 1.));
        assert!(l.capture.contains(corner));
        assert_ne!(l.control_at(corner), Some(Control::Capture));
    }

    // Window mode is laid out but not offered yet, and a button nobody can see must not be
    // clickable — an invisible hit region that silently switches modes is worse than no button.
    #[test]
    fn the_unavailable_window_button_does_not_take_clicks() {
        let l = layout();

        let centre_of = |i: usize| {
            let r = l.type_buttons[i];
            Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
        };
        assert_eq!(
            l.control_at(centre_of(0)),
            Some(Control::Type(CaptureType::Selection))
        );
        assert_eq!(
            l.control_at(centre_of(1)),
            Some(Control::Type(CaptureType::Screen))
        );
        assert_eq!(l.control_at(centre_of(2)), None);
    }
}
