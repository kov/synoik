// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Rounded-texture material: sample a bound texture (like texture.frag) and cut the quad's corners
// with the same signed-distance rounded-rect coverage as sdf_rect.frag. This is the owned-renderer
// port of niri's RoundedTextureRenderElement (the GNOME overview rounds the workspace wallpaper).

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius; // physical pixels
    float _pad0;
    vec4 color;          // premultiplied tint, [alpha,alpha,alpha,alpha]
    vec4 st0;            // tex_transform columns (xyz used): v_uv -> normalized UV
    vec4 st1;
    vec4 st2;
} pc;

layout(location = 0) in vec2 v_uv;    // 0..1 across the quad
layout(location = 1) in vec2 v_local; // pixels within the quad
layout(location = 0) out vec4 o;

// Signed distance to a box of half-size b with corner radius r (box-centered p). Same primitive
// as sdf_rect.frag.
float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

void main() {
    vec2 uv = (mat3(pc.st0.xyz, pc.st1.xyz, pc.st2.xyz) * vec3(v_uv, 1.0)).xy;
    vec4 c = texture(tex, uv) * pc.color;

    vec2 half_size = pc.size * 0.5;
    float r = clamp(pc.corner_radius, 0.0, min(half_size.x, half_size.y));
    float d = sd_round_box(v_local - half_size, half_size, r);
    float aa = max(fwidth(d), 1e-4);
    float coverage = 1.0 - smoothstep(-aa, aa, d);

    // The pipeline blends premultiplied-over (ONE / ONE_MINUS_SRC_ALPHA) and `c` is already
    // premultiplied (premultiplied sample × premultiplied tint), so coverage scales rgb *and*
    // alpha together — that is what fades the cut corners to the destination.
    o = c * coverage;
}
