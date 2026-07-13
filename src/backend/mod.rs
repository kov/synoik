use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use niri_config::{Config, ModKey};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::niri::Niri;
use crate::utils::id::IdCounter;

pub mod tty;
pub use tty::Tty;

pub mod winit;
pub use winit::Winit;

pub mod headless;
pub use headless::Headless;

#[allow(clippy::large_enum_variant)]
pub enum Backend {
    Tty(Tty),
    Winit(Winit),
    Headless(Headless),
}

/// Which backend to start the compositor with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendMode {
    /// Winit when nested inside a session, TTY on a bare VT.
    Auto,
    /// Headless: no display or input devices, driven over IPC.
    Headless,
    /// Headless for the in-process test fixture: additionally skips external
    /// integrations (GSettings/dconf) to keep tests hermetic.
    HeadlessTest,
}

/// Which renderer draws the compositor's output.
///
/// GLES is the default production path. Vulkan selects the fork's experimental owned Vulkan
/// renderer (`docs/fork/STRATEGY.md` §3.10) and requires a build with the `vulkan` feature; it is
/// currently only wired on the headless backend (Winit/TTY reject it — no live present path yet,
/// that's Stage 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RendererKind {
    #[default]
    Gles,
    Vulkan,
}

#[derive(PartialEq, Eq)]
pub enum RenderResult {
    /// The frame was submitted to the backend for presentation.
    Submitted,
    /// Rendering succeeded, but there was no damage.
    NoDamage,
    /// The frame was not rendered and submitted, due to an error or otherwise.
    Skipped,
}

pub type IpcOutputMap = HashMap<OutputId, niri_ipc::Output>;

static OUTPUT_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(u64);

impl OutputId {
    fn next() -> OutputId {
        OutputId(OUTPUT_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl Backend {
    pub fn init(&mut self, niri: &mut Niri) {
        let _span = tracy_client::span!("Backend::init");
        match self {
            Backend::Tty(tty) => tty.init(niri),
            Backend::Winit(winit) => winit.init(niri),
            Backend::Headless(headless) => headless.init(niri),
        }
    }

    pub fn seat_name(&self) -> String {
        match self {
            Backend::Tty(tty) => tty.seat_name(),
            Backend::Winit(winit) => winit.seat_name(),
            Backend::Headless(headless) => headless.seat_name(),
        }
    }

    /// Whether this backend composites through the owned Vulkan renderer (rather than GLES).
    /// Governs Vulkan-only capture paths such as the resize crossfade's neutral snapshot buffer.
    pub fn using_vulkan(&self) -> bool {
        match self {
            Backend::Tty(tty) => tty.using_vulkan(),
            Backend::Winit(_) => false,
            Backend::Headless(headless) => headless.using_vulkan(),
        }
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        match self {
            Backend::Tty(tty) => tty.with_primary_renderer(f),
            Backend::Winit(winit) => winit.with_primary_renderer(f),
            Backend::Headless(headless) => headless.with_primary_renderer(f),
        }
    }

    /// Run `f` with the owned Vulkan renderer if this backend composites through it, else `None`.
    /// The dual of [`Self::with_primary_renderer`] (which yields the always-GLES primary renderer)
    /// for owned-renderer-only setup such as installing custom animation shaders.
    ///
    /// Must yield a renderer for every backend [`Self::using_vulkan`] reports true for: the capture
    /// paths dispatch on that flag and have no GLES fallback once it is set, so a backend that
    /// claims Vulkan but hands out nothing here silently loses screencast and screencopy.
    #[cfg(feature = "vulkan")]
    pub fn with_vulkan_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut crate::render_helpers::vulkan::VulkanRenderer) -> T,
    ) -> Option<T> {
        match self {
            Backend::Tty(tty) => tty.with_vulkan_renderer(f),
            Backend::Headless(headless) => headless.with_vulkan_renderer(f),
            Backend::Winit(_) => None,
        }
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        match self {
            Backend::Tty(tty) => tty.render(niri, output, target_presentation_time),
            Backend::Winit(winit) => winit.render(niri, output),
            Backend::Headless(headless) => headless.render(niri, output),
        }
    }

    pub fn mod_key(&self, config: &Config) -> ModKey {
        match self {
            Backend::Winit(_) => config.input.mod_key_nested.unwrap_or({
                if let Some(ModKey::Alt) = config.input.mod_key {
                    ModKey::Super
                } else {
                    ModKey::Alt
                }
            }),
            Backend::Tty(_) | Backend::Headless(_) => config.input.mod_key.unwrap_or(ModKey::Super),
        }
    }

    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Tty(tty) => tty.change_vt(vt),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn suspend(&mut self) {
        match self {
            Backend::Tty(tty) => tty.suspend(),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn toggle_debug_tint(&mut self) {
        match self {
            Backend::Tty(tty) => tty.toggle_debug_tint(),
            Backend::Winit(winit) => winit.toggle_debug_tint(),
            Backend::Headless(_) => (),
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            Backend::Tty(tty) => tty.import_dmabuf(dmabuf),
            Backend::Winit(winit) => winit.import_dmabuf(dmabuf),
            Backend::Headless(headless) => headless.import_dmabuf(dmabuf),
        }
    }

    /// Prefetch a just-committed client buffer into the renderer's texture cache.
    ///
    /// Skipped entirely on a Vulkan session: this uploads into the *GLES* texture cache, keyed by
    /// the GLES context, and nothing a Vulkan session displays ever samples it — the owned renderer
    /// imports through its own `ImportAll`. Left in, it is a full client-buffer upload per surface
    /// per commit (every frame, for every animating client) into a renderer that draws nothing.
    ///
    /// It is a prefetch, not a correctness dependency: the GLES paths that do still run on a Vulkan
    /// session (the snapshot bakes) import lazily themselves via `import_surface`
    /// (`render_helpers/surface.rs`), so at worst they pay a cold import instead of a warm one.
    ///
    /// Gated here rather than inside `Tty` so the headless Vulkan suite exercises the same skip.
    pub fn early_import(&mut self, surface: &WlSurface) {
        if self.using_vulkan() {
            return;
        }

        match self {
            Backend::Tty(tty) => tty.early_import(surface),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        match self {
            Backend::Tty(tty) => tty.ipc_outputs(),
            Backend::Winit(winit) => winit.ipc_outputs(),
            Backend::Headless(headless) => headless.ipc_outputs(),
        }
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(
        &self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::backend::drm::DrmDeviceFd>>
    {
        match self {
            Backend::Tty(tty) => tty.primary_gbm_device(),
            Backend::Winit(_) => None,
            Backend::Headless(_) => None,
        }
    }

    pub fn set_monitors_active(&mut self, active: bool) {
        match self {
            Backend::Tty(tty) => tty.set_monitors_active(active),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn set_output_on_demand_vrr(&mut self, niri: &mut Niri, output: &Output, enable_vrr: bool) {
        match self {
            Backend::Tty(tty) => tty.set_output_on_demand_vrr(niri, output, enable_vrr),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn update_ignored_nodes_config(&mut self, niri: &mut Niri) {
        match self {
            Backend::Tty(tty) => tty.update_ignored_nodes_config(niri),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn on_output_config_changed(&mut self, niri: &mut Niri) {
        match self {
            Backend::Tty(tty) => tty.on_output_config_changed(niri),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn tty_checked(&mut self) -> Option<&mut Tty> {
        if let Self::Tty(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn tty(&mut self) -> &mut Tty {
        if let Self::Tty(v) = self {
            v
        } else {
            panic!("backend is not Tty");
        }
    }

    pub fn winit(&mut self) -> &mut Winit {
        if let Self::Winit(v) = self {
            v
        } else {
            panic!("backend is not Winit")
        }
    }

    pub fn headless(&mut self) -> &mut Headless {
        if let Self::Headless(v) = self {
            v
        } else {
            panic!("backend is not Headless")
        }
    }
}
