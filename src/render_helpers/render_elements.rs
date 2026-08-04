// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

// Generates the element enums the render tree passes around: an `Element` impl that forwards to
// each variant, plus the `RenderElement<VulkanRenderer>` impl and the `From` impls.
//
// These enums used to come in a `$name<R>` flavour too, back when the tree was generic over the
// renderer. There is only one renderer now, so every variant type names it concretely.
#[macro_export]
macro_rules! synoik_render_elements {
    ($name:ident => { $($(#[$attr:meta])* $variant:ident = $type:ty),+ $(,)? }) => {
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug)]
        pub enum $name {
            $($(#[$attr])* $variant($type)),+
        }

        $($(#[$attr])* impl From<$type> for $name {
            fn from(x: $type) -> Self {
                Self::$variant(x)
            }
        })+

        impl smithay::backend::renderer::element::Element for $name {
            fn id(&self) -> &smithay::backend::renderer::element::Id {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.id()),+
                }
            }

            fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.current_commit()),+
                }
            }

            fn geometry(&self, scale: smithay::utils::Scale<f64>) -> smithay::utils::Rectangle<i32, smithay::utils::Physical> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.geometry(scale)),+
                }
            }

            fn transform(&self) -> smithay::utils::Transform {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.transform()),+
                }
            }

            fn src(&self) -> smithay::utils::Rectangle<f64, smithay::utils::Buffer> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.src()),+
                }
            }

            fn damage_since(
                &self,
                scale: smithay::utils::Scale<f64>,
                commit: Option<smithay::backend::renderer::utils::CommitCounter>,
            ) -> smithay::backend::renderer::utils::DamageSet<i32, smithay::utils::Physical> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.damage_since(scale, commit)),+
                }
            }

            fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> smithay::backend::renderer::utils::OpaqueRegions<i32, smithay::utils::Physical> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.opaque_regions(scale)),+
                }
            }

            fn alpha(&self) -> f32 {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.alpha()),+
                }
            }

            fn kind(&self) -> smithay::backend::renderer::element::Kind {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.kind()),+
                }
            }

            fn is_framebuffer_effect(&self) -> bool {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.is_framebuffer_effect()),+
                }
            }
        }

        impl smithay::backend::renderer::element::RenderElement<$crate::render_helpers::vulkan::VulkanRenderer>
            for $name
        {
            fn draw(
                &self,
                frame: &mut $crate::render_helpers::vulkan::VulkanFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                opaque_regions: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
                cache: Option<&smithay::utils::user_data::UserDataMap>,
            ) -> Result<(), $crate::render_helpers::vulkan::VulkanError> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => {
                        smithay::backend::renderer::element::RenderElement::<$crate::render_helpers::vulkan::VulkanRenderer>::draw(elem, frame, src, dst, damage, opaque_regions, cache)
                    })+
                }
            }

            // Forward capture_framebuffer to the variant. Smithay's default is `unimplemented!()`
            // (a panic), and it IS called by OutputDamageTracker for any element whose
            // `is_framebuffer_effect()` is true (the GNOME blur/postprocess element) — so the enum
            // must dispatch, and every variant must provide at least a no-op (the degraded effects
            // do, via `degraded_vulkan_element!`), or a Vulkan session panics the moment a
            // framebuffer effect is on screen.
            fn capture_framebuffer(
                &self,
                frame: &mut $crate::render_helpers::vulkan::VulkanFrame<'_, '_>,
                src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
                dst: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
                cache: &smithay::utils::user_data::UserDataMap,
            ) -> Result<(), $crate::render_helpers::vulkan::VulkanError> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => {
                        smithay::backend::renderer::element::RenderElement::<$crate::render_helpers::vulkan::VulkanRenderer>::capture_framebuffer(elem, frame, src, dst, cache)
                    })+
                }
            }

            fn underlying_storage(
                &self,
                renderer: &mut $crate::render_helpers::vulkan::VulkanRenderer,
            ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
                match self {
                    $($(#[$attr])* $name::$variant(elem) => elem.underlying_storage(renderer)),+
                }
            }
        }
    };
}
