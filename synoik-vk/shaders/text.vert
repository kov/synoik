// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Glyph-quad vertex stage: like quad.vert but carries an atlas sub-rect (uv_origin/uv_size) so
// each glyph samples its own slot in the shared coverage atlas. Kept separate from quad.vert to
// leave the committed solid/SDF/textured path untouched; the real renderer folds these into one
// instanced pipeline later.

layout(push_constant) uniform TextPush {
    vec2 origin;    // glyph top-left in pixels
    vec2 size;      // glyph width/height in pixels
    vec2 target;    // render target size in pixels
    vec2 uv_origin; // glyph slot origin in atlas UV
    vec2 uv_size;   // glyph slot size in atlas UV
    vec2 _pad;
    vec4 color;     // text color (coverage modulates alpha)
} pc;

layout(location = 0) out vec2 v_uv;

vec2 corner(int i) {
    if (i == 0) return vec2(0.0, 0.0);
    if (i == 1) return vec2(1.0, 0.0);
    if (i == 2) return vec2(1.0, 1.0);
    if (i == 3) return vec2(0.0, 0.0);
    if (i == 4) return vec2(1.0, 1.0);
    return vec2(0.0, 1.0);
}

void main() {
    vec2 c = corner(gl_VertexIndex);
    v_uv = pc.uv_origin + c * pc.uv_size;
    vec2 p = pc.origin + c * pc.size;
    gl_Position = vec4(p / pc.target * 2.0 - 1.0, 0.0, 1.0);
}
