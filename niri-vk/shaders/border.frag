#version 450

// Border material: an angled linear gradient (sRGB / sRGB-linear / Oklab / Oklch color spaces,
// with the CSS hue-interpolation modes) clipped to a rounded-rectangle ring (outer rounded rect
// minus inner rounded rect). Ported verbatim from render_helpers/shaders/border.frag +
// rounding_alpha.frag; the GLES named uniforms become the shared BorderPush push constant, and
// niri_v_coords becomes v_uv (0..1). Outputs premultiplied color (premultiplied-over blend).

layout(push_constant) uniform Push {
    vec2 origin;
    vec2 size;
    vec2 target;
    float border_width;
    float colorspace;
    vec4 color_from;
    vec4 color_to;
    vec4 outer_radius;
    vec2 grad_offset;
    vec2 grad_vec;
    vec2 area_size;
    vec2 geo_loc;
    vec2 geo_size;
    float grad_width;
    float hue_interpolation;
    float niri_scale;
    float niri_alpha;
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o;

vec4 premul_rect(vec4 color) {
    color.rgb *= color.a;
    return color;
}

vec4 premul_lch(vec4 color) {
    color.xy *= color.a;
    return color;
}

vec4 unpremul_rect(vec4 color) {
    if (color.a == 0.0)
        return color;
    color.rgb /= color.a;
    return color;
}

vec4 unpremul_lch(vec4 color) {
    if (color.a == 0.0)
        return color;
    color.xy /= color.a;
    return color;
}

vec4 premul_mix_unpremul_rect(vec4 color1, vec4 color2, float ratio) {
    vec4 mixed = mix(premul_rect(color1), premul_rect(color2), ratio);
    return unpremul_rect(mixed);
}

vec4 premul_mix_unpremul_lch(vec4 color1, vec4 color2, float ratio) {
    vec4 mixed = mix(premul_lch(color1), premul_lch(color2), ratio);
    return unpremul_lch(mixed);
}

vec3 srgb_to_linear(vec3 color) {
    return pow(color, vec3(2.2));
}

vec3 linear_to_srgb(vec3 color) {
    return pow(color, vec3(1.0 / 2.2));
}

vec3 lab_to_lch(vec3 color) {
    float c = sqrt(pow(color.y, 2.0) + pow(color.z, 2.0));
    float h = degrees(atan(color.z, color.y));
    h += h <= 0.0 ? 360.0 : 0.0;
    return vec3(color.x, c, h);
}

vec3 lch_to_lab(vec3 color) {
    float a = color.y * clamp(cos(radians(color.z)), -1.0, 1.0);
    float b = color.y * clamp(sin(radians(color.z)), -1.0, 1.0);
    return vec3(color.x, a, b);
}

vec3 linear_to_oklab(vec3 color) {
    mat3 rgb_to_lms = mat3(
        vec3(0.4122214708, 0.5363325363, 0.0514459929),
        vec3(0.2119034982, 0.6806995451, 0.1073969566),
        vec3(0.0883024619, 0.2817188376, 0.6299787005)
    );
    mat3 lms_to_oklab = mat3(
        vec3(0.2104542553, 0.7936177850, -0.0040720468),
        vec3(1.9779984951, -2.4285922050, 0.4505937099),
        vec3(0.0259040371, 0.7827717662, -0.8086757660)
    );
    vec3 lms = color * rgb_to_lms;
    lms = pow(lms, vec3(1.0 / 3.0));
    return lms * lms_to_oklab;
}

vec3 oklab_to_linear(vec3 color) {
    mat3 oklab_to_lms = mat3(
        vec3(1.0, 0.3963377774, 0.2158037573),
        vec3(1.0, -0.1055613458, -0.0638541728),
        vec3(1.0, -0.0894841775, -1.2914855480)
    );
    mat3 lms_to_rgb = mat3(
        vec3(4.0767416621, -3.3077115913, 0.2309699292),
        vec3(-1.2684380046, 2.6097574011, -0.3413193965),
        vec3(-0.0041960863, -0.7034186147, 1.7076147010)
    );
    vec3 lms = color * oklab_to_lms;
    lms = pow(lms, vec3(3.0));
    return lms * lms_to_rgb;
}

vec4 color_mix(vec4 color1, vec4 color2, float color_ratio) {
    vec4 color_out;

    // srgb
    if (pc.colorspace == 0.0) {
        return mix(premul_rect(color1), premul_rect(color2), color_ratio);
    }

    color1.rgb = srgb_to_linear(color1.rgb);
    color2.rgb = srgb_to_linear(color2.rgb);

    // srgb-linear
    if (pc.colorspace == 1.0) {
        color_out = premul_mix_unpremul_rect(color1, color2, color_ratio);
    // oklab
    } else if (pc.colorspace == 2.0) {
        color1.xyz = linear_to_oklab(color1.rgb);
        color2.xyz = linear_to_oklab(color2.rgb);
        color_out = premul_mix_unpremul_rect(color1, color2, color_ratio);
        color_out.rgb = oklab_to_linear(color_out.xyz);
    // oklch
    } else if (pc.colorspace == 3.0) {
        color1.xyz = lab_to_lch(linear_to_oklab(color1.rgb));
        color2.xyz = lab_to_lch(linear_to_oklab(color2.rgb));
        color_out = premul_mix_unpremul_lch(color1, color2, color_ratio);

        float min_hue = min(color1.z, color2.z);
        float max_hue = max(color1.z, color2.z);
        float path_direct_distance = (max_hue - min_hue) * color_ratio;
        float path_mod_distance = (360.0 - max_hue + min_hue) * color_ratio;

        float path_mod =
            color1.z == min_hue ?
                mod(color1.z - path_mod_distance, 360.0) :
                mod(color1.z + path_mod_distance, 360.0);
        float path_direct =
            color1.z == min_hue ?
                color1.z + path_direct_distance :
                color1.z - path_direct_distance;

        // shorter
        if (pc.hue_interpolation == 0.0) {
            color_out.z =
                max_hue - min_hue > 360.0 - max_hue + min_hue ? path_mod : path_direct;
        // longer
        } else if (pc.hue_interpolation == 1.0) {
            color_out.z =
                max_hue - min_hue <= 360.0 - max_hue + min_hue ? path_mod : path_direct;
        // increasing
        } else if (pc.hue_interpolation == 2.0) {
            color_out.z = color1.z > color2.z ? path_mod : path_direct;
        // decreasing
        } else if (pc.hue_interpolation == 3.0) {
            color_out.z = color1.z <= color2.z ? path_mod : path_direct;
        }
        color_out.rgb = clamp(oklab_to_linear(lch_to_lab(color_out.xyz)), 0.0, 1.0);
    }

    return premul_rect(vec4(linear_to_srgb(color_out.rgb), color_out.a));
}

vec4 gradient_color(vec2 coords) {
    coords = coords + pc.grad_offset;

    if ((pc.grad_vec.x < 0.0 && 0.0 <= pc.grad_vec.y) || (0.0 <= pc.grad_vec.x && pc.grad_vec.y < 0.0))
        coords.x -= pc.grad_width;

    float frac = dot(coords, pc.grad_vec) / dot(pc.grad_vec, pc.grad_vec);

    if (pc.grad_vec.y < 0.0)
        frac += 1.0;

    frac = clamp(frac, 0.0, 1.0);
    return color_mix(pc.color_from, pc.color_to, frac);
}

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

void main() {
    vec2 coords_geo = v_uv * pc.area_size - pc.geo_loc;
    vec4 color = gradient_color(coords_geo);
    color = color * niri_rounding_alpha(coords_geo, pc.geo_size, pc.outer_radius);

    if (pc.border_width > 0.0) {
        vec2 cg = coords_geo - vec2(pc.border_width);
        vec2 inner_geo_size = pc.geo_size - vec2(pc.border_width * 2.0);
        if (0.0 <= cg.x && cg.x <= inner_geo_size.x && 0.0 <= cg.y && cg.y <= inner_geo_size.y) {
            vec4 inner_radius = max(pc.outer_radius - vec4(pc.border_width), 0.0);
            color = color * (1.0 - niri_rounding_alpha(cg, inner_geo_size, inner_radius));
        }
    }

    color = color * pc.niri_alpha;
    o = color;
}
