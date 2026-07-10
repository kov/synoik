#version 450

// Clipped-surface material: sample a bound texture (like texture.frag, via the tex_transform that
// folds the src crop + buffer rotation/flip + y-invert) and clip it to a rounded rectangle
// expressed in a general geometry space (like clipped_surface.frag + rounding_alpha.frag). This is
// the owned-renderer port of niri's ClippedSurfaceRenderElement — the window rounded-corner /
// clip-to-geometry path.
//
// It differs from postprocess.frag in two ways that matter for a *client* surface: (1) sampling
// goes through the folded tex_transform (so a y-flipped or buffer-transformed client buffer samples
// correctly — postprocess only ever samples renderer-internal captures via src_rect); (2) the
// pipeline blends straight-alpha and attenuates only the alpha (like texture.frag/rounded_texture.
// frag), so the antialiased corner edge fades to the destination without double-darkening.
//
// input_to_geo maps v_uv (0..1 across the quad, i.e. the surface's on-screen placement) directly to
// [0,1] geometry space — a pure dst->geo affine built CPU-side. Because it acts on v_uv (not the
// sampled UV), it is independent of the buffer transform, and independent of the output transform
// (which `proj` applies to placement in the vertex stage), so the clip is rotation-correct.

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius; // unused SDF scalar; declared for offset agreement with the vertex stage
    float _pad0;
    vec4 color;              // straight-alpha tint, [1,1,1,alpha]
    vec4 st0;                // tex_transform columns (xyz used): v_uv -> normalized UV
    vec4 st1;
    vec4 st2;
    vec2 geo_size;           // logical geometry size in pixels (rounding coordinate space)
    vec2 _pad1;
    vec4 clip_corner_radius; // per-corner radii (logical px): TL, TR, BR, BL
    vec4 i2g0;               // input_to_geo columns (xyz used): v_uv -> [0,1] geometry space
    vec4 i2g1;
    vec4 i2g2;
    float niri_scale;
} pc;

layout(location = 0) in vec2 v_uv;    // 0..1 across the quad
layout(location = 1) in vec2 v_local; // pixels within the quad (unused here)
layout(location = 0) out vec4 o;

// Antialiased coverage of a rounded rectangle. Ported verbatim from postprocess.frag /
// render_helpers/shaders/rounding_alpha.frag.
float niri_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius) {
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
    float t = clamp((dist - radius) * pc.niri_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}

void main() {
    vec2 uv = (mat3(pc.st0.xyz, pc.st1.xyz, pc.st2.xyz) * vec3(v_uv, 1.0)).xy;
    vec4 c = texture(tex, uv) * pc.color;

    vec2 coords_geo = (mat3(pc.i2g0.xyz, pc.i2g1.xyz, pc.i2g2.xyz) * vec3(v_uv, 1.0)).xy;

    float mask;
    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside geometry.
        mask = 0.0;
    } else {
        // Apply corner rounding inside geometry.
        mask = niri_rounding_alpha(coords_geo * pc.geo_size, pc.geo_size, pc.clip_corner_radius);
    }

    // The pipeline blends straight-alpha (SRC_ALPHA / ONE_MINUS_SRC_ALPHA), so attenuate only the
    // alpha (mirrors rounded_texture.frag): the hardware multiplies rgb by src alpha at blend time.
    o = vec4(c.rgb, c.a * mask);
}
