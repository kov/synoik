// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cmp::min;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use smithay::reexports::wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, Capability, ExtBackgroundEffectManagerV1,
};
use smithay::reexports::wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;
use smithay::reexports::wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use smithay::reexports::wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1,
};
use smithay::reexports::wayland_protocols::wp::single_pixel_buffer;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    self, ZwpTextInputV3,
};
use smithay::reexports::wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay::reexports::wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    self, ZwlrLayerShellV1,
};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_backend::client::Backend;

use crate::protocols::raw::xdg_session_management::v1::client::xdg_session_manager_v1::{
    Reason, XdgSessionManagerV1,
};
use crate::protocols::raw::xdg_session_management::v1::client::xdg_session_v1::{
    self, XdgSessionV1,
};
use crate::protocols::raw::xdg_session_management::v1::client::xdg_toplevel_session_v1::{
    self, XdgToplevelSessionV1,
};
use wayland_client::globals::Global;
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_data_offer::WlDataOffer;
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};

use crate::utils::id::IdCounter;

pub struct Client {
    pub id: ClientId,
    pub event_loop: EventLoop<'static, State>,
    pub connection: Connection,
    pub qh: QueueHandle<State>,
    pub display: WlDisplay,
    pub state: State,
}

pub struct State {
    pub qh: QueueHandle<State>,

    pub globals: Vec<Global>,
    pub outputs: HashMap<WlOutput, String>,

    pub compositor: Option<WlCompositor>,
    pub subcompositor: Option<WlSubcompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub spbm: Option<WpSinglePixelBufferManagerV1>,
    pub shm: Option<WlShm>,
    pub viewporter: Option<WpViewporter>,
    pub background_effect_manager: Option<ExtBackgroundEffectManagerV1>,
    /// The capabilities the compositor announced on bind, if the global was there at all.
    pub background_effect_capabilities: Option<Capability>,
    pub seat: Option<WlSeat>,
    pub shortcuts_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    pub screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    /// The in-flight wlr-screencopy capture, if any. One at a time is enough for tests.
    pub screencopy: Option<ScreencopyCapture>,

    pub data_device_manager: Option<WlDataDeviceManager>,
    /// The seat's data device, once [`Client::offer_clipboard`] made one.
    pub data_device: Option<WlDataDevice>,
    /// The selection this client is offering, and what it writes when asked for it.
    pub data_source: Option<WlDataSource>,
    pub selection_payload: Vec<u8>,
    /// Mime types the compositor asked this client's source for, in order.
    pub selection_sends: Vec<String>,

    pub text_input_manager: Option<ZwpTextInputManagerV3>,
    /// The text-input object, once [`Client::create_text_input`] made one.
    pub text_input: Option<ZwpTextInputV3>,
    /// Everything the compositor sent on it, in order.
    pub text_input_events: Vec<TextInputEvent>,

    pub keyboard: Option<WlKeyboard>,
    /// `wl_keyboard.key` events received, as `(evdev code, state)`.
    pub key_events: Vec<(u32, wl_keyboard::KeyState)>,

    pub session_manager: Option<XdgSessionManagerV1>,
    /// Every `xdg_session_v1` / `xdg_toplevel_session_v1` event this client received, in order.
    pub session_events: Vec<SessionEvent>,

    pub windows: Vec<Window>,
    pub layers: Vec<LayerSurface>,
}

/// What the compositor told us about a session, flattened across objects because a fixture client
/// only ever juggles a couple of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// `xdg_session_v1.created`, carrying the id the compositor minted.
    Created(String),
    /// `xdg_session_v1.restored` — the id we passed was known.
    Restored,
    /// `xdg_session_v1.replaced` — another client took our session over.
    Replaced,
    /// `xdg_toplevel_session_v1.restored`, with the name we asked to restore.
    ToplevelRestored(String),
}

/// The in-flight state of one wlr-screencopy capture, updated by the frame's [`Dispatch`] impl.
pub struct ScreencopyCapture {
    pub frame: ZwlrScreencopyFrameV1,
    /// The shm `(format, width, height, stride)` the compositor asked for, from the `buffer`
    /// event.
    pub shm_params: Option<(wl_shm::Format, u32, u32, u32)>,
    pub buffer_done: bool,
    pub ready: bool,
    pub failed: bool,
}

/// A standalone shm buffer whose backing memfd the client keeps mapped, so it can read back what
/// the compositor rendered into it (the screencopy destination). Holds `pool` and `file` alive:
/// smithay keeps its own mapping of the fd, and dropping ours before the read could pull the memory
/// out from under it.
pub struct ShmReadback {
    pub buffer: WlBuffer,
    pool: WlShmPool,
    file: std::fs::File,
    len: usize,
}

impl ShmReadback {
    /// The buffer's current bytes (what the compositor last wrote).
    pub fn read(&mut self) -> Vec<u8> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        self.file.seek(SeekFrom::Start(0)).expect("seek shm memfd");
        let mut buf = vec![0u8; self.len];
        self.file.read_exact(&mut buf).expect("read shm memfd");
        buf
    }
}

impl Drop for ShmReadback {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

pub struct Window {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,
    // Only read by the `attach_shm_buffer*` helpers; bound unconditionally from `State.shm`.
    pub shm: Option<WlShm>,

    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub xdg_toplevel: XdgToplevel,
    pub viewport: WpViewport,
    pub shortcuts_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    pub pending_configure: Configure,
    pub configures_received: Vec<(u32, Configure)>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

pub struct LayerSurface {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,

    pub surface: WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub viewport: WpViewport,
    pub configures_received: Vec<(u32, LayerConfigure)>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Configure {
    pub size: (i32, i32),
    pub bounds: Option<(i32, i32)>,
    pub states: Vec<xdg_toplevel::State>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayerConfigure {
    pub size: (u32, u32),
}

#[derive(Clone, Copy, Default)]
pub struct LayerMargin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

#[derive(Clone, Copy, Default)]
pub struct LayerConfigureProps {
    pub size: Option<(u32, u32)>,
    pub anchor: Option<zwlr_layer_surface_v1::Anchor>,
    pub exclusive_zone: Option<i32>,
    pub margin: Option<LayerMargin>,
    pub kb_interactivity: Option<zwlr_layer_surface_v1::KeyboardInteractivity>,
    pub layer: Option<zwlr_layer_shell_v1::Layer>,
    pub exclusive_edge: Option<zwlr_layer_surface_v1::Anchor>,
}

#[derive(Default)]
pub struct SyncData {
    pub done: AtomicBool,
}

static CLIENT_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    fn next() -> ClientId {
        ClientId(CLIENT_ID_COUNTER.next())
    }
}

impl fmt::Display for Configure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}, ", self.size.0, self.size.1)?;
        if let Some(bounds) = self.bounds {
            write!(f, "bounds: {} × {}, ", bounds.0, bounds.1)?;
        } else {
            write!(f, "bounds: none, ")?;
        }
        write!(f, "states: {:?}", self.states)?;
        Ok(())
    }
}

impl fmt::Display for LayerConfigure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}", self.size.0, self.size.1)?;
        Ok(())
    }
}

impl Client {
    /// Take the clipboard, offering `payload` under each of `mime_types` — a real client-owned
    /// selection, which the compositor can only read back over a pipe.
    ///
    /// The client must have keyboard focus: smithay drops `set_selection` from anyone else
    /// (`data_device/device.rs:148-160`). The serial is unchecked there, so 0 will do.
    pub fn offer_clipboard(&mut self, mime_types: &[&str], payload: &[u8]) {
        let manager = self
            .state
            .data_device_manager
            .clone()
            .expect("compositor advertises wl_data_device_manager");
        let seat = self.state.seat.clone().expect("seat");
        let qh = self.state.qh.clone();

        let device = self
            .state
            .data_device
            .get_or_insert_with(|| manager.get_data_device(&seat, &qh, ()))
            .clone();
        let source = manager.create_data_source(&qh, ());
        for mime in mime_types {
            source.offer((*mime).to_owned());
        }
        device.set_selection(Some(&source), 0);

        self.state.data_source = Some(source);
        self.state.selection_payload = payload.to_vec();
        self.connection.flush().unwrap();
    }

    /// Create a `zwp_text_input_v3` on this client's seat.
    ///
    /// The compositor only sends `enter` when something is acting as the input method, so a
    /// text input created without one stays silent — which is exactly the state that takes
    /// GTK's own compose table away and gives it nothing back.
    pub fn create_text_input(&mut self) {
        let manager = self
            .state
            .text_input_manager
            .clone()
            .expect("compositor advertises zwp_text_input_manager_v3");
        let seat = self.state.seat.clone().expect("seat");
        let qh = self.state.qh.clone();
        self.state.text_input = Some(manager.get_text_input(&seat, &qh, ()));
    }

    /// `enable` + `commit` — what a client does when its entry takes focus.
    pub fn enable_text_input(&mut self) {
        let ti = self.state.text_input.clone().expect("text input");
        ti.enable();
        ti.commit();
    }

    /// `set_surrounding_text` + `commit`.
    pub fn set_surrounding_text(&mut self, text: &str, cursor: i32, anchor: i32) {
        let ti = self.state.text_input.clone().expect("text input");
        ti.set_surrounding_text(text.to_owned(), cursor, anchor);
        ti.commit();
    }

    /// Declare what kind of field the caret is in — a password entry, say.
    pub fn set_content_type(
        &mut self,
        hint: zwp_text_input_v3::ContentHint,
        purpose: zwp_text_input_v3::ContentPurpose,
    ) {
        let ti = self.state.text_input.clone().expect("text input");
        ti.set_content_type(hint, purpose);
        ti.commit();
    }

    /// Drain what the compositor has sent the text input so far.
    pub fn text_input_events(&mut self) -> Vec<TextInputEvent> {
        std::mem::take(&mut self.state.text_input_events)
    }

    pub fn new(stream: UnixStream) -> Self {
        let id = ClientId::next();

        let event_loop = EventLoop::try_new().unwrap();
        let backend = Backend::connect(stream).unwrap();
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();

        let display = connection.display();
        let _registry = display.get_registry(&qh, ());
        connection.flush().unwrap();

        let state = State {
            qh: qh.clone(),
            globals: Vec::new(),
            data_device_manager: None,
            data_device: None,
            data_source: None,
            selection_payload: Vec::new(),
            selection_sends: Vec::new(),
            text_input_manager: None,
            text_input: None,
            text_input_events: Vec::new(),
            outputs: HashMap::new(),
            compositor: None,
            subcompositor: None,
            xdg_wm_base: None,
            layer_shell: None,
            spbm: None,
            shm: None,
            viewporter: None,
            background_effect_manager: None,
            background_effect_capabilities: None,
            seat: None,
            shortcuts_inhibit_manager: None,
            screencopy_manager: None,
            screencopy: None,
            keyboard: None,
            key_events: Vec::new(),
            session_manager: None,
            session_events: Vec::new(),
            windows: Vec::new(),
            layers: Vec::new(),
        };

        Self {
            id,
            event_loop,
            connection,
            qh,
            display,
            state,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();

        if let Some(error) = self.connection.protocol_error() {
            panic!("{error}");
        }
    }

    pub fn send_sync(&self) -> Arc<SyncData> {
        let data = Arc::new(SyncData::default());
        self.display.sync(&self.qh, data.clone());
        self.connection.flush().unwrap();
        data
    }

    pub fn create_window(&mut self) -> &mut Window {
        self.state.create_window()
    }

    /// `xdg_session_manager_v1.get_session`. `session_id` is `None` to ask for a fresh session.
    pub fn get_session(&mut self, reason: Reason, session_id: Option<&str>) -> XdgSessionV1 {
        self.state.get_session(reason, session_id)
    }

    /// Every session event received so far, in order.
    pub fn session_events(&self) -> &[SessionEvent] {
        &self.state.session_events
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.state.window(surface)
    }

    pub fn create_subsurface(
        &mut self,
        parent: &WlSurface,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: [u32; 4],
    ) -> (WlSurface, WlSubsurface) {
        self.state.create_subsurface(parent, x, y, w, h, color)
    }

    pub fn set_opaque_region(
        &mut self,
        surface: &WlSurface,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        self.state.set_opaque_region(surface, x, y, width, height)
    }

    pub fn set_blur_region(
        &mut self,
        surface: &WlSurface,
        rect: (i32, i32, i32, i32),
    ) -> ExtBackgroundEffectSurfaceV1 {
        self.state.set_blur_region(surface, rect)
    }

    pub fn update_blur_region(
        &mut self,
        effect: &ExtBackgroundEffectSurfaceV1,
        surface: &WlSurface,
        rect: (i32, i32, i32, i32),
    ) {
        self.state.update_blur_region(effect, surface, rect);
    }

    pub fn background_effect_capabilities(&self) -> Option<Capability> {
        self.state.background_effect_capabilities
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: &str,
    ) -> &mut LayerSurface {
        self.state.create_layer(output, layer, namespace.to_owned())
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.state.layer(surface)
    }

    pub fn inhibit_shortcuts(&mut self, surface: &WlSurface) {
        self.state.inhibit_shortcuts(surface);
    }

    /// Begin a wlr-screencopy capture of `output`. The compositor answers with `buffer` +
    /// `buffer_done` events describing the destination it wants; roundtrip until
    /// `self.state.screencopy.buffer_done`, then create the matching buffer and call
    /// [`Self::copy_screencopy`].
    pub fn begin_screencopy(&mut self, output: &WlOutput) {
        let manager = self
            .state
            .screencopy_manager
            .clone()
            .expect("wlr-screencopy manager not bound");
        // overlay_cursor = 0: composite without the pointer.
        let frame = manager.capture_output(0, output, &self.qh, ());
        self.state.screencopy = Some(ScreencopyCapture {
            frame,
            shm_params: None,
            buffer_done: false,
            ready: false,
            failed: false,
        });
    }

    /// Hand the compositor `readback`'s buffer to fill. A plain `copy` (not `copy_with_damage`)
    /// renders synchronously server-side, so one roundtrip after this delivers `ready`.
    pub fn copy_screencopy(&self, readback: &ShmReadback) {
        let capture = self
            .state
            .screencopy
            .as_ref()
            .expect("no capture in flight");
        capture.frame.copy(&readback.buffer);
    }

    /// Create a standalone `w`×`h` shm buffer in `format`, backed by a memfd the client keeps so it
    /// can [`ShmReadback::read`] back what the compositor rendered. Not attached to any surface.
    pub fn create_shm_readback_buffer(
        &self,
        w: i32,
        h: i32,
        format: wl_shm::Format,
    ) -> ShmReadback {
        use std::os::fd::AsFd;

        use smithay::reexports::rustix::fs::{ftruncate, memfd_create, MemfdFlags};

        let shm = self.state.shm.as_ref().expect("wl_shm not bound");
        let stride = w * 4;
        let len = (stride * h) as usize;

        let fd = memfd_create("synoik-test-screencopy", MemfdFlags::CLOEXEC).expect("memfd_create");
        ftruncate(&fd, len as u64).expect("ftruncate");
        let file = std::fs::File::from(fd);

        let pool = shm.create_pool(file.as_fd(), len as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, format, &self.qh, ());
        ShmReadback {
            buffer,
            pool,
            file,
            len,
        }
    }

    pub fn release_shortcuts_inhibitor(&mut self, surface: &WlSurface) {
        self.state.release_shortcuts_inhibitor(surface);
    }

    /// Start receiving `wl_keyboard` events; they accumulate in
    /// [`take_key_events`].
    ///
    /// [`take_key_events`]: Self::take_key_events
    pub fn get_keyboard(&mut self) {
        let seat = self.state.seat.clone().unwrap();
        self.state.keyboard = Some(seat.get_keyboard(&self.qh, ()));
    }

    /// Drain the `wl_keyboard.key` events received so far, as
    /// `(evdev code, state)`.
    pub fn take_key_events(&mut self) -> Vec<(u32, wl_keyboard::KeyState)> {
        std::mem::take(&mut self.state.key_events)
    }

    pub fn output(&mut self, name: &str) -> WlOutput {
        self.state
            .outputs
            .iter()
            .find(|(_, v)| *v == name)
            .unwrap()
            .0
            .clone()
    }
}

impl State {
    /// Set an opaque region on `surface` built from one `wl_region.add` rectangle, then commit.
    ///
    /// `width`/`height` go on the wire verbatim, so a test can send a **negative** extent — which
    /// the protocol permits and real clients do send (Firefox, while resizing). Used to pin that
    /// the compositor survives it.
    /// Ask for background blur behind `rect` of `surface`, through
    /// `ext-background-effect-v1`, and commit it.
    ///
    /// Returns the effect object: the protocol allows only one per surface (a second is a
    /// `background_effect_exists` error), so a caller that wants to change or unset the region
    /// keeps this rather than calling again.
    pub fn set_blur_region(
        &mut self,
        surface: &WlSurface,
        rect: (i32, i32, i32, i32),
    ) -> ExtBackgroundEffectSurfaceV1 {
        let compositor = self.compositor.as_ref().unwrap();
        let manager = self.background_effect_manager.as_ref().unwrap();

        let effect = manager.get_background_effect(surface, &self.qh, ());
        let region = compositor.create_region(&self.qh, ());
        let (x, y, width, height) = rect;
        region.add(x, y, width, height);
        effect.set_blur_region(Some(&region));
        region.destroy();
        surface.commit();
        effect
    }

    /// Re-specify the region on an effect the surface already has.
    ///
    /// The protocol allows only one effect object per surface, so calling
    /// [`set_blur_region`](Self::set_blur_region) twice is a protocol error, not a second region.
    /// A client that reshapes its blur on resize — which is the normal case — must come through
    /// here with the object it was handed the first time.
    pub fn update_blur_region(
        &mut self,
        effect: &ExtBackgroundEffectSurfaceV1,
        surface: &WlSurface,
        rect: (i32, i32, i32, i32),
    ) {
        let compositor = self.compositor.as_ref().unwrap();
        let region = compositor.create_region(&self.qh, ());
        let (x, y, width, height) = rect;
        region.add(x, y, width, height);
        effect.set_blur_region(Some(&region));
        region.destroy();
        surface.commit();
    }

    pub fn set_opaque_region(
        &mut self,
        surface: &WlSurface,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        let compositor = self.compositor.as_ref().unwrap();
        let region = compositor.create_region(&self.qh, ());
        region.add(x, y, width, height);
        surface.set_opaque_region(Some(&region));
        surface.commit();
        region.destroy();
    }

    pub fn get_session(&mut self, reason: Reason, session_id: Option<&str>) -> XdgSessionV1 {
        let manager = self
            .session_manager
            .as_ref()
            .expect("the compositor must advertise xdg_session_manager_v1");
        manager.get_session(reason, session_id.map(String::from), &self.qh, ())
    }

    pub fn create_window(&mut self) -> &mut Window {
        let compositor = self.compositor.as_ref().unwrap();
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        let xdg_toplevel = xdg_surface.get_toplevel(&self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let window = Window {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),
            shm: self.shm.clone(),

            surface,
            xdg_surface,
            xdg_toplevel,
            viewport,
            shortcuts_inhibitor: None,
            pending_configure: Configure::default(),
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.windows.push(window);
        self.windows.last_mut().unwrap()
    }

    /// Add a `wl_subsurface` under `parent` at `(x, y)`, with a solid buffer already attached and
    /// committed.
    ///
    /// Returned in **synchronized** mode, which is the `wl_subsurface` default: the subsurface's
    /// state is not applied until the *parent* commits. A caller that wants it on screen has to
    /// commit the parent afterwards — forgetting that is the usual reason a subsurface test sees
    /// nothing and blames the compositor.
    pub fn create_subsurface(
        &mut self,
        parent: &WlSurface,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: [u32; 4],
    ) -> (WlSurface, WlSubsurface) {
        let compositor = self.compositor.as_ref().unwrap();
        let subcompositor = self
            .subcompositor
            .as_ref()
            .expect("the compositor must advertise wl_subcompositor");

        let surface = compositor.create_surface(&self.qh, ());
        let subsurface = subcompositor.get_subsurface(&surface, parent, &self.qh, ());
        subsurface.set_position(x, y);

        // A single-pixel buffer is 1x1, and the surface's *view* is what the compositor sizes
        // everything else from — an effect over this surface would come out one pixel wide, and
        // `damage_buffer(0, 0, w, h)` would not fix that because damage is not size. A viewport
        // destination is what makes the surface genuinely `w`x`h` without needing real pixels.
        let viewport = self
            .viewporter
            .as_ref()
            .expect("the compositor must advertise wp_viewporter")
            .get_viewport(&surface, &self.qh, ());
        viewport.set_destination(w, h);

        let buffer = self.spbm.as_ref().unwrap().create_u32_rgba_buffer(
            color[0],
            color[1],
            color[2],
            color[3],
            &self.qh,
            (),
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w, h);
        surface.commit();

        (surface, subsurface)
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.windows
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: String,
    ) -> &mut LayerSurface {
        let compositor = self.compositor.as_ref().unwrap();
        let layer_shell = self.layer_shell.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let layer_surface =
            layer_shell.get_layer_surface(&surface, output, layer, namespace, &self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let layer_surface = LayerSurface {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface,
            layer_surface,
            viewport,
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.layers.push(layer_surface);
        self.layers.last_mut().unwrap()
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.layers
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    pub fn inhibit_shortcuts(&mut self, surface: &WlSurface) {
        let manager = self.shortcuts_inhibit_manager.clone().unwrap();
        let seat = self.seat.clone().unwrap();
        let inhibitor = manager.inhibit_shortcuts(surface, &seat, &self.qh, ());
        self.window(surface).shortcuts_inhibitor = Some(inhibitor);
    }

    pub fn release_shortcuts_inhibitor(&mut self, surface: &WlSurface) {
        let inhibitor = self.window(surface).shortcuts_inhibitor.take().unwrap();
        inhibitor.destroy();
    }
}

impl Window {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.xdg_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    /// Attach a null buffer, which unmaps the surface on the next commit.
    pub fn attach_null_buffer(&self) {
        self.surface.attach(None, 0, 0);
    }

    /// Attach an opaque single-pixel buffer of the given color (premultiplied u32 channels, as the
    /// protocol expects), so the mapped window has visible content in a screenshot.
    // Only the `vulkan` render tests map client buffers, so gate the attach helpers on it to avoid
    // a dead-code warning in the default build.
    pub fn attach_solid_buffer(&self, r: u32, g: u32, b: u32, a: u32) {
        let buffer = self.spbm.create_u32_rgba_buffer(r, g, b, a, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    /// Attach an opaque `w`×`h` **shm** buffer filled with a solid RGBA color. Unlike a
    /// single-pixel buffer, this carries a real texture, so the compositor's snapshot path
    /// (`render_snapshot_from_surface_tree`) can bake it and the renderer's shm import/cache path
    /// runs — required to exercise resize/close animations and the shm texture cache. Uses `wl_shm`
    /// `Argb8888` (0xAARRGGBB little-endian ⇒ bytes `[B, G, R, A]`).
    pub fn attach_shm_buffer(&self, w: i32, h: i32, r: u8, g: u8, b: u8, a: u8) {
        self.attach_shm_buffer_with_format(w, h, [b, g, r, a], wl_shm::Format::Argb8888);
    }

    /// As [`Self::attach_shm_buffer`], but a `wl_shm` `Abgr8888` buffer with `[R, G, B, A]` memory
    /// order — a *different* fourcc at the same size, to exercise the renderer's format-change
    /// re-import: Argb/Abgr map to different VkFormats, so a wrong same-size cache reuse would
    /// sample the new bytes through the old view and swap R↔B (red would read back as blue).
    pub fn attach_shm_buffer_abgr(&self, w: i32, h: i32, r: u8, g: u8, b: u8, a: u8) {
        self.attach_shm_buffer_with_format(w, h, [r, g, b, a], wl_shm::Format::Abgr8888);
    }

    /// Attach a `w`×`h` shm buffer tiling the 4-byte `texel` (already in `format`'s memory order).
    fn attach_shm_buffer_with_format(
        &self,
        w: i32,
        h: i32,
        texel: [u8; 4],
        format: wl_shm::Format,
    ) {
        use std::io::Write as _;
        use std::os::fd::{AsFd, OwnedFd};

        use smithay::reexports::rustix::fs::{ftruncate, memfd_create, MemfdFlags};

        let shm = self.shm.as_ref().expect("wl_shm not bound");
        let stride = w * 4;
        let size = (stride * h) as usize;

        let fd = memfd_create("synoik-test-shm", MemfdFlags::CLOEXEC).expect("memfd_create");
        ftruncate(&fd, size as u64).expect("ftruncate");

        let data: Vec<u8> = texel.iter().copied().cycle().take(size).collect();
        let mut file = std::fs::File::from(fd);
        file.write_all(&data).expect("write shm buffer");
        let fd: OwnedFd = file.into();

        let pool = shm.create_pool(fd.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, format, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, w, h);
        pool.destroy();
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    /// The xdg window geometry — what a configure's size actually refers to.
    ///
    /// A client whose decorations are subsurfaces *above* its content sends a negative `y` and a
    /// height larger than its own buffer. Until those subsurfaces have buffers of their own they
    /// are not in the surface tree's bounding box, so the declared geometry is legitimately
    /// clamped down to the content — see `a_window_is_not_configured_smaller_than_it_asked_for`.
    pub fn set_window_geometry(&self, x: i32, y: i32, w: i32, h: i32) {
        self.xdg_surface.set_window_geometry(x, y, w, h);
    }

    pub fn set_fullscreen(&self, output: Option<&WlOutput>) {
        self.xdg_toplevel.set_fullscreen(output);
    }

    pub fn unset_fullscreen(&self) {
        self.xdg_toplevel.unset_fullscreen();
    }

    pub fn set_maximized(&self) {
        self.xdg_toplevel.set_maximized();
    }

    pub fn unset_maximized(&self) {
        self.xdg_toplevel.unset_maximized();
    }

    pub fn set_parent(&self, parent: Option<&XdgToplevel>) {
        self.xdg_toplevel.set_parent(parent);
    }

    pub fn set_title(&self, title: &str) {
        self.xdg_toplevel.set_title(title.to_owned());
    }

    pub fn set_app_id(&self, app_id: &str) {
        self.xdg_toplevel.set_app_id(app_id.to_owned());
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &Configure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl LayerSurface {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.layer_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn set_configure_props(&self, props: LayerConfigureProps) {
        let LayerConfigureProps {
            size,
            anchor,
            exclusive_zone,
            margin,
            kb_interactivity,
            layer,
            exclusive_edge,
        } = props;

        if let Some(x) = size {
            self.layer_surface.set_size(x.0, x.1);
        }
        if let Some(x) = anchor {
            self.layer_surface.set_anchor(x);
        }
        if let Some(x) = exclusive_zone {
            self.layer_surface.set_exclusive_zone(x);
        }
        if let Some(x) = margin {
            self.layer_surface
                .set_margin(x.top, x.right, x.bottom, x.left);
        }
        if let Some(x) = kb_interactivity {
            self.layer_surface.set_keyboard_interactivity(x);
        }
        if let Some(x) = layer {
            self.layer_surface.set_layer(x);
        }
        if let Some(x) = exclusive_edge {
            self.layer_surface.set_exclusive_edge(x);
        }
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &LayerConfigure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl Dispatch<WlCallback, Arc<SyncData>> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCallback,
        event: <WlCallback as wayland_client::Proxy>::Event,
        data: &Arc<SyncData>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_callback::Event::Done { .. } => data.done.store(true, Ordering::Relaxed),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlCompositor::interface().name {
                    let version = min(version, WlCompositor::interface().version);
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlSubcompositor::interface().name {
                    let version = min(version, WlSubcompositor::interface().version);
                    state.subcompositor = Some(registry.bind(name, version, qh, ()));
                } else if interface == XdgWmBase::interface().name {
                    let version = min(version, XdgWmBase::interface().version);
                    state.xdg_wm_base = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwlrLayerShellV1::interface().name {
                    let version = min(version, ZwlrLayerShellV1::interface().version);
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpSinglePixelBufferManagerV1::interface().name {
                    let version = min(version, WpSinglePixelBufferManagerV1::interface().version);
                    state.spbm = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlShm::interface().name {
                    let version = min(version, WlShm::interface().version);
                    state.shm = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpViewporter::interface().name {
                    let version = min(version, WpViewporter::interface().version);
                    state.viewporter = Some(registry.bind(name, version, qh, ()));
                } else if interface == ExtBackgroundEffectManagerV1::interface().name {
                    let version = min(version, ExtBackgroundEffectManagerV1::interface().version);
                    state.background_effect_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlOutput::interface().name {
                    let version = min(version, WlOutput::interface().version);
                    let output = registry.bind(name, version, qh, ());
                    state.outputs.insert(output, String::new());
                } else if interface == WlDataDeviceManager::interface().name {
                    let version = min(version, WlDataDeviceManager::interface().version);
                    state.data_device_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwpTextInputManagerV3::interface().name {
                    let version = min(version, ZwpTextInputManagerV3::interface().version);
                    state.text_input_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlSeat::interface().name {
                    let version = min(version, WlSeat::interface().version);
                    state.seat = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwpKeyboardShortcutsInhibitManagerV1::interface().name {
                    let version = min(
                        version,
                        ZwpKeyboardShortcutsInhibitManagerV1::interface().version,
                    );
                    state.shortcuts_inhibit_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwlrScreencopyManagerV1::interface().name {
                    let version = min(version, ZwlrScreencopyManagerV1::interface().version);
                    state.screencopy_manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == XdgSessionManagerV1::interface().name {
                    let version = min(version, XdgSessionManagerV1::interface().version);
                    state.session_manager = Some(registry.bind(name, version, qh, ()));
                }

                let global = Global {
                    name,
                    interface,
                    version,
                };
                state.globals.push(global);
            }
            wl_registry::Event::GlobalRemove { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Geometry { .. } => (),
            wl_output::Event::Mode { .. } => (),
            wl_output::Event::Done => (),
            wl_output::Event::Scale { .. } => (),
            wl_output::Event::Name { name } => {
                *state.outputs.get_mut(output).unwrap() = name;
            }
            wl_output::Event::Description { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSubcompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubcompositor,
        _event: <WlSubcompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSubsurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSubsurface,
        _event: <WlSubsurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _state: &mut Self,
        xdg_wm_base: &XdgWmBase,
        event: <XdgWmBase as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_wm_base::Event::Ping { serial } => {
                xdg_wm_base.pong(serial);
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => (),
            wl_surface::Event::Leave { .. } => (),
            wl_surface::Event::PreferredBufferScale { .. } => (),
            wl_surface::Event::PreferredBufferTransform { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: <XdgSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_surface::Event::Configure { serial } => {
                let window = state
                    .windows
                    .iter_mut()
                    .find(|w| w.xdg_surface == *xdg_surface)
                    .unwrap();
                let configure = window.pending_configure.clone();
                window.configures_received.push((serial, configure));
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        xdg_toplevel: &XdgToplevel,
        event: <XdgToplevel as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let window = state
            .windows
            .iter_mut()
            .find(|w| w.xdg_toplevel == *xdg_toplevel)
            .unwrap();

        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let configure = &mut window.pending_configure;
                configure.size = (width, height);
                configure.states = states
                    .chunks_exact(4)
                    .flat_map(TryInto::<[u8; 4]>::try_into)
                    .map(u32::from_ne_bytes)
                    .flat_map(xdg_toplevel::State::try_from)
                    .collect();
            }
            xdg_toplevel::Event::Close => {
                window.close_requested = true;
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } => {
                window.pending_configure.bounds = Some((width, height));
            }
            xdg_toplevel::Event::WmCapabilities { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let layer_surface = state
            .layers
            .iter_mut()
            .find(|w| w.layer_surface == *layer_surface)
            .unwrap();

        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                let configure = LayerConfigure {
                    size: (width, height),
                };
                layer_surface.configures_received.push((serial, configure));
            }
            zwlr_layer_surface_v1::Event::Closed => layer_surface.close_requested = true,
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Only the `format` advertisement; we hardcode Argb8888, so ignore it.
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpSinglePixelBufferManagerV1,
        _event: <WpSinglePixelBufferManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(capture) = state.screencopy.as_mut() else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let format = format.into_result().expect("unknown shm format");
                capture.shm_params = Some((format, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => capture.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => capture.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => capture.failed = true,
            // LinuxDmabuf/Damage/Flags: not needed for the shm byte check.
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        event: <ExtBackgroundEffectManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_background_effect_manager_v1::Event::Capabilities { flags } => {
                state.background_effect_capabilities = flags.into_result().ok();
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectSurfaceV1,
        _event: <ExtBackgroundEffectSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewport, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { .. } => (),
            wl_seat::Event::Name { .. } => (),
            _ => unreachable!(),
        }
    }
}

/// What the compositor sent a `zwp_text_input_v3`, flattened for assertions.
#[derive(Debug, Clone, PartialEq)]
pub enum TextInputEvent {
    Enter,
    Leave,
    PreeditString {
        text: Option<String>,
        cursor_begin: i32,
        cursor_end: i32,
    },
    CommitString(Option<String>),
    DeleteSurroundingText {
        before_length: u32,
        after_length: u32,
    },
    Done(u32),
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDevice,
        _event: <WlDataDevice as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Offers coming *back* to this client are not what these tests are about.
    }

    wayland_client::event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataOffer,
        _event: <WlDataOffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: <WlDataSource as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { mime_type, fd } = event {
            state.selection_sends.push(mime_type);
            // Write and close: the reader is waiting on EOF, and an fd left open here would
            // hold the compositor's paste open until its timeout.
            let mut file = std::fs::File::from(fd);
            use std::io::Write as _;
            let _ = file.write_all(&state.selection_payload);
        }
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let recorded = match event {
            zwp_text_input_v3::Event::Enter { .. } => TextInputEvent::Enter,
            zwp_text_input_v3::Event::Leave { .. } => TextInputEvent::Leave,
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => TextInputEvent::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            },
            zwp_text_input_v3::Event::CommitString { text } => TextInputEvent::CommitString(text),
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => TextInputEvent::DeleteSurroundingText {
                before_length,
                after_length,
            },
            zwp_text_input_v3::Event::Done { serial } => TextInputEvent::Done(serial),
            _ => return,
        };
        state.text_input_events.push(recorded);
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { .. } => (),
            wl_keyboard::Event::Enter { .. } => (),
            wl_keyboard::Event::Leave { .. } => (),
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                state
                    .key_events
                    .push((key, key_state.into_result().unwrap()));
            }
            wl_keyboard::Event::Modifiers { .. } => (),
            wl_keyboard::Event::RepeatInfo { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpKeyboardShortcutsInhibitManagerV1,
        _event: <ZwpKeyboardShortcutsInhibitManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpKeyboardShortcutsInhibitorV1,
        event: <ZwpKeyboardShortcutsInhibitorV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => (),
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgSessionManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &XdgSessionManagerV1,
        _event: <XdgSessionManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // xdg_session_manager_v1 has no events.
    }
}

impl Dispatch<XdgSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &XdgSessionV1,
        event: <XdgSessionV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let event = match event {
            xdg_session_v1::Event::Created { session_id } => SessionEvent::Created(session_id),
            xdg_session_v1::Event::Restored => SessionEvent::Restored,
            xdg_session_v1::Event::Replaced => SessionEvent::Replaced,
        };
        state.session_events.push(event);
    }
}

/// The toplevel session's `restored` carries no name, so the name rides along as user data.
impl Dispatch<XdgToplevelSessionV1, String> for State {
    fn event(
        state: &mut Self,
        _proxy: &XdgToplevelSessionV1,
        event: <XdgToplevelSessionV1 as wayland_client::Proxy>::Event,
        name: &String,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let xdg_toplevel_session_v1::Event::Restored = event;
        state
            .session_events
            .push(SessionEvent::ToplevelRestored(name.clone()));
    }
}
