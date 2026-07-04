#version 450

// Vertex stage for the shadow material. Emits the unit quad and declares the full ShadowPush block
// (so vert and frag agree on the push layout); only origin/size/target are used here.

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec2 target;
    float sigma;
    float niri_scale;
    vec4 shadow_color;
    vec4 corner_radius;
    vec4 window_corner_radius;
    vec2 area_size;
    vec2 geo_loc;
    vec2 geo_size;
    vec2 window_geo_loc;
    vec2 window_geo_size;
    float niri_alpha;
    float _pad0;
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
