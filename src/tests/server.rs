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
