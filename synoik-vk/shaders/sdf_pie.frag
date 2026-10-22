// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// The pie-timer material: GNOME's `PieTimer.vfunc_repaint` (`js/ui/pointerA11yTimeout.js`), a
// cairo path from the centre up to twelve o'clock, clockwise along an arc of `radius -
// border_width` through `angle`, and back — filled, then stroked `border_width` wide with round
// caps and joins. At a full turn the path is the circle alone. Distances are to that path, so the
// round caps and joins fall out of the unsigned distance and both passes get the same 1px
// analytic antialiasing as `sdf_rect.frag`.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius; // unused
    // The border width, physical px.
    float stroke_width;
    // The fill, premultiplied.
    vec4 color;
    // `[0]` is the border colour, premultiplied; the rest unused.
    vec4 tex_transform[3];
    // `.x` is the swept angle, radians in 0..2π, at the shared block's `cutoff` offset.
    vec2 sweep;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

const float TAU = 6.28318530718;

float segment(vec2 p, vec2 b) {
    float t = clamp(dot(p, b) / dot(b, b), 0.0, 1.0);
    return length(p - b * t);
}

void main() {
    vec2 c = pc.size * 0.5;
    float bw = pc.stroke_width;
    float r = min(c.x, c.y) - bw;
    float sweep = clamp(pc.sweep.x, 0.0, TAU);
    vec2 p = v_local - c;

    // Cairo's angles run clockwise on a y-down surface; the path starts at 3π/2, straight up.
    float t = mod(atan(p.y, p.x) - 0.75 * TAU, TAU);
    bool full = sweep >= TAU;
    bool within = full || t <= sweep;
    vec2 a = vec2(0.0, -r);
    vec2 b = r * vec2(cos(0.75 * TAU + sweep), sin(0.75 * TAU + sweep));

    float d = within ? abs(length(p) - r) : min(length(p - a), length(p - b));
    if (!full) {
        d = min(d, min(segment(p, a), segment(p, b)));
    }
    bool inside = within && length(p) < r;
    float sd = inside ? -d : d;

    float aa = max(fwidth(sd), 1e-4);
    float fill = 1.0 - smoothstep(-aa, aa, sd);
    float stroke = bw > 0.0 ? 1.0 - smoothstep(bw * 0.5 - aa, bw * 0.5 + aa, d) : 0.0;

    // Both premultiplied; the stroke goes over the fill, as cairo strokes after `fillPreserve`.
    vec4 f = pc.color * fill;
    vec4 s = pc.tex_transform[0] * stroke;
    o = s + f * (1.0 - s.a);
}
