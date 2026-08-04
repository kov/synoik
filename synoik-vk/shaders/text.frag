// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Glyph material: the atlas holds grayscale coverage in its red channel (swash Format::Alpha).
// Output the text color modulated by coverage. `pc.color` is premultiplied (the frame method
// premultiplies the toolkit's straight Rgba) and the pipeline blends premultiplied-over, so
// coverage scales rgb and alpha together. Hinting/AA quality lives entirely in the atlas bitmap;
// this stage just places and tints it.

layout(set = 0, binding = 0) uniform sampler2D atlas;

layout(push_constant) uniform TextPush {
    vec2 origin;
    vec2 size;
    vec2 target;
    vec2 uv_origin;
    vec2 uv_size;
    vec2 _pad;
    vec4 color;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

void main() {
    float coverage = texture(atlas, v_uv).r;
    o = pc.color * coverage;
}
