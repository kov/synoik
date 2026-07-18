use std::cell::RefCell;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;

use niri_config::{Action, Config};
use niri_ipc::SizeChange;
use niri_vk::text::{SpanFamily, TextSpan};
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::TouchSlot;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind as _, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::output::{Output, WeakOutput};
use smithay::utils::{
    Buffer as BufferCoord, Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use crate::animation::{Animation, Clock};
use crate::layout::floating::DIRECTIONAL_MOVE_PX;
use crate::niri_render_elements;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::RenderTarget;
use crate::utils::to_physical_precise_round;

/// Per-element cache of a neutral CPU buffer uploaded once to a `VkTexture` (see
/// [`upload_cached`]).
type VkCache = RefCell<Option<TextureBuffer<VkTexture>>>;

const SELECTION_BORDER: i32 = 2;

const PADDING: i32 = 8;
const RADIUS: i32 = 16;
const FONT_PX: f64 = 14.;
const BORDER: i32 = 4;

/// Dark panel background, grey border, and the grey keycap patch (`#2C2C2C`).
const PANEL_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const PANEL_BORDER_COLOR: [f32; 4] = [0.3, 0.3, 0.3, 1.];
const KEYCAP_BG: [f32; 4] = [0.172, 0.172, 0.172, 1.];
const TEXT_COLOR: [f32; 4] = [1., 1., 1., 1.];

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
        *self.panel.borrow_mut() = PanelCache {
            scale,
            context: Some(context),
            show,
            hide,
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
fn generate_panel(
    renderer: &mut VulkanRenderer,
    scale: f64,
    verb: &str,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("screenshot_ui::generate_panel");

    let px = (FONT_PX * scale) as f32;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    // Keep the border width even to avoid blurry edges.
    let border_width = ((f64::from(BORDER) / 2. * scale).round() as i32 * 2).max(2);
    let half_border_width = border_width / 2;
    let radius: i32 = to_physical_precise_round(scale, RADIUS);
    // 2 px between the two lines, matching the old cairo line spacing.
    let line_gap: i32 = to_physical_precise_round(scale, 2);
    // Breathing room around the grey keycap patches.
    let kpad_x = (px * 0.28).round() as i32;
    let kpad_y = (px * 0.12).round() as i32;

    fn sans(text: &str, px: f32) -> TextSpan<'_> {
        TextSpan {
            text,
            family: SpanFamily::Sans,
            bold: false,
            px,
        }
    }
    fn mono(text: &str, px: f32) -> TextSpan<'_> {
        TextSpan {
            text,
            family: SpanFamily::Mono,
            bold: false,
            px,
        }
    }

    // Each line is its own single-line run so the two share a left edge (the paragraph builder
    // center-aligns; we strip that per line via its ink-x). The keycap is span index 1.
    let pointer_line = format!(" to {verb} the pointer.");
    const WRAP: f32 = 100_000.;
    let lines = [
        vec![
            sans("Press ", px),
            mono(" Space ", px),
            sans(" to save the screenshot.", px),
        ],
        vec![sans("Press ", px), mono(" P ", px), sans(&pointer_line, px)],
    ];
    let runs = lines
        .iter()
        .map(|spans| renderer.build_glyph_paragraph(spans, WRAP, px))
        .collect::<Result<Vec<_>, _>>()?;
    let ink: Vec<(i32, i32, i32, i32)> = runs.iter().map(|r| r.ink_bounds()).collect();

    let text_w = ink.iter().map(|b| b.2).max().unwrap_or(0);
    let line_bottom = ink.iter().map(|b| b.1 + b.3).max().unwrap_or(0);
    let row_advance = line_bottom + line_gap;
    let text_h = row_advance * (lines.len() as i32 - 1) + line_bottom;

    let width = text_w + padding + radius * 2 + padding - half_border_width + padding;
    let height = max(text_h, radius * 2) + padding * 2;
    let text_x = padding + radius * 2 + padding - half_border_width;

    let size = Size::<i32, Physical>::from((width, height));
    let full = Rectangle::from_size(size);
    let inner = Rectangle::new(
        Point::from((half_border_width, half_border_width)),
        Size::from((
            (width - half_border_width * 2).max(0),
            (height - half_border_width * 2).max(0),
        )),
    );

    // The capture button is composited as a separate element over this panel (see
    // `OutputData::button_element`); the offscreen quad pipelines misrender here, and only the
    // clear + glyph paths are reliable from a hand-bound offscreen.
    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((width, height)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        // Grey border = whole box grey, then the inner rect dark.
        frame.clear(Color32F::from(PANEL_BORDER_COLOR), &[full])?;
        frame.clear(Color32F::from(PANEL_BG), &[inner])?;

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
                if let Some(patch) = patch.intersection(inner) {
                    frame.clear(Color32F::from(KEYCAP_BG), &[patch])?;
                }
            }
            frame.render_glyphs(run, origin, TEXT_COLOR, full, &[full])?;
        }

        let _sync = frame.finish()?;
    }
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}
