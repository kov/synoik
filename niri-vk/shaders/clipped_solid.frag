#version 450

// Clipped **solid** material: a flat premultiplied colour, masked to the same rounded geometry
// `clipped_texture.frag` masks a sampled texture to. It exists because a surface can arrive as a
// solid colour rather than a texture — a single-pixel `wl_buffer`, or a window blocked out from a
// screencast — and such a surface must still round its corners. Without it the clip a
// `ClippedSurfaceRenderElement` arms is silently dropped for exactly those surfaces, so a rounded
// window whose content happens to be one colour renders square.
//
// The clip math below is `clipped_texture.frag`'s verbatim; only the colour source differs. Both
// take `input_to_geo` on `v_uv` (not on a sampled UV), so the mask is independent of any buffer
// transform and of the output transform.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius; // unused SDF scalar; declared for offset agreement with the vertex stage
    float _pad0;
    vec4 color;              // premultiplied fill (smithay's `Color32F` contract)
    vec4 st0;                // tex_transform columns; unused here, kept for block agreement
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

// Antialiased coverage of a rounded rectangle. Ported verbatim from clipped_texture.frag.
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
    vec2 coords_geo = (mat3(pc.i2g0.xyz, pc.i2g1.xyz, pc.i2g2.xyz) * vec3(v_uv, 1.0)).xy;

    float mask;
    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside geometry.
        mask = 0.0;
    } else {
        // Apply corner rounding inside geometry.
        mask = niri_rounding_alpha(coords_geo * pc.geo_size, pc.geo_size, pc.clip_corner_radius);
    }

    // Premultiplied-over blend, premultiplied colour: the mask scales rgb and alpha together.
    o = pc.color * mask;
}
