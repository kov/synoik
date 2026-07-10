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
    vec4 st0; // tex_transform columns (xyz used): v_uv -> normalized UV (crop + buffer transform)
    vec4 st1;
    vec4 st2;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

void main() {
    vec2 uv = (mat3(pc.st0.xyz, pc.st1.xyz, pc.st2.xyz) * vec3(v_uv, 1.0)).xy;
    o = texture(tex, uv) * pc.color;
}
