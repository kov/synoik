#version 450

// Dual-Kawase upsample tap (ported from render_helpers/shaders/blur_up.frag): four axis edges at
// ±2*offset plus four diagonal corners at ±offset weighted 2, normalized by 12. half_pixel is
// half a source pixel here (the source is the smaller level).

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 half_pixel;
    float offset;
    float _pad0;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

void main() {
    vec2 off = pc.half_pixel * pc.offset;

    vec4 sum = vec4(0.0);
    sum += texture(tex, v_uv + vec2(-off.x * 2.0, 0.0));
    sum += texture(tex, v_uv + vec2(off.x * 2.0, 0.0));
    sum += texture(tex, v_uv + vec2(0.0, -off.y * 2.0));
    sum += texture(tex, v_uv + vec2(0.0, off.y * 2.0));

    sum += texture(tex, v_uv + vec2(-off.x, off.y)) * 2.0;
    sum += texture(tex, v_uv + vec2(off.x, off.y)) * 2.0;
    sum += texture(tex, v_uv + vec2(-off.x, -off.y)) * 2.0;
    sum += texture(tex, v_uv + vec2(off.x, -off.y)) * 2.0;

    o = sum / 12.0;
}
