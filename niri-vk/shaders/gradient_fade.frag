#version 450

// Gradient-fade material: sample a bound texture (with the same partial-src remap as texture.frag)
// and fade its alpha out horizontally across `cutoff` — the owned-renderer port of niri's
// GradientFadeTextureRenderElement (the MRU window switcher fades horizontally-clipped thumbnails).

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    float corner_radius;
    float _pad0;
    vec4 color;          // straight-alpha tint, [1,1,1,alpha]
    vec4 st0;            // tex_transform columns (xyz used): v_uv -> normalized UV
    vec4 st1;
    vec4 st2;
    vec2 cutoff;         // fade band [left, right] in sampled-texture u coords; left>=right = off
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec2 v_local;
layout(location = 0) out vec4 o;

void main() {
    vec2 uv = (mat3(pc.st0.xyz, pc.st1.xyz, pc.st2.xyz) * vec3(v_uv, 1.0)).xy;
    vec4 c = texture(tex, uv) * pc.color;

    float fade = 1.0;
    if (pc.cutoff.x < pc.cutoff.y) {
        fade = clamp((pc.cutoff.y - uv.x) / (pc.cutoff.y - pc.cutoff.x), 0.0, 1.0);
    }

    // Straight-alpha blend: attenuate alpha only (the GLES shader premultiplies by fade, which is
    // equivalent once the hardware multiplies rgb by src alpha at blend time).
    o = vec4(c.rgb, c.a * fade);
}
