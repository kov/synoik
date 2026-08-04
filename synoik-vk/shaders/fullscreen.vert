// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Fullscreen-triangle vertex stage (no vertex buffer): three vertices whose UVs span 0..2 so
// the triangle covers the whole [0,1] target. Used by the blur passes, which sample a source
// texture at v_uv across the full destination.

layout(location = 0) out vec2 v_uv;

void main() {
    v_uv = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    gl_Position = vec4(v_uv * 2.0 - 1.0, 0.0, 1.0);
}
