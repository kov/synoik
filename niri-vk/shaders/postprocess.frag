#version 450

// Postprocess-and-clip material: sample a bound texture, apply saturation / noise / a premultiplied
// background (postprocess.frag), then clip it to a rounded rectangle expressed in a general
// geometry space (clipped_surface.frag + rounding_alpha.frag). This is the owned-renderer port of
// niri's clipped-surface / framebuffer-effect postprocess: the `clipped_surface` program is this
// with saturation=1, noise=0, bg=0; the `postprocess_and_clip` program is the full thing.
//
// Ported verbatim from render_helpers/shaders/{clipped_surface,postprocess,rounding_alpha}.frag.
// The GLES named uniforms + the input_to_geo mat3 become the PostprocessPush push constant (the
// mat3 passed as three vec4 columns, xyz used). niri_v_coords becomes v_uv (0..1 across the quad),
// used both to sample (via src_rect) and as the input to input_to_geo. Outputs premultiplied color
// (premultiplied-over blend), matching the GLES tex-program convention.

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec4 proj; // unused here; declared so the push-block offsets match the vertex stage
    vec2 target;
    vec2 geo_size;
    vec4 src_rect;       // sub-rect to sample, normalized [u0, v0, du, dv]
    vec4 corner_radius;
    vec4 bg_color;       // premultiplied
    vec4 i2g0;           // input_to_geo columns (xyz used): maps vec3(v_uv, 1) -> [0,1] geo space
    vec4 i2g1;
    vec4 i2g2;
    float niri_scale;
    float niri_alpha;
    float saturation;
    float noise;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

float niri_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius) {
    vec2 center;
    float radius;

    if (coords.x < corner_radius.x && coords.y < corner_radius.x) {
        radius = corner_radius.x;
        center = vec2(radius, radius);
    } else if (size.x - corner_radius.y < coords.x && coords.y < corner_radius.y) {
        radius = corner_radius.y;
        center = vec2(size.x - radius, radius);
    } else if (size.x - corner_radius.z < coords.x && size.y - corner_radius.z < coords.y) {
        radius = corner_radius.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (coords.x < corner_radius.w && size.y - corner_radius.w < coords.y) {
        radius = corner_radius.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }

    float dist = distance(coords, center);
    float t = clamp((dist - radius) * pc.niri_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}

// Sin-less white noise by David Hoskins (MIT License). https://www.shadertoy.com/view/4djSRW
float hash12(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

vec3 saturate_color(vec3 color, float sat) {
    const vec3 w = vec3(0.2126, 0.7152, 0.0722);
    return mix(vec3(dot(color, w)), color, sat);
}

vec4 postprocess(vec4 color) {
    if (pc.saturation != 1.0) {
        color.rgb = saturate_color(color.rgb, pc.saturation);
    }

    if (pc.noise > 0.0) {
        color.rgb += (hash12(gl_FragCoord.xy) - 0.5) * pc.noise;
    }

    // Mix bg_color behind the texture (both premultiplied alpha).
    color = color + pc.bg_color * (1.0 - color.a);

    return color;
}

void main() {
    mat3 input_to_geo = mat3(pc.i2g0.xyz, pc.i2g1.xyz, pc.i2g2.xyz);
    vec3 coords_geo = input_to_geo * vec3(v_uv, 1.0);

    vec2 uv = pc.src_rect.xy + v_uv * pc.src_rect.zw;
    vec4 color = texture(tex, uv);

    color = postprocess(color);

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside geometry.
        color = vec4(0.0);
    } else {
        // Apply corner rounding inside geometry.
        color = color * niri_rounding_alpha(coords_geo.xy * pc.geo_size, pc.geo_size, pc.corner_radius);
    }

    color = color * pc.niri_alpha;
    o = color;
}
