//! Offscreen render target + a minimal quad graphics pipeline, built on the [`Gpu`] context.
//!
//! A classic render pass / framebuffer (not dynamic rendering) keeps the spike portable across
//! ICDs without enabling extra features. The target's final layout is `TRANSFER_SRC_OPTIMAL`, so
//! [`RenderTarget::read_back`] can copy straight to a host buffer after the pass.

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;

pub const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

pub const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// Identity value for the `proj` output-transform field (column-major `[m00, m10, m01, m11]`).
/// The `#[derive(Default)]` push blocks default `proj` to zeros (a collapse-to-nothing matrix);
/// `VulkanFrame` overwrites `proj` on every draw, so that only matters for a hand-built push that
/// forgets to — use this to stay explicit.
pub const IDENTITY_PROJ: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

/// Push-constant block shared by `quad.vert` and its material fragment stages. `repr(C)` layout
/// matches the GLSL `Push` block (std430 push-constant rules: `proj` at offset 16, `color` at 48,
/// `tex_transform` columns at 64/80/96, `cutoff` at 112). Materials that don't sample (e.g.
/// `solid.frag`, `sdf_rect.frag`) simply declare a shorter `Push` block ending at `color` — a
/// shader may access a prefix of the range. 120 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct QuadPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2, column-major `[m00, m10, m01, m11]` for `mat2(pc.proj)` — rotates the
    /// (already y-down) ortho NDC into the physical framebuffer's orientation. Identity for
    /// `Transform::Normal`; see `VulkanFrame`'s `ndc_transform`.
    pub proj: [f32; 4],
    pub target: [f32; 2],
    pub corner_radius: f32,
    /// For `sdf_rect.frag`: `> 0` draws an inset stroke (ring) of this physical-px width instead
    /// of a solid fill. Padding (kept 0) for every other material sharing this block.
    pub stroke_width: f32,
    pub color: [f32; 4],
    /// Texture sampling transform as 3 `vec4` columns (`.xyz` used): maps `v_uv` (0..1 across the
    /// quad) to normalized texture UV, folding the src crop, the buffer/`src_transform` rotation
    /// or flip, and y-inversion — the owned-renderer analogue of GLES `build_texture_mat`.
    /// Identity-ish (a crop) for `Transform::Normal`. Used only by the sampling materials.
    pub tex_transform: [[f32; 4]; 3],
    /// Horizontal fade band `[left, right]` in the sampled texture's u coordinate, for
    /// `gradient_fade.frag`; `left >= right` disables the fade. Ignored by other materials.
    pub cutoff: [f32; 2],
}

// The Rust layout must match the GLSL std430 `Push` block byte-for-byte; catch drift at compile
// time (offsets: proj@16, color@48, tex_transform@64/80/96, cutoff@112).
const _: () = {
    assert!(std::mem::size_of::<QuadPush>() == 120);
    assert!(std::mem::offset_of!(QuadPush, color) == 48);
    assert!(std::mem::offset_of!(QuadPush, tex_transform) == 64);
    assert!(std::mem::offset_of!(QuadPush, cutoff) == 112);
};

/// Identity `tex_transform` (as 3 `vec4` columns): samples `v_uv` straight, i.e. the whole texture
/// with no rotation. The neutral value for a hand-built sampling push or a non-sampling material.
pub const IDENTITY_TEX_TRANSFORM: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// Push constants for the clipped-surface material (`quad.vert`/`clipped_texture.frag`) — the
/// window rounded-corner / clip-to-geometry path (niri's `ClippedSurfaceRenderElement`). The
/// leading fields through `tex_transform` mirror [`QuadPush`] byte-for-byte (so the shared
/// `quad.vert` places the quad and the sampling matches `texture.frag`); the tail carries the clip:
/// `geo_size` (logical geometry size, the rounding coordinate space), `clip_corner_radius` (per-
/// corner radii, logical px), `input_to_geo` (a `mat3` mapping `vec3(v_uv, 1)` to `[0, 1]` geometry
/// space, passed as three `vec4` columns — the shader reads `.xyz`), and `niri_scale` (edge AA).
/// 208 bytes — equal to the largest built-in ([`PostprocessPush`]); the renderer's push-budget
/// guard already covers it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClippedTexturePush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2 (see [`QuadPush::proj`]).
    pub proj: [f32; 4],
    pub target: [f32; 2],
    /// Unused SDF scalar; kept only so the block offsets match the shared vertex stage.
    pub corner_radius: f32,
    pub _pad0: f32,
    /// Straight-alpha tint `[1, 1, 1, alpha]` (see [`QuadPush::color`]).
    pub color: [f32; 4],
    /// Sampling transform (see [`QuadPush::tex_transform`]).
    pub tex_transform: [[f32; 4]; 3],
    /// Logical geometry size in pixels (the rounding coordinate space).
    pub geo_size: [f32; 2],
    pub _pad1: [f32; 2],
    /// Per-corner radii in logical pixels: `[top_left, top_right, bottom_right, bottom_left]`.
    pub clip_corner_radius: [f32; 4],
    /// `input_to_geo` as 3 column vectors (`.xyz` used); build from a `glam::Mat3`'s columns.
    pub input_to_geo: [[f32; 4]; 3],
    pub niri_scale: f32,
    pub _pad2: [f32; 3],
}

// The Rust layout must match the GLSL std430 `Push` block byte-for-byte; catch drift at compile
// time (offsets: proj@16, color@48, tex_transform@64/80/96, geo_size@112, clip_corner_radius@128,
// input_to_geo@144/160/176, niri_scale@192).
const _: () = {
    assert!(std::mem::size_of::<ClippedTexturePush>() == 208);
    assert!(std::mem::offset_of!(ClippedTexturePush, color) == 48);
    assert!(std::mem::offset_of!(ClippedTexturePush, tex_transform) == 64);
    assert!(std::mem::offset_of!(ClippedTexturePush, geo_size) == 112);
    assert!(std::mem::offset_of!(ClippedTexturePush, clip_corner_radius) == 128);
    assert!(std::mem::offset_of!(ClippedTexturePush, input_to_geo) == 144);
    assert!(std::mem::offset_of!(ClippedTexturePush, niri_scale) == 192);
};

/// Push constants for the border material (`border.vert`/`border.frag`). `repr(C)` field order is
/// chosen so each member lands at its natural std430 offset (all `vec4`s at 16-aligned offsets),
/// matching the GLSL block exactly. 152 bytes (well under the 256-byte push limit).
/// `origin`/`size`/`proj`/`target` mirror [`QuadPush`]'s first four fields so the shared
/// quad-emission vertex stage works.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BorderPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2 (see [`QuadPush::proj`]).
    pub proj: [f32; 4],
    pub target: [f32; 2],
    pub border_width: f32,
    pub colorspace: f32,
    pub color_from: [f32; 4],
    pub color_to: [f32; 4],
    pub outer_radius: [f32; 4],
    pub grad_offset: [f32; 2],
    pub grad_vec: [f32; 2],
    pub area_size: [f32; 2],
    pub geo_loc: [f32; 2],
    pub geo_size: [f32; 2],
    pub grad_width: f32,
    pub hue_interpolation: f32,
    pub niri_scale: f32,
    pub niri_alpha: f32,
}

/// Push constants for the shadow material (`shadow.vert`/`shadow.frag`). Same layout discipline as
/// [`BorderPush`] (natural std430 offsets, `origin`/`size`/`proj`/`target` first). 144 bytes.
/// `area_size` is shared by the shadow and window-cutout geometry transforms.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ShadowPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2 (see [`QuadPush::proj`]).
    pub proj: [f32; 4],
    pub target: [f32; 2],
    pub sigma: f32,
    pub niri_scale: f32,
    pub shadow_color: [f32; 4],
    pub corner_radius: [f32; 4],
    pub window_corner_radius: [f32; 4],
    pub area_size: [f32; 2],
    pub geo_loc: [f32; 2],
    pub geo_size: [f32; 2],
    pub window_geo_loc: [f32; 2],
    pub window_geo_size: [f32; 2],
    pub niri_alpha: f32,
    pub _pad0: f32,
}

/// Push constants for the postprocess-and-clip material (`postprocess.vert`/`postprocess.frag`).
/// Matches the GLSL `Push` block: `vec2`s paired and `vec4`s at 16-aligned offsets (natural std430
/// layout, `origin`/`size`/`proj`/`target` first like [`QuadPush`] so the shared quad emission
/// works). `input_to_geo` (a `mat3` mapping `vec3(v_uv, 1)` to `[0, 1]` geometry space) is passed
/// as three `vec4` columns — the shader reads `.xyz` of each — to avoid `mat3` push-constant layout
/// ambiguity (`sample_transform` likewise). 208 bytes. Callers must set every field (there is no
/// meaningful identity default).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PostprocessPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2 (see [`QuadPush::proj`]).
    pub proj: [f32; 4],
    pub target: [f32; 2],
    pub geo_size: [f32; 2],
    pub src_rect: [f32; 4],
    pub corner_radius: [f32; 4],
    /// Premultiplied background mixed behind the (premultiplied) texture.
    pub bg_color: [f32; 4],
    /// `input_to_geo` as 3 column vectors (`.xyz` used); build from a `glam::Mat3`'s columns.
    pub input_to_geo: [[f32; 4]; 3],
    /// `sample_transform` as 3 column vectors (`.xyz` used): maps `v_uv` to the capture's UV,
    /// undoing the output transform baked into the physically-oriented mid-frame capture. Identity
    /// for `Transform::Normal`. Separate from `input_to_geo` because sampling and the geometry
    /// clip need opposite handling of the rotation (see `postprocess.frag`).
    pub sample_transform: [[f32; 4]; 3],
    pub niri_scale: f32,
    pub niri_alpha: f32,
    pub saturation: f32,
    pub noise: f32,
}

/// Push constants for the resize cross-fade material (`resize.vert`/`resize.frag`). Blends two
/// window snapshots (bound at set 0 / set 1) by `clamped_progress`, then optionally clips/rounds to
/// the current geometry. The three used transforms are affine-diagonal, each packed as a `vec4`
/// `[scale.xy, translate.xy]` (the GLES shader's two unused matrices and `niri_progress` are
/// dropped). `vec2`s paired and `vec4`s at 16-aligned offsets (natural std430). 128 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ResizePush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Output-transform 2×2 (see [`QuadPush::proj`]).
    pub proj: [f32; 4],
    pub target: [f32; 2],
    pub curr_geo_size: [f32; 2],
    pub input_to_curr_geo: [f32; 4],
    pub geo_to_tex_prev: [f32; 4],
    pub geo_to_tex_next: [f32; 4],
    pub corner_radius: [f32; 4],
    pub clamped_progress: f32,
    pub clip_to_geometry: f32,
    pub niri_scale: f32,
    pub niri_alpha: f32,
}

/// Push constants for the glyph material (`text.vert`/`text.frag`). Each glyph draws as its own
/// quad placed at `origin` (pixels, top-left) of `size`, sampling the sub-rect
/// `uv_origin`/`uv_size` of the shared R8 coverage atlas, tinted by `color` (coverage modulates the
/// alpha). std430 layout: `color` (a `vec4`) lands at offset 48; 64 bytes total. Unlike the
/// quad-family pushes there is **no** `proj` — the text material is offscreen-only (identity
/// transform); the offscreen is composited through the transform-aware texture material.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TextPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub target: [f32; 2],
    pub uv_origin: [f32; 2],
    pub uv_size: [f32; 2],
    pub _pad: [f32; 2],
    pub color: [f32; 4],
}

const _: () = {
    assert!(std::mem::size_of::<TextPush>() == 64);
    assert!(std::mem::offset_of!(TextPush, color) == 48);
};

impl Default for QuadPush {
    /// A full-texture, un-rounded, un-faded, opaque-white quad — materials override only the fields
    /// they use (and callers set `origin`/`size`/`target`). Note `tex_transform` defaults to
    /// identity (samples the *full* texture), not zeros, so a defaulted sampling quad samples
    /// everything.
    fn default() -> Self {
        QuadPush {
            origin: [0.0, 0.0],
            size: [0.0, 0.0],
            proj: IDENTITY_PROJ,
            target: [0.0, 0.0],
            corner_radius: 0.0,
            stroke_width: 0.0,
            color: [1.0, 1.0, 1.0, 1.0],
            tex_transform: IDENTITY_TEX_TRANSFORM,
            cutoff: [0.0, 0.0],
        }
    }
}

pub struct RenderTarget {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub framebuffer: vk::Framebuffer,
    pub render_pass: vk::RenderPass,
    pub width: u32,
    pub height: u32,
}

impl RenderTarget {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let device = &gpu.device;

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("create target image")?;

        let req = unsafe { device.get_image_memory_requirements(image) };
        let index =
            gpu.find_memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }.context("target memory")?;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("target view")?;

        let render_pass = create_render_pass(device)?;

        let fb_ci = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(width)
            .height(height)
            .layers(1);
        let framebuffer =
            unsafe { device.create_framebuffer(&fb_ci, None) }.context("framebuffer")?;

        Ok(RenderTarget {
            image,
            memory,
            view,
            framebuffer,
            render_pass,
            width,
            height,
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.width,
            height: self.height,
        }
    }

    /// Begin the render pass with `clear` as the load-clear color.
    pub fn begin(&self, gpu: &Gpu, cbuf: vk::CommandBuffer, clear: [f32; 4]) {
        let clears = [vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
        }];
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent(),
            })
            .clear_values(&clears);
        unsafe {
            gpu.device
                .cmd_begin_render_pass(cbuf, &begin, vk::SubpassContents::INLINE);
        }
    }

    /// Copy the rendered image (already in `TRANSFER_SRC_OPTIMAL`) into host memory as RGBA8.
    pub fn read_back(&self, gpu: &Gpu, pool: vk::CommandPool) -> Result<Vec<u8>> {
        let device = &gpu.device;
        let size = (self.width as vk::DeviceSize) * (self.height as vk::DeviceSize) * 4;

        let buf_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&buf_ci, None) }.context("readback buffer")?;
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let index = gpu.find_memory_type(
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let buf_mem = unsafe { device.allocate_memory(&alloc, None) }.context("readback memory")?;
        unsafe { device.bind_buffer_memory(buffer, buf_mem, 0)? };

        gpu.run_commands(pool, |cbuf| unsafe {
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cbuf,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            let host = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[host],
                &[],
                &[],
            );
        })?;

        let mut pixels = vec![0u8; size as usize];
        unsafe {
            let ptr = device
                .map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty())
                .context("map readback")? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, pixels.as_mut_ptr(), size as usize);
            device.unmap_memory(buf_mem);
            device.destroy_buffer(buffer, None);
            device.free_memory(buf_mem, None);
        }
        Ok(pixels)
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_framebuffer(self.framebuffer, None);
            d.destroy_render_pass(self.render_pass, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}

fn create_render_pass(device: &ash::Device) -> Result<vk::RenderPass> {
    let attachment = vk::AttachmentDescription::default()
        .format(FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));
    // Order the color writes after the layout transition, and the later transfer-read after the
    // color writes (so read_back sees a complete image).
    let deps = [
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
    ];
    let ci = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(&deps);
    unsafe { device.create_render_pass(&ci, None) }.context("create render pass")
}

/// A graphics pipeline for `quad.vert` + one material fragment stage. All share the [`QuadPush`]
/// push-constant range, so switching materials mid-pass is just a rebind + re-push + redraw.
pub struct QuadPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
}

impl QuadPipeline {
    pub fn new(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        extent: vk::Extent2D,
        vert_spv: &[u8],
        frag_spv: &[u8],
        set_layouts: &[vk::DescriptorSetLayout],
    ) -> Result<Self> {
        let device = &gpu.device;
        let vert = load_module(device, vert_spv)?;
        let frag = load_module(device, frag_spv)?;

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];

        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);

        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Premultiplied-over compositing: every material fragment stage in this crate outputs
        // premultiplied color, and every color that reaches a push constant is premultiplied.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<QuadPush>() as u32);
        let layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(set_layouts)
            .push_constant_ranges(std::slice::from_ref(&push_range));
        let layout = unsafe { device.create_pipeline_layout(&layout_ci, None) }
            .context("pipeline layout")?;

        let pipeline_ci = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_ci], None)
        }
        .map_err(|(_, e)| e)
        .context("create graphics pipeline")?[0];

        Ok(QuadPipeline {
            layout,
            pipeline,
            vert,
            frag,
        })
    }

    /// Bind, (optionally) bind descriptor set 0, push `quad`'s params, and draw the quad.
    pub fn draw(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        quad: &QuadPush,
        set: Option<vk::DescriptorSet>,
    ) {
        unsafe {
            gpu.device
                .cmd_bind_pipeline(cbuf, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            if let Some(set) = set {
                gpu.device.cmd_bind_descriptor_sets(
                    cbuf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.layout,
                    0,
                    &[set],
                    &[],
                );
            }
            gpu.device.cmd_push_constants(
                cbuf,
                self.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(quad),
            );
            gpu.device.cmd_draw(cbuf, 6, 1, 0, 0);
            crate::stats::draw();
        }
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.frag, None);
        }
    }
}

pub fn load_module(device: &ash::Device, spv: &[u8]) -> Result<vk::ShaderModule> {
    let code = ash::util::read_spv(&mut std::io::Cursor::new(spv)).context("read SPIR-V")?;
    let ci = vk::ShaderModuleCreateInfo::default().code(&code);
    unsafe { device.create_shader_module(&ci, None) }.context("create shader module")
}

/// Descriptor set layout for a single combined image sampler at binding 0 (fragment stage) —
/// what the textured / glyph-atlas materials sample from.
pub fn sampler_set_layout(gpu: &Gpu) -> Result<vk::DescriptorSetLayout> {
    let binding = vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT);
    let ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
    unsafe { gpu.device.create_descriptor_set_layout(&ci, None) }.context("descriptor set layout")
}

/// View a `repr(C)` POD as bytes for `cmd_push_constants`.
pub fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), std::mem::size_of::<T>()) }
}
