// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#version 450

// Isoceles-triangle material via a signed-distance field, with the same analytic 1px
// antialiasing as `sdf_rect.frag`. This is GNOME's `SwitcherPopup.drawArrow`
// (`js/ui/switcherPopup.js:661-704`): a cairo path whose base spans one edge of the actor and
// whose apex is the midpoint of the opposite edge. GNOME strokes it in `border-color` and fills
// it in `color`; `.switcher-arrow` (`_switcher-popup.scss:62-70`) sets both to the same value in
// both states, so a plain fill is exact rather than an approximation.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    // Unused here; declared so the following fields land at the shared block's offsets.
    float corner_radius;
    float stroke_width;
    vec4 color;
    vec4 tex_transform[3];
    // `.x` is the side the apex points at, in `St.Side` order (0 TOP, 1 RIGHT, 2 BOTTOM,
    // 3 LEFT) — the same enum `drawArrow` switches on, so the two read alike. Declared at the
    // shared block's `cutoff` offset (112), the way `sdf_rect.frag` reads `ramp` there.
    vec2 dir;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

// Exact signed distance to the triangle (p0, p1, p2), after iq. Negative inside.
float sd_triangle(vec2 p, vec2 p0, vec2 p1, vec2 p2) {
    vec2 e0 = p1 - p0, e1 = p2 - p1, e2 = p0 - p2;
    vec2 v0 = p - p0, v1 = p - p1, v2 = p - p2;

    vec2 pq0 = v0 - e0 * clamp(dot(v0, e0) / dot(e0, e0), 0.0, 1.0);
    vec2 pq1 = v1 - e1 * clamp(dot(v1, e1) / dot(e1, e1), 0.0, 1.0);
    vec2 pq2 = v2 - e2 * clamp(dot(v2, e2) / dot(e2, e2), 0.0, 1.0);

    // Winding-independent: `s` flips the sign for a clockwise triangle so callers need not care
    // which order the three corners arrive in.
    float s = sign(e0.x * e2.y - e0.y * e2.x);
    vec2 d = min(
        min(vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
            vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
        vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)));

    return -sqrt(d.x) * sign(d.y);
}

void main() {
    float w = pc.size.x;
    float h = pc.size.y;
    // `floor` on the apex's cross-axis coordinate is `drawArrow`'s own (`Math.floor(width * 0.5)`),
    // kept so an odd-width arrow leans the same way ours does as GNOME's.
    float mx = floor(w * 0.5);
    float my = floor(h * 0.5);

    vec2 p0, p1, p2;
    int side = int(pc.dir.x + 0.5);
    if (side == 0) {          // TOP: base along the bottom edge, apex up
        p0 = vec2(0.0, h);
        p1 = vec2(mx, 0.0);
        p2 = vec2(w, h);
    } else if (side == 1) {   // RIGHT: base along the left edge, apex right
        p0 = vec2(0.0, 0.0);
        p1 = vec2(w, my);
        p2 = vec2(0.0, h);
    } else if (side == 2) {   // BOTTOM: base along the top edge, apex down
        p0 = vec2(w, 0.0);
        p1 = vec2(mx, h);
        p2 = vec2(0.0, 0.0);
    } else {                  // LEFT: base along the right edge, apex left
        p0 = vec2(w, h);
        p1 = vec2(0.0, my);
        p2 = vec2(w, 0.0);
    }

    float d = sd_triangle(v_local, p0, p1, p2);
    float aa = max(fwidth(d), 1e-4);
    float coverage = 1.0 - smoothstep(-aa, aa, d);

    // `pc.color` is premultiplied (the frame method premultiplies the toolkit's straight Rgba) and
    // the pipeline blends premultiplied-over, so coverage scales rgb and alpha together.
    o = pc.color * coverage;
}
