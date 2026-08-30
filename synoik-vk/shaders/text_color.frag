// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Colour-glyph material: the atlas holds premultiplied RGBA painted from a COLRv1 paint graph
// (`colr.rs`), so the glyph carries its own colours and the text tint has nothing to say about
// them. Only `pc.color.a` applies, and it must — a label fading out has to take its emoji with it.
// Shares `TextPush` and the pipeline layout with `text.frag`; only the sampled atlas differs.

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
    o = texture(atlas, v_uv) * pc.color.a;
}
