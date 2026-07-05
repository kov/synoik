#version 450

// Vertex stage for the postprocess-and-clip material. Emits the same unit quad as quad.vert but
// declares the full PostprocessPush block (so the vertex and fragment stages agree on the
// push-constant layout); only origin/size/target are used here. v_uv (0..1) is the fragment's
// niri_v_coords, both the sampling coordinate and the input to the geometry mapping.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec2 target;
    vec2 geo_size;
    vec4 src_rect;
    vec4 corner_radius;
    vec4 bg_color;
    vec4 i2g0;
    vec4 i2g1;
    vec4 i2g2;
    float niri_scale;
    float niri_alpha;
    float saturation;
    float noise;
} pc;

layout(location = 0) out vec2 v_uv;

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
    vec2 p = pc.origin + c * pc.size;
    vec2 ndc = p / pc.target * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
