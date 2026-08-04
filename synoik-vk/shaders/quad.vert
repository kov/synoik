// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Shared quad vertex stage: emit a unit quad (two triangles from gl_VertexIndex, no vertex
// buffer) placed at a pixel-space rect and converted to Vulkan NDC (origin top-left, y down).
// This is the seed of the unified instanced-quad pipeline; for now the rect comes via push
// constants and each material is a different fragment stage sharing this vertex stage.

layout(push_constant) uniform Push {
    vec2 origin;         // top-left in pixels (logical output space)
    vec2 size;           // width/height in pixels
    vec4 proj;           // output-transform 2x2, col-major [m00,m10,m01,m11]; mat2(proj)
    vec2 target;         // logical output size in pixels
    float corner_radius; // pixels (used by SDF materials)
    float _pad0;
    vec4 color;          // straight-alpha RGBA
} pc;

layout(location = 0) out vec2 v_uv;    // 0..1 across the quad
layout(location = 1) out vec2 v_local; // pixels within the quad

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
    v_uv = c;
    v_local = c * pc.size;
    vec2 p = pc.origin + c * pc.size;
    // Ortho into y-down NDC (logical space), then rotate into the physical framebuffer.
    vec2 ndc = p / pc.target * 2.0 - 1.0;
    gl_Position = vec4(mat2(pc.proj) * ndc, 0.0, 1.0);
}
