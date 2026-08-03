use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::io::Cursor;
use std::iter::zip;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::time::Duration;
use std::{io, mem, slice};

use anyhow::{ensure, Context as _};
use calloop::timer::{TimeoutAction, Timer};
use calloop::RegistrationToken;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::spa::{self};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::{pw_buffer, pw_check_library_version, pw_stream_queue_buffer};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Format, Fourcc};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::ExportMem;
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Logical, Physical, Point, Scale, Size, Transform};
use zbus::object_server::SignalEmitter;

use crate::dbus::mutter_screen_cast::{self, CursorMode};
use crate::niri::{CastTarget, State};
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{
    clear_dmabuf, encompassing_geo, render_and_copy_to_memory, render_and_download_as,
    render_to_dmabuf,
};
use crate::screencasting::CastRenderElement;
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};

// Give a 0.1 ms allowance for presentation time errors.
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);

const CURSOR_FORMAT: spa_video_format = SPA_VIDEO_FORMAT_BGRA;
const CURSOR_BPP: u32 = 4;
const CURSOR_WIDTH: u32 = 384;
const CURSOR_HEIGHT: u32 = 384;
const CURSOR_BITMAP_SIZE: usize = (CURSOR_WIDTH * CURSOR_HEIGHT * CURSOR_BPP) as usize;
const CURSOR_META_SIZE: usize =
    mem::size_of::<spa_meta_cursor>() + mem::size_of::<spa_meta_bitmap>() + CURSOR_BITMAP_SIZE;
const BITMAP_META_OFFSET: usize = mem::size_of::<spa_meta_cursor>();
const BITMAP_DATA_OFFSET: usize = mem::size_of::<spa_meta_bitmap>();

pub struct PipeWire {
    _context: ContextRc,
    pub core: CoreRc,
    pub token: RegistrationToken,
    event_loop: LoopHandle<'static, State>,
    to_niri: calloop::channel::Sender<PwToNiri>,
}

pub enum PwToNiri {
    StopCast { session_id: CastSessionId },
    Redraw { stream_id: CastStreamId },
    FatalError,
}

pub struct Cast {
    event_loop: LoopHandle<'static, State>,
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    pub stream: StreamRc,
    pub target: CastTarget,
    pub dynamic_target: bool,
    formats: FormatSet,
    offer_alpha: bool,
    cursor_mode: CursorMode,
    pub last_frame_time: Duration,
    scheduled_redraw: Option<RegistrationToken>,
    // Incremented once per successful frame, stored in buffer meta.
    sequence_counter: u64,
    inner: Rc<RefCell<CastInner>>,
}

/// Mutable `Cast` state shared with PipeWire callbacks.
#[derive(Debug)]
struct CastInner {
    is_active: bool,
    node_id: Option<u32>,
    state: CastState,
    refresh: u32,
    min_time_between_frames: Duration,
    dmabufs: HashMap<i64, Dmabuf>,
    /// Memfd-backed buffers we allocated, keyed like [`Self::dmabufs`] by the block's fd.
    memory_buffers: HashMap<i64, MemoryBuffer>,
    /// Buffers dequeued from PipeWire in process of rendering.
    ///
    /// This is an ordered list of buffers that we started rendering to and waiting for the
    /// rendering to complete. The completion can be checked from the `SyncPoint`s. The buffers are
    /// stored in order from oldest to newest, and the same ordering should be preserved when
    /// submitting completed buffers to PipeWire.
    rendering_buffers: Vec<(NonNull<pw_buffer>, SyncPoint)>,
}

/// How the consumer takes frames.
///
/// Not every consumer can import a dmabuf. OBS and gnome-software both fail negotiation outright
/// when dmabuf is all we offer, so we advertise plain memory as well and have to remember which
/// one the stream settled on. Keeping it in one enum rather than an `Option<Modifier>` means the
/// buffer-allocation, render and teardown paths cannot disagree about which world they are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    Dmabuf {
        modifier: Modifier,
        plane_count: i32,
    },
    /// PipeWire MemFd/MemPtr: one block of tightly packed BGRA that PipeWire allocates for us.
    Memory,
}

impl Sink {
    /// `SPA_PARAM_BUFFERS_blocks` — one per dmabuf plane, or a single block of memory.
    fn blocks(self) -> i32 {
        match self {
            Sink::Dmabuf { plane_count, .. } => plane_count,
            Sink::Memory => 1,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum CastState {
    ResizePending {
        pending_size: Size<u32, Physical>,
    },
    ConfirmationPending {
        size: Size<u32, Physical>,
        alpha: bool,
        sink: Sink,
    },
    Ready {
        size: Size<u32, Physical>,
        alpha: bool,
        sink: Sink,
        // Lazily-initialized to keep the initialization to a single place.
        damage_tracker: Option<OutputDamageTracker>,
        cursor_damage_tracker: Option<OutputDamageTracker>,
        last_cursor_location: Option<Point<i32, Physical>>,
    },
}

#[derive(PartialEq, Eq)]
pub enum CastSizeChange {
    Ready,
    Pending,
}

/// Data for drawing a cursor either as metadata or embedded.
///
/// The cursor elements are expected to be at the start of the main elements slice. `elem_count` is
/// the count of the pointer elements. This way, the full slice includes both main and cursor
/// elements for embedded mode, and `&elements[elem_count..]` gives just the main elements for
/// metadata mode.
///
/// We have weird borrowed references here in order to support both metadata and embedded cases.
/// The cursor damage tracker needs a slice of impl Element at (0, 0), so we pass it `relocated`
/// (luckily, &impl Element also impls Element). Then, if we need to embed the cursor, we use the
/// full elements slice which starts with non-relocated pointer elements (that we borrow from).
#[derive(Debug)]
pub struct CursorData<'a, E> {
    /// Count of the pointer elements in the slice (index of the first non-pointer element).
    elem_count: usize,
    /// Cursor elements relocated to (0, 0).
    relocated: Vec<RelocateRenderElement<&'a E>>,
    /// Location of the cursor's hotspot in the video buffer.
    location: Point<i32, Physical>,
    /// Location of the cursor's hotspot on the cursor bitmap.
    hotspot: Point<i32, Physical>,
    /// Size of the elements' encompassing geo.
    size: Size<i32, Physical>,
    /// Scale the elements should be rendered at.
    scale: Scale<f64>,
}

impl<'a, E: Element> CursorData<'a, E> {
    pub fn compute(
        elements: &'a [E],
        elem_count: usize,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        let pointer_elements = &elements[..elem_count];
        let location = location.to_physical_precise_round(scale);

        let geo = encompassing_geo(scale, pointer_elements.iter());
        let relocated = Vec::from_iter(pointer_elements.iter().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        }));

        Self {
            elem_count,
            relocated,
            location,
            hotspot: location - geo.loc,
            size: geo.size,
            scale,
        }
    }
}

/// Build the full `EnumFormat` offer.
///
/// Order matters and follows mutter's `build_format_params`
/// (`meta-screen-cast-stream-src.c:1576-1592`): every format **with** modifiers first, then every
/// format **without**. A consumer that can import a dmabuf still picks one; a consumer that cannot
/// falls through to the memory variant instead of failing negotiation outright.
macro_rules! make_params {
    ($params:ident, $formats:expr, $size:expr, $refresh:expr, $alpha:expr) => {
        let mut b1 = Vec::new();
        let mut b2 = Vec::new();
        let mut b3 = Vec::new();
        let mut b4 = Vec::new();

        let o1 = make_video_params($formats, $size, $refresh, false, true);
        let o2 = if $alpha {
            make_video_params($formats, $size, $refresh, true, true)
        } else {
            None
        };
        let o3 = make_video_params($formats, $size, $refresh, false, false);
        let o4 = if $alpha {
            make_video_params($formats, $size, $refresh, true, false)
        } else {
            None
        };

        let mut pods = Vec::new();
        if let Some(o) = o1 {
            pods.push(make_pod(&mut b1, o));
        }
        if let Some(o) = o2 {
            pods.push(make_pod(&mut b2, o));
        }
        if let Some(o) = o3 {
            pods.push(make_pod(&mut b3, o));
        }
        if let Some(o) = o4 {
            pods.push(make_pod(&mut b4, o));
        }

        $params = pods;
    };
}

impl PipeWire {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        to_niri: calloop::channel::Sender<PwToNiri>,
    ) -> anyhow::Result<Self> {
        let main_loop = MainLoopRc::new(None).context("error creating MainLoop")?;
        let context = ContextRc::new(&main_loop, None).context("error creating Context")?;
        let core = context.connect_rc(None).context("error creating Core")?;

        let to_niri_ = to_niri.clone();
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "pw error");

                // Reset PipeWire on connection errors.
                if id == PW_ID_CORE && res == -32 {
                    if let Err(err) = to_niri_.send(PwToNiri::FatalError) {
                        warn!("error sending FatalError to niri: {err:?}");
                    }
                }
            })
            .register();
        mem::forget(listener);

        struct AsFdWrapper(MainLoopRc);
        impl AsFd for AsFdWrapper {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }
        let generic = Generic::new(AsFdWrapper(main_loop), Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(generic, move |_, wrapper, _| {
                let _span = tracy_client::span!("pipewire iteration");
                wrapper.0.loop_().iterate(Duration::ZERO);
                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self {
            _context: context,
            core,
            token,
            event_loop,
            to_niri,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_cast(
        &self,
        gbm: GbmDevice<DrmDeviceFd>,
        formats: FormatSet,
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: CastTarget,
        size: Size<i32, Physical>,
        refresh: u32,
        alpha: bool,
        mut cursor_mode: CursorMode,
        signal_ctx: SignalEmitter<'static>,
    ) -> anyhow::Result<Cast> {
        let _span = tracy_client::span!("PipeWire::start_cast");

        // Every format pod offers the modifiers `formats` holds for the fourcc it picks, and needs
        // at least one of them: an empty offer would index out of bounds, and a stream with no
        // modifier to negotiate could not produce a buffer anyway. Fail the cast instead, so a
        // renderer that advertises nothing usable is a stopped cast and a log line rather than a
        // compositor panic.
        for fourcc in [Fourcc::Xrgb8888]
            .into_iter()
            .chain(alpha.then_some(Fourcc::Argb8888))
        {
            ensure!(
                formats.iter().any(|f| f.code == fourcc),
                "the renderer advertises no {fourcc:?} dmabuf modifier to offer",
            );
        }

        let to_niri_ = self.to_niri.clone();
        let stop_cast = move || {
            if let Err(err) = to_niri_.send(PwToNiri::StopCast { session_id }) {
                warn!(%session_id, "error sending StopCast to niri: {err:?}");
            }
        };
        let to_niri_ = self.to_niri.clone();
        let redraw = move || {
            if let Err(err) = to_niri_.send(PwToNiri::Redraw { stream_id }) {
                warn!(%stream_id, "error sending Redraw to niri: {err:?}");
            }
        };
        let redraw_ = redraw.clone();

        let stream = StreamRc::new(
            self.core.clone(),
            "niri-screen-cast-src",
            PropertiesBox::new(),
        )
        .context("error creating Stream")?;

        if cursor_mode == CursorMode::Metadata && !pw_version_supports_cursor_metadata() {
            debug!(
                "metadata cursor mode requested, but PipeWire is too old (need >= 1.4.8); \
                 switching to embedded cursor"
            );
            cursor_mode = CursorMode::Embedded;
        }

        let pending_size = Size::from((size.w as u32, size.h as u32));

        // Like in good old wayland-rs times...
        let inner = Rc::new(RefCell::new(CastInner {
            is_active: false,
            node_id: None,
            state: CastState::ResizePending { pending_size },
            refresh,
            min_time_between_frames: Duration::ZERO,
            dmabufs: HashMap::new(),
            memory_buffers: HashMap::new(),
            rendering_buffers: Vec::new(),
        }));

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                move |stream, (), old, new| {
                    let _span = debug_span!("state_changed", %stream_id).entered();
                    debug!("{old:?} -> {new:?}");
                    let mut inner = inner.borrow_mut();

                    match new {
                        StreamState::Paused => {
                            if inner.node_id.is_none() {
                                let id = stream.node_id();
                                inner.node_id = Some(id);
                                debug!("sending signal with {id}");

                                let _span = tracy_client::span!("sending PipeWireStreamAdded");
                                async_io::block_on(async {
                                    let res = mutter_screen_cast::Stream::pipe_wire_stream_added(
                                        &signal_ctx,
                                        id,
                                    )
                                    .await;

                                    if let Err(err) = res {
                                        warn!("error sending PipeWireStreamAdded: {err:?}");
                                        stop_cast();
                                    }
                                });
                            }

                            inner.is_active = false;
                        }
                        StreamState::Error(_) => {
                            if inner.is_active {
                                inner.is_active = false;
                                stop_cast();
                            }
                        }
                        StreamState::Unconnected => (),
                        StreamState::Connecting => (),
                        StreamState::Streaming => {
                            inner.is_active = true;
                            redraw();
                        }
                    }
                }
            })
            .param_changed({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                let gbm = gbm.clone();
                let formats = formats.clone();
                move |stream, (), id, pod| {
                    let id = ParamType::from_raw(id);
                    trace!(%stream_id, ?id, "param_changed");
                    let mut inner = inner.borrow_mut();
                    let inner = &mut *inner;

                    if id != ParamType::Format {
                        return;
                    }

                    let _span = debug_span!("param_changed", %stream_id).entered();

                    let Some(pod) = pod else { return };

                    let (m_type, m_subtype) = match parse_format(pod) {
                        Ok(x) => x,
                        Err(err) => {
                            warn!("error parsing format: {err:?}");
                            return;
                        }
                    };

                    if m_type != MediaType::Video || m_subtype != MediaSubtype::Raw {
                        return;
                    }

                    let mut format = VideoInfoRaw::new();
                    format.parse(pod).unwrap();
                    debug!("got format = {format:?}");

                    let format_size = Size::from((format.size().width, format.size().height));

                    let state = &mut inner.state;
                    if format_size != state.expected_format_size() {
                        if !matches!(&*state, CastState::ResizePending { .. }) {
                            warn!("wrong size, but we're not resizing");
                            stop_cast();
                            return;
                        }

                        debug!("wrong size, waiting");
                        return;
                    }

                    let format_has_alpha = format.format() == VideoFormat::BGRA;
                    let fourcc = if format_has_alpha {
                        Fourcc::Argb8888
                    } else {
                        Fourcc::Xrgb8888
                    };

                    let max_frame_rate = format.max_framerate();
                    let min_frame_time = Duration::from_micros(
                        1_000_000 * u64::from(max_frame_rate.denom) / u64::from(max_frame_rate.num),
                    );
                    inner.min_time_between_frames = min_frame_time;

                    let object = pod.as_object().unwrap();
                    let prop_modifier =
                        object.find_prop(spa::utils::Id(FormatProperties::VideoModifier.0));

                    // No modifier means the consumer took our memory offer. That is not an error —
                    // it is the whole point of advertising the modifier-less variant. Mutter picks
                    // its buffer type the same way: `prop_modifier ? DmaBuf : MemFd`
                    // (`meta-screen-cast-stream-src.c:1929-1934`).
                    let Some(prop_modifier) = prop_modifier else {
                        debug!("no modifier negotiated, using memory buffers");

                        let (damage_tracker, cursor_damage_tracker) = if let CastState::Ready {
                            damage_tracker,
                            cursor_damage_tracker,
                            ..
                        } = &mut *state
                        {
                            (damage_tracker.take(), cursor_damage_tracker.take())
                        } else {
                            (None, None)
                        };

                        *state = CastState::Ready {
                            size: format_size,
                            alpha: format_has_alpha,
                            sink: Sink::Memory,
                            damage_tracker,
                            cursor_damage_tracker,
                            last_cursor_location: None,
                        };

                        update_buffer_params(stream, Sink::Memory, cursor_mode, &stop_cast);
                        return;
                    };

                    if prop_modifier.flags().contains(PodPropFlags::DONT_FIXATE) {
                        debug!("fixating the modifier");

                        let pod_modifier = prop_modifier.value();
                        let Ok((_, modifiers)) = PodDeserializer::deserialize_from::<Choice<i64>>(
                            pod_modifier.as_bytes(),
                        ) else {
                            warn!("wrong modifier property type");
                            stop_cast();
                            return;
                        };

                        let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                            warn!("wrong modifier choice type");
                            stop_cast();
                            return;
                        };

                        let (modifier, plane_count) = match find_preferred_modifier(
                            &gbm,
                            format_size,
                            fourcc,
                            alternatives,
                        ) {
                            Ok(x) => x,
                            Err(err) => {
                                warn!("couldn't find preferred modifier: {err:?}");
                                stop_cast();
                                return;
                            }
                        };

                        debug!(
                            "allocation successful \
                             (modifier={modifier:?}, plane_count={plane_count}), \
                             moving to confirmation pending"
                        );

                        *state = CastState::ConfirmationPending {
                            size: format_size,
                            alpha: format_has_alpha,
                            sink: Sink::Dmabuf {
                                modifier,
                                plane_count: plane_count as i32,
                            },
                        };

                        let fixated_format = FormatSet::from_iter([Format {
                            code: fourcc,
                            modifier,
                        }]);

                        let mut b1 = Vec::new();
                        let mut b2 = Vec::new();
                        let mut b3 = Vec::new();

                        // The fixated single modifier, then the full modifier set, then the
                        // memory fallback — so a re-negotiation can still land on memory.
                        let mut params = Vec::new();
                        if let Some(o) = make_video_params(
                            &fixated_format,
                            format_size,
                            inner.refresh,
                            format_has_alpha,
                            true,
                        ) {
                            params.push(make_pod(&mut b1, o));
                        }
                        if let Some(o) = make_video_params(
                            &formats,
                            format_size,
                            inner.refresh,
                            format_has_alpha,
                            true,
                        ) {
                            params.push(make_pod(&mut b2, o));
                        }
                        if let Some(o) = make_video_params(
                            &formats,
                            format_size,
                            inner.refresh,
                            format_has_alpha,
                            false,
                        ) {
                            params.push(make_pod(&mut b3, o));
                        }

                        if let Err(err) = stream.update_params(&mut params[..]) {
                            warn!("error updating stream params: {err:?}");
                            stop_cast();
                        }

                        return;
                    }

                    // Verify that alpha and modifier didn't change.
                    let sink = match &*state {
                        CastState::ConfirmationPending { size, alpha, sink }
                        | CastState::Ready {
                            size, alpha, sink, ..
                        } if *alpha == format_has_alpha
                            && matches!(
                                sink,
                                Sink::Dmabuf { modifier, .. }
                                    if *modifier == Modifier::from(format.modifier())
                            ) =>
                        {
                            let size = *size;
                            let alpha = *alpha;
                            let sink = *sink;

                            let (damage_tracker, cursor_damage_tracker) =
                                if let CastState::Ready {
                                    damage_tracker,
                                    cursor_damage_tracker,
                                    ..
                                } = &mut *state
                                {
                                    (damage_tracker.take(), cursor_damage_tracker.take())
                                } else {
                                    (None, None)
                                };

                            debug!("moving to ready state");

                            *state = CastState::Ready {
                                size,
                                alpha,
                                sink,
                                damage_tracker,
                                cursor_damage_tracker,
                                last_cursor_location: None,
                            };

                            sink
                        }
                        _ => {
                            // We're negotiating a single modifier, or alpha or modifier changed,
                            // so we need to do a test allocation.
                            let (modifier, plane_count) = match find_preferred_modifier(
                                &gbm,
                                format_size,
                                fourcc,
                                vec![format.modifier() as i64],
                            ) {
                                Ok(x) => x,
                                Err(err) => {
                                    warn!("test allocation failed: {err:?}");
                                    stop_cast();
                                    return;
                                }
                            };

                            debug!(
                                "allocation successful \
                                 (modifier={modifier:?}, plane_count={plane_count}), \
                                 moving to ready"
                            );

                            let sink = Sink::Dmabuf {
                                modifier,
                                plane_count: plane_count as i32,
                            };

                            *state = CastState::Ready {
                                size: format_size,
                                alpha: format_has_alpha,
                                sink,
                                damage_tracker: None,
                                cursor_damage_tracker: None,
                                last_cursor_location: None,
                            };

                            sink
                        }
                    };

                    update_buffer_params(stream, sink, cursor_mode, &stop_cast);
                }
            })
            .add_buffer({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                move |stream, (), buffer| {
                    let _span = debug_span!("add_buffer", %stream_id).entered();
                    let mut inner = inner.borrow_mut();

                    let (size, alpha, sink) = if let CastState::Ready {
                        size, alpha, sink, ..
                    } = &inner.state
                    {
                        (*size, *alpha, *sink)
                    } else {
                        trace!("add_buffer, but not ready yet");
                        return;
                    };

                    trace!("size={size:?}, alpha={alpha}, sink={sink:?}");

                    let Sink::Dmabuf { modifier, .. } = sink else {
                        // Memory sink: PipeWire gave us an empty buffer to fill in, and we own the
                        // memfd behind it, exactly as mutter does
                        // (`meta-screen-cast-stream-src.c:2318-2358`).
                        match unsafe { attach_memfd(buffer, size) } {
                            Ok(mapping) => {
                                let fd = mapping.fd;
                                assert!(inner.memory_buffers.insert(fd, mapping).is_none());
                            }
                            Err(err) => {
                                warn!("error allocating memfd buffer: {err:?}");
                                stop_cast();
                                return;
                            }
                        }

                        if inner.memory_buffers.len() == 1
                            && stream.state() == StreamState::Streaming
                        {
                            redraw_();
                        }
                        return;
                    };

                    unsafe {
                        let spa_buffer = (*buffer).buffer;

                        let fourcc = if alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };

                        let dmabuf = match allocate_dmabuf(&gbm, size, fourcc, modifier) {
                            Ok(dmabuf) => dmabuf,
                            Err(err) => {
                                warn!("error allocating dmabuf: {err:?}");
                                stop_cast();
                                return;
                            }
                        };

                        let plane_count = dmabuf.num_planes();
                        assert_eq!((*spa_buffer).n_datas as usize, plane_count);

                        for (i, (fd, (stride, offset))) in
                            zip(dmabuf.handles(), zip(dmabuf.strides(), dmabuf.offsets()))
                                .enumerate()
                        {
                            let spa_data = (*spa_buffer).datas.add(i);
                            assert!((*spa_data).type_ & (1 << DataType::DmaBuf.as_raw()) > 0);

                            (*spa_data).type_ = DataType::DmaBuf.as_raw();

                            // With DMA-BUFs, consumers should ignore the maxsize field, and
                            // producers are allowed to set it to 0.
                            //
                            // https://docs.pipewire.org/page_dma_buf.html
                            (*spa_data).maxsize = 1;
                            (*spa_data).fd = fd.as_raw_fd() as i64;
                            (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                            let chunk = (*spa_data).chunk;
                            (*chunk).stride = stride as i32;
                            (*chunk).offset = offset;

                            trace!(
                                "pw buffer plane: fd={}, stride={stride}, offset={offset}",
                                (*spa_data).fd
                            );
                        }

                        let fd = (*(*spa_buffer).datas).fd;
                        assert!(inner.dmabufs.insert(fd, dmabuf).is_none());
                    }

                    // During size re-negotiation, the stream sometimes just keeps running, in
                    // which case we may need to force a redraw once we got a newly sized buffer.
                    if inner.dmabufs.len() == 1 && stream.state() == StreamState::Streaming {
                        redraw_();
                    }
                }
            })
            .remove_buffer({
                let inner = inner.clone();
                move |_stream, (), buffer| {
                    trace!(%stream_id, "remove_buffer");
                    let mut inner = inner.borrow_mut();

                    inner
                        .rendering_buffers
                        .retain(|(buf, _)| buf.as_ptr() != buffer);

                    unsafe {
                        let spa_buffer = (*buffer).buffer;
                        let spa_data = (*spa_buffer).datas;
                        assert!((*spa_buffer).n_datas > 0);

                        let fd = (*spa_data).fd;
                        inner.dmabufs.remove(&fd);
                        // Dropping the mapping unmaps it; PipeWire owns the fd itself.
                        inner.memory_buffers.remove(&fd);
                    }
                }
            })
            .register()
            .unwrap();

        trace!(
            %stream_id,
            "starting pw stream with size={pending_size:?}, refresh={refresh:?}"
        );

        let mut params;
        make_params!(params, &formats, pending_size, refresh, alpha);
        ensure!(
            !params.is_empty(),
            "no formats to offer, refusing to start a stream nothing can accept"
        );
        stream
            .connect(
                Direction::Output,
                None,
                // ALLOC_BUFFERS both ways: we allocate the dmabuf *and* the memfd ourselves, which
                // is what mutter does too (`meta-screen-cast-stream-src.c:2477`).
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut params[..],
            )
            .context("error connecting stream")?;

        let cast = Cast {
            event_loop: self.event_loop.clone(),
            session_id,
            stream_id,
            stream,
            _listener: listener,
            target,
            dynamic_target: false,
            formats,
            offer_alpha: alpha,
            cursor_mode,
            last_frame_time: Duration::ZERO,
            scheduled_redraw: None,
            sequence_counter: 0,
            inner,
        };
        Ok(cast)
    }
}

impl Cast {
    pub fn is_active(&self) -> bool {
        self.inner.borrow().is_active
    }

    pub fn node_id(&self) -> Option<u32> {
        self.inner.borrow().node_id
    }

    pub fn ensure_size(&self, size: Size<i32, Physical>) -> anyhow::Result<CastSizeChange> {
        let mut inner = self.inner.borrow_mut();

        let new_size = Size::from((size.w as u32, size.h as u32));

        let state = &mut inner.state;
        if matches!(state, CastState::Ready { size, .. } if *size == new_size) {
            return Ok(CastSizeChange::Ready);
        }

        if state.pending_size() == Some(new_size) {
            debug!("stream size still hasn't changed, skipping frame");
            return Ok(CastSizeChange::Pending);
        }

        let _span = tracy_client::span!("Cast::ensure_size");
        debug!("cast size changed, updating stream size");

        *state = CastState::ResizePending {
            pending_size: new_size,
        };

        let mut params;
        make_params!(
            params,
            &self.formats,
            new_size,
            inner.refresh,
            self.offer_alpha
        );
        self.stream
            .update_params(&mut params[..])
            .context("error updating stream params")?;

        Ok(CastSizeChange::Pending)
    }

    pub fn set_refresh(&mut self, refresh: u32) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();

        if inner.refresh == refresh {
            return Ok(());
        }

        let _span = tracy_client::span!("Cast::set_refresh");
        debug!("cast FPS changed, updating stream FPS");
        inner.refresh = refresh;

        let size = inner.state.expected_format_size();
        let mut params;
        make_params!(params, &self.formats, size, refresh, self.offer_alpha);
        self.stream
            .update_params(&mut params[..])
            .context("error updating stream params")?;

        Ok(())
    }

    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let inner = self.inner.borrow();

        let last = self.last_frame_time;
        let min = inner.min_time_between_frames;

        if last.is_zero() {
            trace!(?target_frame_time, ?last, "last is zero, recording");
            return Duration::ZERO;
        }

        if target_frame_time < last {
            // Record frame with a warning; in case it was an overflow this will fix it.
            warn!(
                ?target_frame_time,
                ?last,
                "target frame time is below last, did it overflow or did we mispredict?"
            );
            return Duration::ZERO;
        }

        let diff = target_frame_time - last;
        if diff < min {
            let delay = min - diff;
            trace!(
                ?target_frame_time,
                ?last,
                "frame is too soon: min={min:?}, delay={:?}",
                delay
            );
            return delay;
        } else {
            trace!("overshoot={:?}", diff - min);
        }

        Duration::ZERO
    }

    fn schedule_redraw(&mut self, output: Output, target_time: Duration) {
        if self.scheduled_redraw.is_some() {
            return;
        }

        let now = get_monotonic_time();
        let duration = target_time.saturating_sub(now);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                // Guard against output disconnecting before the timer has a chance to run.
                if state.niri.output_state.contains_key(&output) {
                    state.niri.queue_redraw(&output);
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.scheduled_redraw = Some(token);
    }

    fn remove_scheduled_redraw(&mut self) {
        if let Some(token) = self.scheduled_redraw.take() {
            self.event_loop.remove(token);
        }
    }

    /// Checks whether this frame should be skipped because it's too soon.
    ///
    /// If the frame should be skipped, schedules a redraw and returns `true`. Otherwise, removes a
    /// scheduled redraw, if any, and returns `false`.
    ///
    /// When this method returns `false`, the calling code is assumed to follow up with
    /// [`Cast::dequeue_buffer_and_render()`].
    pub fn check_time_and_schedule(
        &mut self,
        output: &Output,
        target_frame_time: Duration,
    ) -> bool {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay >= CAST_DELAY_ALLOWANCE {
            trace!("delay >= allowance, scheduling redraw");
            self.schedule_redraw(output.clone(), target_frame_time + delay);
            true
        } else {
            self.remove_scheduled_redraw();
            false
        }
    }

    fn dequeue_available_buffer(&mut self) -> Option<NonNull<pw_buffer>> {
        unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) }
    }

    fn queue_completed_buffers(&mut self) {
        let mut inner = self.inner.borrow_mut();

        // We want to queue buffers in order, so find the first still-rendering buffer, and queue
        // everything up to that. Even if there are completed buffers past the first
        // still-rendering buffer, we do not want to queue them, since that would send frames out
        // of order.
        let first_in_progress_idx = inner
            .rendering_buffers
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(inner.rendering_buffers.len());

        for (buffer, _) in inner.rendering_buffers.drain(..first_in_progress_idx) {
            trace!("queueing completed buffer");
            unsafe {
                pw_stream_queue_buffer(self.stream.as_raw_ptr(), buffer.as_ptr());
            }
        }
    }

    unsafe fn queue_after_sync(&mut self, pw_buffer: NonNull<pw_buffer>, sync_point: SyncPoint) {
        let _span = tracy_client::span!("Cast::queue_after_sync");

        let mut inner = self.inner.borrow_mut();

        let mut sync_point = sync_point;
        let sync_fd = match sync_point.export() {
            Some(sync_fd) => Some(sync_fd),
            None => {
                // There are two main ways this can happen. First is that the SyncPoint is
                // pre-signalled, then the buffer is already ready and no waiting is needed. Second
                // is that the SyncPoint is potentially still not signalled, but exporting a fence
                // fd had failed. In this case, there's not much we can do (perhaps do a blocking
                // wait for the SyncPoint, which itself might fail).
                //
                // So let's hope for the best and mark the buffer as submittable. We do not reuse
                // the original SyncPoint because if we do hit the second case (when it's not
                // signalled), then without a sync fd we cannot schedule a queue upon its
                // completion, effectively going stuck. It's better to queue an incomplete buffer
                // than getting stuck.
                sync_point = SyncPoint::signaled();
                None
            }
        };

        inner.rendering_buffers.push((pw_buffer, sync_point));
        drop(inner);

        match sync_fd {
            None => {
                trace!("sync_fd is None, queueing completed buffers");
                // In case this is the only buffer in the list, we will queue it right away.
                self.queue_completed_buffers();
            }
            Some(sync_fd) => {
                trace!("scheduling buffer to queue");
                let stream_id = self.stream_id;
                let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
                self.event_loop
                    .insert_source(source, move |_, _, state| {
                        for cast in &mut state.niri.casting.casts {
                            if cast.stream_id == stream_id {
                                cast.queue_completed_buffers();
                            }
                        }

                        Ok(PostAction::Remove)
                    })
                    .unwrap();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dequeue_buffer_and_render(
        &mut self,
        renderer: &mut VulkanRenderer,
        mut elements: &[CastRenderElement],
        cursor_data: &CursorData<CastRenderElement>,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();

        let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            last_cursor_location,
            ..
        } = &mut inner.state
        else {
            error!("cast must be in Ready state to render");
            return false;
        };
        let damage_tracker = damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));
        let cursor_damage_tracker = cursor_damage_tracker.get_or_insert_with(|| {
            OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            )
        });

        // Size change will drop the damage tracker, but scale change won't, so check it here.
        let OutputModeSource::Static { scale: t_scale, .. } = damage_tracker.mode() else {
            unreachable!();
        };
        if *t_scale != scale {
            *damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            *cursor_damage_tracker = OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            );
        }

        let mut has_cursor_update = false;
        let mut redraw_cursor = false;

        // For embedded cursor, pass the full slice (cursor + main) to the damage tracker.
        // For metadata or hidden cursor, pass only the main elements.
        if self.cursor_mode == CursorMode::Metadata || self.cursor_mode == CursorMode::Hidden {
            elements = &elements[cursor_data.elem_count..];
        }
        let (damage, states) = damage_tracker.damage_output(1, elements).unwrap();

        if self.cursor_mode == CursorMode::Metadata {
            let (damage, _states) = cursor_damage_tracker
                .damage_output(1, &cursor_data.relocated)
                .unwrap();
            redraw_cursor = damage.is_some();
            has_cursor_update =
                redraw_cursor || *last_cursor_location != Some(cursor_data.location);
        }

        if damage.is_none() && !has_cursor_update {
            trace!("no damage, skipping frame");
            return false;
        }
        *last_cursor_location = Some(cursor_data.location);
        drop(inner);

        let Some(pw_buffer) = self.dequeue_available_buffer() else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        let mut inner = self.inner.borrow_mut();
        let inner_ = &mut *inner;
        let CastState::Ready {
            damage_tracker,
            sink,
            ..
        } = &mut inner_.state
        else {
            unreachable!()
        };
        let sink = *sink;
        let damage_tracker = damage_tracker.as_mut().unwrap();

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CursorMode::Metadata {
                add_cursor_metadata(renderer, spa_buffer, cursor_data, redraw_cursor);
            }

            // FIXME: would be good to skip rendering the full frame if only the pointer changed.
            // Unfortunately, I think the OBS PipeWire code needs to be updated first to cleanly
            // allow for that codepath.
            let fd = (*(*spa_buffer).datas).fd;

            if sink == Sink::Memory {
                let Some(mapping) = inner_.memory_buffers.get(&fd) else {
                    warn!("no mapping for memory buffer fd={fd}, skipping frame");
                    drop(inner);
                    return_unused_buffer(&self.stream, pw_buffer);
                    return false;
                };
                let (dst, dst_len, stride) = (mapping.ptr.as_ptr(), mapping.len, mapping.stride);
                let (size, scale, transform) = damage_tracker.mode().try_into().unwrap();

                // Full frame, every time — see `render_and_copy_to_memory`. The damage tracker
                // above decided *whether* we render; it must not decide how much.
                let res = render_and_copy_to_memory(
                    renderer,
                    size,
                    scale,
                    transform,
                    dst,
                    stride,
                    elements.iter().rev(),
                );
                drop(inner);

                return match res {
                    Ok(()) => {
                        // The readback already synchronized with the GPU, so unlike the dmabuf
                        // path there is nothing left to wait for: queue it straight away.
                        let chunk = (*(*spa_buffer).datas).chunk;
                        (*chunk).offset = 0;
                        (*chunk).stride = stride as i32;

                        mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, dst_len as u32);
                        trace!("queueing memory buffer with seq={}", self.sequence_counter);
                        pw_stream_queue_buffer(self.stream.as_raw_ptr(), pw_buffer.as_ptr());
                        true
                    }
                    Err(err) => {
                        warn!("error rendering to memory buffer: {err:?}");
                        return_unused_buffer(&self.stream, pw_buffer);
                        false
                    }
                };
            }

            let dmabuf = inner_.dmabufs[&fd].clone();

            let res = render_to_dmabuf(renderer, damage_tracker, dmabuf, elements, states);
            drop(inner);

            match res {
                Ok(sync_point) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, DMABUF_CHUNK_SIZE);
                    trace!("queueing buffer with seq={}", self.sequence_counter);
                    self.queue_after_sync(pw_buffer, sync_point);
                    true
                }
                Err(err) => {
                    warn!("error rendering to dmabuf: {err:?}");
                    return_unused_buffer(&self.stream, pw_buffer);
                    false
                }
            }
        }
    }

    pub fn dequeue_buffer_and_clear(&mut self, renderer: &mut VulkanRenderer) -> bool {
        let mut inner = self.inner.borrow_mut();

        // Clear out the damage tracker if we're in Ready state.
        if let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            ..
        } = &mut inner.state
        {
            *damage_tracker = None;
            *cursor_damage_tracker = None;
        };
        drop(inner);

        let Some(pw_buffer) = self.dequeue_available_buffer() else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CursorMode::Metadata {
                add_invisible_cursor(spa_buffer);
            }

            let fd = (*(*spa_buffer).datas).fd;

            // A memory sink has nothing in `dmabufs`, and indexing it would take the compositor
            // down the first time a cast target disappeared. Zero the mapping instead — that *is*
            // the clear for this sink, and it needs no GPU work or fence.
            if let Some(mapping) = self.inner.borrow().memory_buffers.get(&fd) {
                ptr::write_bytes(mapping.ptr.as_ptr(), 0, mapping.len);

                let chunk = (*(*spa_buffer).datas).chunk;
                (*chunk).offset = 0;
                (*chunk).stride = mapping.stride as i32;

                mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, mapping.len as u32);
                trace!(
                    "queueing cleared memory buffer with seq={}",
                    self.sequence_counter
                );
                pw_stream_queue_buffer(self.stream.as_raw_ptr(), pw_buffer.as_ptr());
                return true;
            }

            let dmabuf = self.inner.borrow().dmabufs[&fd].clone();

            match clear_dmabuf(renderer, dmabuf) {
                Ok(sync_point) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, DMABUF_CHUNK_SIZE);
                    trace!("queueing clear buffer with seq={}", self.sequence_counter);
                    self.queue_after_sync(pw_buffer, sync_point);
                    true
                }
                Err(err) => {
                    warn!("error clearing dmabuf: {err:?}");
                    return_unused_buffer(&self.stream, pw_buffer);
                    false
                }
            }
        }
    }
}

impl CastState {
    fn pending_size(&self) -> Option<Size<u32, Physical>> {
        match self {
            CastState::ResizePending { pending_size } => Some(*pending_size),
            CastState::ConfirmationPending { size, .. } => Some(*size),
            CastState::Ready { .. } => None,
        }
    }

    fn expected_format_size(&self) -> Size<u32, Physical> {
        match self {
            CastState::ResizePending { pending_size } => *pending_size,
            CastState::ConfirmationPending { size, .. } => *size,
            CastState::Ready { size, .. } => *size,
        }
    }
}

fn pw_version_supports_cursor_metadata() -> bool {
    // This PipeWire version fixed a critical memory issue with cursor metadata:
    // https://gitlab.freedesktop.org/pipewire/pipewire/-/merge_requests/2538
    unsafe { pw_check_library_version(1, 4, 8) }
}

/// One `EnumFormat` offer.
///
/// `with_modifier: false` emits the format with **no** `VideoModifier` property at all, which is
/// what lets PipeWire fall back to MemFd/MemPtr buffers. Mutter offers every format both ways —
/// `build_format_params` (`meta-screen-cast-stream-src.c:1576-1592`) loops all formats with
/// modifiers and then loops them all again without, and `push_format_object` only attaches the
/// modifier when it has one (`:297`). We used to send the modifier variant alone, and a consumer
/// that could not import any of our modifiers was left with nothing to accept: OBS and
/// gnome-software both died at connect with `no more input formats`.
fn make_video_params(
    formats: &FormatSet,
    size: Size<u32, Physical>,
    refresh: u32,
    alpha: bool,
    with_modifier: bool,
) -> Option<pod::Object> {
    let format = if alpha {
        VideoFormat::BGRA
    } else {
        VideoFormat::BGRx
    };

    let fourcc = if alpha {
        Fourcc::Argb8888
    } else {
        Fourcc::Xrgb8888
    };

    let modifiers: Vec<_> = formats
        .iter()
        .filter_map(|f| (f.code == fourcc).then_some(u64::from(f.modifier) as i64))
        .collect();

    let mut properties = vec![
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, format),
    ];

    if with_modifier {
        // Nothing to advertise: mutter simply does not emit this variant (`:1526-1531`), and
        // emitting it with an empty choice would offer a format nothing can satisfy.
        if modifiers.is_empty() {
            debug!("no modifiers for {fourcc}, not offering it with modifiers");
            return None;
        }

        trace!("offering {fourcc} with modifiers: {modifiers:?}");

        let dont_fixate = if modifiers.len() > 1 {
            PropertyFlags::DONT_FIXATE
        } else {
            PropertyFlags::empty()
        };

        properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | dont_fixate,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifiers[0],
                    alternatives: modifiers,
                },
            ))),
        });
    } else {
        trace!("offering {fourcc} without a modifier (memory buffers)");
    }

    properties.extend([
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: refresh,
                denom: 1000
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1000
            }
        ),
    ]);

    Some(pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    })
}

/// A memfd we allocated for a [`Sink::Memory`] buffer, kept mapped for the buffer's lifetime.
///
/// PipeWire hands us an empty buffer and we own the storage behind it, so the mapping has to live
/// exactly as long as the `pw_buffer` does — [`remove_buffer`](StreamRef) is what unmaps it. The
/// fd itself is owned by the `spa_data` after we set it, and PipeWire closes it.
#[derive(Debug)]
struct MemoryBuffer {
    fd: i64,
    ptr: NonNull<u8>,
    len: usize,
    /// Bytes per row, as announced in the buffer's chunk. Carried rather than recomputed so the
    /// render path cannot derive a different stride from the one the consumer was told.
    stride: usize,
}

// The pointer is a private mmap handle used only from the PipeWire loop thread; it is never shared
// and never aliased, so the raw pointer does not make the struct thread-unsafe on its own.
unsafe impl Send for MemoryBuffer {}

impl Drop for MemoryBuffer {
    fn drop(&mut self) {
        unsafe {
            if libc::munmap(self.ptr.as_ptr().cast(), self.len) < 0 {
                warn!(
                    "error unmapping screencast memfd: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
}

/// Allocate, seal and map a memfd, and point `buffer`'s single block at it.
///
/// Mirrors mutter's fallback path (`meta-screen-cast-stream-src.c:2318-2358`): sealed against
/// resize so the consumer can trust `maxsize`, mapped `MAP_SHARED` so our writes are what it
/// reads, and flagged readable+mappable rather than read-write — the consumer only ever reads.
///
/// # Safety
///
/// `buffer` must be a live `pw_buffer` with at least one data block, as delivered by `add_buffer`.
unsafe fn attach_memfd(
    buffer: *mut pw_buffer,
    size: Size<u32, Physical>,
) -> anyhow::Result<MemoryBuffer> {
    unsafe {
        let spa_buffer = (*buffer).buffer;
        ensure!((*spa_buffer).n_datas >= 1, "no data blocks in the buffer");

        let spa_data = (*spa_buffer).datas;
        ensure!(
            (*spa_data).type_ & (1 << DataType::MemFd.as_raw()) > 0,
            "buffer does not accept MemFd"
        );

        let stride = size.w as usize * 4;
        let len = stride * size.h as usize;

        let name = c"gnome-shell-rs-screencast";
        let fd = libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        ensure!(
            fd >= 0,
            "memfd_create failed: {}",
            io::Error::last_os_error()
        );
        // From here on the fd is ours to close on failure; on success it belongs to the spa_data.
        let guard = OwnedFd::from_raw_fd(fd);

        ensure!(
            libc::ftruncate(fd, len as libc::off_t) == 0,
            "ftruncate to {len} failed: {}",
            io::Error::last_os_error()
        );

        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if libc::fcntl(fd, libc::F_ADD_SEALS, seals) == -1 {
            // Mutter only warns here too: sealing is a hardening measure, not a requirement.
            warn!(
                "failed to seal screencast memfd: {}",
                io::Error::last_os_error()
            );
        }

        let ptr = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        ensure!(
            ptr != libc::MAP_FAILED,
            "mmap of {len} bytes failed: {}",
            io::Error::last_os_error()
        );
        let ptr = NonNull::new(ptr.cast::<u8>()).context("mmap returned null")?;

        (*spa_data).type_ = DataType::MemFd.as_raw();
        (*spa_data).flags = SPA_DATA_FLAG_READABLE | SPA_DATA_FLAG_MAPPABLE;
        (*spa_data).fd = guard.into_raw_fd() as i64;
        (*spa_data).maxsize = len as u32;
        (*spa_data).mapoffset = 0;
        (*spa_data).data = ptr.as_ptr().cast();

        let chunk = (*spa_data).chunk;
        (*chunk).stride = stride as i32;
        (*chunk).offset = 0;
        (*chunk).size = len as u32;

        trace!(
            "memfd buffer: fd={}, stride={stride}, len={len}",
            (*spa_data).fd
        );

        Ok(MemoryBuffer {
            fd: (*spa_data).fd,
            ptr,
            len,
            stride,
        })
    }
}

/// Announce the buffer layout the negotiated [`Sink`] needs.
///
/// Shared by both negotiation outcomes so the block count and the data type can never disagree
/// with the sink the stream actually settled on — the memory path wants one block of MemFd, the
/// dmabuf path one block per plane. Mutter selects the type the same way
/// (`meta-screen-cast-stream-src.c:1929-1934`).
fn update_buffer_params(
    stream: &Stream,
    sink: Sink,
    cursor_mode: CursorMode,
    stop_cast: &impl Fn(),
) {
    let data_type = match sink {
        Sink::Dmabuf { .. } => 1 << DataType::DmaBuf.as_raw(),
        Sink::Memory => 1 << DataType::MemFd.as_raw(),
    };

    let o1 = pod::object!(
        SpaTypes::ObjectParamBuffers,
        ParamType::Buffers,
        Property::new(
            SPA_PARAM_BUFFERS_buffers,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: 8,
                    min: 2,
                    max: 16
                }
            ))),
        ),
        Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(sink.blocks())),
        Property::new(
            SPA_PARAM_BUFFERS_dataType,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: data_type,
                    flags: vec![data_type],
                },
            ))),
        ),
    );

    let o2 = pod::object!(
        SpaTypes::ObjectParamMeta,
        ParamType::Meta,
        Property::new(
            SPA_PARAM_META_type,
            pod::Value::Id(spa::utils::Id(SPA_META_Header))
        ),
        Property::new(
            SPA_PARAM_META_size,
            pod::Value::Int(size_of::<spa_meta_header>() as i32)
        ),
    );
    let mut b1 = vec![];
    let mut b2 = vec![];
    let mut params = vec![make_pod(&mut b1, o1), make_pod(&mut b2, o2)];

    let mut b_cursor = vec![];
    if cursor_mode == CursorMode::Metadata {
        let o_cursor = pod::object!(
            SpaTypes::ObjectParamMeta,
            ParamType::Meta,
            Property::new(
                SPA_PARAM_META_type,
                pod::Value::Id(spa::utils::Id(SPA_META_Cursor))
            ),
            Property::new(
                SPA_PARAM_META_size,
                pod::Value::Int(CURSOR_META_SIZE as i32)
            ),
        );
        params.push(make_pod(&mut b_cursor, o_cursor));
    }

    if let Err(err) = stream.update_params(&mut params) {
        warn!("error updating stream params: {err:?}");
        stop_cast();
    }
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}

fn find_preferred_modifier(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: Vec<i64>,
) -> anyhow::Result<(Modifier, usize)> {
    debug!("find_preferred_modifier: size={size:?}, fourcc={fourcc}, modifiers={modifiers:?}");

    let (buffer, modifier) = allocate_buffer(gbm, size, fourcc, &modifiers)?;

    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")?;
    let plane_count = dmabuf.num_planes();

    // FIXME: Ideally this also needs to try binding the dmabuf for rendering.

    Ok((modifier, plane_count))
}

fn allocate_buffer(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: &[i64],
) -> anyhow::Result<(GbmBuffer, Modifier)> {
    let (w, h) = (size.w, size.h);
    let flags = GbmBufferFlags::RENDERING;

    if modifiers.len() == 1 && Modifier::from(modifiers[0] as u64) == Modifier::Invalid {
        let bo = gbm
            .create_buffer_object::<()>(w, h, fourcc, flags)
            .context("error creating GBM buffer object")?;

        let buffer = GbmBuffer::from_bo(bo, true);
        Ok((buffer, Modifier::Invalid))
    } else {
        let modifiers = modifiers
            .iter()
            .map(|m| Modifier::from(*m as u64))
            .filter(|m| *m != Modifier::Invalid);

        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(w, h, fourcc, modifiers, flags)
            .context("error creating GBM buffer object")?;

        let modifier = bo.modifier();
        let buffer = GbmBuffer::from_bo(bo, false);
        Ok((buffer, modifier))
    }
}

fn allocate_dmabuf(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifier: Modifier,
) -> anyhow::Result<Dmabuf> {
    let (buffer, _modifier) = allocate_buffer(gbm, size, fourcc, &[u64::from(modifier) as i64])?;
    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")?;
    Ok(dmabuf)
}

unsafe fn return_unused_buffer(stream: &Stream, pw_buffer: NonNull<pw_buffer>) {
    // pw_stream_return_buffer() requires too new PipeWire (1.4.0). So, mark as
    // corrupted and queue.
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;
    // Some (older?) consumers will check for size == 0 instead of the CORRUPTED flag.
    (*chunk).size = 0;
    (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;

    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        (*header).flags = SPA_META_HEADER_FLAG_CORRUPTED;
    }

    pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer);
}

/// The `chunk.size` a **dmabuf** frame carries.
///
/// With DMA-BUFs consumers should ignore the size field and producers may set it to 0
/// (<https://docs.pipewire.org/page_dma_buf.html>), but OBS checks `size != 0` as a workaround for
/// old compositor versions, so it has to be non-zero.
const DMABUF_CHUNK_SIZE: u32 = 1;

/// Hand a filled buffer to the consumer.
///
/// `chunk_size` is **not** optional and has no sensible default, which is the point: for a dmabuf
/// it is the meaningless [`DMABUF_CHUNK_SIZE`] sentinel, but for a memory buffer it is the number
/// of valid bytes and the consumer believes it. This function used to hardcode the dmabuf
/// sentinel; when the memory sink landed it silently overwrote the real length that
/// `dequeue_buffer_and_render` had just written, so every 10MB frame announced one valid byte and
/// OBS kept showing its last good frame while our buffers were perfectly correct.
unsafe fn mark_buffer_as_good(pw_buffer: NonNull<pw_buffer>, sequence: &mut u64, chunk_size: u32) {
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;

    (*chunk).size = chunk_size;
    // Clear the corrupted flag we may have set before.
    (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;

    *sequence = sequence.wrapping_add(1);
    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        // Clear the corrupted flag we may have set before.
        (*header).flags = 0;
        (*header).seq = *sequence;
    }
}

unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    let p = spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
    NonNull::new(p)
}

unsafe fn add_invisible_cursor(spa_buffer: *mut spa_buffer) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        // The cursor is present but invisible.
        cursor_meta.id = 1;
        cursor_meta.position.x = 0;
        cursor_meta.position.y = 0;
        cursor_meta.hotspot.x = 0;
        cursor_meta.hotspot.y = 0;
        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // HACK: PipeWire docs say offset = 0 means invisible.
        //
        // Unfortunately, OBS doesn't actually check that, instead it checks that size isn't zero:
        // https://github.com/obsproject/obs-studio/blob/f4aaa5f0417c5ec40a3799551e125129fce1e007/plugins/linux-pipewire/pipewire.c#L900
        //
        // Unfortunately, libwebrtc, on top of ignoring offset, also treats size = 0 as "preserve
        // previous cursor":
        // https://webrtc.googlesource.com/src/+/97b46e12582606a238d4f0c8524365cf5bdcb411/modules/desktop_capture/linux/wayland/shared_screencast_stream.cc#765
        //
        // So, send a 1x1 transparent pixel instead...
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);
    }
}

unsafe fn add_cursor_metadata(
    renderer: &mut VulkanRenderer,
    spa_buffer: *mut spa_buffer,
    cursor_data: &CursorData<impl RenderElement<VulkanRenderer>>,
    redraw: bool,
) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        cursor_meta.id = 1;
        cursor_meta.position.x = cursor_data.location.x;
        cursor_meta.position.y = cursor_data.location.y;
        cursor_meta.hotspot.x = cursor_data.hotspot.x;
        cursor_meta.hotspot.y = cursor_data.hotspot.y;

        if !redraw {
            trace!("cursor not damaged, skipping rerendering");
            cursor_meta.bitmap_offset = 0;
            return;
        }

        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // Start with a 1x1 transparent pixel; see comment in add_invisible_cursor().
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);

        let size = Size::new(
            min(cursor_data.size.w, CURSOR_WIDTH as i32),
            min(cursor_data.size.h, CURSOR_HEIGHT as i32),
        );
        if size.w == 0 || size.h == 0 {
            trace!("cursor is invisible, skipping rendering");
            return;
        }

        let _span = tracy_client::span!("add_cursor_metadata render cursor");

        // FIXME: use a reliable buffer whenever we're rendering the cursor.
        //
        // PipeWire buffers are not normally guaranteed to reach the destination, so our buffer
        // with the rendered cursor bitmap may not reach the consumer.
        //
        // Reliable buffers should be available starting from 1.6.0:
        // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/4885
        // Render RGBA but read back `Argb8888` — byte order B,G,R,A, the `CURSOR_FORMAT`
        // (`SPA_VIDEO_FORMAT_BGRA`) the bitmap declares. The owned Vulkan renderer can only render
        // RGBA-order offscreens (asking for a BGRA one fails outright, and the cursor used to
        // silently disappear from the stream), but it converts on readback, so the channel swap
        // happens on the GPU instead of in a CPU pass over the bitmap.
        let mapping = match render_and_download_as(
            renderer,
            size,
            cursor_data.scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            Fourcc::Argb8888,
            cursor_data.relocated.iter().rev(),
        ) {
            Ok(mapping) => mapping,
            Err(err) => {
                warn!("error rendering cursor: {err:?}");
                return;
            }
        };
        let pixels = match renderer.map_texture(&mapping) {
            Ok(pixels) => pixels,
            Err(err) => {
                warn!("error mapping cursor texture: {err:?}");
                return;
            }
        };

        bitmap_slice[..pixels.len()].copy_from_slice(pixels);

        // Fill the metadata now that everything succeeded.
        bitmap_meta.size.width = size.w as _;
        bitmap_meta.size.height = size.h as _;
        bitmap_meta.stride = size.w * CURSOR_BPP as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_format_set() -> FormatSet {
        FormatSet::from_iter([
            Format {
                code: Fourcc::Xrgb8888,
                modifier: Modifier::Linear,
            },
            Format {
                code: Fourcc::Xrgb8888,
                modifier: Modifier::Invalid,
            },
            Format {
                code: Fourcc::Argb8888,
                modifier: Modifier::Linear,
            },
        ])
    }

    fn params(object: &pod::Object) -> Vec<u32> {
        object.properties.iter().map(|p| p.key).collect()
    }

    fn has_modifier(object: &pod::Object) -> bool {
        params(object).contains(&FormatProperties::VideoModifier.as_raw())
    }

    /// The whole point of the memory fallback: the modifier-less offer must carry **no**
    /// `VideoModifier` property at all. Emitting it with an empty or wildcard choice would still
    /// be a dmabuf offer, and a consumer that cannot import one would keep failing negotiation
    /// with "no more input formats" — which is exactly how OBS and gnome-software died.
    #[test]
    fn the_memory_offer_carries_no_modifier() {
        let size = Size::from((1920, 1080));

        let with = make_video_params(&a_format_set(), size, 60_000, false, true)
            .expect("we have Xrgb modifiers, so the dmabuf offer must exist");
        assert!(has_modifier(&with), "the dmabuf offer must name modifiers");

        let without = make_video_params(&a_format_set(), size, 60_000, false, false)
            .expect("the memory offer never depends on having modifiers");
        assert!(
            !has_modifier(&without),
            "the memory offer must omit VideoModifier entirely, got {:?}",
            params(&without)
        );

        // Both still describe the same video format, so a consumer picking either gets the
        // frames it asked for.
        for object in [&with, &without] {
            let keys = params(object);
            for required in [
                FormatProperties::MediaType,
                FormatProperties::MediaSubtype,
                FormatProperties::VideoFormat,
                FormatProperties::VideoSize,
                FormatProperties::VideoFramerate,
            ] {
                assert!(
                    keys.contains(&required.as_raw()),
                    "{required:?} missing from an offer"
                );
            }
        }
    }

    /// A format we have no modifiers for gets no dmabuf offer — mutter returns early rather than
    /// advertising an empty modifier list (`meta-screen-cast-stream-src.c:1526-1531`). The memory
    /// offer is unaffected, which is what keeps such a format usable at all.
    #[test]
    fn a_format_without_modifiers_is_only_offered_as_memory() {
        let size = Size::from((800, 600));
        // Only Argb has modifiers here, so the Xrgb (alpha = false) dmabuf offer has nothing.
        let only_argb = FormatSet::from_iter([Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        }]);

        assert!(
            make_video_params(&only_argb, size, 60_000, false, true).is_none(),
            "no modifiers for Xrgb, so there must be no dmabuf offer for it"
        );
        assert!(
            make_video_params(&only_argb, size, 60_000, false, false).is_some(),
            "the memory offer must survive having no modifiers"
        );
        assert!(
            make_video_params(&only_argb, size, 60_000, true, true).is_some(),
            "Argb does have a modifier, so its dmabuf offer stands"
        );
    }

    /// The block count follows the sink, because PipeWire is told it once and both the allocation
    /// and the render path have to agree with what it was told.
    #[test]
    fn a_memory_sink_asks_for_a_single_block() {
        assert_eq!(Sink::Memory.blocks(), 1);
        assert_eq!(
            Sink::Dmabuf {
                modifier: Modifier::Linear,
                plane_count: 3,
            }
            .blocks(),
            3
        );
    }
}
