// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Runtime-compiled custom animation shaders — the owned-renderer port of niri's user-supplied
//! `resize` / `close` / `open` GLSL snippets (`shaders/mod.rs`, `set_custom_{resize,close,open}`).
//!
//! niri lets a user drop a GLSL fragment in config; GLES compiles it (wrapped in a prelude +
//! epilogue) at config load. This module does the Vulkan equivalent: it assembles a **GLSL 450**
//! vertex + fragment pair (a hand-translated prelude/epilogue around the user's snippet), compiles
//! both to SPIR-V at runtime with `glslangValidator` — the *same* compiler `synoik-vk/build.rs`
//! uses for the built-in shaders, so the two renderers agree on what GLSL is valid — and hands the
//! SPIR-V back for [`super::renderer::VulkanRenderer::set_custom_shader`] to build a cached
//! pipeline from. The subprocess sits behind [`compile_spirv`]; swapping to an in-process compiler
//! (shaderc/naga) later is a one-function change.
//!
//! ## GLES-100 → Vulkan-450 translation
//! niri's snippets are GLSL ES 1.00. Vulkan forbids loose `uniform`s, `varying`, and
//! `gl_FragColor`, so the prelude puts every `synoik_*` uniform in a push-constant block and
//! re-exposes the names the snippet expects via `#define`s (plus `#define texture2D texture` /
//! `texture2DProj textureProj` shims). Each transform matrix niri feeds these shaders is
//! affine-diagonal (scale + translate, no rotation/shear — verified: they are all built from
//! `Mat3::from_scale`/`from_translation` in `resize.rs`/`closing_window.rs`/`opening_window.rs`),
//! so it is transmitted as a single packed `vec4 [sx, sy, tx, ty]` (see [`pack_affine`]) and
//! rebuilt into a real `mat3` in the shader via `SYNOIK_AFFINE`, keeping the whole uniform set
//! inside the push-constant budget with no UBO. The snippet's `synoik_geo_to_tex * coords`
//! therefore behaves exactly as under GLES.
//!
//! Live wiring: config snippets are compiled in via
//! `VulkanRenderer::set_custom_{resize,close,open}_shader`, and the **resize**, **open** and
//! **close** animations construct their elements through this on a Vulkan session (resize via
//! `ResizeRenderElement`, open/close via `CustomAnimRenderElement`). The closing-window animation
//! captures its snapshot to a `MemoryBuffer` at close time and re-uploads it to a `VkTexture`, then
//! routes a custom close shader through `render_custom_anim` (see `ClosingWindow::render_vulkan`).

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use glam::Mat3;

use super::error::VulkanError;

/// Which niri custom animation shader is being compiled. `Close`/`Open` share one uniform/texture
/// layout ([`CustomAnimPush`]); `Resize` has its own two-texture layout ([`CustomResizePush`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustomShaderType {
    Resize,
    Close,
    Open,
}

/// Push constants for a custom **resize** shader. Mirrors niri's resize uniform set; the five
/// transform matrices are affine-diagonal, each packed as `vec4 [sx, sy, tx, ty]`. Field order is
/// the std430 layout the assembled GLSL push block declares (all `vec4`s land on 16-byte
/// boundaries because `proj` is 16 bytes and the four following `vec2`s sum to 32). 160 bytes.
///
/// `proj` leads the block (matching `quad.vert`/`resize.vert`) so the shared vertex stage can
/// rotate placement into a rotated output; prepending keeps every `vec4` 16-aligned so std430 and
/// `#[repr(C)]` agree at every offset (appending after the trailing scalars would skew them).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CustomResizePush {
    pub proj: [f32; 4],
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub target: [f32; 2],
    pub curr_geo_size: [f32; 2],
    pub input_to_curr_geo: [f32; 4],
    pub curr_geo_to_prev_geo: [f32; 4],
    pub curr_geo_to_next_geo: [f32; 4],
    pub geo_to_tex_prev: [f32; 4],
    pub geo_to_tex_next: [f32; 4],
    pub corner_radius: [f32; 4],
    pub progress: f32,
    pub clamped_progress: f32,
    pub alpha: f32,
    pub scale: f32,
}

/// Push constants for a custom **close** or **open** shader (one snapshot texture). 100 bytes.
/// `proj` leads the block (see [`CustomResizePush`]) so the shared vertex stage rotates placement
/// into a rotated output.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CustomAnimPush {
    pub proj: [f32; 4],
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub target: [f32; 2],
    pub geo_size: [f32; 2],
    pub input_to_geo: [f32; 4],
    pub geo_to_tex: [f32; 4],
    pub progress: f32,
    pub clamped_progress: f32,
    pub random_seed: f32,
    pub alpha: f32,
    pub scale: f32,
}

/// A compiled custom shader: SPIR-V for both stages plus the pipeline shape they need.
pub(super) struct CompiledCustom {
    pub vert_spv: Vec<u8>,
    pub frag_spv: Vec<u8>,
    /// Combined-image-sampler sets the fragment binds (1 for close/open, 2 for resize).
    pub sampler_count: u32,
    /// `size_of` the matching push struct — passed to `build_pipeline` and used for the guard.
    pub push_size: u32,
}

/// Pack an affine-diagonal `Mat3` (scale + translate, glam column-major) into `[sx, sy, tx, ty]`
/// for the shader's `SYNOIK_AFFINE` reconstruction. Debug-asserts the no-rotation/no-shear
/// invariant the whole packing scheme relies on — every matrix niri feeds these shaders satisfies
/// it, and this catches a future refactor that silently violates it.
pub(crate) fn pack_affine(m: Mat3) -> [f32; 4] {
    debug_assert!(
        m.x_axis.y.abs() < 1e-4
            && m.x_axis.z.abs() < 1e-4
            && m.y_axis.x.abs() < 1e-4
            && m.y_axis.z.abs() < 1e-4
            && m.z_axis.x.is_finite()
            && m.z_axis.y.is_finite()
            && (m.z_axis.z - 1.0).abs() < 1e-4,
        "custom-shader transform must be affine-diagonal (scale+translate), got {m:?}"
    );
    [m.x_axis.x, m.y_axis.y, m.z_axis.x, m.z_axis.y]
}

impl CustomShaderType {
    fn sampler_count(self) -> u32 {
        match self {
            CustomShaderType::Resize => 2,
            CustomShaderType::Close | CustomShaderType::Open => 1,
        }
    }

    fn push_size(self) -> u32 {
        let size = match self {
            CustomShaderType::Resize => std::mem::size_of::<CustomResizePush>(),
            CustomShaderType::Close | CustomShaderType::Open => {
                std::mem::size_of::<CustomAnimPush>()
            }
        };
        size as u32
    }
}

/// Assemble + compile a custom shader from the user's GLSL snippet. Returns SPIR-V for both stages
/// or a [`VulkanError::CustomShader`] carrying the glslang error log (so a bad snippet degrades
/// gracefully instead of panicking).
pub(super) fn compile_custom(
    ty: CustomShaderType,
    user_src: &str,
) -> Result<CompiledCustom, VulkanError> {
    let vert_src = assemble_vertex(ty);
    let frag_src = assemble_fragment(ty, user_src);
    let vert_spv = compile_spirv(Stage::Vertex, &vert_src)?;
    let frag_spv = compile_spirv(Stage::Fragment, &frag_src)?;
    Ok(CompiledCustom {
        vert_spv,
        frag_spv,
        sampler_count: ty.sampler_count(),
        push_size: ty.push_size(),
    })
}

// --- GLSL assembly ------------------------------------------------------------------------------

fn assemble_vertex(ty: CustomShaderType) -> String {
    format!("#version 450\n\n{}\n{VERT_BODY}", push_block(ty))
}

fn assemble_fragment(ty: CustomShaderType, user_src: &str) -> String {
    let (prelude_tail, epilogue) = match ty {
        CustomShaderType::Resize => (RESIZE_PRELUDE_TAIL, RESIZE_EPILOGUE),
        CustomShaderType::Close => (ANIM_PRELUDE_TAIL, CLOSE_EPILOGUE),
        CustomShaderType::Open => (ANIM_PRELUDE_TAIL, OPEN_EPILOGUE),
    };
    let mut src = format!(
        "#version 450\n\n{}\n{prelude_tail}\n{user_src}\n{epilogue}\n",
        push_block(ty),
    );
    // Resize clips/rounds to geometry, so it needs the SDF corner-coverage helper appended (niri
    // appends rounding_alpha.frag only for resize).
    if ty == CustomShaderType::Resize {
        src.push_str(ROUNDING_ALPHA);
    }
    src
}

fn push_block(ty: CustomShaderType) -> &'static str {
    match ty {
        CustomShaderType::Resize => RESIZE_PUSH_BLOCK,
        CustomShaderType::Close | CustomShaderType::Open => ANIM_PUSH_BLOCK,
    }
}

/// Shared vertex body: emit the unit quad (6 verts from `gl_VertexIndex`) placed by
/// `origin`/`size`/`target`, and hand the fragment `synoik_v_coords` in `[0, 1]` (niri's normalized
/// varying). Every custom vertex stage is identical; only the push block preceding it differs.
const VERT_BODY: &str = "\
layout(location = 0) out vec2 synoik_v_coords;

vec2 synoik_corner(int i) {
    if (i == 0) return vec2(0.0, 0.0);
    if (i == 1) return vec2(1.0, 0.0);
    if (i == 2) return vec2(1.0, 1.0);
    if (i == 3) return vec2(0.0, 0.0);
    if (i == 4) return vec2(1.0, 1.0);
    return vec2(0.0, 1.0);
}

void main() {
    vec2 c = synoik_corner(gl_VertexIndex);
    synoik_v_coords = c;
    vec2 p = pc.origin + c * pc.size;
    // Ortho into y-down NDC (logical space), then rotate into the physical framebuffer via the
    // output-transform 2x2 in pc.proj (same convention as quad.vert / resize.vert).
    vec2 ndc = p / pc.target * 2.0 - 1.0;
    gl_Position = vec4(mat2(pc.proj) * ndc, 0.0, 1.0);
}
";

const RESIZE_PUSH_BLOCK: &str = "\
layout(push_constant) uniform Push {
    vec4 proj;
    vec2 origin;
    vec2 size;
    vec2 target;
    vec2 curr_geo_size;
    vec4 input_to_curr_geo;
    vec4 curr_geo_to_prev_geo;
    vec4 curr_geo_to_next_geo;
    vec4 geo_to_tex_prev;
    vec4 geo_to_tex_next;
    vec4 corner_radius;
    float progress;
    float clamped_progress;
    float alpha;
    float scale;
} pc;
";

const ANIM_PUSH_BLOCK: &str = "\
layout(push_constant) uniform Push {
    vec4 proj;
    vec2 origin;
    vec2 size;
    vec2 target;
    vec2 geo_size;
    vec4 input_to_geo;
    vec4 geo_to_tex;
    float progress;
    float clamped_progress;
    float random_seed;
    float alpha;
    float scale;
} pc;
";

/// The prelude tail bridges GLES-100 snippets to Vulkan 450: `texture2D`/`texture2DProj` name
/// shims, `SYNOIK_AFFINE` (rebuild an affine-diagonal `mat3` from a packed `vec4 [sx,sy,tx,ty]`,
/// the inverse of [`pack_affine`]; column-major so `M * vec3(x,y,1) = (sx*x+tx, sy*y+ty, 1)`), and
/// a `#define` per `synoik_*` name the snippet expects. Kept inline per shader type for
/// readability.
const RESIZE_PRELUDE_TAIL: &str = "\
layout(set = 0, binding = 0) uniform sampler2D synoik_tex_prev;
layout(set = 1, binding = 0) uniform sampler2D synoik_tex_next;

layout(location = 0) in vec2 synoik_v_coords;
layout(location = 0) out vec4 synoik_out_color;

#define texture2D texture
#define texture2DProj textureProj

#define SYNOIK_AFFINE(v) mat3(vec3((v).x, 0.0, 0.0), vec3(0.0, (v).y, 0.0), vec3((v).z, (v).w, 1.0))

#define synoik_size (pc.size)
#define synoik_curr_geo_size (pc.curr_geo_size)
#define synoik_progress (pc.progress)
#define synoik_clamped_progress (pc.clamped_progress)
#define synoik_corner_radius (pc.corner_radius)
#define synoik_alpha (pc.alpha)
#define synoik_scale (pc.scale)
#define synoik_input_to_curr_geo SYNOIK_AFFINE(pc.input_to_curr_geo)
#define synoik_curr_geo_to_prev_geo SYNOIK_AFFINE(pc.curr_geo_to_prev_geo)
#define synoik_curr_geo_to_next_geo SYNOIK_AFFINE(pc.curr_geo_to_next_geo)
#define synoik_geo_to_tex_prev SYNOIK_AFFINE(pc.geo_to_tex_prev)
#define synoik_geo_to_tex_next SYNOIK_AFFINE(pc.geo_to_tex_next)

float synoik_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);
";

const RESIZE_EPILOGUE: &str = "\
void main() {
    vec3 coords_curr_geo = synoik_input_to_curr_geo * vec3(synoik_v_coords, 1.0);
    vec3 size_curr_geo = vec3(synoik_curr_geo_size, 1.0);

    vec4 color = resize_color(coords_curr_geo, size_curr_geo);

    color = color * synoik_alpha;
    synoik_out_color = color;
}
";

const ANIM_PRELUDE_TAIL: &str = "\
layout(set = 0, binding = 0) uniform sampler2D synoik_tex;

layout(location = 0) in vec2 synoik_v_coords;
layout(location = 0) out vec4 synoik_out_color;

#define texture2D texture
#define texture2DProj textureProj

#define SYNOIK_AFFINE(v) mat3(vec3((v).x, 0.0, 0.0), vec3(0.0, (v).y, 0.0), vec3((v).z, (v).w, 1.0))

#define synoik_size (pc.size)
#define synoik_geo_size (pc.geo_size)
#define synoik_progress (pc.progress)
#define synoik_clamped_progress (pc.clamped_progress)
#define synoik_random_seed (pc.random_seed)
#define synoik_alpha (pc.alpha)
#define synoik_scale (pc.scale)
#define synoik_input_to_geo SYNOIK_AFFINE(pc.input_to_geo)
#define synoik_geo_to_tex SYNOIK_AFFINE(pc.geo_to_tex)
";

const CLOSE_EPILOGUE: &str = "\
void main() {
    vec3 coords_geo = synoik_input_to_geo * vec3(synoik_v_coords, 1.0);
    vec3 size_geo = vec3(synoik_geo_size, 1.0);
    vec4 color = close_color(coords_geo, size_geo);
    color = color * synoik_alpha;
    synoik_out_color = color;
}
";

const OPEN_EPILOGUE: &str = "\
void main() {
    vec3 coords_geo = synoik_input_to_geo * vec3(synoik_v_coords, 1.0);
    vec3 size_geo = vec3(synoik_geo_size, 1.0);
    vec4 color = open_color(coords_geo, size_geo);
    color = color * synoik_alpha;
    synoik_out_color = color;
}
";

/// SDF rounded-corner coverage, verbatim from `shaders/rounding_alpha.frag` (pure GLSL, uses the
/// `#define`d `synoik_scale`). Appended to resize only.
const ROUNDING_ALPHA: &str = "\
float synoik_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius) {
    vec2 center;
    float radius;

    if (coords.x < corner_radius.x && coords.y < corner_radius.x) {
        radius = corner_radius.x;
        center = vec2(radius, radius);
    } else if (size.x - corner_radius.y < coords.x && coords.y < corner_radius.y) {
        radius = corner_radius.y;
        center = vec2(size.x - radius, radius);
    } else if (size.x - corner_radius.z < coords.x && size.y - corner_radius.z < coords.y) {
        radius = corner_radius.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (coords.x < corner_radius.w && size.y - corner_radius.w < coords.y) {
        radius = corner_radius.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }

    float dist = distance(coords, center);
    float t = clamp((dist - radius) * synoik_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}
";

// --- glslangValidator subprocess ----------------------------------------------------------------

#[derive(Clone, Copy)]
enum Stage {
    Vertex,
    Fragment,
}

impl Stage {
    fn glslang_flag(self) -> &'static str {
        match self {
            Stage::Vertex => "vert",
            Stage::Fragment => "frag",
        }
    }
}

/// Unique-per-call counter for the SPIR-V output temp path (pid + counter avoids collisions across
/// concurrent compiles and processes without needing randomness).
static OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compile one GLSL 450 stage to SPIR-V bytes with `glslangValidator` (Vulkan 1.3 target, matching
/// `synoik-vk/build.rs`). Source is piped on stdin; the SPIR-V is written to a temp file and read
/// back. Any failure — missing binary, compile error — returns a [`VulkanError::CustomShader`] with
/// the diagnostic log, never a panic.
fn compile_spirv(stage: Stage, glsl: &str) -> Result<Vec<u8>, VulkanError> {
    let n = OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let out_path =
        std::env::temp_dir().join(format!("synoik-vk-custom-{}-{n}.spv", std::process::id()));

    let mut child = Command::new("glslangValidator")
        .args([
            "--stdin",
            "-S",
            stage.glslang_flag(),
            "-V",
            "--target-env",
            "vulkan1.3",
            "-o",
        ])
        .arg(&out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| VulkanError::CustomShader(format!("could not run glslangValidator: {e}")))?;

    // Small inputs (a few KB) fit the pipe buffer, so writing-then-waiting cannot deadlock. Ignore
    // a write error (e.g. EPIPE if an incompatible glslang exited during argument parsing, before
    // draining stdin) and fall through to wait_with_output: that reaps the child (no zombie) and
    // surfaces glslang's real diagnostic via the `!success` branch, instead of masking it with a
    // generic "broken pipe" and leaking the child + temp file. `stdin` drops at the block's end,
    // closing the write end so glslang sees EOF and wait_with_output does not hang.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(glsl.as_bytes());
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(VulkanError::CustomShader(format!(
                "glslangValidator did not complete: {e}"
            )));
        }
    };

    if !output.status.success() {
        let _ = std::fs::remove_file(&out_path);
        // glslang prints its error log to stdout; include stderr too for spawn/IO surprises.
        let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
        log.push_str(&String::from_utf8_lossy(&output.stderr));
        return Err(VulkanError::CustomShader(log.trim().to_string()));
    }

    let spv = std::fs::read(&out_path)
        .map_err(|e| VulkanError::CustomShader(format!("could not read compiled SPIR-V: {e}")));
    let _ = std::fs::remove_file(&out_path);
    spv
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn pack_affine_roundtrips_scale_translate() {
        // Mat3 mapping [0,1] to a scaled+translated box; pack then check the four coefficients.
        let m = Mat3::from_translation(glam::Vec2::new(3.0, -2.0))
            * Mat3::from_scale(glam::Vec2::new(2.0, 0.5));
        // Composition order: from_translation(t) * from_scale(s) has effective translation t (the
        // translate is applied last / outermost), scale s.
        let [sx, sy, tx, ty] = pack_affine(m);
        assert_eq!([sx, sy], [2.0, 0.5]);
        assert_eq!([tx, ty], [3.0, -2.0]);
    }

    #[test]
    fn push_structs_have_the_documented_sizes() {
        // The GLSL push blocks (std430) and these `#[repr(C)]` structs must agree byte-for-byte.
        // `proj` leads both blocks so every `vec4` stays 16-aligned and the two layout systems
        // never disagree; a reorder that reintroduces padding skew changes these sizes.
        assert_eq!(std::mem::size_of::<CustomResizePush>(), 160);
        assert_eq!(std::mem::size_of::<CustomAnimPush>(), 100);
    }

    #[test]
    fn compat_and_affine_macros_are_embedded() {
        // Guard against accidentally dropping the shims from the assembled prelude tails.
        assert!(RESIZE_PRELUDE_TAIL.contains("#define texture2D texture"));
        assert!(RESIZE_PRELUDE_TAIL.contains("#define texture2DProj textureProj"));
        assert!(ANIM_PRELUDE_TAIL.contains("SYNOIK_AFFINE"));
    }
}
