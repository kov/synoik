#version 450

// Textured material: sample a bound texture at the quad UV, tinted by the push-constant color
// (white = pass-through). This is the window/icon/glyph-atlas path — everything that samples
// pixels rather than generating them.

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius;
    float _pad0;
    vec4 color;
    vec4 src_rect; // sub-rect to sample, normalized [u0, v0, du, dv]
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

void main() {
    vec2 uv = pc.src_rect.xy + v_uv * pc.src_rect.zw;
    o = texture(tex, uv) * pc.color;
}
