// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Resize cross-fade material: sample two window snapshots (prev + next), blend them by
// clamped_progress, then optionally clip / round-corner to the current geometry. This is the
// owned-renderer port of niri's resize shader (render_helpers/shaders/resize_prelude.frag +
// resize.frag + resize_epilogue.frag).
//
// The GLES shader carries five mat3 uniforms but only three are used
// (synoik_input_to_curr_geo, synoik_geo_to_tex_prev, synoik_geo_to_tex_next), and synoik_progress /
// synoik_curr_geo_to_{prev,next}_geo are dead — so they are dropped here. Each used transform is
// affine-diagonal (scale + translate, no rotation), so it is passed as a single vec4
// [scale.xy, translate.xy] and applied as `t.xy * v + t.zw` (== the mat3 * vec3(v, 1)). The two
// textures bind at set 0 / set 1 (each is a VkTexture's own combined-image-sampler set). Outputs
// premultiplied color (premultiplied-over blend).

layout(set = 0, binding = 0) uniform sampler2D tex_prev;
layout(set = 1, binding = 0) uniform sampler2D tex_next;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    vec2 curr_geo_size;
    vec4 input_to_curr_geo; // [scale.xy, translate.xy]
    vec4 geo_to_tex_prev;
    vec4 geo_to_tex_next;
    vec4 corner_radius;
    float clamped_progress;
    float clip_to_geometry;
    float synoik_scale;
    float synoik_alpha;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

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
    float t = clamp((dist - radius) * pc.synoik_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}

// Apply an affine-diagonal transform packed as [scale.xy, translate.xy].
vec2 affine(vec4 t, vec2 v) {
    return t.xy * v + t.zw;
}

void main() {
    vec2 coords_curr_geo = affine(pc.input_to_curr_geo, v_uv);

    vec2 coords_tex_prev = affine(pc.geo_to_tex_prev, coords_curr_geo);
    vec4 color_prev = texture(tex_prev, coords_tex_prev);

    vec2 coords_tex_next = affine(pc.geo_to_tex_next, coords_curr_geo);
    vec4 color_next = texture(tex_next, coords_tex_next);

    vec4 color = mix(color_prev, color_next, pc.clamped_progress);

    if (pc.clip_to_geometry == 1.0) {
        if (coords_curr_geo.x < 0.0 || 1.0 < coords_curr_geo.x
                || coords_curr_geo.y < 0.0 || 1.0 < coords_curr_geo.y) {
            // Clip outside geometry.
            color = vec4(0.0);
        } else {
            // Apply corner rounding inside geometry.
            color = color * synoik_rounding_alpha(coords_curr_geo * pc.curr_geo_size, pc.curr_geo_size, pc.corner_radius);
        }
    }

    color = color * pc.synoik_alpha;
    o = color;
}
