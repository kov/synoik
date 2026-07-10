#version 450

// Solid-fill material: the simplest quad, straight through to the push-constant color.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius;
    float _pad0;
    vec4 color;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

void main() {
    o = pc.color;
}
