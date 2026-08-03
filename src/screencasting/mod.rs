use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::time::Duration;

use anyhow::Context as _;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::desktop::Window;
use smithay::output::{Output, WeakOutput};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use zbus::object_server::SignalEmitter;

use crate::dbus::mutter_screen_cast::{self, CursorMode, ScreenCastToNiri, StreamTargetId};
use crate::niri::{CastTarget, Niri, OutputRenderElements, PointerRenderElements, State};
use crate::niri_render_elements;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};
use crate::window::mapped::{Mapped, MappedId, WindowCastRenderElements};

mod pw_utils;
use pw_utils::{Cast, CastSizeChange, CursorData, PipeWire, PwToNiri};

use crate::render_helpers::vulkan::VulkanRenderer;

pub struct Screencasting {
    pub casts: Vec<Cast>,

    /// Active screen *recordings* (casts started with `is-recording`), the source of truth for the
    /// R1 panel indicator. Distinct from `casts`: a plain portal capture is a cast but not a
    /// recording. One entry per recording session.
    pub recordings: Vec<ScreenRecording>,

    /// Dynamic-target casts waiting for their first target to start.
    pub pending_dynamic_casts: Vec<PendingCast>,

    pub pw_to_niri: calloop::channel::Sender<PwToNiri>,

    /// Screencast output for each mapped window.
    pub mapped_cast_output: HashMap<Window, Output>,

    /// Window ID for the "dynamic cast" special window for the xdp-gnome picker.
    pub dynamic_cast_id_for_portal: MappedId,

    // Drop PipeWire last, and specifically after casts, to prevent a double-free (yay).
    pub pipewire: Option<PipeWire>,
}

/// A live screen recording, tracked for the R1 panel indicator.
pub struct ScreenRecording {
    pub session_id: CastSessionId,
    /// Monotonic time the recording started, for the `M:SS` elapsed label.
    pub started_at: Duration,
    pub kind: RecordingKind,
}

/// How a recording is driven and stopped.
pub enum RecordingKind {
    /// A screencast started with the `is-recording` property (gnome-shell's recorder path).
    /// Stopped by tearing down the cast via [`Niri::stop_cast`].
    External,
    /// Our own recorder: the compositor captures frames and feeds an encoder. Stopped by
    /// finalizing the encoder.
    Native(NativeRecording),
}

/// State for a compositor-driven ("native") recording.
pub struct NativeRecording {
    /// Output being captured.
    output: WeakOutput,
    /// The encoder worker; captured frames are pushed here, and `finish()` finalizes the file.
    recorder: crate::recording::encoder::ThreadedRecorder,
    /// Physical capture size: the whole output (mode, transformed) or, for an area recording, the
    /// cropped region — rounded to even dimensions for 4:2:0 encoding.
    size: Size<i32, Physical>,
    /// Output fractional scale.
    scale: Scale<f64>,
    /// The recorded region in global logical coordinates, for `ScreencastArea`. `None` records the
    /// whole output. Used per frame to shift output-local content into the cropped buffer.
    crop: Option<Rectangle<i32, Logical>>,
    /// Whether to composite the pointer into the recording (the `draw-cursor` option).
    draw_cursor: bool,
    /// Where the finished file lands.
    path: std::path::PathBuf,
    /// Presentation time of the last captured frame, for framerate pacing.
    last_frame_time: Duration,
    /// Minimum spacing between captured frames (the target framerate).
    frame_interval: Duration,
    /// A pending self-driven redraw that keeps frames flowing while the output is otherwise idle.
    /// Unlike a screencast (whose consumer pulls frames), a recording has no external driver, so
    /// it schedules its own redraws at the frame cadence.
    scheduled_redraw: Option<RegistrationToken>,
}

/// A screencast request that hasn't been started yet.
pub struct PendingCast {
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    pub cursor_mode: CursorMode,
    pub signal_ctx: SignalEmitter<'static>,
}

impl Screencasting {
    pub fn new(event_loop: &LoopHandle<'static, State>) -> Self {
        let pw_to_niri = {
            let (pw_to_niri, from_pipewire) = calloop::channel::channel();
            event_loop
                .insert_source(from_pipewire, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_pw_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            pw_to_niri
        };

        Self {
            casts: vec![],
            recordings: vec![],
            pending_dynamic_casts: vec![],
            pw_to_niri,
            mapped_cast_output: HashMap::new(),
            dynamic_cast_id_for_portal: MappedId::next(),
            pipewire: None,
        }
    }
}

impl State {
    fn prepare_pw_cast(&mut self) -> anyhow::Result<(GbmDevice<DrmDeviceFd>, FormatSet)> {
        let gbm = self
            .backend
            .gbm_device()
            .context("no GBM device available")?;

        // Ensure PipeWire is initialized.
        if self.niri.casting.pipewire.is_none() {
            let pw = PipeWire::new(
                self.niri.event_loop.clone(),
                self.niri.casting.pw_to_niri.clone(),
            )
            .context("error initializing PipeWire")?;
            self.niri.casting.pipewire = Some(pw);
        }

        // Offer the formats the renderer that will actually render into the negotiated buffers can
        // bind. The owned Vulkan renderer imports a narrow set (the four 8888 byte orders, LINEAR
        // only), so offering anything wider would let a consumer pick a modifier we then fail to
        // bind on every single frame.
        let render_formats: FormatSet = crate::render_helpers::vulkan::dmabuf_formats();

        Ok((gbm, render_formats))
    }

    pub fn on_pw_msg(&mut self, msg: PwToNiri) {
        match msg {
            PwToNiri::StopCast { session_id } => self.niri.stop_cast(session_id),
            PwToNiri::Redraw { stream_id } => self.redraw_cast(stream_id),
            PwToNiri::FatalError => {
                warn!("stopping PipeWire due to fatal error");
                let casting = &mut self.niri.casting;
                if let Some(pw) = casting.pipewire.take() {
                    let mut ids = HashSet::new();
                    for cast in &casting.pending_dynamic_casts {
                        ids.insert(cast.session_id);
                    }
                    for cast in &casting.casts {
                        ids.insert(cast.session_id);
                    }
                    for id in ids {
                        self.niri.stop_cast(id);
                    }
                    self.niri.event_loop.remove(pw.token);
                }
            }
        }
    }

    fn redraw_cast(&mut self, stream_id: CastStreamId) {
        let _span = tracy_client::span!("State::redraw_cast");

        let casts = &mut self.niri.casting.casts;
        let Some(idx) = casts.iter().position(|cast| cast.stream_id == stream_id) else {
            warn!("cast to redraw is missing");
            return;
        };
        let cast = &mut casts[idx];

        let id = match &cast.target {
            CastTarget::Nothing => {
                let cleared = self
                    .backend
                    .with_vulkan_renderer(|renderer| cast.dequeue_buffer_and_clear(renderer));

                if cleared == Some(true) {
                    cast.last_frame_time = get_monotonic_time();
                }
                return;
            }
            CastTarget::Output { output, .. } | CastTarget::Area { output, .. } => {
                if let Some(output) = output.upgrade() {
                    self.niri.queue_redraw(&output);
                }
                return;
            }
            CastTarget::Window { id } => *id,
        };

        // Lack of partial borrowing strikes again...
        let mut casts = mem::take(&mut self.niri.casting.casts);
        let cast = &mut casts[idx];
        let mut stop = false;
        // Use a loop {} so we can break instead of early-return.
        #[allow(clippy::never_loop)]
        loop {
            let mut windows = self.niri.layout.windows();
            let Some((_, mapped)) = windows.find(|(_, mapped)| mapped.id().get() == id) else {
                break;
            };

            // Use the cached output since it will be present even if the output was
            // currently disconnected.
            let Some(output) = self.niri.casting.mapped_cast_output.get(&mapped.window) else {
                break;
            };

            let scale = Scale::from(output.current_scale().fractional_scale());
            let bbox = mapped
                .window
                .bbox_with_popups()
                .to_physical_precise_up(scale);

            match cast.ensure_size(bbox.size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => break,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    stop = true;
                    break;
                }
            }

            let rendered = self.backend.with_vulkan_renderer(|renderer| {
                self.niri
                    .redraw_window_cast_with(renderer, cast, mapped, output, bbox, scale);
            });

            if rendered.is_none() {
                warn!("no renderer available to redraw the window cast");
            }

            break;
        }
        let session_id = cast.session_id;
        self.niri.casting.casts = casts;

        if stop {
            self.niri.stop_cast(session_id);
        }
    }

    pub fn set_dynamic_cast_target(&mut self, target: CastTarget) {
        let _span = tracy_client::span!("State::set_dynamic_cast_target");

        let mut refresh = None;
        match &target {
            // Leave refresh as is when clearing. Chances are, the next refresh will match it,
            // then we'll avoid reconfiguring.
            CastTarget::Nothing => (),
            CastTarget::Output { output, .. } | CastTarget::Area { output, .. } => {
                if let Some(output) = output.upgrade() {
                    refresh = Some(output.current_mode().unwrap().refresh as u32);
                }
            }
            CastTarget::Window { id } => {
                let mut windows = self.niri.layout.windows();
                if let Some((_, mapped)) = windows.find(|(_, mapped)| mapped.id().get() == *id) {
                    if let Some(output) = self.niri.casting.mapped_cast_output.get(&mapped.window) {
                        refresh = Some(output.current_mode().unwrap().refresh as u32);
                    }
                }
            }
        }

        let mut to_redraw = Vec::new();
        let mut to_stop = Vec::new();
        for cast in &mut self.niri.casting.casts {
            if !cast.dynamic_target {
                continue;
            }

            if let Some(refresh) = refresh {
                if let Err(err) = cast.set_refresh(refresh) {
                    warn!("error changing cast FPS: {err:?}");
                    to_stop.push(cast.session_id);
                    continue;
                }
            }

            cast.target = target.clone();
            to_redraw.push(cast.stream_id);
        }

        for id in to_redraw {
            self.redraw_cast(id);
        }

        // Start any pending dynamic casts if we have a real target.
        if !matches!(target, CastTarget::Nothing) {
            self.start_pending_dynamic_casts(&target);
        }
    }

    fn start_pending_dynamic_casts(&mut self, target: &CastTarget) {
        let pending = &self.niri.casting.pending_dynamic_casts;
        if pending.is_empty() {
            return;
        }
        debug!("starting {} pending dynamic cast(s)", pending.len());

        let _span = tracy_client::span!("State::start_pending_dynamic_casts");

        // We don't stop dynamic casts on missing output/window.
        let (size, refresh) = match target {
            CastTarget::Nothing => panic!("dynamic cast starting target must not be Nothing"),
            CastTarget::Output { output, .. } => {
                let Some(output) = output.upgrade() else {
                    return;
                };
                cast_params_for_output(&output)
            }
            CastTarget::Window { id } => {
                let Some((size, refresh)) = self.niri.cast_params_for_window(*id) else {
                    return;
                };
                (size, refresh)
            }
            // Area casts are never dynamic-target, so they never start this way.
            CastTarget::Area { .. } => return,
        };

        let (gbm, render_formats) = match self.prepare_pw_cast() {
            Ok(x) => x,
            Err(err) => {
                warn!("error starting pending screencasts: {err:?}");
                let mut ids = HashSet::new();
                for pending in self.niri.casting.pending_dynamic_casts.drain(..) {
                    ids.insert(pending.session_id);
                }
                for id in ids {
                    self.niri.stop_cast(id);
                }
                return;
            }
        };
        let pw = self.niri.casting.pipewire.as_ref().unwrap();

        // Alpha is always true since the dynamic target can change between window & output.
        let alpha = true;

        // Start each pending cast.
        let mut to_stop = HashSet::new();
        for pending in self.niri.casting.pending_dynamic_casts.drain(..) {
            let res = pw.start_cast(
                gbm.clone(),
                render_formats.clone(),
                pending.session_id,
                pending.stream_id,
                target.clone(),
                size,
                refresh,
                alpha,
                pending.cursor_mode,
                pending.signal_ctx,
            );
            match res {
                Ok(mut cast) => {
                    cast.dynamic_target = true;
                    self.niri.casting.casts.push(cast);
                }
                Err(err) => {
                    warn!("error starting pending screencast: {err:?}");
                    to_stop.insert(pending.session_id);
                }
            }
        }

        for session_id in to_stop {
            self.niri.stop_cast(session_id);
        }
    }

    pub fn on_screen_cast_msg(&mut self, msg: ScreenCastToNiri) {
        match msg {
            ScreenCastToNiri::StartCast {
                session_id,
                stream_id,
                target,
                cursor_mode,
                is_recording,
                signal_ctx,
            } => {
                let _span = tracy_client::span!("StartCast");
                let _span = debug_span!("StartCast", %session_id, %stream_id).entered();

                let (target, size, refresh, alpha) = match target {
                    StreamTargetId::Output { name } => {
                        let global_space = &self.niri.global_space;
                        let output = global_space.outputs().find(|out| out.name() == name);
                        let Some(output) = output else {
                            warn!("error starting screencast: requested output is missing");
                            self.niri.stop_cast(session_id);
                            return;
                        };

                        let (size, refresh) = cast_params_for_output(output);
                        (CastTarget::output(output), size, refresh, false)
                    }
                    StreamTargetId::Window { id }
                        if id == self.niri.casting.dynamic_cast_id_for_portal.get() =>
                    {
                        debug!("delaying dynamic cast until target is set");
                        self.niri.casting.pending_dynamic_casts.push(PendingCast {
                            session_id,
                            stream_id,
                            cursor_mode,
                            signal_ctx,
                        });
                        return;
                    }
                    StreamTargetId::Window { id } => {
                        let Some((size, refresh)) = self.niri.cast_params_for_window(id) else {
                            warn!("error starting screencast: requested window is missing");
                            self.niri.stop_cast(session_id);
                            return;
                        };
                        (CastTarget::Window { id }, size, refresh, true)
                    }
                    StreamTargetId::Area { x, y, w, h } => {
                        let rect = Rectangle::new(Point::from((x, y)), Size::from((w, h)));
                        let Some((target, size, refresh)) = self.niri.cast_params_for_area(rect)
                        else {
                            warn!("error starting screencast: requested area is off all outputs");
                            self.niri.stop_cast(session_id);
                            return;
                        };
                        (target, size, refresh, false)
                    }
                };

                let (gbm, render_formats) = match self.prepare_pw_cast() {
                    Ok(x) => x,
                    Err(err) => {
                        warn!("error starting screencast: {err:?}");
                        self.niri.stop_cast(session_id);
                        return;
                    }
                };
                let pw = self.niri.casting.pipewire.as_ref().unwrap();

                let res = pw.start_cast(
                    gbm,
                    render_formats,
                    session_id,
                    stream_id,
                    target,
                    size,
                    refresh,
                    alpha,
                    cursor_mode,
                    signal_ctx,
                );
                match res {
                    Ok(cast) => {
                        self.niri.casting.casts.push(cast);
                        if is_recording {
                            self.niri.screen_recording_started(session_id);
                        }
                    }
                    Err(err) => {
                        warn!("error starting screencast: {err:?}");
                        self.niri.stop_cast(session_id);
                    }
                }
            }
            ScreenCastToNiri::StopCast { session_id } => self.niri.stop_cast(session_id),
            ScreenCastToNiri::StopStream { stream_id } => self.niri.stop_stream(stream_id),
        }
    }
}

impl Niri {
    /// Build the elements for a window cast and push one frame to its PipeWire stream.
    ///
    /// Split out of `State::redraw_cast` so the renderer can be chosen at the call site: a Vulkan
    /// session must cast through the owned renderer, not the co-resident GLES one.
    fn redraw_window_cast_with(
        &self,
        renderer: &mut VulkanRenderer,
        cast: &mut Cast,
        mapped: &Mapped,
        output: &Output,
        bbox: Rectangle<i32, Physical>,
        scale: Scale<f64>,
    ) {
        let mut elements = Vec::new();
        let mut pointer_location = Point::default();

        if self.pointer_visibility.is_visible() {
            if let Some((pointer_pos, win_pos)) = self.pointer_pos_for_window_cast(mapped) {
                // Pointer location must be relative to the screencast buffer.
                // - win_pos is the position of the main window surface in output-local coordinates
                // - bbox.loc moves us relative to the screencast buffer
                let buf_pos = win_pos + bbox.loc.to_f64().to_logical(scale);
                let output_pos = self.global_space.output_geometry(output).unwrap().loc;
                pointer_location = pointer_pos - output_pos.to_f64() - buf_pos;

                let pos = buf_pos.to_physical_precise_round(scale).upscale(-1);
                self.render_pointer(renderer, output, &mut |elem| {
                    let elem = RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                    elements.push(CastRenderElement::from(elem));
                });
            }
        }

        let main_start = elements.len();
        mapped.render_for_screen_cast(renderer, scale, &mut |elem| {
            elements.push(CastRenderElement::from(elem))
        });

        let cursor_data = CursorData::compute(&elements, main_start, pointer_location, scale);

        if cast.dequeue_buffer_and_render(renderer, &elements, &cursor_data, bbox.size, scale) {
            cast.last_frame_time = get_monotonic_time();
        }
    }

    pub fn refresh_mapped_cast_window_rules(&mut self) {
        // O(N^2) but should be fine since there aren't many casts usually.
        self.layout.with_windows_mut(|mapped, _| {
            let id = mapped.id().get();
            // Find regardless of cast.is_active.
            let value = self
                .casting
                .casts
                .iter()
                .any(|cast| cast.target == (CastTarget::Window { id }));
            mapped.set_is_window_cast_target(value);
        });
    }

    pub fn refresh_mapped_cast_outputs(&mut self) {
        let mut seen = HashSet::new();
        let mut output_changed = vec![];

        self.layout.with_windows(|mapped, output, _, _| {
            seen.insert(mapped.window.clone());

            let Some(output) = output else {
                return;
            };

            match self.casting.mapped_cast_output.entry(mapped.window.clone()) {
                Entry::Occupied(mut entry) => {
                    if entry.get() != output {
                        entry.insert(output.clone());
                        output_changed.push((mapped.id(), output.clone()));
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(output.clone());
                }
            }
        });

        self.casting
            .mapped_cast_output
            .retain(|win, _| seen.contains(win));

        let mut to_stop = vec![];
        for (id, out) in output_changed {
            let refresh = out.current_mode().unwrap().refresh as u32;
            let target = CastTarget::Window { id: id.get() };
            for cast in self
                .casting
                .casts
                .iter_mut()
                .filter(|cast| cast.target == target)
            {
                if let Err(err) = cast.set_refresh(refresh) {
                    warn!("error changing cast FPS: {err:?}");
                    to_stop.push(cast.session_id);
                };
            }
        }

        for session_id in to_stop {
            self.stop_cast(session_id);
        }
    }

    pub fn render_for_screen_cast(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        let _span = tracy_client::span!("Niri::render_for_screen_cast");

        let weak = output.downgrade();
        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let scale = Scale::from(output.current_scale().fractional_scale());

        let mut elements = Vec::new();
        let mut cursor_data = None;

        let mut casts_to_stop = vec![];

        let mut casts = mem::take(&mut self.casting.casts);
        for cast in &mut casts {
            if !cast.is_active() {
                continue;
            }

            // Only full-output casts render here. Area casts share the same output but
            // crop to a sub-rect at a different size — they are driven by
            // render_area_for_screen_cast. matches_output() also matches Area (used by
            // stop_casts_for_target), so filter to Output explicitly or the two passes
            // fight over ensure_size every frame and the stream never stabilizes.
            let CastTarget::Output {
                output: cast_output,
                ..
            } = &cast.target
            else {
                continue;
            };
            if cast_output != &weak {
                continue;
            }

            match cast.ensure_size(size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => continue,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    casts_to_stop.push(cast.session_id);
                }
            }

            if cast.check_time_and_schedule(output, target_presentation_time) {
                continue;
            }

            if cursor_data.is_none() {
                let mut pointer_pos = Point::default();
                if self.pointer_visibility.is_visible() {
                    let output_geo = self.global_space.output_geometry(output).unwrap().to_f64();
                    let pointer_loc = self
                        .tablet_cursor_location
                        .unwrap_or_else(|| self.seat.get_pointer().unwrap().current_location());
                    // Only render when the pointer is within the output. Otherwise, it will
                    // happily appear anywhere outside the output video source in OBS.
                    if output_geo.contains(pointer_loc) {
                        pointer_pos = pointer_loc - output_geo.loc;
                        self.render_pointer(renderer, output, &mut |elem| {
                            elements.push(elem.into())
                        });
                    }
                }

                let main_start = elements.len();
                let ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Screencast,
                    xray: None,
                };
                self.render(ctx, output, false, &mut |elem| elements.push(elem.into()));

                cursor_data = Some(CursorData::compute(
                    &elements,
                    main_start,
                    pointer_pos,
                    scale,
                ));
            }
            let cursor_data = cursor_data.as_ref().unwrap();

            if cast.dequeue_buffer_and_render(renderer, &elements, cursor_data, size, scale) {
                cast.last_frame_time = target_presentation_time;
            }
        }
        self.casting.casts = casts;

        for id in casts_to_stop {
            self.stop_cast(id);
        }
    }

    pub fn render_windows_for_screen_cast(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        let _span = tracy_client::span!("Niri::render_windows_for_screen_cast");

        let scale = Scale::from(output.current_scale().fractional_scale());

        let mut casts_to_stop = vec![];

        let mut casts = mem::take(&mut self.casting.casts);
        for cast in &mut casts {
            if !cast.is_active() {
                continue;
            }

            let CastTarget::Window { id } = cast.target else {
                continue;
            };

            let mut windows = self.layout.windows_for_output(output);
            let Some(mapped) = windows.find(|win| win.id().get() == id) else {
                continue;
            };

            let bbox = mapped
                .window
                .bbox_with_popups()
                .to_physical_precise_up(scale);

            match cast.ensure_size(bbox.size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => continue,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    casts_to_stop.push(cast.session_id);
                }
            }

            if cast.check_time_and_schedule(output, target_presentation_time) {
                continue;
            }

            let mut elements = Vec::new();
            let mut pointer_location = Point::default();

            if self.pointer_visibility.is_visible() {
                if let Some((pointer_pos, win_pos)) = self.pointer_pos_for_window_cast(mapped) {
                    // Pointer location must be relative to the screencast buffer.
                    // - win_pos is the position of the main window surface in output-local
                    //   coordinates
                    // - bbox.loc moves us relative to the screencast buffer
                    let buf_pos = win_pos + bbox.loc.to_f64().to_logical(scale);
                    let output_pos = self.global_space.output_geometry(output).unwrap().loc;
                    pointer_location = pointer_pos - output_pos.to_f64() - buf_pos;

                    let pos = buf_pos.to_physical_precise_round(scale).upscale(-1);
                    self.render_pointer(renderer, output, &mut |elem| {
                        let elem =
                            RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                        elements.push(CastRenderElement::from(elem));
                    });
                }
            }

            let main_start = elements.len();
            mapped.render_for_screen_cast(renderer, scale, &mut |elem| {
                elements.push(CastRenderElement::from(elem))
            });

            let cursor_data = CursorData::compute(&elements, main_start, pointer_location, scale);

            if cast.dequeue_buffer_and_render(renderer, &elements, &cursor_data, bbox.size, scale) {
                cast.last_frame_time = target_presentation_time;
            }
        }
        self.casting.casts = casts;

        for id in casts_to_stop {
            self.stop_cast(id);
        }
    }

    /// Render area screencasts of this output: the output's content, cropped to the recorded
    /// rectangle.
    ///
    /// Reuses the same `RenderTarget::Screencast` element list as a monitor cast (so block-out /
    /// privacy semantics hold with no new capture path), shifted so the area's top-left maps to the
    /// buffer origin — the `RelocateRenderElement` crop is the same trick the cursor and window
    /// paths already use.
    pub fn render_area_for_screen_cast(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        let _span = tracy_client::span!("Niri::render_area_for_screen_cast");

        let weak = output.downgrade();
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_geo = self.global_space.output_geometry(output).unwrap();

        let mut casts_to_stop = vec![];

        let mut casts = mem::take(&mut self.casting.casts);
        for cast in &mut casts {
            if !cast.is_active() {
                continue;
            }

            let CastTarget::Area {
                output: cast_output,
                rect,
                ..
            } = &cast.target
            else {
                continue;
            };
            if cast_output != &weak {
                continue;
            }
            let rect = *rect;

            let size = rect.size.to_physical_precise_round(scale);

            match cast.ensure_size(size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => continue,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    casts_to_stop.push(cast.session_id);
                    continue;
                }
            }

            if cast.check_time_and_schedule(output, target_presentation_time) {
                continue;
            }

            // Shift output-local content so the area's top-left maps to the buffer origin.
            let neg_offset = area_crop_offset(rect, output_geo, scale).upscale(-1);

            let mut elements = Vec::new();
            let mut pointer_location = Point::default();

            if self.pointer_visibility.is_visible() {
                let pointer_loc = self
                    .tablet_cursor_location
                    .unwrap_or_else(|| self.seat.get_pointer().unwrap().current_location());
                // Only render the pointer when it's within the recorded area.
                if rect.to_f64().contains(pointer_loc) {
                    pointer_location = pointer_loc - rect.loc.to_f64();
                    self.render_pointer(renderer, output, &mut |elem| {
                        let elem = RelocateRenderElement::from_element(
                            elem,
                            neg_offset,
                            Relocate::Relative,
                        );
                        elements.push(CastRenderElement::from(elem));
                    });
                }
            }

            let main_start = elements.len();
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Screencast,
                xray: None,
            };
            self.render(ctx, output, false, &mut |elem| {
                let elem =
                    RelocateRenderElement::from_element(elem, neg_offset, Relocate::Relative);
                elements.push(CastRenderElement::from(elem));
            });

            let cursor_data = CursorData::compute(&elements, main_start, pointer_location, scale);

            if cast.dequeue_buffer_and_render(renderer, &elements, &cursor_data, size, scale) {
                cast.last_frame_time = target_presentation_time;
            }
        }
        self.casting.casts = casts;

        for id in casts_to_stop {
            self.stop_cast(id);
        }
    }

    /// Stop one stream of a session, leaving the session and its other streams alone —
    /// `org.gnome.Mutter.ScreenCast.Stream.Stop`.
    ///
    /// Not a thin wrapper around [`Self::stop_cast`]: a session may carry several streams (a
    /// browser sharing two monitors is one session), and tearing the session down because one
    /// stream ended would kill the others and close the D-Bus session object out from under the
    /// caller.
    pub fn stop_stream(&mut self, stream_id: CastStreamId) {
        let _span = tracy_client::span!("Niri::stop_stream");
        let _span = debug_span!("stop_stream", %stream_id).entered();

        self.casting
            .pending_dynamic_casts
            .retain(|p| p.stream_id != stream_id);

        let Some(idx) = self
            .casting
            .casts
            .iter()
            .position(|cast| cast.stream_id == stream_id)
        else {
            return;
        };

        let cast = self.casting.casts.swap_remove(idx);
        let was_recording = self
            .casting
            .recordings
            .iter()
            .any(|r| r.session_id == cast.session_id);
        if let Err(err) = cast.stream.disconnect() {
            warn!("error disconnecting stream: {err:?}");
        }

        // The panel's recording indicator is driven by whether any recording is live, so it has to
        // be re-derived when one of them goes away.
        if was_recording {
            self.casting
                .recordings
                .retain(|r| r.session_id != cast.session_id);
            self.refresh_screen_recording();
        }
    }

    pub fn stop_cast(&mut self, session_id: CastSessionId) {
        let _span = tracy_client::span!("Niri::stop_cast");
        let _span = debug_span!("stop_cast", %session_id).entered();

        self.casting
            .pending_dynamic_casts
            .retain(|p| p.session_id != session_id);

        if self
            .casting
            .recordings
            .iter()
            .any(|r| r.session_id == session_id)
        {
            self.casting
                .recordings
                .retain(|r| r.session_id != session_id);
            self.refresh_screen_recording();
        }

        for i in (0..self.casting.casts.len()).rev() {
            let cast = &self.casting.casts[i];
            if cast.session_id != session_id {
                continue;
            }

            let cast = self.casting.casts.swap_remove(i);
            if let Err(err) = cast.stream.disconnect() {
                warn!("error disconnecting stream: {err:?}");
            }
        }

        // Tolerate a missing D-Bus connection: the headless click-to-stop test reaches this path
        // with no session bus, and skipping the object-server close there is strictly safer than
        // panicking. In production the connection is always present.
        let Some(dbus) = self.dbus.as_ref() else {
            return;
        };
        let Some(conn) = dbus.conn_screen_cast.as_ref() else {
            return;
        };
        let server = conn.object_server();
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id.get());
        if let Ok(iface) = server.interface::<_, mutter_screen_cast::Session>(path) {
            let _span = tracy_client::span!("invoking Session::stop");

            async_io::block_on(async move {
                iface
                    .get()
                    .stop(server.inner(), iface.signal_emitter().clone())
                    .await
            });
        }
    }

    /// Record that `session_id` began a screen recording, and refresh the panel indicator. The
    /// production seam the `StartCast` handler calls when a stream carries `is-recording`; also the
    /// seam headless tests drive directly (no PipeWire).
    pub fn screen_recording_started(&mut self, session_id: CastSessionId) {
        if self
            .casting
            .recordings
            .iter()
            .any(|r| r.session_id == session_id)
        {
            return;
        }
        self.casting.recordings.push(ScreenRecording {
            session_id,
            started_at: self.clock.now_unadjusted(),
            kind: RecordingKind::External,
        });
        self.refresh_screen_recording();
    }

    /// Route an `org.gnome.Shell.Screencast` request (the high-level D-Bus recorder entry point) to
    /// the native recorder. See [`crate::dbus::gnome_shell_screencast`].
    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn on_shell_screencast_msg(
        &mut self,
        msg: crate::dbus::gnome_shell_screencast::ScreencastToNiri,
    ) {
        use crate::dbus::gnome_shell_screencast::ScreencastToNiri;

        match msg {
            ScreencastToNiri::Start {
                area,
                template,
                draw_cursor,
                framerate,
                reply,
            } => {
                let result = self.start_shell_screencast(area, &template, draw_cursor, framerate);
                let _ = reply.send_blocking(result);
            }
            ScreencastToNiri::Stop { reply } => {
                let was_recording = self
                    .casting
                    .recordings
                    .iter()
                    .any(|r| matches!(r.kind, RecordingKind::Native(_)));
                self.stop_screen_recordings();
                self.queue_redraw_all();
                let _ = reply.send_blocking(was_recording);
            }
        }
    }

    /// Start a recording for a `Screencast` (whole active output) or `ScreencastArea` (a
    /// global-logical rectangle) request, returning the absolute output path or a human-readable
    /// reason it was declined.
    #[cfg(feature = "xdp-gnome-screencast")]
    fn start_shell_screencast(
        &mut self,
        area: Option<(i32, i32, i32, i32)>,
        template: &str,
        draw_cursor: bool,
        framerate: u32,
    ) -> Result<String, String> {
        if self
            .casting
            .recordings
            .iter()
            .any(|r| matches!(r.kind, RecordingKind::Native(_)))
        {
            return Err("a recording is already in progress".to_owned());
        }

        // `ScreencastArea` records a global-logical rectangle: pick the output it lands on (largest
        // intersection) and record that crop. `Screencast` records the whole active output.
        let (output, crop) = match area {
            Some((x, y, w, h)) => {
                if w <= 0 || h <= 0 {
                    return Err(format!("invalid recording area size {w}x{h}"));
                }
                let rect = Rectangle::new(Point::from((x, y)), Size::from((w, h)));
                let (CastTarget::Area { output, rect, .. }, _, _) = self
                    .cast_params_for_area(rect)
                    .ok_or_else(|| "the recording area is not on any output".to_owned())?
                else {
                    unreachable!("cast_params_for_area returns an Area target");
                };
                let output = output
                    .upgrade()
                    .ok_or_else(|| "the recording area's output went away".to_owned())?;
                (output, Some(rect))
            }
            None => {
                let output = self
                    .layout
                    .active_output()
                    .cloned()
                    .ok_or_else(|| "no active output to record".to_owned())?;
                (output, None)
            }
        };

        let path = crate::recording::resolve_file_template(template, "webm")
            .map_err(|err| format!("could not resolve the recording path: {err:#}"))?;
        self.start_native_recording(&output, path.clone(), framerate, draw_cursor, crop)
            .map_err(|err| format!("could not start the recorder: {err:#}"))?;
        self.queue_redraw_all();
        Ok(path.to_string_lossy().into_owned())
    }

    /// Start a compositor-driven recording of `output` to `path` (WebM/VP8 via ffmpeg). Returns the
    /// synthetic session id tracking it. Frames are captured on the output's redraws by
    /// [`Niri::render_for_recorders`]; stop it via [`Niri::stop_screen_recordings`].
    pub fn start_native_recording(
        &mut self,
        output: &Output,
        path: std::path::PathBuf,
        fps: u32,
        draw_cursor: bool,
        crop: Option<Rectangle<i32, Logical>>,
    ) -> anyhow::Result<CastSessionId> {
        use crate::recording::encoder::{FfmpegEncoder, ThreadedRecorder};
        use crate::recording::RecordConfig;

        let fps = fps.clamp(1, 120);

        let transform = output.current_transform();
        let scale = Scale::from(output.current_scale().fractional_scale());
        // The whole output, or the cropped area (its physical size on this output).
        let mut size = match crop {
            Some(rect) => rect.size.to_physical_precise_round(scale),
            None => {
                transform.transform_size(output.current_mode().context("output has no mode")?.size)
            }
        };
        // 4:2:0 (yuv420p) needs even dimensions; an arbitrary area selection may be odd.
        size.w &= !1;
        size.h &= !1;
        anyhow::ensure!(size.w > 0 && size.h > 0, "recording area is too small");

        let config = RecordConfig {
            width: size.w as u32,
            height: size.h as u32,
            fps,
            bitrate_kbps: 8000,
        };
        let encoder = FfmpegEncoder::new(&path, config).context("starting the recorder encoder")?;
        // Queue a second of frames before dropping, so a brief encoder stall doesn't lose frames.
        let recorder = ThreadedRecorder::spawn(Box::new(encoder), fps as usize);

        let session_id = CastSessionId::next();
        self.casting.recordings.push(ScreenRecording {
            session_id,
            started_at: self.clock.now_unadjusted(),
            kind: RecordingKind::Native(NativeRecording {
                output: output.downgrade(),
                recorder,
                size,
                scale,
                crop,
                draw_cursor,
                path,
                last_frame_time: Duration::ZERO,
                frame_interval: Duration::from_nanos(1_000_000_000 / fps as u64),
                scheduled_redraw: None,
            }),
        });
        self.refresh_screen_recording();
        Ok(session_id)
    }

    /// Stop every live recording (the R1 indicator's click action). External casts are torn down
    /// via `stop_cast`; native recordings finalize their encoder (writing the file trailer).
    /// Both prune the ledger and refresh the panel.
    /// Returns the file each finalized native recording is being written to, in stop order, for
    /// the caller to notify about. Empty when only external casts were torn down — those belong to
    /// a client that is doing its own reporting.
    pub fn stop_screen_recordings(&mut self) -> Vec<std::path::PathBuf> {
        let mut finished = Vec::new();
        let ids: Vec<_> = self
            .casting
            .recordings
            .iter()
            .map(|r| r.session_id)
            .collect();
        for id in ids {
            let idx = self
                .casting
                .recordings
                .iter()
                .position(|r| r.session_id == id);
            let Some(idx) = idx else { continue };
            if matches!(self.casting.recordings[idx].kind, RecordingKind::Native(_)) {
                let rec = self.casting.recordings.remove(idx);
                if let RecordingKind::Native(n) = rec.kind {
                    if let Some(token) = n.scheduled_redraw {
                        self.event_loop.remove(token);
                    }
                    // Finalize off-thread so the stop-click doesn't stall the compositor on encoder
                    // drain + WebM finalize.
                    n.recorder.finish_async(n.path.display().to_string());
                    finished.push(n.path);
                }
                self.refresh_screen_recording();
            } else {
                self.stop_cast(id);
            }
        }
        finished
    }

    /// Finalize and drop every native recording targeting `output` (its connector is going away).
    /// External casts are handled by `stop_casts_for_target`; native recordings live in a separate
    /// ledger, so without this an unplugged output would leave a zombie recording (live ffmpeg +
    /// forever-ticking R1 indicator), and a replugged connector is a fresh `Output` that never
    /// matches the stored weak.
    pub fn stop_native_recordings_for_output(&mut self, output: &Output) {
        let weak = output.downgrade();
        let mut changed = false;
        let mut i = 0;
        while i < self.casting.recordings.len() {
            let is_here = matches!(
                &self.casting.recordings[i].kind,
                RecordingKind::Native(n) if n.output == weak
            );
            if is_here {
                if let RecordingKind::Native(n) = self.casting.recordings.remove(i).kind {
                    if let Some(token) = n.scheduled_redraw {
                        self.event_loop.remove(token);
                    }
                    // Finalize off-thread (an output can be removed on the compositor thread too).
                    n.recorder.finish_async(n.path.display().to_string());
                }
                changed = true;
            } else {
                i += 1;
            }
        }
        if changed {
            self.refresh_screen_recording();
        }
    }

    /// Capture this output's frame for any native recording targeting it, paced to the recording's
    /// framerate. Renders the same `RenderTarget::Screencast` element list as the screencast path
    /// (so block-out-from-screencast privacy holds), reads it back as RGBA, and hands it to the
    /// encoder worker (dropping the frame if the worker is behind).
    pub fn render_for_recorders(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        use smithay::backend::allocator::Fourcc;
        use smithay::utils::Transform;

        use crate::recording::encoder::PushResult;
        use crate::recording::RecordFrame;
        use crate::render_helpers::render_to_vec;

        let weak = output.downgrade();
        let loop_handle = self.event_loop.clone();

        // Snapshot the native recordings on this output: (id, last_frame_time, interval,
        // started_at). started_at anchors a fixed capture grid so timer latency can't accumulate.
        let recs: Vec<(CastSessionId, Duration, Duration, Duration)> = self
            .casting
            .recordings
            .iter()
            .filter_map(|r| match &r.kind {
                RecordingKind::Native(n) if n.output == weak => Some((
                    r.session_id,
                    n.last_frame_time,
                    n.frame_interval,
                    r.started_at,
                )),
                _ => None,
            })
            .collect();

        for (id, last_frame, interval, started_at) in recs {
            // The capture grid: slot k covers [started_at + k*interval, started_at +
            // (k+1)*interval). Anchoring the next deadline to this grid (not to
            // last_frame + interval) keeps the cadence drift-free even when a redraw
            // lands a refresh late.
            let interval_ns = (interval.as_nanos() as u64).max(1);
            let slot_of = |t: Duration| -> u64 {
                (t.saturating_sub(started_at).as_nanos() as u64) / interval_ns
            };

            // Capture at most once per slot; a redraw that lands in an already-captured slot only
            // reschedules.
            let due =
                last_frame.is_zero() || slot_of(target_presentation_time) > slot_of(last_frame);

            let mut disconnected = false;
            if due {
                let Some((size, scale, draw_cursor, crop)) =
                    self.casting.recordings.iter().find_map(|r| match &r.kind {
                        RecordingKind::Native(n) if r.session_id == id => {
                            Some((n.size, n.scale, n.draw_cursor, n.crop))
                        }
                        _ => None,
                    })
                else {
                    continue;
                };

                // Shift output-local content so the recorded region's top-left maps to the buffer
                // origin. Zero for a whole-output recording (a no-op relocate); the area's offset
                // otherwise. Content outside the buffer is clipped, so the pointer shows only when
                // it falls inside the area.
                let neg_offset = match crop {
                    Some(rect) => {
                        let Some(geo) = self.global_space.output_geometry(output) else {
                            continue;
                        };
                        area_crop_offset(rect, geo, scale).upscale(-1)
                    }
                    None => Point::from((0, 0)),
                };

                // Build the output's screencast elements and read them back as RGBA.
                let ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Screencast,
                    xray: None,
                };
                let elements: Vec<_> = self
                    .render_to_vec(ctx, output, draw_cursor)
                    .into_iter()
                    .map(|elem| {
                        RelocateRenderElement::from_element(elem, neg_offset, Relocate::Relative)
                    })
                    .collect();
                match render_to_vec(
                    renderer,
                    size,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                ) {
                    Ok(rgba) => {
                        let pts = target_presentation_time.saturating_sub(started_at);
                        if let Some(RecordingKind::Native(n)) = self
                            .casting
                            .recordings
                            .iter_mut()
                            .find(|r| r.session_id == id)
                            .map(|r| &mut r.kind)
                        {
                            n.last_frame_time = target_presentation_time;
                            disconnected = n.recorder.try_push(RecordFrame { rgba, pts })
                                == PushResult::Disconnected;
                        }
                    }
                    // A transient capture failure must not abandon the self-drive chain: fall
                    // through and reschedule so an idle recording keeps retrying.
                    Err(err) => warn!("error capturing a recording frame: {err:?}"),
                }
            }

            // The encoder worker died (e.g. ffmpeg crashed); stop this recording so the panel
            // indicator clears and the compositor keeps running.
            if disconnected {
                warn!("recorder worker exited unexpectedly; stopping the recording");
                if let Some(pos) = self
                    .casting
                    .recordings
                    .iter()
                    .position(|r| r.session_id == id)
                {
                    if let RecordingKind::Native(n) = self.casting.recordings.remove(pos).kind {
                        if let Some(token) = n.scheduled_redraw {
                            loop_handle.remove(token);
                        }
                    }
                }
                self.refresh_screen_recording();
                continue;
            }

            // Keep frames flowing while the output is idle: schedule the next redraw at the next
            // grid deadline. A screencast leans on its consumer to pull; a recording has none, so
            // without this an unchanging screen would capture almost nothing.
            let base = if due {
                target_presentation_time
            } else {
                last_frame
            };
            let next_slot = slot_of(base) + 1;
            let deadline = started_at + Duration::from_nanos(next_slot.saturating_mul(interval_ns));
            if let Some(RecordingKind::Native(n)) = self
                .casting
                .recordings
                .iter_mut()
                .find(|r| r.session_id == id)
                .map(|r| &mut r.kind)
            {
                if let Some(token) = n.scheduled_redraw.take() {
                    loop_handle.remove(token);
                }
                let delay = deadline.saturating_sub(get_monotonic_time());
                let out = output.clone();
                let token = loop_handle
                    .insert_source(Timer::from_duration(delay), move |_, _, state| {
                        if state.niri.output_state.contains_key(&out) {
                            state.niri.queue_redraw(&out);
                        }
                        TimeoutAction::Drop
                    })
                    .unwrap();
                n.scheduled_redraw = Some(token);
            }
        }
    }

    /// Reconcile the panel's R1 indicator with the recording ledger: show it (timer from the
    /// earliest active recording) while any recording lives, and drive a 1 s tick for the elapsed
    /// label. Idempotent; called whenever the ledger changes.
    pub fn refresh_screen_recording(&mut self) {
        let started = self.casting.recordings.iter().map(|r| r.started_at).min();

        let redraw = self.panel.set_recording(started);

        if started.is_some() {
            // Drive the M:SS label from a dedicated 1 s timer while recording, independent of the
            // clock's minute cadence, so the elapsed time never sits frozen at 0:00.
            if self.recording_tick.is_none() {
                let token = self
                    .event_loop
                    .insert_source(
                        Timer::from_duration(Duration::from_secs(1)),
                        |_, _, state| {
                            if state.niri.casting.recordings.is_empty() {
                                state.niri.recording_tick = None;
                                return TimeoutAction::Drop;
                            }
                            if state.niri.panel.update_recording_label() {
                                state.niri.queue_redraw_all();
                            }
                            TimeoutAction::ToDuration(Duration::from_secs(1))
                        },
                    )
                    .unwrap();
                self.recording_tick = Some(token);
            }
        } else if let Some(token) = self.recording_tick.take() {
            self.event_loop.remove(token);
        }

        if redraw {
            self.queue_redraw_all();
        }
    }

    pub fn stop_casts_for_target(&mut self, target: CastTarget) {
        let _span = tracy_client::span!("Niri::stop_casts_for_target");

        // This is O(N^2) but it shouldn't be a problem I think.
        let mut saw_dynamic = false;
        let mut ids = Vec::new();
        for cast in &self.casting.casts {
            // Stopping an output stops everything sourced from it — including area casts
            // resolved to that output, which a strict target compare would miss. Otherwise
            // an area recording zombies on output removal, and its R1 ledger entry (only
            // pruned by `stop_cast`) leaves the panel indicator ticking over a dead cast.
            let matches = match &target {
                CastTarget::Output { output, .. } => cast.target.matches_output(output),
                _ => cast.target == target,
            };
            if !matches {
                continue;
            }

            if cast.dynamic_target {
                saw_dynamic = true;
                continue;
            }

            ids.push(cast.session_id);
        }

        for id in ids {
            self.stop_cast(id);
        }

        // We don't stop dynamic casts, instead we switch them to Nothing.
        if saw_dynamic {
            self.event_loop
                .insert_idle(|state| state.set_dynamic_cast_target(CastTarget::Nothing));
        }
    }

    fn cast_params_for_window(&self, window_id: u64) -> Option<(Size<i32, Physical>, u32)> {
        let (_, mapped) = self
            .layout
            .windows()
            .find(|(_, m)| m.id().get() == window_id)?;
        let output = self.casting.mapped_cast_output.get(&mapped.window)?;
        let scale = Scale::from(output.current_scale().fractional_scale());
        let bbox = mapped
            .window
            .bbox_with_popups()
            .to_physical_precise_up(scale);
        let refresh = output.current_mode().unwrap().refresh as u32;
        Some((bbox.size, refresh))
    }

    /// Resolve an area screencast (global logical `rect`) to a single output, its physical buffer
    /// size, and refresh.
    ///
    /// Single-output MVP: the area is recorded from the output with the largest intersection with
    /// `rect`, cropped to that output. mutter composites every intersecting view instead; a
    /// cross-output area cast is a follow-up. Returns `None` when the rect intersects no output
    /// (mutter fails the stream in that case too).
    pub(crate) fn cast_params_for_area(
        &self,
        rect: Rectangle<i32, Logical>,
    ) -> Option<(CastTarget, Size<i32, Physical>, u32)> {
        let mut best: Option<(&Output, i32)> = None;
        for output in self.global_space.outputs() {
            let geo = self.global_space.output_geometry(output).unwrap();
            let Some(isect) = geo.intersection(rect) else {
                continue;
            };
            let area = isect.size.w * isect.size.h;
            if area > 0 && best.is_none_or(|(_, best_area)| area > best_area) {
                best = Some((output, area));
            }
        }

        let (output, _) = best?;
        let geo = self.global_space.output_geometry(output).unwrap();
        if !geo.contains_rect(rect) {
            warn!(
                "screencast area spans beyond one output; recording only the \
                 largest-intersection output (cross-output area casts are not yet supported)"
            );
        }

        let scale = Scale::from(output.current_scale().fractional_scale());
        // Buffer size = round(area size * scale), matching mutter's meta-stream-area.
        let size = rect.size.to_physical_precise_round(scale);
        let refresh = output.current_mode().unwrap().refresh as u32;
        let target = CastTarget::Area {
            output: output.downgrade(),
            name: output.name(),
            rect,
        };
        Some((target, size, refresh))
    }
}

/// The physical shift mapping output-local content into an area cast's cropped buffer: the
/// area's top-left in the output's physical space. Negate it for `Relocate::Relative`. The
/// `- output_geo.loc` term is what makes the crop correct for outputs that are not at the
/// global origin.
pub(crate) fn area_crop_offset(
    rect: Rectangle<i32, Logical>,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    (rect.loc - output_geo.loc).to_physical_precise_round(scale)
}

fn cast_params_for_output(output: &Output) -> (Size<i32, Physical>, u32) {
    let mode = output.current_mode().unwrap();
    let transform = output.current_transform();
    let size = transform.transform_size(mode.size);
    let refresh = mode.refresh as u32;
    (size, refresh)
}

niri_render_elements! {
    CastRenderElement => {
        Output = OutputRenderElements,
        Window = WindowCastRenderElements,
        Pointer = PointerRenderElements,
        RelocatedPointer = RelocateRenderElement<PointerRenderElements>,
        // Output content shifted into an area cast's cropped buffer.
        RelocatedOutput = RelocateRenderElement<OutputRenderElements>,
    }
}
