use std::time::Duration;

use calloop::EventLoop;
use niri_config::Config;
use smithay::reexports::wayland_server::Display;

use crate::backend::{BackendMode, RendererKind};
use crate::niri::State;

pub struct Server {
    pub event_loop: EventLoop<'static, State>,
    pub state: State,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let display = Display::new().unwrap();
        let state = State::new(
            config,
            handle.clone(),
            event_loop.get_signal(),
            display,
            BackendMode::HeadlessTest,
            RendererKind::Gles,
            false,
            false,
        )
        .unwrap();

        Self { event_loop, state }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();
        self.state.refresh_and_flush_clients();
    }
}

/// `--renderer=vulkan` is only wired on the headless backend (and only with the `vulkan` feature).
/// Requesting it on the auto (winit/tty) backend — or in a build without the feature — must be
/// rejected cleanly by `State::new`, before any backend/socket setup. Covers both the per-backend
/// bail (vulkan build) and the missing-feature bail (default build) with one assertion.
#[test]
fn vulkan_renderer_is_rejected_where_unsupported() {
    let event_loop = EventLoop::try_new().unwrap();
    let display = Display::new().unwrap();
    let result = State::new(
        Config::default(),
        event_loop.handle(),
        event_loop.get_signal(),
        display,
        BackendMode::Auto,
        RendererKind::Vulkan,
        false,
        false,
    );
    let err = result
        .err()
        .expect("Vulkan on the auto backend must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("vulkan"),
        "unexpected error: {err}"
    );
}
