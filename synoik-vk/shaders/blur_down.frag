#version 450

// Dual-Kawase downsample tap (ported from render_helpers/shaders/blur_down.frag): center*4 plus
// the four diagonal corners at ±offset, normalized by 8. half_pixel is half a destination pixel.

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

    vec4 sum = texture(tex, v_uv) * 4.0;
    sum += texture(tex, v_uv + vec2(-off.x, -off.y));
    sum += texture(tex, v_uv + vec2(off.x, -off.y));
    sum += texture(tex, v_uv + vec2(-off.x, off.y));
    sum += texture(tex, v_uv + vec2(off.x, off.y));

    o = sum / 8.0;
}
