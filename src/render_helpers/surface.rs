// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Scale};
use smithay::wayland::compositor::{with_surface_tree_downward, SurfaceData, TraversalAction};

use crate::render_helpers::background_effect::BackgroundEffectElement;
use crate::render_helpers::vulkan::VulkanRenderer;

/// What a surface-tree walk emits: the surfaces themselves, plus anything a caller's hook wants
/// interleaved between them.
///
/// The two cannot be separate callbacks. `with_surface_tree_downward` walks nearest-to-deepest and
/// we push front-to-back, so an element that must sit *directly below* a given surface has to be
/// pushed immediately after it — which means one ordered stream, not two lists to merge afterwards.
/// The per-surface hook [`push_elements_from_surface_tree_with_effects`] calls: given the renderer,
/// the surface and its physical origin, push any effects that belong beneath it.
pub type SurfaceEffectHook<'a> = dyn FnMut(
        &mut VulkanRenderer,
        &WlSurface,
        &SurfaceData,
        Point<f64, Physical>,
        &mut dyn FnMut(BackgroundEffectElement),
    ) + 'a;

// `Surface` is ~456 bytes against `Effect`'s handful, which clippy wants boxed. It is the hot
// variant — one per surface in every tree walk — so boxing it would trade a padded stack slot for
// a heap allocation per element, on the path that runs most. The values are moved straight into a
// push callback and dropped, never stored in a long-lived collection, so the padding costs nothing
// that lasts.
#[allow(clippy::large_enum_variant)]
pub enum SurfaceTreeElement {
    Surface(WaylandSurfaceRenderElement<VulkanRenderer>),
    /// A background effect belonging to the surface just pushed, and therefore drawn beneath it.
    Effect(BackgroundEffectElement),
}

pub fn push_elements_from_surface_tree(
    renderer: &mut VulkanRenderer,
    surface: &WlSurface,
    // Fractional scale expects surface buffers to be aligned to physical pixels.
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    kind: Kind,
    push: &mut dyn FnMut(WaylandSurfaceRenderElement<VulkanRenderer>),
) {
    push_elements_from_surface_tree_with_effects(
        renderer,
        surface,
        location,
        scale,
        alpha,
        kind,
        &mut |elem| match elem {
            SurfaceTreeElement::Surface(s) => push(s),
            SurfaceTreeElement::Effect(_) => unreachable!("no hook was installed"),
        },
        &mut |_, _, _, _, _| {},
    );
}

/// As [`push_elements_from_surface_tree`], but `after_each` runs once per surface, immediately
/// after that surface's own element, and may push effects that belong *beneath* it.
///
/// This is how a subsurface gets a background effect: `ext-background-effect-v1` lets a client
/// attach one to any `wl_surface`, and a client with blurred chrome on a subsurface expects that
/// chrome's backdrop — everything drawn below it, its own parent surface included — to be blurred.
/// Resolving effects only for the toplevel cannot express that, because the effect would land under
/// the whole window rather than under the one subsurface.
#[allow(clippy::too_many_arguments)]
pub fn push_elements_from_surface_tree_with_effects(
    renderer: &mut VulkanRenderer,
    surface: &WlSurface,
    // Fractional scale expects surface buffers to be aligned to physical pixels.
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    kind: Kind,
    push: &mut dyn FnMut(SurfaceTreeElement),
    after_each: &mut SurfaceEffectHook<'_>,
) {
    let _span = tracy_client::span!("push_elements_from_surface_tree");

    let location = location.to_f64();

    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64().to_physical(scale);
                    TraversalAction::DoChildren(location)
                } else {
                    TraversalAction::SkipChildren
                }
            } else {
                TraversalAction::SkipChildren
            }
        },
        |surface, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                let has_view = if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64().to_physical(scale);
                    true
                } else {
                    false
                };

                if has_view {
                    match WaylandSurfaceRenderElement::from_surface(
                        renderer, surface, states, location, alpha, kind,
                    ) {
                        Ok(Some(elem)) => push(SurfaceTreeElement::Surface(elem)),
                        Ok(None) => {} // surface is not mapped
                        Err(err) => {
                            warn!("failed to import surface: {}", err);
                        }
                    };

                    // After the surface, so anything pushed here lands beneath it.
                    after_each(renderer, surface, states, location, &mut |effect| {
                        push(SurfaceTreeElement::Effect(effect))
                    });
                }
            }
        },
        |_, _, _| true,
    );
}
