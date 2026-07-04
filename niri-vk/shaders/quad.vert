#version 450

// Shared quad vertex stage: emit a unit quad (two triangles from gl_VertexIndex, no vertex
// buffer) placed at a pixel-space rect and converted to Vulkan NDC (origin top-left, y down).
// This is the seed of the unified instanced-quad pipeline; for now the rect comes via push
// constants and each material is a different fragment stage sharing this vertex stage.

layout(push_constant) uniform Push {
    vec2 origin;         // top-left in pixels
    vec2 size;           // width/height in pixels
    vec2 target;         // render target size in pixels
    float corner_radius; // pixels (used by SDF materials)
    float _pad0;
    vec4 color;          // straight-alpha RGBA
} pc;

layout(location = 0) out vec2 v_uv;    // 0..1 across the quad
layout(location = 1) out vec2 v_local; // pixels within the quad

vec2 corner(int i) {
    if (i == 0) return vec2(0.0, 0.0);
    if (i == 1) return vec2(1.0, 0.0);
    if (i == 2) return vec2(1.0, 1.0);
    if (i == 3) return vec2(0.0, 0.0);
    if (i == 4) return vec2(1.0, 1.0);
    return vec2(0.0, 1.0);
}

void main() {
    vec2 c = corner(gl_VertexIndex);
    v_uv = c;
    v_local = c * pc.size;
    vec2 p = pc.origin + c * pc.size;
    vec2 ndc = p / pc.target * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
