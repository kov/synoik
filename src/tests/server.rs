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
        Self::new_with_renderer(config, RendererKind::Gles)
    }

    pub fn new_with_renderer(config: Config, renderer: RendererKind) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let display = Display::new().unwrap();
        let state = State::new(
            config,
            handle.clone(),
            event_loop.get_signal(),
            display,
            BackendMode::HeadlessTest,
            renderer,
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

/// `State::new`'s handling of `--renderer=vulkan` on the auto backend depends on the build and the
/// selected backend. In a build *without* the `vulkan` feature it is rejected up front regardless
/// of backend. With the feature, the tty backend now supports Vulkan (it scans out through the
/// owned renderer), so only the winit backend still rejects it. `State::new` always fails in a
/// headless test (no seat/TTY, no display), so we assert on *which* error we get.
#[test]
fn vulkan_renderer_matches_backend_support() {
    let has_display = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("WAYLAND_SOCKET").is_some()
        || std::env::var_os("DISPLAY").is_some();

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
        .expect("Vulkan on the auto backend cannot succeed in a headless test");
    let msg = err.to_string().to_lowercase();

    #[cfg(not(feature = "vulkan"))]
    {
        let _ = has_display;
        // No feature: rejected before any backend selection, with a clear "vulkan" message.
        assert!(msg.contains("vulkan"), "unexpected error: {err}");
    }
    #[cfg(feature = "vulkan")]
    if has_display {
        // Auto picks the winit backend, which has no Vulkan present path yet.
        assert!(msg.contains("winit"), "unexpected error: {err}");
    } else {
        // Auto picks the tty backend, which now supports Vulkan: the renderer-kind check passes and
        // we fail only in backend init (no seat/TTY here), not with a "not yet supported" bail.
        assert!(
            !msg.contains("not yet supported"),
            "Vulkan should be accepted on the tty backend: {err}"
        );
    }
}
