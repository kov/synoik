// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Resize cross-fade material: sample two window snapshots (prev + next) and blend them by
// clamped_progress. This is the owned-renderer port of niri's resize shader (render_helpers/shaders/resize_prelude.frag +
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
    // curr_geo_size, corner_radius and synoik_scale are unused since clip-to-geometry was
    // removed. They stay because dropping a vec2/vec4 mid-block would shift every later vec4
    // off its std430 16-byte alignment, and CustomResizePush still exposes them to custom
    // shader snippets through the prelude.
    vec4 corner_radius;
    float clamped_progress;
    float synoik_scale;
    float synoik_alpha;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

// Apply an affine-diagonal transform packed as [scale.xy, translate.xy].
vec2 affine(vec4 t, vec2 v) {
    return t.xy * v + t.zw;
}

// Sample a snapshot, treating everything outside it as transparent rather than as its edge texel.
//
// The quad is grown to fit BOTH snapshots, each scaled by its own factor (`resize_transforms`:
// `merge(tex_prev_geo * scale_prev, tex_next_geo * scale_next)`), so it routinely reaches past one
// of them — a client whose surface is bigger than its window geometry (a CSD shadow/corner ring)
// grows it by `ring * scale_prev`, which at the end of a maximize is the ring times the full size
// ratio. Our samplers are CLAMP_TO_EDGE, so an unguarded sample there smears the snapshot's edge
// row across that band: a hard-edged opaque skirt around a window that is transparent there.
vec4 sample_snapshot(sampler2D tex, vec2 coords) {
    if (coords.x < 0.0 || 1.0 < coords.x || coords.y < 0.0 || 1.0 < coords.y) {
        return vec4(0.0);
    }
    return texture(tex, coords);
}

void main() {
    vec2 coords_curr_geo = affine(pc.input_to_curr_geo, v_uv);

    vec2 coords_tex_prev = affine(pc.geo_to_tex_prev, coords_curr_geo);
    vec4 color_prev = sample_snapshot(tex_prev, coords_tex_prev);

    vec2 coords_tex_next = affine(pc.geo_to_tex_next, coords_curr_geo);
    vec4 color_next = sample_snapshot(tex_next, coords_tex_next);

    vec4 color = mix(color_prev, color_next, pc.clamped_progress);

    color = color * pc.synoik_alpha;
    o = color;
}
