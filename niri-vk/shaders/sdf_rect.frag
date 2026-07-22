#version 450

// Rounded-rectangle material via a signed-distance field — the same primitive niri draws for
// window/chrome corners (render_helpers/shaders/rounding_alpha.frag), ported to a self-contained
// SDF with analytic 1px antialiasing from screen-space derivatives.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius;
    // > 0 draws an inset stroke (ring) of this physical-px width along the inside of the edge,
    // instead of a solid fill. A focus ring, a 1px outline, etc.
    float stroke_width;
    vec4 color;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

// Signed distance to a box of half-size b with corner radius r, evaluated at p (box-centered).
float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

void main() {
    vec2 half_size = pc.size * 0.5;
    float r = clamp(pc.corner_radius, 0.0, min(half_size.x, half_size.y));
    float d = sd_round_box(v_local - half_size, half_size, r);

    // Stroke mode: keep only the inset band `-w <= d <= 0` — a ring hugging the inside of the edge,
    // its inner corners concentric (the SDF offsets a rounded shape correctly). `max(d, -(d+w))` is
    // that band's own SDF, so the same analytic AA applies. `stroke_width <= 0` → solid fill.
    if (pc.stroke_width > 0.0) {
        d = max(d, -(d + pc.stroke_width));
    }

    float aa = max(fwidth(d), 1e-4);
    float coverage = 1.0 - smoothstep(-aa, aa, d);
    o = vec4(pc.color.rgb, pc.color.a * coverage);
}
