// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Straight resample: one LINEAR fetch per destination pixel.
//
// Used for the gaussian path's rungs — halving on the way down to the working size, and the single
// magnifying draw back up. At exactly half size a linear fetch is a 2x2 box, which is what makes
// successive halvings a cheap and clean minification; the alternative, one draw straight to 1/8,
// samples only 2x2 of every 8x8 and aliases (GNOME does that, then blurs the aliasing away, so the
// end result agrees).

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

void main() {
    o = texture(tex, v_uv);
}
